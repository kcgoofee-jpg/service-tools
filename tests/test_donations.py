"""首页「捐赠 Key」：入库契约 + 公开端点的校验 / 限流 / 同源检查。不含任何真实 Token。"""
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import httpx
from fastapi import FastAPI

from app.database import Database
from app.registration_routes import member_router, _DONATE_HITS

TOKEN = "pst-victim-free-token-for-tests-000000000000"      # 51 字符，过 20~4000 校验


class DonationDBTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = Database(str(Path(self.tmp.name) / "gate.sqlite"))
        await self.db.connect()

    async def asyncTearDown(self):
        await self.db.close()
        self.tmp.cleanup()

    async def test_save_list_handle_roundtrip(self):
        await self.db.save_donation(TOKEN, "只用 4.5", "qq:123")
        await self.db.save_donation(TOKEN + "-second", "周末别用", "")
        self.assertEqual(await self.db.count_unhandled_donations(), 2)
        d = await self.db.list_donations()
        self.assertEqual(len(d["unhandled"]), 2) and self.assertEqual(len(d["handled"]), 0)
        self.assertEqual(d["unhandled"][0]["message"], "周末别用")          # 新的在前
        first_id = d["unhandled"][0]["id"]
        self.assertTrue(await self.db.handle_donation(first_id, "已接入"))
        self.assertFalse(await self.db.handle_donation(first_id, "再处理"))  # 幂等：处理过不能再处理
        d = await self.db.list_donations()
        self.assertEqual(len(d["unhandled"]), 1)
        self.assertEqual(d["handled"][0]["handled_note"], "已接入")
        self.assertEqual(await self.db.count_unhandled_donations(), 1)

    async def test_long_message_and_contact_are_truncated(self):
        await self.db.save_donation(TOKEN, "长" * 900, "c" * 500)
        (row,) = (await self.db.list_donations())["unhandled"]
        self.assertEqual(len(row["message"]), 500)
        self.assertEqual(len(row["contact"]), 100)


class DonateEndpointTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = Database(str(Path(self.tmp.name) / "gate.sqlite"))
        await self.db.connect()
        app = FastAPI()
        app.include_router(member_router)
        app.state.gate = SimpleNamespace(db=self.db)
        self.client = httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="https://fixture.invalid")
        _DONATE_HITS.clear()

    async def asyncTearDown(self):
        await self.client.aclose()
        await self.db.close()
        self.tmp.cleanup()
        _DONATE_HITS.clear()

    async def test_ok_then_validation_errors_then_rate_limit(self):
        ok = await self.client.post("/public/donate", json={"token": TOKEN, "message": "只用 4.5", "contact": "dc:abc"})
        self.assertEqual(ok.status_code, 200)
        rows = await self.db.list_donations()
        self.assertEqual(rows["unhandled"][0]["token"], TOKEN)
        actions = await self.db._db.execute_fetchall("SELECT action, target FROM admin_actions")
        self.assertTrue(any("收到捐赠 Key" in a[0] for a in actions))        # 日志只记动作，绝不记 Token 原文
        self.assertFalse(any(TOKEN in (a[0] or "") + (a[1] or "") for a in actions))

        short = await self.client.post("/public/donate", json={"token": "too-short", "message": "x"})
        self.assertEqual(short.status_code, 422)
        no_msg = await self.client.post("/public/donate", json={"token": TOKEN, "message": "  "})
        self.assertEqual(no_msg.status_code, 422)
        evil = await self.client.post("/public/donate", json={"token": TOKEN, "message": "x"},
                                      headers={"Origin": "https://evil.example"})
        self.assertEqual(evil.status_code, 403)

        for _ in range(4):                      # 命中过 1 次 + 再 4 次 = 5 次/小时上限
            await self.client.post("/public/donate", json={"token": TOKEN, "message": "x"})
        flood = await self.client.post("/public/donate", json={"token": TOKEN, "message": "x"})
        self.assertEqual(flood.status_code, 429)


if __name__ == "__main__":
    unittest.main()
