"""No billable upstream calls: Discord OAuth enrollment contract."""
import asyncio
import tempfile
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
                    REGISTRATION_BRIDGE_SECRET="x", DISCORD_GUILD_ID="123", SITE_URL="https://gate.example.com")
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
