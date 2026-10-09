"""No billable upstream calls: Discord OAuth enrollment contract."""
import asyncio
import tempfile
import time
import unittest
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import httpx
from fastapi import FastAPI

from app.database import Database
from app.registration import RegistrationService, RegistrationError
from app.registration_routes import router


class RegistrationTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = Database(str(Path(self.tmp.name) / "gate.sqlite"))
        await self.db.connect()
        self.calls = []
        self.dm_fails = False
        self.roles = ["1335363403870502912"]

        def discord(request):
            self.calls.append((request.method, request.url.path))
            if request.url.path == "/api/oauth2/token":
                return httpx.Response(200, json={"access_token": "temporary-user-token"})
            if request.url.path == "/api/users/@me":
                return httpx.Response(200, json={"id": "777"})
            if request.url.path == "/api/users/@me/guilds/1134557553011998840/member":
                return httpx.Response(200, json={"roles": self.roles})
            if request.url.path == "/api/users/@me/channels":
                return httpx.Response(200, json={"id": "dm-1"})
            if request.url.path == "/api/channels/dm-1/messages":
                self.dm_body = request.content.decode()
                return httpx.Response(403 if self.dm_fails else 200, json={})
            raise AssertionError(f"Unexpected Discord request {request.method} {request.url}")
        await self.db.set_setting("register_open", "1")        # registration is closed by default (safe default)
        self.http = httpx.AsyncClient(transport=httpx.MockTransport(discord), base_url="https://discord.com")
        self.service = RegistrationService(self.db, self.http, client_id="client-id", client_secret="client-secret",
            bot_token="fake-bot-token", bridge_secret="bridge-secret",
            redirect_uri="https://novelai.fangchen2003.asia/self-register/callback")

    async def asyncTearDown(self):
        await self.http.aclose()
        await self.db.close()
        self.tmp.cleanup()

    async def begin(self, user="777", guild="1480185480048808009"):
        link = await self.service.begin(user, guild)
        return parse_qs(urlparse(link).query)["state"][0]

    async def test_valid_role_receives_key_and_site_in_dm_once(self):
        state = await self.begin()
        result = await self.service.finish("auth-code", state)
        self.assertEqual(result, "sent")
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 1)
        row = (await self.db._db.execute_fetchall("SELECT daily_v5,daily_images,allow_anlas,allow_img2img,exclude_global_v5,image_model_scope FROM api_keys"))[0]
        self.assertEqual(tuple(row), (50, 100, 0, 0, 0, "all"))
        self.assertEqual((await self.db._db.execute_fetchall("SELECT discord_id FROM discord_registrations"))[0][0], "777")
        self.assertIn(("POST", "/api/channels/dm-1/messages"), self.calls)
        self.assertIn("https://novelai.fangchen2003.asia/", self.dm_body)
        self.assertIn("nai-", self.dm_body)
        with self.assertRaises(RegistrationError):
            await self.service.finish("auth-code", state)
        with self.assertRaises(RegistrationError):
            await self.begin()

    async def test_wrong_server_and_role_never_mint(self):
        with self.assertRaises(RegistrationError):
            await self.begin(guild="1134557553011998840")
        self.roles = []
        with self.assertRaises(RegistrationError):
            await self.service.finish("auth-code", await self.begin())
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 0)

    async def test_dm_failure_rolls_back_key_and_allows_retry(self):
        self.dm_fails = True
        with self.assertRaises(RegistrationError):
            await self.service.finish("auth-code", await self.begin())
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 0)
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM discord_registrations"))[0][0], 0)
        self.dm_fails = False
        self.assertEqual(await self.service.finish("auth-code", await self.begin()), "sent")

    async def test_oauth_user_mismatch_never_mint(self):
        state = await self.begin(user="778")
        with self.assertRaises(RegistrationError):
            await self.service.finish("auth-code", state)
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 0)

    async def test_http_intent_requires_bridge_secret_and_callback_only_reports_status(self):
        app = FastAPI()
        app.include_router(router)
        app.state.registrar = self.service
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="https://novelai.fangchen2003.asia") as client:
            denied = await client.post("/self-register/intent", json={"discord_id": "777", "guild_id": "1480185480048808009"})
            self.assertEqual(denied.status_code, 401)
            allowed = await client.post("/self-register/intent", headers={"Authorization": "Bearer bridge-secret"},
                json={"discord_id": "777", "guild_id": "1480185480048808009"})
            self.assertEqual(allowed.status_code, 200)
            state = parse_qs(urlparse(allowed.json()["url"]).query)["state"][0]
            done = await client.get("/self-register/callback", params={"code": "auth-code", "state": state})
            self.assertEqual(done.status_code, 200)
            self.assertNotIn("nai-", done.text)
            self.assertEqual(done.headers["cache-control"], "no-store")
            self.assertEqual(done.headers["referrer-policy"], "no-referrer")


class ConfiguredServiceTests(unittest.IsolatedAsyncioTestCase):
    def _env(self, **extra):
        base = dict(DISCORD_CLIENT_ID="c", DISCORD_CLIENT_SECRET="s", DISCORD_BOT_TOKEN="b",
                    REGISTRATION_BRIDGE_SECRET="x" * 32, DISCORD_GUILD_ID="123", SITE_URL="https://gate.example.com")
        base.update(extra)
        return base

    async def test_disabled_unless_fully_configured(self):
        from unittest.mock import patch
        from app.registration import configured_service
        env = self._env()
        del env["SITE_URL"]
        with patch.dict("os.environ", env, clear=True):
            self.assertIsNone(configured_service(None, None))

    async def test_env_values_drive_guild_role_site_and_key_defaults(self):
        from unittest.mock import patch
        from app.registration import configured_service
        with patch.dict("os.environ", self._env(DISCORD_ROLE_ID="999"), clear=True):
            svc = configured_service(None, None)
        self.assertEqual((svc.command_guild, svc.membership_guild, svc.membership_role), ("123", "123", "999"))
        self.assertEqual(svc.site_url, "https://gate.example.com/")
        self.assertEqual((svc.key_daily_images, svc.key_daily_v5, svc.key_image_scope, svc.key_expires_days),
                         (30, 0, "legacy", 30))
        self.assertEqual(svc.redirect_uri, "https://gate.example.com/self-register/callback")
        with self.assertRaises(RegistrationError):
            await svc.begin("777", "1480185480048808009")   # the original author's guild is not accepted


class CapacityAndAdminTests(RegistrationTests):
    """Reuses the Discord mock from RegistrationTests (inherited tests re-run harmlessly)."""

    async def mint(self, user="777"):
        self.service.max_users = 0
        link = await self.service.begin(user, "1480185480048808009")
        state = parse_qs(urlparse(link).query)["state"][0]
        return await self.service.finish("auth-code", state)

    async def test_capacity_blocks_new_registrations_and_revoke_frees_slot(self):
        await self.mint()
        self.assertEqual(await self.service.count_active(), 1)
        self.service.max_users = 1
        with self.assertRaises(RegistrationError):
            await self.service.begin("888", "1480185480048808009")
        self.assertTrue(await self.service.revoke("777"))
        self.assertEqual(await self.service.count_active(), 0)
        link = await self.service.begin("888", "1480185480048808009")   # slot is free again
        self.assertIn("state=", link)
        self.assertFalse(await self.service.revoke("777"))

    async def test_reset_all_removes_keys_and_registrations_so_users_can_return(self):
        await self.mint()
        self.assertEqual(await self.service.reset_all(), 1)
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 0)
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM discord_registrations"))[0][0], 0)
        self.assertEqual(await self.mint(), "sent")

    async def test_quota_and_resetkey_endpoints_need_secret_and_right_guild(self):
        await self.mint()
        app = FastAPI()
        app.include_router(router)
        app.state.registrar = self.service
        from types import SimpleNamespace
        app.state.gate = SimpleNamespace(db=self.db, day=lambda: "2026-10-09")
        auth = {"Authorization": "Bearer bridge-secret"}
        body = {"discord_id": "777", "guild_id": "1480185480048808009"}
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="https://x") as client:
            self.assertEqual((await client.post("/self-register/quota", json=body)).status_code, 401)
            self.assertEqual((await client.post("/self-register/quota", headers=auth,
                json={**body, "guild_id": "1"})).status_code, 403)
            quota = await client.post("/self-register/quota", headers=auth, json=body)
            self.assertEqual(quota.status_code, 200)
            self.assertEqual(quota.json()["daily_images"], 100)
            old = (await self.db._db.execute_fetchall("SELECT token FROM api_keys"))[0][0]
            new = (await client.post("/self-register/resetkey", headers=auth, json=body)).json()["key"]
            self.assertNotEqual(old, new)
            self.assertIsNone(await self.db.get_key_by_token(old))
            self.assertIsNotNone(await self.db.get_key_by_token(new))
            self.assertEqual((await client.post("/self-register/quota", headers=auth,
                json={**body, "discord_id": "999"})).status_code, 404)


class OpenRegistrationTests(RegistrationTests):
    """First-come-first-served registration: no role needed, expired members rotate out, young accounts blocked."""

    async def mint(self, user="777"):
        link = await self.service.begin(user, "1480185480048808009")
        return await self.service.finish("auth-code", parse_qs(urlparse(link).query)["state"][0])

    async def test_no_role_required_when_role_is_empty(self):
        self.service.membership_role = ""
        self.roles = []
        self.assertEqual(await self.mint(), "sent")

    async def test_expired_key_frees_slot_and_member_can_register_again(self):
        self.service.max_users = 1
        await self.mint()
        self.assertEqual(await self.service.count_active(), 1)
        with self.assertRaises(RegistrationError):
            await self.service.begin("888", "1480185480048808009")      # full
        await self.db._db.execute("UPDATE api_keys SET expires_at=?", (time.time() - 5,))
        await self.db._db.commit()
        self.assertEqual(await self.service.count_active(), 0)           # expired key no longer holds the slot
        self.assertEqual([w["discord_id"] for w in await self.service.waitlist()], ["888"])   # 名额满时进了候补
        with self.assertRaises(RegistrationError) as caught:
            await self.service.begin("777", "1480185480048808009")      # 候补排在前面，后来者不能插队
        self.assertIn("第 2 位", str(caught.exception))
        self.assertIn("state=", await self.service.begin("888", "1480185480048808009"))   # 排第一的候补可以领
        self.assertEqual([w["discord_id"] for w in await self.service.waitlist()], ["888", "777"])

    async def test_waitlist_invites_in_order_and_holds_slot_24h(self):
        from unittest.mock import AsyncMock
        from app.registration import WAITLIST_HOLD
        self.service.max_users = 1
        self.service.send_dm = AsyncMock(return_value=True)
        await self.mint()
        for who in ("901", "902"):
            with self.assertRaises(RegistrationError):
                await self.service.begin(who, "1480185480048808009", name="u" + who)
        self.assertEqual([w["name"] for w in await self.service.waitlist()], ["u901", "u902"])
        self.assertEqual(await self.service.invite_waitlist(), 0)                     # 没有空位
        await self.db._db.execute("UPDATE api_keys SET expires_at=?", (time.time() - 5,))
        await self.db._db.commit()
        self.assertEqual(await self.service.invite_waitlist(), 1)                     # 只邀请第一位
        self.assertEqual(self.service.send_dm.await_args.args[0], "901")
        with self.assertRaises(RegistrationError):
            await self.service.begin("903", "1480185480048808009")                    # 名额为 901 保留
        self.assertIn("state=", await self.service.begin("901", "1480185480048808009"))
        later = time.time() + WAITLIST_HOLD + 1
        self.assertEqual(await self.service.invite_waitlist(now=later), 1)            # 901 过期 → 邀请 902
        self.assertEqual(self.service.send_dm.await_args.args[0], "902")
        self.assertEqual([w["discord_id"] for w in await self.service.waitlist()], ["902", "903"])

    async def test_registration_and_ban_leave_the_waitlist(self):
        self.service.max_users = 1
        await self.db._db.execute("INSERT INTO waitlist(discord_id, name, joined_at) VALUES ('777','',0), ('999','',1)")
        await self.db._db.commit()
        self.assertEqual(await self.mint("777"), "sent")
        await self.service.ban("999")
        self.assertEqual(await self.service.waitlist(), [])

    async def test_young_discord_accounts_cannot_grab_slots(self):
        self.service.min_account_days = 7
        young = str(((int(time.time() * 1000) - 86_400_000) - 1420070400000) << 22)   # created yesterday
        with self.assertRaises(RegistrationError):
            await self.service.begin(young, "1480185480048808009")
        old = str(((int(time.time() * 1000) - 30 * 86_400_000) - 1420070400000) << 22)
        self.assertIn("state=", await self.service.begin(old, "1480185480048808009"))


class MemberRoleTests(RegistrationTests):
    """Registering grants the 'has key' role; expiry/revoke/delete remove it; Discord errors never block registration."""

    async def asyncSetUp(self):
        await super().asyncSetUp()
        self.role_calls = []
        self.role_status = 204
        inner = self.http._transport

        def with_roles(request):
            if "/roles/" in request.url.path:
                self.role_calls.append((request.method, request.url.path.rsplit("/", 3)[-3:]))
                return httpx.Response(self.role_status)
            return inner.handler(request)
        self.http = httpx.AsyncClient(transport=httpx.MockTransport(with_roles), base_url="https://discord.com")
        self.service.http = self.http
        self.service.member_role_id = "555"

    async def mint(self, user="777"):
        link = await self.service.begin(user, "1480185480048808009")
        return await self.service.finish("auth-code", parse_qs(urlparse(link).query)["state"][0])

    async def test_role_granted_on_success_and_removed_on_revoke(self):
        self.assertEqual(await self.mint(), "sent")
        self.assertEqual(self.role_calls[0][0], "PUT")
        flag = (await self.db._db.execute_fetchall("SELECT role_granted FROM discord_registrations"))[0][0]
        self.assertEqual(flag, 1)
        await self.service.revoke("777")
        self.assertEqual(self.role_calls[-1][0], "DELETE")

    async def test_discord_role_failure_does_not_break_registration(self):
        self.role_status = 403
        self.assertEqual(await self.mint(), "sent")
        flag = (await self.db._db.execute_fetchall("SELECT role_granted FROM discord_registrations"))[0][0]
        self.assertEqual(flag, 1)                    # marked first so a later expiry sync always tries to remove it

    async def test_sync_removes_role_when_key_expires_or_is_deleted(self):
        await self.mint()
        self.assertEqual(await self.service.sync_roles(), 0)             # still valid
        await self.db._db.execute("UPDATE api_keys SET expires_at=?", (time.time() - 5,))
        await self.db._db.commit()
        self.assertEqual(await self.service.sync_roles(), 1)
        self.assertEqual(self.role_calls[-1][0], "DELETE")
        self.assertEqual(await self.service.sync_roles(), 0)             # flag cleared, idempotent

    async def test_inactivity_cleanup_hook_removes_role(self):
        await self.mint()
        key_id = (await self.db._db.execute_fetchall("SELECT key_id FROM discord_registrations"))[0][0]
        ids = await self.db.forget_registration_for_key(key_id)
        self.assertEqual(ids, ["777"])


class HardeningTests(MemberRoleTests):
    """Bans, suspended members, deferred role removal retries."""

    async def test_short_bridge_secret_keeps_feature_off(self):
        from unittest.mock import patch
        from app.registration import configured_service
        env = dict(DISCORD_CLIENT_ID="c", DISCORD_CLIENT_SECRET="s", DISCORD_BOT_TOKEN="b",
                   REGISTRATION_BRIDGE_SECRET="short", DISCORD_GUILD_ID="123", SITE_URL="https://x")
        with patch.dict("os.environ", env, clear=True):
            self.assertIsNone(configured_service(None, None))

    async def test_ban_blocks_registration_and_survives_reset_and_expiry(self):
        await self.mint("777")
        await self.service.ban("777")
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM api_keys"))[0][0], 0)
        with self.assertRaises(RegistrationError):
            await self.service.begin("777", "1480185480048808009")
        await self.service.reset_all()
        with self.assertRaises(RegistrationError):
            await self.service.begin("777", "1480185480048808009")            # still banned
        self.assertTrue(await self.service.unban("777"))
        self.assertIn("state=", await self.service.begin("777", "1480185480048808009"))

    async def test_disabled_member_is_not_released_by_expiry_or_reset_all(self):
        await self.mint("777")
        await self.db._db.execute("UPDATE api_keys SET enabled=0, expires_at=?", (time.time() - 5,))
        await self.db._db.commit()
        with self.assertRaises(RegistrationError):
            await self.service.begin("777", "1480185480048808009")            # suspended, not auto-released
        self.assertEqual(await self.service.reset_all(), 0)
        self.assertEqual((await self.db._db.execute_fetchall("SELECT count(*) FROM discord_registrations"))[0][0], 1)
        self.assertEqual(await self.db.inactive_key_ids(time.time() + 10 * 86400), [])   # inactivity cleanup skips it too

    async def test_failed_role_removal_is_retried_after_the_registration_row_is_gone(self):
        await self.mint("777")
        self.role_status = 429
        await self.service.revoke("777")                                        # key + row gone, role DELETE failed
        pending = await self.db._db.execute_fetchall("SELECT discord_id FROM pending_role_removals")
        self.assertEqual([r[0] for r in pending], ["777"])
        self.role_status = 204
        self.assertEqual(await self.service.sync_roles(), 1)                    # retried successfully
        self.assertEqual(await self.db._db.execute_fetchall("SELECT 1 FROM pending_role_removals"), [])

    async def test_expiry_release_inside_finish_defers_role_http(self):
        await self.mint("777")
        await self.db._db.execute("UPDATE api_keys SET expires_at=?", (time.time() - 5,))
        await self.db._db.commit()
        before = len(self.role_calls)
        link = await self.service.begin("777", "1480185480048808009")           # outside lock: removes now
        self.assertGreater(len(self.role_calls), before)
