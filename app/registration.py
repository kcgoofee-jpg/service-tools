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


class RegistrationService:
    def __init__(self, db, http: httpx.AsyncClient, *, client_id: str, client_secret: str,
                 bot_token: str, bridge_secret: str, redirect_uri: str,
                 command_guild: str = COMMAND_GUILD, membership_guild: str = MEMBERSHIP_GUILD,
                 membership_role: str = MEMBERSHIP_ROLE, site_url: str = SITE_URL,
                 key_daily_images: int = 100, key_daily_v5: int = 50,
                 key_image_scope: str = "all", key_expires_days: int = 0, key_rpm: int = 5,
                 max_users: int = 0, reset_at: str = "", key_features: str | None = None,
                 min_account_days: int = 0, member_role_id: str = "",
                 admin_ids: tuple = ()):
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

    async def _check_capacity(self) -> dict:
        cfg = await self.settings()
        if not cfg["open"]:
            raise RegistrationError("注册暂未开放，请等待站长开放。")
        if cfg["max_users"] and await self.count_active() >= cfg["max_users"]:
            raise RegistrationError(f"名额已满（上限 {cfg['max_users']} 人），请联系站长。")
        return cfg

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

    async def begin(self, user_id: str, guild_id: str) -> str:
        if guild_id != self.command_guild or not user_id.isdecimal():
            raise RegistrationError("请在指定服务器使用 /register。")
        if self.min_account_days:
            age_days = (time.time() * 1000 - ((int(user_id) >> 22) + 1420070400000)) / 86_400_000
            if age_days < self.min_account_days:
                raise RegistrationError(f"Discord 账号注册满 {self.min_account_days} 天后才能领取，请稍后再来。")
        if await self.is_banned(user_id):
            raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
        await self._release_if_expired(user_id)
        if (await self.db._db.execute_fetchall(
            "SELECT 1 FROM discord_registrations WHERE discord_id=?", (user_id,)
        )):
            raise RegistrationError("这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。")
        await self._check_capacity()
        self.pending = {k: v for k, v in self.pending.items() if v[1] > time.time()}
        if sum(u == user_id for u, _ in self.pending.values()) >= 2:
            raise RegistrationError("授权链接已发送，请先完成授权或稍后重试。")
        state = secrets.token_urlsafe(32)
        self.pending[state] = (user_id, time.time() + 600)
        return "https://discord.com/oauth2/authorize?" + urlencode({
            "client_id": self.client_id, "redirect_uri": self.redirect_uri,
            "response_type": "code", "scope": "identify guilds.members.read", "state": state,
        })

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
            cfg = await self._check_capacity()
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
                member = await self._discord("GET", f"/users/@me/guilds/{self.membership_guild}/member",
                                             bearer="Bearer " + token)
                if self.membership_role and self.membership_role not in member.get("roles", []):
                    raise RegistrationError("未检测到指定身份组，无法领取 Key。")
                channel = await self._discord("POST", "/users/@me/channels",
                    bearer="Bot " + self.bot_token, json={"recipient_id": expected_id})
                if await self.is_banned(expected_id):      # 网络等待期间可能刚被封禁
                    raise RegistrationError("这个 Discord 账号已被站长停用，无法领取 Key。")
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
                from .audit import audit_flags, audit_notice
                from .ops import env_audit_defaults
                notice = audit_notice(*(await audit_flags(self.db, env_audit_defaults())))
                quota = f"V4.5 及以下 {cfg['daily_images']} 张" + (
                    f"；V5 {cfg['daily_v5']} 张" if cfg["daily_v5"] else "")
                if cfg["features"] is not None:
                    quota += "。已开通：" + "、".join(features.FEATURES[f] for f in cfg["features"])
                try:
                    await self._discord("POST", f"/channels/{channel['id']}/messages",
                        bearer="Bot " + self.bot_token,
                        json={"content": f"你的猫头鹰公益站 API Key：`{key}`\n网址：{self.site_url}\n每日额度：{quota}。请勿公开分享此 Key（本站会记录来源网段用于防分享，不保存完整 IP，7 天后删除）。" + (f"\n{notice}" if notice else ""),
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
        admin_ids=tuple(x.strip() for x in os.getenv("ADMIN_DISCORD_IDS", "").split(",") if x.strip().isdecimal()))
