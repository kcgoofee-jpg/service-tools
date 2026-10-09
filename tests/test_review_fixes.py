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
    # one admitted; older waiters are superseded by newer ones (None), only the newest keeps waiting
    assert results.count(True) == 1 and results.count(None) == 4 and results.count(False) == 0
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


@pytest.mark.asyncio
async def test_owner_can_change_admin_password_from_the_panel(tmp_path, db):
    settings = Settings(admin_password="a-strong-password-123", secret_key="s", data_dir=tmp_path,
                        admin_cookie_secure=False)
    state = AdminState(settings)
    state.db = db
    app = FastAPI()
    app.state.gate = state
    app.include_router(admin_router)
    hdr = {"Origin": "http://t"}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as c:
        assert (await c.post("/admin/api/login", json={"password": "a-strong-password-123"})).status_code == 200
        assert (await c.put("/admin/api/password", headers=hdr, json={"current": "wrong", "new": "x" * 14})).status_code == 401
        assert (await c.put("/admin/api/password", headers=hdr, json={"current": "a-strong-password-123", "new": "short"})).status_code == 422
        done = await c.put("/admin/api/password", headers=hdr,
                           json={"current": "a-strong-password-123", "new": "a-new-very-strong-pw"})
        assert done.status_code == 200 and done.json()["relogin"] is True
        assert (await c.get("/admin/api/me")).status_code == 401                              # old session is dead
        assert (await c.post("/admin/api/login", json={"password": "a-strong-password-123"})).status_code == 401
        assert (await c.post("/admin/api/login", json={"password": "a-new-very-strong-pw"})).status_code == 200
        assert (await c.get("/admin/api/me")).status_code == 200
    assert (await db.get_setting("admin_password_hash")).startswith("scrypt$")                # only a hash is stored
    assert "a-new-very-strong-pw" not in (await db.get_setting("admin_password_hash"))


@pytest.mark.asyncio
async def test_deleting_a_discord_member_clears_registration_and_can_ban_and_members_show_profile(tmp_path, db):
    settings = Settings(admin_password="a-strong-password-123", secret_key="s", data_dir=tmp_path, admin_cookie_secure=False)
    service = RegistrationService(db, None, client_id="c", client_secret="s", bot_token="b",
                                  bridge_secret="x" * 40, redirect_uri="https://x/cb")

    async def member(uid, name):
        k = await db.create_key(dict(name=name, token="t" + uid, daily_images=5, monthly_anlas=0, daily_text_tokens=0, rpm=5))
        await db._db.execute(
            "INSERT INTO discord_registrations(discord_id,key_id,created_at,username,display_name,avatar) VALUES (?,?,?,?,?,?)",
            (uid, k["id"], time.time(), "someone", "Some One", "abc123"))
        await db._db.commit()
        return k["id"]
    first, second = await member("1010000000000000001", "Some One (@someone)"), await member("2020000000000000002", "Other")

    class State(AdminState):
        def __init__(self):
            super().__init__(settings)
            self.db = db
            self.admin_pw_hash = None

        def day(self):
            return "2026-10-09"

        def week_days(self, n=7):
            return ["2026-10-09"]
    app = FastAPI()
    app.state.gate = State()
    app.state.registrar = service
    app.include_router(admin_router)
    hdr = {"Origin": "http://t"}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as c:
        await c.post("/admin/api/login", json={"password": "a-strong-password-123"})
        members = (await c.get("/admin/api/members")).json()["members"]
        profile = next(m for m in members if m["id"] == first)["discord"]
        assert profile["display_name"] == "Some One" and profile["username"] == "someone"
        assert profile["avatar_url"].endswith("/avatars/1010000000000000001/abc123.png?size=64")
        assert profile["profile_url"] == "https://discord.com/users/1010000000000000001"
        gone = await c.delete(f"/admin/api/keys/{first}", headers=hdr)
        assert gone.status_code == 200 and gone.json()["banned"] is False
        assert await db.get_key(first) is None
        assert await db._db.execute_fetchall("SELECT 1 FROM discord_registrations WHERE key_id=?", (first,)) == []
        assert not await service.is_banned("1010000000000000001")                         # plain delete = can come back
        banned = await c.delete(f"/admin/api/keys/{second}?ban=true", headers=hdr)
        assert banned.json()["banned"] is True and await service.is_banned("2020000000000000002")


# ---------------------------------------------------------------- upstream tokens managed from the panel
from app.nai import NaiClient
from app import token_store

TOKEN_A = "pst-" + "A" * 40
TOKEN_B = "pst-" + "B" * 40
TOKEN_C = "pst-" + "C" * 40


async def make_nai(db, tmp_path, tokens=(TOKEN_A,), subscription=200):
    seen = []

    def upstream(request):
        seen.append(request.headers["authorization"])
        if subscription == 200:
            return httpx.Response(200, json={"tier": 3, "active": True})
        return httpx.Response(subscription, json={})
    nai = NaiClient(list(tokens), "https://image.example", "https://text.example", "https://legacy.example",
                    db=db, day_fn=lambda: "2026-10-09", v5_daily_limits=[], allow_anlas=[True] * len(tokens))
    nai._client = httpx.AsyncClient(transport=httpx.MockTransport(upstream))
    nai.managed_path = tmp_path / "upstream_tokens.json"
    return nai, seen


@pytest.mark.asyncio
async def test_token_store_roundtrip_is_private_and_tolerates_garbage(tmp_path):
    path = tmp_path / "t.json"
    assert token_store.load(path) is None
    token_store.save(path, [{"token": TOKEN_A, "allow_anlas": True}])
    assert token_store.load(path) == [{"token": TOKEN_A, "allow_anlas": True}]
    assert path.stat().st_mode & 0o777 == 0o600
    path.write_text("not json")
    assert token_store.load(path) is None
    assert not token_store.valid_token("pst-short") and not token_store.valid_token("nai-" + "A" * 40)


@pytest.mark.asyncio
async def test_add_replace_remove_keep_settings_and_never_touch_the_database_with_raw_tokens(db, tmp_path):
    nai, _ = await make_nai(db, tmp_path)
    old = nai.pool[0]
    await nai.set_v5_daily_limit(old.token_id, 40)
    await nai.set_image_concurrency(old.token_id, 2)
    await db.bump_upstream_counter(old.token_id, "2026-10-09", 3) if hasattr(db, "bump_upstream_counter") else None
    added = await nai.add_token(TOKEN_B, allow_anlas=False)
    assert [t.position for t in nai.pool] == [1, 2] and added.allow_anlas is False
    with pytest.raises(ValueError):
        await nai.add_token(TOKEN_B)                                           # duplicate
    replaced = await nai.replace_token(old.token_id, TOKEN_C)
    assert replaced.token_id != old.token_id and replaced.v5_daily_limit == 40 and replaced.image_slots.limit == 2
    assert (await db.get_upstream_token_limits()).get(replaced.token_id) == 40            # settings moved to the new id
    assert old.token_id not in await db.get_upstream_token_limits()
    saved = token_store.load(tmp_path / "upstream_tokens.json")
    assert [e["token"] for e in saved] == [TOKEN_C, TOKEN_B]
    with pytest.raises(LookupError):
        await nai.replace_token("token-nope", TOKEN_A)
    assert await nai.remove_token(added.token_id) is True
    with pytest.raises(ValueError):
        await nai.remove_token(nai.pool[0].token_id)                           # never remove the last token
    raw = "".join(str(r) for r in await db._db.execute_fetchall("SELECT * FROM site_settings"))
    assert "pst-" not in raw


@pytest.mark.asyncio
async def test_verify_token_maps_upstream_answers(db, tmp_path):
    nai, seen = await make_nai(db, tmp_path)
    assert await nai.verify_token(TOKEN_B) == {"ok": True, "tier": 3}
    assert seen[-1] == f"Bearer {TOKEN_B}"
    bad, _ = await make_nai(db, tmp_path, subscription=401)
    assert (await bad.verify_token(TOKEN_B))["ok"] is False


@pytest.mark.asyncio
async def test_admin_token_endpoints_validate_then_change_the_pool(tmp_path, db):
    nai, _ = await make_nai(db, tmp_path)
    settings = Settings(admin_password="a-strong-password-123", secret_key="s", data_dir=tmp_path, admin_cookie_secure=False)
    announced = []

    class State(AdminState):
        def __init__(self):
            super().__init__(settings)
            self.db, self.nai, self.admin_pw_hash = db, nai, None
            self.alerter = SimpleNamespace(notify=lambda kind, text, cooldown=0: announced.append(text))
    app = FastAPI()
    app.state.gate = State()
    app.include_router(admin_router)
    hdr = {"Origin": "http://t"}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as c:
        assert (await c.post("/admin/api/upstream-tokens", headers=hdr, json={"token": TOKEN_B})).status_code == 401
        await c.post("/admin/api/login", json={"password": "a-strong-password-123"})
        assert (await c.post("/admin/api/upstream-tokens", headers=hdr, json={"token": "nope"})).status_code == 422
        ok = await c.post("/admin/api/upstream-tokens", headers=hdr, json={"token": TOKEN_B, "allow_anlas": False})
        assert ok.status_code == 200 and len(ok.json()["pool"]) == 2 and ok.json()["tier"] == 3
        assert TOKEN_A not in ok.text and TOKEN_B not in ok.text                    # raw tokens are never returned
        assert all("…" in row["token"] for row in ok.json()["pool"])                 # only masked forms
        assert (await c.post("/admin/api/upstream-tokens", headers=hdr, json={"token": TOKEN_B})).status_code == 409
        first_id = ok.json()["pool"][0]["token_id"]
        rep = await c.put(f"/admin/api/upstream-tokens/{first_id}", headers=hdr, json={"token": TOKEN_C})
        assert rep.status_code == 200 and rep.json()["pool"][0]["token_id"] != first_id
        gone = await c.delete(f"/admin/api/upstream-tokens/{rep.json()['pool'][1]['token_id']}", headers=hdr)
        assert gone.status_code == 200 and len(gone.json()["pool"]) == 1
        last = await c.delete(f"/admin/api/upstream-tokens/{gone.json()['pool'][0]['token_id']}", headers=hdr)
        assert last.status_code == 409
    assert len(announced) == 3                                                   # owner is told about every change
