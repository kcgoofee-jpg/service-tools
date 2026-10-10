"""Discord OAuth self-enrollment for restricted NAI Gate keys (no image requests)."""
from __future__ import annotations

import asyncio
import os
import secrets
import time
from urllib.parse import urlencode

import httpx

from . import features
from .policy import gen_key

COMMAND_GUILD = "1480185480048808009"
MEMBERSHIP_GUILD = "1134557553011998840"
MEMBERSHIP_ROLE = "1335363403870502912"
SITE_URL = "https://novelai.fangchen2003.asia/"


class RegistrationError(Exception):
    pass


def _clip(value, limit: int = 80):
    return str(value)[:limit] if value else None


def member_label(user: dict) -> str:
    """后台里展示的成员名：显示名（@用户名），取不到时退回用户名或 ID。"""
    username, display = _clip(user.get("username")), _clip(user.get("global_name"))
    if display and username and display != username:
        return f"{display} (@{username})"[:60]
    return (display or username or "Discord:" + str(user.get("id", "")))[:60]


def welcome_dm(key: str, site: str, quota: str, expires_days: int, idle_days: int, notice: str = "",
               opened: str = "") -> str:
    """领取成功私信：Key + 一步步配置教程 + 规则。控制在 Discord 2000 字以内。"""
    rules = [f"• 每日额度：{quota}（次日自动恢复）"]
    if opened:
        rules.append(f"• 已开通功能：{opened}")
    if expires_days:
        rules.append(f"• 有效期 {expires_days} 天，到期后可重新 /register")
    if idle_days:
        rules.append(f"• 连续 {idle_days} 天没有使用会被自动回收（回收前 1 天私信提醒）")
    rules.append("• 每把 Key 同时生成 1 张，多发的会被退回，等前一张出完再发；凌晨出图会放慢（保护上游账号）")
    rules.append("• 只提供免费出图：总像素 ≤1024×1024（尺寸可自定义，超出自动等比缩小）、≤28 步、每次 1 张；"
                 "图生图、Vibe 等会消耗 Anlas 的功能不开放")
    rules.append("• 一人一把，请勿分享（本站记录打码后的来源网段防分享，不存完整 IP，7 天后删除）")
    text = (f"🦉 **欢迎来到猫头鹰公益站！** 这是你的 API Key（只发这一次，请先保存）：\n`{key}`\n\n"
            f"**三步开始出图（以柏宝绘为例）**\n"
            f"1. 打开柏宝绘 → 渠道 → 配置 → 新建接入点\n"
            f"2. 接口地址填 `{site}`（**不要**加 /v1）\n"
            f"3. API Key 填上面 `nai-` 开头的整串，保存后随便生成一张图试试\n"
            f"其他支持自定义 NovelAI 地址的客户端同样这样填。\n\n"
            f"**不确定填对没有？** 打开 {site} 在「查看我的额度」里粘贴 Key，能显示额度就说明 Key 正常。\n\n"
            f"**规则**\n" + "\n".join(rules) + "\n\n"
            f"**常用命令**：`/quota` 看额度和服务状态 · `/resetkey` Key 丢了或泄露时换新 · `/help` 简要说明\n"
            f"遇到问题到 🛠️｜问题反馈 发截图（记得打码 Key）。")
    if notice:
        text += f"\n{notice}"
    return text[:1990]


WAITLIST_HOLD = 24 * 3600     # 候补被邀请后保留名额的时长


class RegistrationService:
    def __init__(self, db, http: httpx.AsyncClient, *, client_id: str, client_secret: str,
                 bot_token: str, bridge_secret: str, redirect_uri: str,
                 command_guild: str = COMMAND_GUILD, membership_guild: str = MEMBERSHIP_GUILD,
                 membership_role: str = MEMBERSHIP_ROLE, site_url: str = SITE_URL,
                 key_daily_images: int = 100, key_daily_v5: int = 50,
                 key_image_scope: str = "all", key_expires_days: int = 0, key_rpm: int = 5,
                 max_users: int = 0, reset_at: str = "", key_features: str | None = None,
                 min_account_days: int = 0, member_role_id: str = "",
                 admin_ids: tuple = (), idle_days: int = 0):
        self.idle_days = idle_days
        self.member_role_id = member_role_id
        self.admin_ids = tuple(admin_ids)
        self.max_users, self.reset_at, self.key_features = max_users, reset_at, key_features
        self.min_account_days = min_account_days
        self.command_guild, self.membership_guild = command_guild, membership_guild
        self.membership_role, self.site_url = membership_role, site_url
        self.key_daily_images, self.key_daily_v5 = key_daily_images, key_daily_v5
        self.key_image_scope, self.key_expires_days, self.key_rpm = key_image_scope, key_expires_days, key_rpm
        self.db, self.http = db, http
        self.client_id, self.client_secret = client_id, client_secret
        self.bot_token, self.bridge_secret = bot_token, bridge_secret
        self.redirect_uri = redirect_uri
        self.pending: dict[str, tuple[str, float]] = {}
        self.lock = asyncio.Lock()

    async def count_active(self) -> int:
        """占用名额的人数：持有未过期 Key 的已注册用户。Key 过期或被删则名额释放。

        被站长停用（暂停）的成员仍占名额：否则停用即空出名额，重新启用后会超出上限。
        """
        return await count_registered(self.db)

    async def _set_role(self, discord_id: str, grant: bool) -> bool:
        """给 / 摘「已领 Key」身份组。失败只记日志，不影响注册或撤销。"""
        if not self.member_role_id:
            return False
        try:
            response = await self.http.request(
                "PUT" if grant else "DELETE",
                f"https://discord.com/api/v10/guilds/{self.command_guild}/members/{discord_id}/roles/{self.member_role_id}",
                headers={"Authorization": "Bot " + self.bot_token, "X-Audit-Log-Reason": "NAI Gate key " + ("issued" if grant else "ended")},
                timeout=10)
            return response.status_code in (200, 204, 404)     # 404: 成员已离开服务器，也算完成
        except httpx.HTTPError:
            return False

    async def _queue_role_removal(self, discord_id: str) -> None:
        await self.db._db.execute(
            "INSERT OR REPLACE INTO pending_role_removals(discord_id, created_at) VALUES (?,?)",
            (discord_id, time.time()))
        await self.db._db.commit()

    async def release_role(self, discord_id: str) -> None:
        """摘身份组：先登记待办，成功后再划掉；失败（限流、网络）会由后台同步重试，不会永久残留。"""
        if not self.member_role_id:
            return
        await self._queue_role_removal(discord_id)
        if await self._set_role(discord_id, False):
            await self.db._db.execute("DELETE FROM pending_role_removals WHERE discord_id=?", (discord_id,))
            await self.db._db.commit()

    async def sync_roles(self) -> int:
        """后台同步：Key 已过期 / 被停用 / 被删除的成员摘掉身份组，并重试之前失败的摘除。"""
        if not self.member_role_id:
            return 0
        done = 0
        rows = await self.db._db.execute_fetchall(
            """SELECT r.discord_id FROM discord_registrations r LEFT JOIN api_keys k ON k.id=r.key_id
               WHERE r.role_granted=1 AND (k.id IS NULL OR k.enabled=0
                     OR (k.expires_at IS NOT NULL AND k.expires_at < ?))""", (time.time(),))
        for (discord_id,) in rows:
            await self._queue_role_removal(str(discord_id))
            await self.db._db.execute("UPDATE discord_registrations SET role_granted=0 WHERE discord_id=?",
                                      (str(discord_id),))
        await self.db._db.commit()
        pending = await self.db._db.execute_fetchall("SELECT discord_id FROM pending_role_removals")
        for (discord_id,) in pending:
            if await self._set_role(str(discord_id), False):
                await self.db._db.execute("DELETE FROM pending_role_removals WHERE discord_id=?", (str(discord_id),))
                # 立即提交：不能在持有未提交写事务时去等下一个 Discord 请求，
                # 否则另一连接上的计费写入会因 database is locked 失败、用量丢失。
                await self.db._db.commit()
                done += 1
        return done

    async def backfill_profiles(self, limit: int = 5) -> int:
        """给还没有 Discord 用户名 / 头像的登记补全（用机器人读取公开资料）；每次最多处理几条，避免触发限流。"""
        rows = await self.db._db.execute_fetchall(
            "SELECT discord_id, key_id FROM discord_registrations WHERE username IS NULL LIMIT ?", (limit,))
        done = 0
        for discord_id, key_id in rows:
            try:
                response = await self.http.get(f"https://discord.com/api/v10/users/{discord_id}",
                                               headers={"Authorization": "Bot " + self.bot_token}, timeout=10)
                if response.status_code != 200:
                    continue
                user = response.json()
            except (httpx.HTTPError, ValueError):
                continue
            await self.db._db.execute(
                "UPDATE discord_registrations SET username=?, display_name=?, avatar=? WHERE discord_id=?",
                (_clip(user.get("username")) or "", _clip(user.get("global_name")), _clip(user.get("avatar")), str(discord_id)))
            key = await self.db.get_key(key_id)
            if key is not None and str(key["name"]).startswith("Discord:"):
                await self.db.update_key(key_id, {"name": member_label(user)})
            await self.db._db.commit()      # 下一轮网络请求前提交，原因同 sync_roles
            done += 1
        return done

    async def registration_for_key(self, key_id: int):
        rows = await self.db._db.execute_fetchall(
            "SELECT discord_id FROM discord_registrations WHERE key_id=?", (key_id,))
        return str(rows[0][0]) if rows else None

    async def is_banned(self, discord_id: str) -> bool:
        rows = await self.db._db.execute_fetchall("SELECT 1 FROM discord_bans WHERE discord_id=?", (discord_id,))
        return bool(rows)

    async def ban(self, discord_id: str) -> None:
        """永久禁止该 Discord 账号领取：写入封禁表，并撤销其现有 Key 与身份组。

        与 finish 共用 self.lock：否则在对方 OAuth 回调进行中封禁，回调仍会发出有效 Key。
        """
        async with self.lock:
            await self.db._db.execute("INSERT OR IGNORE INTO discord_bans(discord_id, created_at) VALUES (?,?)",
                                      (discord_id, time.time()))
            await self.db._db.execute("DELETE FROM waitlist WHERE discord_id=?", (discord_id,))
            await self.db._db.commit()
            had_key = await self.revoke(discord_id, remove_role=False)
        if had_key:
            await self.release_role(discord_id)

    async def unban(self, discord_id: str) -> bool:
        cur = await self.db._db.execute("DELETE FROM discord_bans WHERE discord_id=?", (discord_id,))
        await self.db._db.commit()
        return cur.rowcount == 1

    async def _release_if_expired(self, discord_id: str, *, defer_role: bool = False) -> None:
        """成员的 Key 已过期：自动清掉旧 Key 和记录，让他可以在有名额时重新领取（每个周期自然轮换）。
        被站长停用（enabled=0）的 Key 不会被自动释放：停用等于暂停该成员，不能靠过期绕过。"""
        key = await self.key_row_for(discord_id)
        if key is not None and key["enabled"] and key["expires_at"] and key["expires_at"] < time.time():
            await self.revoke(discord_id, remove_role=not defer_role)
            from .action_log import log_action
            await log_action(self.db, "系统", "Key 到期释放名额", f"Key #{key['id']} {key['name']}",
                             f"Discord:{discord_id} 重新领取时自动释放")

    async def settings(self) -> dict:
        from .ops import registration_settings
        return await registration_settings(self.db, self)

    async def _check_capacity(self, user_id: str = "", name: str = "") -> dict:
        """名额检查 + 候补名单。被邀请（24 小时内）的候补可以占用为他保留的名额；
        其他人只有在「空位 > 排在前面还没被邀请的候补人数」时才能直接领取，否则加入候补并告知排位。"""
        cfg = await self.settings()
        if not cfg["open"]:
            raise RegistrationError("领 Key 暂未开放，请等待站长开放。")
        if not cfg["max_users"]:
            return cfg
        now = time.time()
        rows = await self.db._db.execute_fetchall(
            "SELECT discord_id, invited_at FROM waitlist ORDER BY joined_at")
        invited = {r[0] for r in rows if r[1] and now - r[1] < WAITLIST_HOLD}
        waiting = [r[0] for r in rows if not r[1]]
        free = cfg["max_users"] - await self.count_active() - len(invited - {user_id})
        if user_id in invited and free > 0:
            return cfg
        ahead = [d for d in waiting if d != user_id]
        if user_id in waiting:
            ahead = waiting[:waiting.index(user_id)]
        if free > len(ahead):
            return cfg
        if not user_id:
            raise RegistrationError(f"名额已满（上限 {cfg['max_users']} 人）。")
        if user_id not in waiting:
            await self.db._db.execute(
                "INSERT OR REPLACE INTO waitlist(discord_id, name, joined_at, invited_at) VALUES (?,?,?,NULL)",
                (user_id, _clip(name) or "", now))
            await self.db._db.commit()
            waiting.append(user_id)
            ahead = waiting[:-1]
        raise RegistrationError(
            f"名额已满（上限 {cfg['max_users']} 人）。已把你加入候补，目前排第 {len(ahead) + 1} 位；"
            "有名额时机器人会私信你，届时 24 小时内再用 /register 领取。")

    async def waitlist(self) -> list[dict]:
        rows = await self.db._db.execute_fetchall(
            "SELECT discord_id, name, joined_at, invited_at FROM waitlist ORDER BY joined_at")
        return [{"discord_id": r[0], "name": r[1], "joined_at": r[2], "invited_at": r[3]} for r in rows]

    async def invite_waitlist(self, now: float | None = None, announce=None) -> int:
        """维护循环调用：过期的邀请让给下一位；有空位就按顺序为候补保留 24 小时名额并通知。返回本次邀请数。

        通知方式由设置 waitlist_dm 决定：1（默认）逐个私信；0 = 不私信，只在公告频道发一条汇总
        （Discord 应用审核期间用 0：批量私信正是 2026-10-10 被标记的信号之一）。announce 是发公告频道的函数。"""
        now = time.time() if now is None else now
        use_dm = str(await self.db.get_setting("waitlist_dm", "1")).strip() != "0"
        cfg = await self.settings()
        expired = await self.db._db.execute_fetchall(
            "SELECT discord_id FROM waitlist WHERE invited_at IS NOT NULL AND invited_at <= ?", (now - WAITLIST_HOLD,))
        if expired:
            await self.db._db.execute("DELETE FROM waitlist WHERE invited_at IS NOT NULL AND invited_at <= ?",
                                      (now - WAITLIST_HOLD,))
            await self.db._db.commit()
            from .action_log import log_action
            await log_action(self.db, "系统", "候补邀请过期", f"{len(expired)} 人", "24 小时内未领取，名额让给下一位")
        if not cfg["open"] or not cfg["max_users"]:
            return 0
        held = (await self.db._db.execute_fetchall(
            "SELECT COUNT(*) FROM waitlist WHERE invited_at IS NOT NULL"))[0][0]
        free = cfg["max_users"] - await self.count_active() - held
        if free <= 0:
            return 0
        rows = await self.db._db.execute_fetchall(
            "SELECT discord_id, name FROM waitlist WHERE invited_at IS NULL ORDER BY joined_at LIMIT ?", (free,))
        from .action_log import log_action
        for discord_id, name in rows:
            await self.db._db.execute("UPDATE waitlist SET invited_at=? WHERE discord_id=?", (now, discord_id))
            await self.db._db.commit()
            if not use_dm:
                await log_action(self.db, "系统", "邀请候补", name or f"Discord {discord_id}",
                                 "已保留 24 小时（不私信，已在公告频道统一通知）")
                continue
            sent = await self.send_dm(discord_id,
                "🦉 猫头鹰公益站有空位了！为你保留 24 小时：请在 🔑｜领取key 输入 /register 领取。"
                "超过 24 小时未领取，名额会让给下一位候补。")
            await log_action(self.db, "系统", "邀请候补", name or f"Discord {discord_id}",
                             "已私信" if sent else "私信失败（对方可能关闭了私信），名额仍保留 24 小时", ok=sent)
        if rows and not use_dm and announce is not None:
            announce(f"🦉 **候补有空位了**：已为候补前 {len(rows)} 位保留名额 24 小时。"
                     "在候补里的朋友请到 🔑｜领取key 输入 `/register` 领取；24 小时内没领，名额会顺延给下一位。")
        return len(rows)

    async def registration_profile(self, discord_id: str) -> Optional[dict]:
        rows = await self.db._db.execute_fetchall(
            "SELECT username, display_name, avatar FROM discord_registrations WHERE discord_id=?", (discord_id,))
        if not rows:
            return {"id": str(discord_id)}
        u, d, a = rows[0]
        return {"id": str(discord_id), "username": u, "display_name": d, "avatar": a}

    async def key_row_for(self, discord_id: str):
        rows = await self.db._db.execute_fetchall(
            "SELECT key_id FROM discord_registrations WHERE discord_id=?", (discord_id,))
        return await self.db.get_key(rows[0][0]) if rows else None

    async def revoke(self, discord_id: str, *, remove_role: bool = True) -> bool:
        """删除该用户的 Key 和领取记录，名额释放，用户可重新领取。"""
        rows = await self.db._db.execute_fetchall(
            "SELECT key_id FROM discord_registrations WHERE discord_id=?", (discord_id,))
        if not rows:
            return False
        await self.db.delete_key(rows[0][0])
        await self.db._db.execute("DELETE FROM discord_registrations WHERE discord_id=?", (discord_id,))
        await self.db._db.commit()
        if remove_role:
            await self.release_role(discord_id)
        elif self.member_role_id:
            await self._queue_role_removal(discord_id)          # 由后台同步稍后执行，避免在锁内等 Discord
        return True

    async def reset_all(self) -> int:
        """清空所有自助注册用户（删 Key 和记录，保留用量账本），用户需重新 /register。"""
        rows = await self.db._db.execute_fetchall(
            """SELECT r.discord_id FROM discord_registrations r LEFT JOIN api_keys k ON k.id=r.key_id
               WHERE k.id IS NULL OR k.enabled=1""")           # 被停用的成员不在清空范围内
        for (discord_id,) in rows:
            await self.revoke(str(discord_id), remove_role=False)
        await self.sync_roles()
        if rows:
            from .action_log import log_action
            await log_action(self.db, "系统", "每日清空自助注册", "", f"清空 {len(rows)} 人（REGISTER_RESET_AT）")
        return len(rows)

    async def begin(self, user_id: str, guild_id: str, name: str = "") -> str:
        if guild_id != self.command_guild or not user_id.isdecimal():
            raise RegistrationError("请在指定服务器使用 /register。")
        if self.min_account_days:
            age_days = (time.time() * 1000 - ((int(user_id) >> 22) + 1420070400000)) / 86_400_000
            if age_days < self.min_account_days:
                raise RegistrationError(f"Discord 账号创建满 {self.min_account_days} 天后才能领 Key，请稍后再来。")
        if await self.is_banned(user_id):
            raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
        await self._release_if_expired(user_id)
        if (await self.db._db.execute_fetchall(
            "SELECT 1 FROM discord_registrations WHERE discord_id=?", (user_id,)
        )):
            raise RegistrationError("这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。")
        await self._check_capacity(user_id, name)
        self.pending = {k: v for k, v in self.pending.items() if v[1] > time.time()}
        if sum(u == user_id for u, _ in self.pending.values()) >= 2:
            raise RegistrationError("授权链接已发送，请先完成授权或稍后重试。")
        state = secrets.token_urlsafe(32)
        self.pending[state] = (user_id, time.time() + 600)
        # 只请求 identify：身份组门槛（若启用）改用机器人 Token 核验，不再要敏感的 guilds.members.read。
        # 过去申请该敏感权限 + 短时间大量授权，是本应用被 Discord 反滥用标记（680009）的主因之一。
        return "https://discord.com/oauth2/authorize?" + urlencode({
            "client_id": self.client_id, "redirect_uri": self.redirect_uri,
            "response_type": "code", "scope": self._oauth_scope(), "state": state,
        })

    def _oauth_scope(self) -> str:
        """默认只要 identify；只有后台配了跨服身份组门槛时才追加 guilds.members.read。"""
        return "identify"

    def web_login_url(self, state: str) -> str:
        """网页「用 Discord 登录」的授权链接（回调到 /login/callback，与机器人领取用的回调分开）。"""
        return "https://discord.com/oauth2/authorize?" + urlencode({
            "client_id": self.client_id, "redirect_uri": self.site_url + "login/callback",
            "response_type": "code", "scope": self._oauth_scope(), "state": state,
        })

    async def _issue_rate_blocked(self, now: float | None = None) -> bool:
        """每小时领取 Key 总数的硬上限：把短时间的授权/领取突增抹平（2026-10-10 Discord 应用因「增长异常」被标记，
        就是网页登录上线后一小时内 ~43 人集中授权触发的）。0 = 关闭。默认 12/小时，远高于自然速率、远低于会触发风控的突增。"""
        try:
            cap = int(float(await self.db.get_setting("issue_hourly_cap", 12) or 12))
        except (TypeError, ValueError):
            cap = 12
        if cap <= 0:
            return False
        now = time.time() if now is None else now
        rows = await self.db._db.execute_fetchall(
            "SELECT COUNT(*) FROM discord_registrations WHERE created_at>=?", (now - 3600,))
        return bool(rows) and int(rows[0][0]) >= cap

    async def issue_direct(self, user_id: str, guild_id: str, *, username: str = "",
                           global_name: str = "", avatar: str = "", name: str = "") -> dict:
        """/register 直接发 Key：斜杠命令的 interaction 已被 Discord 签名验明发起人身份，
        不必再走 OAuth 授权（应用被标记审查期间 OAuth 被封；这条通路同时去掉了批量私信）。
        校验（限服务器 / 账号年龄 / 封禁 / 去重 / 名额 / 可选身份组）与 OAuth 路径一致；
        身份组门槛用机器人 Token 核验。返回 {"key","message"}；不通过抛 RegistrationError。"""
        user_id = str(user_id)
        if guild_id != self.command_guild or not user_id.isdecimal():
            raise RegistrationError("请在指定服务器使用 /register。")
        if self.min_account_days:
            age_days = (time.time() * 1000 - ((int(user_id) >> 22) + 1420070400000)) / 86_400_000
            if age_days < self.min_account_days:
                raise RegistrationError(f"Discord 账号创建满 {self.min_account_days} 天后才能领 Key，请稍后再来。")
        async with self.lock:
            if await self.is_banned(user_id):
                raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
            await self._release_if_expired(user_id, defer_role=True)
            if (await self.db._db.execute_fetchall(
                    "SELECT 1 FROM discord_registrations WHERE discord_id=?", (user_id,))):
                raise RegistrationError("这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。")
            if await self._issue_rate_blocked():
                raise RegistrationError("本小时领取人数较多，为保护服务稳定已暂时限流，请过几分钟再用 /register 领取。")
            cfg = await self._check_capacity(user_id, name)
            # 身份组门槛（后台「领 Key」可配，可跨服）：用机器人 Token 查，不需要用户 OAuth
            role_guild = cfg.get("role_guild") or (self.membership_guild if self.membership_role else "")
            role_id = cfg.get("role_id") if cfg.get("role_guild") else (self.membership_role or "")
            if role_guild and role_id:
                note = cfg.get("role_note") or "指定身份组"
                try:
                    member = await self._discord("GET", f"/guilds/{role_guild}/members/{user_id}",
                                                 bearer="Bot " + self.bot_token)
                except RegistrationError:
                    raise RegistrationError(f"目前只开放给「{note}」：请先加入对应的社区服务器后再用 /register。")
                if role_id not in (member.get("roles") or []):
                    raise RegistrationError(f"目前只开放给「{note}」，没有检测到这个身份组，暂时不能领取 Key。")
            user = {"id": user_id, "username": username, "global_name": global_name, "avatar": avatar}
            key = gen_key("nai")
            row = await self.db.create_key({
                "name": "Discord:" + user_id, "token": key,
                "daily_images": cfg["daily_images"], "daily_v5": cfg["daily_v5"],
                "features": features.dump(cfg["features"]) if cfg["features"] is not None else None,
                "daily_anlas": 0, "monthly_anlas": 0, "daily_text_tokens": 0,
                "rpm": self.key_rpm, "allow_anlas": False, "allow_img2img": False,
                "exclude_global_v5": False, "image_model_scope": cfg["image_scope"],
                "expires_at": (time.time() + cfg["expires_days"] * 86400) if cfg["expires_days"] > 0 else None,
            })
            await self.db._db.execute(
                "INSERT INTO discord_registrations(discord_id,key_id,created_at,username,display_name,avatar) "
                "VALUES (?,?,?,?,?,?)",
                (user_id, row["id"], time.time(), _clip(username), _clip(global_name), _clip(avatar)))
            await self.db.update_key(row["id"], {"name": member_label(user)})
            if self.member_role_id:
                await self.db._db.execute("DELETE FROM pending_role_removals WHERE discord_id=?", (user_id,))
                await self.db._db.execute("UPDATE discord_registrations SET role_granted=1 WHERE discord_id=?", (user_id,))
            await self.db._db.execute("DELETE FROM waitlist WHERE discord_id=?", (user_id,))
            await self.db._db.commit()
            if self.member_role_id:
                try:
                    await self._set_role(user_id, True)
                except Exception:
                    pass
            from .audit import audit_disclosure
            from .ops import env_audit_defaults
            notice = await audit_disclosure(self.db, env_audit_defaults())
            try:
                base = int(float(await self.db.get_setting("guard_base_daily_images", 100) or 0))
            except (TypeError, ValueError):
                base = 0
            legacy = (f"V4.5 及以下保底 {base} 张、全站空闲时最多 {cfg['daily_images']} 张"
                      if base and cfg["daily_images"] and base < cfg["daily_images"]
                      else f"V4.5 及以下 {cfg['daily_images']} 张")
            quota = legacy + (f"；V5 {cfg['daily_v5']} 张" if cfg["daily_v5"] else "")
            opened = ("、".join(features.FEATURES[f] for f in cfg["features"])
                      if cfg["features"] is not None else "全部已开放功能")
            message = welcome_dm(key, self.site_url, quota, cfg["expires_days"], self.idle_days, notice, opened)
        return {"key": key, "message": message}

    async def web_identify(self, code: str) -> dict:
        """网页登录：用授权码换身份，返回 Discord 资料 + 是否在服务器 / 有指定身份组。不发 Key。"""
        response = await self.http.post("https://discord.com/api/oauth2/token", data={
            "client_id": self.client_id, "client_secret": self.client_secret,
            "grant_type": "authorization_code", "code": code,
            "redirect_uri": self.site_url + "login/callback",
        }, timeout=12)
        if response.status_code != 200:
            raise RegistrationError("Discord 授权失败，请重试。")
        token = response.json()["access_token"]
        user = await self._discord("GET", "/users/@me", bearer="Bearer " + token)
        cfg = await self.settings()
        guild_id = cfg.get("role_guild") or self.membership_guild
        role_id = cfg.get("role_id") if cfg.get("role_guild") else self.membership_role
        # 默认只请求 identify，查不到成员信息，一律放行；只有真的配了身份组门槛时才核验
        # （那时需要在 _oauth_scope 里把 guilds.members.read 加回来）。
        in_server = has_role = True
        if role_id:
            r = await self.http.get(f"https://discord.com/api/users/@me/guilds/{guild_id}/member",
                                    headers={"Authorization": "Bearer " + token}, timeout=12)
            in_server = r.status_code == 200
            has_role = in_server and (role_id in (r.json().get("roles") or []))
        return {"id": str(user.get("id")), "username": user.get("username"),
                "global_name": user.get("global_name"), "avatar": user.get("avatar"),
                "in_server": in_server, "has_role": has_role, "role_note": cfg.get("role_note") or ""}

    async def send_dm(self, discord_id: str, text: str) -> bool:
        """机器人私信成员；对方关闭私信等失败返回 False，不抛异常。"""
        try:
            channel = await self._discord("POST", "/users/@me/channels", bearer="Bot " + self.bot_token,
                                          json={"recipient_id": str(discord_id)})
            await self._discord("POST", f"/channels/{channel['id']}/messages", bearer="Bot " + self.bot_token,
                                json={"content": text[:1900], "allowed_mentions": {"parse": []}})
            return True
        except (RegistrationError, httpx.HTTPError, KeyError, ValueError):
            return False

    async def _discord(self, method: str, path: str, *, bearer: str, **kwargs) -> dict:
        response = await self.http.request(method, "https://discord.com/api" + path,
                                           headers={"Authorization": bearer}, timeout=12, **kwargs)
        if response.status_code >= 400:
            raise RegistrationError("Discord 身份核验或私信失败，请检查授权和私信设置后重试。")
        return response.json()

    async def finish(self, code: str, state: str) -> str:
        pending = self.pending.pop(state, None)
        if not pending or pending[1] <= time.time() or not code:
            raise RegistrationError("授权链接无效或已过期，请重新使用 /register。")
        expected_id = pending[0]
        # The state is one-use; no OAuth token is persisted.
        async with self.lock:
            if await self.is_banned(expected_id):
                raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
            await self._release_if_expired(expected_id, defer_role=True)
            if (await self.db._db.execute_fetchall(
                "SELECT 1 FROM discord_registrations WHERE discord_id=?", (expected_id,)
            )):
                raise RegistrationError("这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。")
            cfg = await self._check_capacity(expected_id)
            try:
                response = await self.http.post("https://discord.com/api/oauth2/token", data={
                    "client_id": self.client_id, "client_secret": self.client_secret,
                    "grant_type": "authorization_code", "code": code,
                    "redirect_uri": self.redirect_uri,
                }, timeout=12)
                if response.status_code != 200:
                    raise RegistrationError("Discord 授权失败，请重新使用 /register。")
                token = response.json()["access_token"]
                user = await self._discord("GET", "/users/@me", bearer="Bearer " + token)
                if str(user.get("id")) != expected_id:
                    raise RegistrationError("授权的 Discord 账号与命令发起者不一致。")
                # 限定身份组（后台「领 Key」可设，可以是别的社区的服务器）优先于启动配置
                guild_id = cfg.get("role_guild") or self.membership_guild
                role_id = cfg.get("role_id") if cfg.get("role_guild") else self.membership_role
                note = cfg.get("role_note") or "指定身份组"
                response = await self.http.get(f"https://discord.com/api/users/@me/guilds/{guild_id}/member",
                                               headers={"Authorization": "Bearer " + token}, timeout=12)
                if response.status_code == 404:
                    raise RegistrationError(f"目前只开放给「{note}」：请先加入对应的社区服务器后再用 /register。")
                if response.status_code >= 400:
                    raise RegistrationError("Discord 身份核验失败，请重新使用 /register 并同意授权。")
                if role_id and role_id not in response.json().get("roles", []):
                    raise RegistrationError(f"目前只开放给「{note}」，没有检测到这个身份组，暂时不能领取 Key。")
                channel = await self._discord("POST", "/users/@me/channels",
                    bearer="Bot " + self.bot_token, json={"recipient_id": expected_id})
                if await self.is_banned(expected_id):      # 网络等待期间可能刚被封禁
                    raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
                if await self._issue_rate_blocked():
                    raise RegistrationError("本小时领取人数较多，为保护服务稳定已暂时限流，请过几分钟再重试。")
                key = gen_key("nai")
                row = await self.db.create_key({
                    "name": "Discord:" + expected_id, "token": key,
                    "daily_images": cfg["daily_images"], "daily_v5": cfg["daily_v5"],
                    "features": features.dump(cfg["features"]) if cfg["features"] is not None else None,
                    "daily_anlas": 0, "monthly_anlas": 0, "daily_text_tokens": 0,
                    "rpm": self.key_rpm,
                    "allow_anlas": False, "allow_img2img": False,
                    "exclude_global_v5": False, "image_model_scope": cfg["image_scope"],
                    "expires_at": (time.time() + cfg["expires_days"] * 86400)
                                  if cfg["expires_days"] > 0 else None,
                })
                await self.db._db.execute(
                    "INSERT INTO discord_registrations(discord_id,key_id,created_at,username,display_name,avatar) VALUES (?,?,?,?,?,?)",
                    (expected_id, row["id"], time.time(), _clip(user.get("username")), _clip(user.get("global_name")),
                     _clip(user.get("avatar"))))
                await self.db.update_key(row["id"], {"name": member_label(user)})
                await self.db._db.commit()
                from .audit import audit_disclosure
                from .ops import env_audit_defaults
                notice = await audit_disclosure(self.db, env_audit_defaults())
                try:
                    base = int(float(await self.db.get_setting("guard_base_daily_images", 100) or 0))
                except (TypeError, ValueError):
                    base = 0
                legacy = (f"V4.5 及以下保底 {base} 张、全站空闲时最多 {cfg['daily_images']} 张"
                          if base and cfg["daily_images"] and base < cfg["daily_images"]
                          else f"V4.5 及以下 {cfg['daily_images']} 张")
                quota = legacy + (f"；V5 {cfg['daily_v5']} 张" if cfg["daily_v5"] else "")
                opened = ("、".join(features.FEATURES[f] for f in cfg["features"])
                          if cfg["features"] is not None else "全部已开放功能")
                try:
                    await self._discord("POST", f"/channels/{channel['id']}/messages",
                        bearer="Bot " + self.bot_token,
                        json={"content": welcome_dm(key, self.site_url, quota, cfg["expires_days"], self.idle_days, notice, opened),
                              "allowed_mentions": {"parse": []}})
                except Exception:
                    await self.db._db.execute("DELETE FROM discord_registrations WHERE discord_id=?", (expected_id,))
                    await self.db.delete_key(row["id"])
                    raise
                if self.member_role_id:
                    # 先记标志再调用 Discord：即使中途崩溃，到期同步也会尝试摘除，不会残留。
                    # 同时清掉此前失败遗留的“待摘除”记录，否则下一轮同步会摘掉刚发的新身份组。
                    await self.db._db.execute("DELETE FROM pending_role_removals WHERE discord_id=?", (expected_id,))
                    await self.db._db.execute("UPDATE discord_registrations SET role_granted=1 WHERE discord_id=?",
                                              (expected_id,))
                    await self.db._db.commit()
                    await self._set_role(expected_id, True)
                await self.db._db.execute("DELETE FROM waitlist WHERE discord_id=?", (expected_id,))
                await self.db._db.commit()
                return "sent"
            except (httpx.HTTPError, KeyError, ValueError) as exc:
                raise RegistrationError("Discord 服务暂时不可用，请稍后重试。") from exc


async def count_registered(db) -> int:
    rows = await db.execute_fetchall_compat(
        """SELECT COUNT(*) FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id
           WHERE k.expires_at IS NULL OR k.expires_at > ?""", (time.time(),))
    return int(rows[0][0])


def configured_service(db, http: httpx.AsyncClient) -> RegistrationService | None:
    """全部配置来自环境变量；缺任意一项则自助注册保持关闭。
    DISCORD_GUILD_ID 为发出 /register 的服务器；DISCORD_ROLE_ID 留空则该服务器任意成员可领取。"""
    names = ("DISCORD_CLIENT_ID", "DISCORD_CLIENT_SECRET", "DISCORD_BOT_TOKEN",
             "REGISTRATION_BRIDGE_SECRET", "DISCORD_GUILD_ID", "SITE_URL")
    if not all(os.getenv(name) for name in names):
        return None
    if len(os.environ["REGISTRATION_BRIDGE_SECRET"]) < 32:
        print("[warn] REGISTRATION_BRIDGE_SECRET 少于 32 个字符，自助注册保持关闭。")
        return None
    site = os.environ["SITE_URL"].rstrip("/") + "/"
    guild = os.environ["DISCORD_GUILD_ID"].strip()

    def number(name: str, default: int) -> int:
        try:
            return max(0, int(os.getenv(name, default)))
        except ValueError:
            return default

    return RegistrationService(db, http, client_id=os.environ["DISCORD_CLIENT_ID"],
        client_secret=os.environ["DISCORD_CLIENT_SECRET"], bot_token=os.environ["DISCORD_BOT_TOKEN"],
        bridge_secret=os.environ["REGISTRATION_BRIDGE_SECRET"], redirect_uri=site + "self-register/callback",
        command_guild=guild, membership_guild=guild, membership_role=os.getenv("DISCORD_ROLE_ID", "").strip(),
        site_url=site, key_daily_images=number("REGISTER_DAILY_IMAGES", 30),
        key_daily_v5=number("REGISTER_DAILY_V5", 0),
        key_image_scope="all" if os.getenv("REGISTER_IMAGE_SCOPE") == "all" else "legacy",
        key_expires_days=number("REGISTER_EXPIRES_DAYS", 30), key_rpm=max(1, number("REGISTER_RPM", 5)),
        max_users=number("REGISTER_MAX_USERS", 10), reset_at=os.getenv("REGISTER_RESET_AT", "").strip(),
        key_features=os.getenv("REGISTER_FEATURES", "image").strip(),
        min_account_days=number("REGISTER_MIN_ACCOUNT_DAYS", 7),
        member_role_id=os.getenv("DISCORD_MEMBER_ROLE_ID", "").strip(),
        admin_ids=tuple(x.strip() for x in os.getenv("ADMIN_DISCORD_IDS", "").split(",") if x.strip().isdecimal()),
        idle_days=number("KEY_INACTIVITY_DELETE_DAYS", 3))
