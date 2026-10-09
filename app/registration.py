"""Discord OAuth self-enrollment for restricted NAI Gate keys (no image requests)."""
from __future__ import annotations

import asyncio
import os
import secrets
import time
from urllib.parse import urlencode

import httpx

from .policy import gen_key

COMMAND_GUILD = "1480185480048808009"
MEMBERSHIP_GUILD = "1134557553011998840"
MEMBERSHIP_ROLE = "1335363403870502912"
SITE_URL = "https://novelai.fangchen2003.asia/"


class RegistrationError(Exception):
    pass


class RegistrationService:
    def __init__(self, db, http: httpx.AsyncClient, *, client_id: str, client_secret: str,
                 bot_token: str, bridge_secret: str, redirect_uri: str,
                 command_guild: str = COMMAND_GUILD, membership_guild: str = MEMBERSHIP_GUILD,
                 membership_role: str = MEMBERSHIP_ROLE, site_url: str = SITE_URL,
                 key_daily_images: int = 100, key_daily_v5: int = 50,
                 key_image_scope: str = "all", key_expires_days: int = 0, key_rpm: int = 5,
                 max_users: int = 0, reset_at: str = ""):
        self.max_users, self.reset_at = max_users, reset_at
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
        """仍持有有效 Key 的已注册用户数（Key 被删则名额释放）。"""
        rows = await self.db._db.execute_fetchall(
            "SELECT COUNT(*) FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id")
        return int(rows[0][0])

    async def _check_capacity(self) -> None:
        if self.max_users and await self.count_active() >= self.max_users:
            raise RegistrationError(f"名额已满（上限 {self.max_users} 人），请联系站长。")

    async def key_row_for(self, discord_id: str):
        rows = await self.db._db.execute_fetchall(
            "SELECT key_id FROM discord_registrations WHERE discord_id=?", (discord_id,))
        return await self.db.get_key(rows[0][0]) if rows else None

    async def revoke(self, discord_id: str) -> bool:
        """删除该用户的 Key 和领取记录，名额释放，用户可重新领取。"""
        rows = await self.db._db.execute_fetchall(
            "SELECT key_id FROM discord_registrations WHERE discord_id=?", (discord_id,))
        if not rows:
            return False
        await self.db.delete_key(rows[0][0])
        await self.db._db.execute("DELETE FROM discord_registrations WHERE discord_id=?", (discord_id,))
        await self.db._db.commit()
        return True

    async def reset_all(self) -> int:
        """清空所有自助注册用户（删 Key 和记录，保留用量账本），用户需重新 /register。"""
        rows = await self.db._db.execute_fetchall("SELECT discord_id FROM discord_registrations")
        for (discord_id,) in rows:
            await self.revoke(str(discord_id))
        return len(rows)

    async def begin(self, user_id: str, guild_id: str) -> str:
        if guild_id != self.command_guild or not user_id.isdecimal():
            raise RegistrationError("请在指定服务器使用 /register。")
        if (await self.db._db.execute_fetchall(
            "SELECT 1 FROM discord_registrations WHERE discord_id=?", (user_id,)
        )):
            raise RegistrationError("这个 Discord 账号已经领取过 Key。")
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
            if (await self.db._db.execute_fetchall(
                "SELECT 1 FROM discord_registrations WHERE discord_id=?", (expected_id,)
            )):
                raise RegistrationError("这个 Discord 账号已经领取过 Key。")
            await self._check_capacity()
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
                key = gen_key("nai")
                row = await self.db.create_key({
                    "name": "Discord:" + expected_id, "token": key,
                    "daily_images": self.key_daily_images, "daily_v5": self.key_daily_v5,
                    "daily_anlas": 0, "monthly_anlas": 0, "daily_text_tokens": 0,
                    "rpm": self.key_rpm,
                    "allow_anlas": False, "allow_img2img": False,
                    "exclude_global_v5": False, "image_model_scope": self.key_image_scope,
                    "expires_at": (time.time() + self.key_expires_days * 86400)
                                  if self.key_expires_days > 0 else None,
                })
                await self.db._db.execute(
                    "INSERT INTO discord_registrations(discord_id,key_id,created_at) VALUES (?,?,?)",
                    (expected_id, row["id"], time.time()))
                await self.db._db.commit()
                from .audit import audit_notice
                notice = audit_notice(os.getenv("AUDIT_PROMPTS", "").lower() in ("1", "true", "yes", "on"),
                                      os.getenv("AUDIT_THUMBS", "").lower() in ("1", "true", "yes", "on"),
                                      int(os.getenv("AUDIT_RETENTION_DAYS", "7") or 7))
                quota = f"V4.5 及以下 {self.key_daily_images} 张" + (
                    f"；V5 {self.key_daily_v5} 张" if self.key_daily_v5 else "")
                try:
                    await self._discord("POST", f"/channels/{channel['id']}/messages",
                        bearer="Bot " + self.bot_token,
                        json={"content": f"你的 NAI Gate API Key：`{key}`\n网址：{self.site_url}\n每日额度：{quota}。请勿公开分享此 Key。" + (f"\n{notice}" if notice else ""),
                              "allowed_mentions": {"parse": []}})
                except Exception:
                    await self.db._db.execute("DELETE FROM discord_registrations WHERE discord_id=?", (expected_id,))
                    await self.db.delete_key(row["id"])
                    raise
                return "sent"
            except (httpx.HTTPError, KeyError, ValueError) as exc:
                raise RegistrationError("Discord 服务暂时不可用，请稍后重试。") from exc


def configured_service(db, http: httpx.AsyncClient) -> RegistrationService | None:
    """全部配置来自环境变量；缺任意一项则自助注册保持关闭。
    DISCORD_GUILD_ID 为发出 /register 的服务器；DISCORD_ROLE_ID 留空则该服务器任意成员可领取。"""
    names = ("DISCORD_CLIENT_ID", "DISCORD_CLIENT_SECRET", "DISCORD_BOT_TOKEN",
             "REGISTRATION_BRIDGE_SECRET", "DISCORD_GUILD_ID", "SITE_URL")
    if not all(os.getenv(name) for name in names):
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
        max_users=number("REGISTER_MAX_USERS", 0), reset_at=os.getenv("REGISTER_RESET_AT", "").strip())
