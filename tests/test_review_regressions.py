"""2026-10-09 发布前审查发现的问题的回归测试（见 docs/REVIEW_2026-10-09.md）。"""
import asyncio
import json
import tempfile
import time
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import httpx
import pytest

from app import main, policy, token_store
from app.body import parse_json
from app.database import Database
from app.nai import NaiClient, UpstreamError, clamp_retry_after, parse_retry_after
from app.registration import RegistrationService
from test_generation_integration import FakeState, image_body, post
from test_nai_integration import FakeDB as NaiDB, PNG


@pytest.fixture
def state(monkeypatch):
    s = FakeState()
    monkeypatch.setattr(main, "STATE", s)
    return s


# ---------------------------------------------------------------- 计费 / 参数

@pytest.mark.asyncio
async def test_v5_model_with_whitespace_is_still_v5(state):
    key = state.db.keys["fixture-1"]
    key.update(allow_anlas=False, daily_v5=1, image_model_scope="all")
    state.db.charges.append((1, dict(anlas=0, v5=1, images=1, legacy_free_images=0)))
    for spelling in ("nai-diffusion-5", "nai-diffusion-5 ", " NAI-Diffusion-5"):
        body = image_body()
        body["model"] = spelling
        assert (await post("/ai/generate-image", body)).status_code == 429   # 每日 V5 已用完
    assert not state.nai.calls


@pytest.mark.asyncio
async def test_model_name_is_normalized_before_forwarding(state):
    body = image_body()
    body["model"] = "  NAI-Diffusion-4-5-Full "
    assert (await post("/ai/generate-image", body)).status_code == 200
    assert state.nai.calls[-1][2]["model"] == "nai-diffusion-4-5-full"


@pytest.mark.asyncio
@pytest.mark.parametrize("field,value", [("steps", 28.99), ("n_samples", 1.99), ("width", 1024.9),
                                         ("steps", True), ("height", -64)])
async def test_non_integer_image_numbers_rejected(state, field, value):
    state.db.keys["fixture-1"].update(allow_anlas=False)
    r = await post("/ai/generate-image", image_body(**{field: value}))
    assert r.status_code == 400 and r.json()["error"]["message"].startswith("图片参数无效")
    assert not state.nai.calls and not state.db.charges


@pytest.mark.asyncio
async def test_integral_float_numbers_are_accepted_as_ints(state):
    assert (await post("/ai/generate-image", image_body(steps=23.0))).status_code == 200
    assert state.nai.calls[-1][2]["parameters"]["steps"] == 23


def test_inpaint_priced_by_larger_strength():
    from test_generation_integration import PNG as PNG_B64
    body = image_body(image=PNG_B64, mask=PNG_B64, strength=1.0, img2img={"strength": 0},
                      width=1536, height=1536)
    body["model"] = "nai-diffusion-4-5-full-inpainting"
    full = image_body(image=PNG_B64, strength=1.0, width=1536, height=1536)
    assert policy.estimate_image_cost(body, is_opus=False)["anlas"] == \
        policy.estimate_image_cost(full, is_opus=False)["anlas"]


@pytest.mark.parametrize("raw", [
    b'{"scale": NaN}', b'{"scale": Infinity}', b'{"x": -Infinity}',
    b'{"x":' + b"[" * 40 + b"]" * 40 + b"}",
])
def test_strict_json_rejects_nan_and_deep_nesting(raw):
    with pytest.raises(ValueError):
        parse_json(raw)


@pytest.mark.asyncio
async def test_nan_and_deep_nesting_return_400_not_500(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app, raise_app_exceptions=False),
                                 base_url="http://gate") as c:
        h = {"Authorization": "Bearer fixture-1", "content-type": "application/json"}
        nan = json.dumps(image_body()).replace('"steps": 28', '"steps": 28, "scale": NaN')
        assert (await c.post("/ai/generate-image", content=nan, headers=h)).status_code == 400
        deep = '{"model":"nai-diffusion-4-5-full","parameters":{"x":' + "[" * 900 + "]" * 900 + "}}"
        assert (await c.post("/ai/generate-image", content=deep, headers=h)).status_code == 400
    assert not state.nai.calls


def test_clamp_does_not_deepcopy_or_mutate_input():
    body = image_body(width=2048, height=2048, n_samples=4)
    junk = [{} for _ in range(10)]
    body["parameters"]["x"] = junk
    out, notes, problem = policy.clamp_image_params(body, max_pixels=1024 * 1024, max_steps=28,
                                                    allow_img2img=True)
    assert problem is None and notes
    assert out["parameters"]["x"] is junk                      # 未深拷贝大字段
    assert body["parameters"]["n_samples"] == 4                # 原始请求未被修改


@pytest.mark.asyncio
async def test_text_route_body_limit_is_small(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app, raise_app_exceptions=False),
                                 base_url="http://gate") as c:
        big = json.dumps({"model": "kayra-v1", "input": "x" * (2 * 1024 * 1024)})
        r = await c.post("/ai/generate", content=big,
                         headers={"Authorization": "Bearer fixture-1", "content-type": "application/json"})
    assert r.status_code == 413


@pytest.mark.asyncio
async def test_inflight_requests_per_key_are_capped(state):
    gate = asyncio.Event()

    async def slow(request):
        await gate.wait()
        return httpx.Response(200)

    wrapped = main.limit_inflight(slow)

    class Req:
        headers = {"authorization": "Bearer same"}

    tasks = [asyncio.create_task(wrapped(Req())) for _ in range(main.MAX_INFLIGHT_PER_KEY)]
    await asyncio.sleep(0)
    with pytest.raises(main.GateError) as e:
        await wrapped(Req())
    assert e.value.status == 429
    gate.set()
    await asyncio.gather(*tasks)
    assert not main._INFLIGHT


# ---------------------------------------------------------------- 上游

def _nai(handler):
    c = NaiClient(["fake-a"], "https://offline.invalid", "https://offline.invalid",
                  "https://offline.invalid", db=NaiDB(), day_fn=lambda: "2026-09-22",
                  v5_daily_limits=[0], allow_anlas=[True], image_min_interval=0)
    c._client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    return c


@pytest.mark.asyncio
async def test_slow_text_request_does_not_block_image_dispatch():
    dispatched = {}

    async def handler(req):
        if req.url.path == "/text":
            await asyncio.sleep(1.0)
            return httpx.Response(200, json={"output": "x"})
        dispatched["image"] = time.monotonic()
        return httpx.Response(200, content=PNG)

    c = _nai(handler)
    t0 = time.monotonic()
    text = asyncio.create_task(c.request("POST", "https://offline.invalid/text"))
    await asyncio.sleep(0.05)
    img = await c.request("POST", "https://offline.invalid/img", image_lane=True, queue_timeout=0.2)
    await text
    assert img.status_code == 200 and dispatched["image"] - t0 < 0.5


@pytest.mark.asyncio
async def test_text_stream_429_does_not_freeze_token_for_images():
    async def handler(req):
        if req.url.path == "/text-stream":
            return httpx.Response(429, headers={"retry-after": "1"})
        return httpx.Response(200, content=PNG)

    c = _nai(handler)
    with pytest.raises(UpstreamError):
        await c.stream("https://offline.invalid/text-stream", {})
    assert (await c.request("POST", "https://offline.invalid/img", image_lane=True)).status_code == 200


@pytest.mark.parametrize("header,expected", [
    ("inf", 20.0), ("nan", 20.0), ("1e9", 20.0), ("-5", 20.0), ("Wed, 21 Oct 2015 07:28:00 GMT", 20.0),
    ("1", 5.0), ("30", 30.0), ("99999999", 3600.0), (None, 20.0),
])
def test_retry_after_is_parsed_and_clamped(header, expected):
    assert parse_retry_after(header) == expected


@pytest.mark.parametrize("value", [float("inf"), float("nan"), 1e30, -1])
def test_retry_after_clamp_is_finite(value):
    assert 5.0 <= clamp_retry_after(value) <= 3600.0


@pytest.mark.asyncio
async def test_retry_after_inf_cannot_poison_cooldown():
    seen = []

    async def handler(req):
        return httpx.Response(429, headers={"retry-after": "inf"})

    c = _nai(handler)

    async def on_rl(seconds):
        seen.append(seconds)

    with pytest.raises(UpstreamError):
        await c.request("POST", "https://offline.invalid/img", image_lane=True, on_rate_limited=on_rl)
    assert seen == [20.0] and c.pool[0].blocked_until < time.time() + 3601


# ---------------------------------------------------------------- 后台

@pytest.mark.parametrize("value", [None, "", "  ", -1, "-3", 1.5, "abc", float("inf"), True])
def test_admin_numbers_never_fail_open(value):
    from fastapi import HTTPException
    from app.admin import _num
    with pytest.raises(HTTPException) as e:
        _num(value, int, 0, 100, "daily_images")
    assert e.value.status_code == 422


def test_admin_numbers_accept_valid_values():
    from app.admin import _num
    assert _num("7", int, 0, 100, "x") == 7 and _num(7.0, int, 0, 100, "x") == 7
    assert _num(2.5, float, 0, 100, "x") == 2.5


# ---------------------------------------------------------------- 注册

ROLE_GUILD = "1480185480048808009"


class _Discord:
    def __init__(self):
        self.calls, self.delay_token, self.role_delay, self.role_status = [], 0.0, 0.0, 204

    async def handler(self, request):
        p = request.url.path
        self.calls.append((request.method, p))
        if p == "/api/oauth2/token":
            await asyncio.sleep(self.delay_token)
            return httpx.Response(200, json={"access_token": "t"})
        if p == "/api/users/@me":
            return httpx.Response(200, json={"id": "777", "username": "u"})
        if p.endswith("/member"):
            return httpx.Response(200, json={"roles": ["1335363403870502912"]})
        if p == "/api/users/@me/channels":
            return httpx.Response(200, json={"id": "dm"})
        if p == "/api/channels/dm/messages":
            return httpx.Response(200, json={})
        if "/roles/" in p:
            await asyncio.sleep(self.role_delay)
            return httpx.Response(self.role_status)
        raise AssertionError(p)


async def _service(tmp, **kw):
    db = Database(str(Path(tmp) / "g.sqlite"))
    await db.connect()
    await db.set_setting("register_open", "1")
    discord = _Discord()
    http = httpx.AsyncClient(transport=httpx.MockTransport(discord.handler))
    svc = RegistrationService(db, http, client_id="c", client_secret="s", bot_token="b",
                              bridge_secret="x" * 40, redirect_uri="https://site/self-register/callback", **kw)
    return db, http, svc, discord


async def _state(svc, uid="777"):
    return parse_qs(urlparse(await svc.begin(uid, ROLE_GUILD)).query)["state"][0]


@pytest.mark.asyncio
async def test_ban_during_inflight_registration_wins():
    with tempfile.TemporaryDirectory() as tmp:
        db, http, svc, discord = await _service(tmp)
        st = await _state(svc)
        discord.delay_token = 0.3
        fin = asyncio.create_task(svc.finish("code", st))
        await asyncio.sleep(0.1)
        await svc.ban("777")
        try:
            await fin
        except Exception:
            pass
        assert await svc.is_banned("777")
        rows = await db._db.execute_fetchall(
            "SELECT 1 FROM discord_registrations WHERE discord_id='777'")
        assert not rows
        await http.aclose()
        await db.close()


@pytest.mark.asyncio
async def test_reregistration_clears_stale_role_removal():
    with tempfile.TemporaryDirectory() as tmp:
        db, http, svc, discord = await _service(tmp, member_role_id="999")
        assert await svc.finish("code", await _state(svc)) == "sent"
        discord.role_status = 429
        await svc.revoke("777")
        discord.role_status = 204
        assert await svc.finish("code", await _state(svc)) == "sent"
        discord.calls.clear()
        await svc.sync_roles()
        assert ("DELETE", f"/api/v10/guilds/{ROLE_GUILD}/members/777/roles/999") not in discord.calls
        await http.aclose()
        await db.close()


@pytest.mark.asyncio
async def test_sync_roles_does_not_hold_write_lock_across_discord_calls():
    with tempfile.TemporaryDirectory() as tmp:
        db, http, svc, discord = await _service(tmp, member_role_id="999")
        for i in range(3):
            await db._db.execute("INSERT INTO pending_role_removals VALUES (?,?)", (str(100 + i), time.time()))
        await db._db.commit()
        key = await db.create_key({"name": "m", "token": "nai-x", "daily_images": 10, "monthly_anlas": 0,
                                   "daily_text_tokens": 0, "rpm": 5})
        discord.role_delay = 1.0
        sync = asyncio.create_task(svc.sync_roles())
        await asyncio.sleep(1.3)
        await db.record_success(key["id"], "m", "image", "nai-diffusion-3", "2026-10-09", images=1)
        await sync
        await http.aclose()
        await db.close()


@pytest.mark.asyncio
async def test_paused_members_keep_their_slot():
    with tempfile.TemporaryDirectory() as tmp:
        db, http, svc, discord = await _service(tmp)
        await db.set_setting("register_max_users", "1")
        assert await svc.finish("code", await _state(svc)) == "sent"
        key = await svc.key_row_for("777")
        await db.update_key(key["id"], {"enabled": False})
        assert await svc.count_active() == 1
        await http.aclose()
        await db.close()


@pytest.mark.asyncio
async def test_register_default_all_features_round_trips():
    from app import ops
    with tempfile.TemporaryDirectory() as tmp:
        db, http, svc, discord = await _service(tmp)
        await ops.set_registration(db, {"features": None})
        assert (await ops.registration_settings(db, svc))["features"] is None      # 全部，而不是“一个都没有”
        await ops.set_registration(db, {"features": []})
        assert (await ops.registration_settings(db, svc))["features"] == []
        await http.aclose()
        await db.close()


def test_token_file_permissions_are_forced(tmp_path):
    path = tmp_path / "upstream_tokens.json"
    tmp = tmp_path / "upstream_tokens.json.tmp"
    tmp.write_text("{}")
    tmp.chmod(0o644)
    token_store.save(path, [{"token": "pst-" + "a" * 20, "allow_anlas": False}])
    assert path.stat().st_mode & 0o777 == 0o600


# ---------------------------------------------------------------- 公开面

@pytest.mark.asyncio
async def test_openapi_hidden_and_admin_headers(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app, raise_app_exceptions=False),
                                 base_url="http://gate") as c:
        assert (await c.get("/openapi.json")).status_code == 404
        admin = await c.get("/admin")
        assert "frame-ancestors 'none'" in admin.headers["content-security-policy"]
        assert admin.headers["x-frame-options"] == "DENY"

    async def inner(scope, receive, send):
        await send({"type": "http.response.start", "status": 200,
                    "headers": [(b"cache-control", b"max-age=60")]})
        await send({"type": "http.response.body", "body": b"{}"})

    wrapped = main.AdminNoStoreMiddleware(inner)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=wrapped), base_url="http://gate") as c:
        assert (await c.get("/admin/api/keys")).headers["cache-control"] == "no-store"
        assert (await c.get("/public/status")).headers["cache-control"] == "max-age=60"


# ---------------------------------------------------------------- 各功能统计

def test_every_recorded_kind_maps_to_a_feature():
    import re
    from app import features
    source = Path("app/main.py").read_text(encoding="utf-8")
    kinds = set(re.findall(r'record\(key, "([a-z_-]+)"', source)) | {"upscale", "augment-image"}
    assert kinds <= set(features.KIND_FEATURE), kinds - set(features.KIND_FEATURE)
    assert set(features.KIND_FEATURE.values()) == set(features.FEATURES)


@pytest.mark.asyncio
async def test_usage_by_kind_and_feature_log_filter():
    with tempfile.TemporaryDirectory() as tmp:
        db = Database(str(Path(tmp) / "g.sqlite"))
        await db.connect()
        key = await db.create_key({"name": "m", "token": "nai-x", "daily_images": 10, "monthly_anlas": 0,
                                   "daily_text_tokens": 1000, "rpm": 5})
        await db.record_success(key["id"], "m", "image", "nai-diffusion-4-5-full", "2026-10-09", images=1)
        await db.record_success(key["id"], "m", "upscale", "nai-diffusion-5-curated", "2026-10-09", anlas=7)
        await db.record_success(key["id"], "m", "chat", "llama-3-erato-v1", "2026-10-09", tokens=12)
        rows = {r["kind"]: r for r in await db.usage_by_kind(0)}
        assert rows["upscale"]["anlas"] == 7 and rows["chat"]["tokens"] == 12
        from app import features
        assert await db.count_logs(kinds=features.kinds_for("text")) == 1
        assert await db.count_logs(kinds=features.kinds_for("image")) == 1
        assert await db.count_logs(kinds=[]) == 0
        assert await db.count_logs() == 3
        await db.close()


# ---------------------------------------------------------------- 防 Key 分享：来源网段

def test_network_of_masks_and_groups():
    from app.key_sources import network_of
    assert network_of("120.235.155.213") == ("120.235.155.0/24", "120.235.*.*")
    assert network_of("120.235.155.7")[0] == network_of("120.235.155.213")[0]          # 同 /24 视为同一来源
    assert network_of("::ffff:120.235.155.9")[1] == "120.235.*.*"
    net, label = network_of("2408:8456:1234:5678::1")
    assert net == "2408:8456:1234::/48" and label == "2408:8456:…"
    assert network_of("testclient") is None and network_of("") is None


class _Alerts:
    def __init__(self):
        self.sent = []

    def notify(self, kind, message, *, cooldown=900):
        self.sent.append((kind, message))


@pytest.mark.asyncio
async def test_source_tracker_counts_networks_alerts_and_never_stores_full_ip():
    from app.key_sources import SourceTracker, WRITE_INTERVAL
    with tempfile.TemporaryDirectory() as tmp:
        db = Database(str(Path(tmp) / "g.sqlite"))
        await db.connect()
        key = await db.create_key({"name": "m", "token": "nai-x", "daily_images": 10, "monthly_anlas": 0,
                                   "daily_text_tokens": 0, "rpm": 5})
        alerts = _Alerts()
        tracker = SourceTracker(db, alerts, threshold=3)
        now = 1_800_000_000.0
        await tracker.observe(key, "120.235.155.213", now)
        await tracker.observe(key, "120.235.155.99", now + 1)          # 同网段：不增加
        await tracker.observe(key, "36.112.10.5", now + 2)
        assert len(await db.key_source_labels(key["id"], now - 10)) == 2 and not alerts.sent
        await tracker.observe(key, "2408:8456:1234:5678::1", now + 3)
        assert len(alerts.sent) == 1 and "3 个不同网段" in alerts.sent[0][1]
        rows = await db._db.execute_fetchall("SELECT net_hash, label FROM key_sources")
        dump = repr(list(map(tuple, rows)))
        assert "155.213" not in dump and "36.112.10" not in dump and "120.235.155" not in dump
        # 节流：同一来源 5 分钟内不重复写库
        before = (await db._db.execute_fetchall("SELECT SUM(hits) FROM key_sources"))[0][0]
        await tracker.observe(key, "36.112.10.5", now + 10)
        await tracker.observe(key, "36.112.10.5", now + 10 + WRITE_INTERVAL)
        assert (await db._db.execute_fetchall("SELECT SUM(hits) FROM key_sources"))[0][0] == before + 1
        # 管理员 Key 不记录；过期清理；删除 Key 时一并删除
        await tracker.observe({"id": 999, "name": "admin", "is_admin": 1}, "8.8.8.8", now)
        assert await db.key_source_labels(999, 0) == []
        assert await db.purge_key_sources(now + 7 * 86400 + 100) >= 3
        await tracker.observe(key, "1.2.3.4", now + 8 * 86400)
        await db.delete_key(key["id"])
        assert (await db._db.execute_fetchall("SELECT COUNT(*) FROM key_sources"))[0][0] == 0
        await db.close()


@pytest.mark.asyncio
async def test_source_tracker_threshold_zero_only_records():
    from app.key_sources import SourceTracker
    with tempfile.TemporaryDirectory() as tmp:
        db = Database(str(Path(tmp) / "g.sqlite"))
        await db.connect()
        key = await db.create_key({"name": "m", "token": "nai-x", "daily_images": 10, "monthly_anlas": 0,
                                   "daily_text_tokens": 0, "rpm": 5})
        alerts = _Alerts()
        tracker = SourceTracker(db, alerts, threshold=0)
        for i, ip in enumerate(("1.1.1.1", "2.2.2.2", "3.3.3.3", "4.4.4.4")):
            await tracker.observe(key, ip, 1_800_000_000.0 + i)
        assert not alerts.sent and len(await db.key_source_labels(key["id"], 0)) == 4
        await db.close()


# ---------------------------------------------------------------- 操作日志

def test_action_summary_hides_secrets():
    from app.action_log import summarize
    text = summarize({"token": "pst-secret-value", "password": "p", "html": "<b>x</b>", "rpm": 5,
                      "name": "x" * 200})
    assert "pst-secret" not in text and '"rpm":5' in text and "已隐去" in text and len(text) <= 301


@pytest.mark.asyncio
async def test_admin_writes_are_logged_with_target_name(tmp_path):
    from app.admin import AuditedRoute, router
    assert router.route_class is AuditedRoute
    db = Database(str(tmp_path / "g.sqlite"))
    await db.connect()
    from app.action_log import log_action
    await log_action(db, "后台 *.*.1.2", "删除 Key", "Key #2 davidzhao_refreshing", "")
    await log_action(db, "系统", "闲置回收 Key", "Key #3 m", "连续 3 天没有任何请求", ok=True)
    rows = await db.list_admin_actions()
    assert [r["action"] for r in rows] == ["闲置回收 Key", "删除 Key"] and await db.count_admin_actions() == 2
    assert await db.purge_admin_actions(time.time() + 1) == 2
    await db.close()


@pytest.mark.asyncio
async def test_inactive_cleanup_is_logged(tmp_path):
    from types import SimpleNamespace
    from app.state import GateState
    db = Database(str(tmp_path / "g.sqlite"))
    await db.connect()
    key = await db.create_key({"name": "idle", "token": "nai-idle", "daily_images": 1, "monthly_anlas": 0,
                               "daily_text_tokens": 0, "rpm": 5})
    await db._db.execute("UPDATE api_keys SET created_at=? WHERE id=?", (time.time() - 10 * 86400, key["id"]))
    await db._db.commit()
    fake = SimpleNamespace(db=db, settings=SimpleNamespace(key_inactivity_delete_days=3))
    assert await GateState.delete_inactive_keys(fake) == 1
    rows = await db.list_admin_actions()
    assert rows[0]["actor"] == "系统" and "idle" in rows[0]["target"]
    await db.close()


@pytest.mark.asyncio
async def test_v1_prefixed_nai_paths_are_aliased(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app, raise_app_exceptions=False),
                                 base_url="http://gate") as c:
        h = {"Authorization": "Bearer fixture-1"}
        assert (await c.get("/v1/ai/user/subscription")).status_code == 401      # 路由存在，只是缺 Key
        assert (await c.post("/v1/ai/generate-image", json=image_body(), headers=h)).status_code == 200
        assert (await c.get("/v1/models")).status_code == 200                    # OpenAI 路由不受影响
    assert state.nai.calls


# ---------------------------------------------------------------- v1.1：上游参数预检、402/403 告警

@pytest.mark.parametrize("change,needle", [
    ({"width": 786}, "64 的倍数"),
    ({"height": 1000}, "64 的倍数"),
    ({"sampler": "ddim"}, "不支持 V4"),
    ({"sampler": "k_dpmpp_3m_sde"}, "不支持 V4"),
])
def test_upstream_parameter_problem_rejects_known_upstream_failures(change, needle):
    body = image_body(**change)
    body["model"] = "nai-diffusion-4-5-full"
    assert needle in (policy.upstream_parameter_problem(body) or "")


def test_upstream_parameter_problem_allows_normal_and_unknown_samplers():
    for sampler in ("k_euler_ancestral", "k_dpmpp_2m", "k_dpm_2", "some_future_sampler"):
        body = image_body(sampler=sampler)
        body["model"] = "nai-diffusion-4-5-full"
        assert policy.upstream_parameter_problem(body) is None
    v3 = image_body(sampler="ddim")
    v3["model"] = "nai-diffusion-3"
    assert policy.upstream_parameter_problem(v3) is None           # V3 对采样器更宽容
    i2i = image_body(width=786)
    i2i["action"] = "img2img"
    i2i["model"] = "nai-diffusion-4-5-full"
    assert policy.upstream_parameter_problem(i2i) is None          # 只约束文生图


def test_prompt_limit_counts_utf8_bytes():
    ok = image_body()
    ok["input"] = "猫" * 17000                                    # 51000 字节
    assert policy.upstream_parameter_problem(ok) is None
    long = image_body()
    long["parameters"]["v4_prompt"] = {"caption": {"base_caption": "x", "char_captions": [{"char_caption": "猫" * 17100}]}}
    assert "字节" in policy.upstream_parameter_problem(long)


@pytest.mark.asyncio
async def test_bad_dimensions_rejected_before_queue(state):
    r = await post("/ai/generate-image", image_body(width=786, height=786))
    assert r.status_code == 400 and "64" in r.json()["error"]["message"]
    assert not state.nai.calls and not state.db.charges


@pytest.mark.asyncio
@pytest.mark.parametrize("status", [402, 403])
async def test_402_403_alert_but_do_not_disable_token(status):
    events = []

    async def handler(req):
        return httpx.Response(status, json={"message": "x"})

    c = _nai(handler)
    c.on_event = lambda kind, msg, cooldown=900: events.append(kind)
    r = await c.request("POST", "https://offline.invalid/text")
    assert r.status_code == status and events == [f"upstream_{status}"]
    assert c.pool[0].usable and not c.pool[0].disabled
