"""Regression tests for the adversarial-review findings."""
import asyncio
import io
import time
from types import SimpleNamespace

from fastapi import FastAPI
import httpx
import pytest
import pytest_asyncio
from PIL import Image

from app import main
from app.admin import _secret, router as admin_router
from app.audit import make_thumbnail
from app.config import Settings
from app.database import Database
from app.registration import RegistrationService
from app.registration_routes import router as bridge_router
from app.state import GateState
from tests.test_admin_hardening import make_client
from tests.test_admin_session import AdminState


@pytest_asyncio.fixture
async def db():
    value = Database(":memory:")
    await value.connect()
    yield value
    await value.close()


def bridge_app(db, admin_ids=("42",)):
    failures = []
    service = RegistrationService(db, None, client_id="c", client_secret="s", bot_token="b",
                                  bridge_secret="x" * 40, redirect_uri="https://x/cb", admin_ids=admin_ids)
    app = FastAPI()
    app.include_router(bridge_router)
    app.state.registrar = service
    app.state.gate = SimpleNamespace(db=db, record_auth_failure=failures.append, day=lambda: "d",
                                     upstream_health=lambda: {}, settings=Settings(), announcer=None)
    return app, service, failures


@pytest.mark.asyncio
async def test_admin_bridge_actions_need_an_allowlisted_actor_and_bad_secrets_are_counted(db):
    app, service, failures = bridge_app(db)
    auth = {"Authorization": "Bearer " + "x" * 40}
    body = {"discord_id": "7", "guild_id": service.command_guild, "action": "open", "value": "on"}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app, client=("9.9.9.9", 1)), base_url="https://t") as c:
        assert (await c.post("/self-register/ops", headers=auth, json=body)).status_code == 403                # no actor
        assert (await c.post("/self-register/ops", headers=auth, json={**body, "actor_id": "7"})).status_code == 403
        assert (await c.post("/self-register/ops", headers=auth, json={**body, "actor_id": "42"})).status_code == 200
        assert (await c.post("/self-register/revoke", headers=auth, json={**body, "actor_id": "7"})).status_code == 403
        assert (await c.post("/self-register/slots", headers=auth, json={**body, "actor_id": "7"})).status_code == 403
        assert (await c.post("/self-register/ops", headers={"Authorization": "Bearer nope"}, json=body)).status_code == 401
        assert failures == ["9.9.9.9"]                                                  # wrong secret counted
        weird = await c.post("/self-register/info", headers={"Authorization": b"Bearer \xe9"}, json=body)
        assert weird.status_code == 401                                                 # non-ASCII is 401, not 500
    app2, _, _ = bridge_app(db, admin_ids=())
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app2), base_url="https://t") as c:
        assert (await c.post("/self-register/ops", headers=auth, json={**body, "actor_id": "42"})).status_code == 403   # empty allowlist = deny


@pytest.mark.asyncio
async def test_ban_action_through_the_bridge(db):
    app, service, _ = bridge_app(db)
    auth = {"Authorization": "Bearer " + "x" * 40}
    body = {"discord_id": "7", "guild_id": service.command_guild, "action": "ban", "target": "555", "actor_id": "42"}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="https://t") as c:
        assert (await c.post("/self-register/ops", headers=auth, json=body)).status_code == 200
        assert await service.is_banned("555")
        assert (await c.post("/self-register/ops", headers=auth, json={**body, "action": "unban"})).status_code == 200
        assert not await service.is_banned("555")


@pytest.mark.asyncio
async def test_valid_keys_are_not_blocked_by_an_ip_block_but_bad_ones_are(db, monkeypatch):
    key = await db.create_key(dict(name="m", token="nai-good", daily_images=5, monthly_anlas=0,
                                   daily_text_tokens=0, rpm=5))
    state = SimpleNamespace(db=db, auth_blocked=lambda ip: 600, record_auth_failure=lambda ip: None)

    async def touch(_id):
        return None
    state.db.touch_key = touch
    monkeypatch.setattr(main, "STATE", state)

    def req(token):
        return SimpleNamespace(client=SimpleNamespace(host="1.2.3.4"),
                               headers={"authorization": "Bearer " + token} if token else {})
    assert (await main.authenticate(req("nai-good")))["id"] == key["id"]          # member behind a blocked NAT still works
    with pytest.raises(main.GateError) as bad:
        await main.authenticate(req("nai-bogus"))
    assert bad.value.status == 429
    with pytest.raises(main.GateError) as none:
        await main.authenticate(req(""))
    assert none.value.status == 429


@pytest.mark.asyncio
async def test_one_key_cannot_fill_the_shared_tag_queue(tmp_path):
    state = GateState(Settings(data_dir=tmp_path))
    state.settings.key_image_min_interval = 30
    results = []

    async def attempt():
        results.append(await state.wait_for_tag_request(1))
    tasks = [asyncio.create_task(attempt()) for _ in range(6)]
    await asyncio.sleep(0.2)
    # one admitted, at most two keep waiting, the rest are refused immediately
    assert results.count(True) == 1 and results.count(False) == 3
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    # key 1's waiters do not stop another key from being admitted once the lane is free
    await state.finish_tag_request(1)
    assert await asyncio.wait_for(state.wait_for_tag_request(2), 1) is True


def test_corrupt_or_empty_secret_file_is_regenerated_atomically(tmp_path):
    settings = Settings(admin_password="p", secret_key="", data_dir=tmp_path)
    (tmp_path / "secret_key").write_text("")                      # e.g. left behind by a crash
    req = SimpleNamespace(app=SimpleNamespace(state=SimpleNamespace(gate=SimpleNamespace(settings=settings))))
    secret = _secret(req)
    assert len(secret) >= 32 and (tmp_path / "secret_key").read_text().strip() == secret
    assert not (tmp_path / "secret_key.tmp").exists()
    assert (tmp_path / "secret_key").stat().st_mode & 0o777 == 0o600


@pytest.mark.asyncio
async def test_non_ascii_admin_password_can_log_in(tmp_path):
    _, client = make_client(tmp_path, password="密码密码密码密码密码密码")
    async with client:
        ok = await client.post("/admin/api/login", json={"password": "密码密码密码密码密码密码"})
        bad = await client.post("/admin/api/login", json={"password": "错误错误错误错误错误错误"})
        assert ok.status_code == 200 and bad.status_code == 401


def test_decompression_bomb_images_are_rejected_not_decoded():
    raw = io.BytesIO()
    Image.new("L", (6000, 6000), 0).save(raw, "PNG")              # 36M pixels, tiny file
    assert make_thumbnail(raw.getvalue()) is None


@pytest.mark.asyncio
async def test_usage_log_retention_and_login_window_pruning(db, tmp_path):
    await db.add_log(None, "k", "image", "m", "ok")
    assert await db.purge_usage_log(time.time() + 5) == 1
    state = GateState(Settings(data_dir=tmp_path, login_window_seconds=1, login_max_attempts=100))
    for i in range(2100):
        state._login_attempts[f"ip{i}"] = __import__("collections").deque([time.time() - 10])
    await state.hit_login("fresh")
    assert len(state._login_attempts) < 100
