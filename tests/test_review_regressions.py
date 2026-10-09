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


@pytest.mark.asyncio
async def test_rotating_24s_inside_one_16_count_as_one_source():
    from app.key_sources import SourceTracker
    with tempfile.TemporaryDirectory() as tmp:
        db = Database(str(Path(tmp) / "g.sqlite"))
        await db.connect()
        key = await db.create_key({"name": "warp", "token": "nai-w", "daily_images": 10, "monthly_anlas": 0,
                                   "daily_text_tokens": 0, "rpm": 5})
        alerts = _Alerts()
        tracker = SourceTracker(db, alerts, threshold=3)
        for i, ip in enumerate(("104.28.1.5", "104.28.77.9", "104.28.200.3", "104.28.9.1")):
            await tracker.observe(key, ip, 1_800_000_000.0 + i)
        assert await db.key_source_labels(key["id"], 0) == ["104.28.*.*"] and not alerts.sent
        assert (await db.key_source_summary(0))[key["id"]] == ["104.28.*.*"]
        await db.close()


# ---------------------------------------------------------------- v1.2：被拒请求统一记日志

def _rejections(state):
    return [(a[2], a[4], kw.get("detail", "")) for a, kw in state.db.logs if a[4] == "rejected"]


@pytest.mark.asyncio
async def test_disabled_key_rejection_is_logged(state):
    state.db.keys["fixture-1"]["enabled"] = 0
    assert (await post("/ai/generate-image", image_body())).status_code == 403
    await asyncio.sleep(0)
    assert _rejections(state) == [("image", "rejected", "403 该 Key 已被禁用")]


@pytest.mark.asyncio
async def test_quota_exhausted_rejection_is_logged_once(state):
    state.db.keys["fixture-1"]["daily_images"] = 1
    assert (await post("/ai/generate-image", image_body())).status_code == 200
    assert (await post("/ai/generate-image", image_body())).status_code == 429
    await asyncio.sleep(0)
    rejected = _rejections(state)
    assert len(rejected) == 1 and "额度已用完" in rejected[0][2]


@pytest.mark.asyncio
async def test_already_recorded_rejection_is_not_duplicated(state):
    assert (await post("/ai/generate-image", image_body(width=786))).status_code == 400
    await asyncio.sleep(0)
    assert len(_rejections(state)) == 1


@pytest.mark.asyncio
async def test_invalid_key_is_not_attributed_to_anyone(state):
    assert (await post("/ai/generate-image", image_body(), token="nai-unknown")).status_code == 401
    await asyncio.sleep(0)
    assert not state.db.logs


def test_kind_for_path_maps_member_routes():
    assert main._kind_for_path("/ai/generate-image") == "image"
    assert main._kind_for_path("/nai/ai/generate-image-stream") == "image_stream"
    assert main._kind_for_path("/ai/generate-image/suggest-tags") == "tags"
    assert main._kind_for_path("/ai/generate") == "text"
    assert main._kind_for_path("/v1/chat/completions") == "chat"
    assert main._kind_for_path("/user/subscription") == "account"


@pytest.mark.asyncio
async def test_idle_reminder_sent_once_24h_before_reclaim(tmp_path):
    from types import SimpleNamespace
    from app.state import GateState
    db = Database(str(tmp_path / "g.sqlite"))
    await db.connect()
    sent = []

    async def dm(discord_id, text):
        sent.append((discord_id, text))
        return True

    fake = SimpleNamespace(db=db, settings=SimpleNamespace(key_inactivity_delete_days=3))
    fresh = await db.create_key({"name": "fresh", "token": "nai-f", "daily_images": 1, "monthly_anlas": 0,
                                 "daily_text_tokens": 0, "rpm": 5})
    idle = await db.create_key({"name": "idle", "token": "nai-i", "daily_images": 1, "monthly_anlas": 0,
                                "daily_text_tokens": 0, "rpm": 5})
    await db._db.execute("UPDATE api_keys SET created_at=? WHERE id=?", (time.time() - 2.2 * 86400, idle["id"]))
    for discord_id, key in (("111", fresh), ("222", idle)):
        await db._db.execute("INSERT INTO discord_registrations(discord_id,key_id,created_at) VALUES (?,?,?)",
                             (discord_id, key["id"], time.time()))
    await db._db.commit()
    assert await GateState.remind_idle_keys(fake, dm, "https://gate.example") == 1
    assert sent[0][0] == "222" and "还没有成功生成过图片" in sent[0][1] and "https://gate.example" in sent[0][1]
    assert await GateState.remind_idle_keys(fake, dm, "https://gate.example") == 0          # 同一段闲置只提醒一次
    await db._db.execute("UPDATE api_keys SET last_used_at=? WHERE id=?", (time.time() - 2.1 * 86400, idle["id"]))
    await db._db.commit()
    assert await GateState.remind_idle_keys(fake, dm, "https://gate.example") == 1          # 有过活动后重新计时
    actions = await db.list_admin_actions()
    assert actions[0]["action"] == "闲置回收前提醒"
    await db.close()


@pytest.mark.asyncio
async def test_get_v1_root_answers_connection_tests(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://gate") as c:
        r = await c.get("/v1")
    assert r.status_code == 200 and r.json()["object"] == "list"


# ---------------------------------------------------------------- v1.3：私信教程、图标、耗时与客户端、精简命令

def test_welcome_dm_is_a_full_tutorial_and_fits_discord():
    from app.registration import welcome_dm
    text = welcome_dm("nai-" + "k" * 48, "https://gate.example/", "V4.5 及以下 300 张", 30, 3, "📝 记录声明", "文生图")
    assert "nai-" + "k" * 48 in text and "https://gate.example/" in text
    assert "不要**加 /v1" in text and "柏宝绘" in text and "/resetkey" in text
    assert "30 天" in text and "连续 3 天" in text and "文生图" in text and "📝 记录声明" in text
    assert len(text) < 2000
    plain = welcome_dm("nai-x", "https://g/", "q", 0, 0)
    assert "有效期" not in plain and "回收" not in plain


def test_client_name_is_sanitized_and_bounded():
    from app.request_timing import client_name
    assert client_name("Mozilla/5.0\r\nX-Evil: 1") == "Mozilla/5.0 X-Evil: 1"
    assert len(client_name("a" * 500)) == 60
    assert client_name("") == ""


@pytest.mark.asyncio
async def test_log_records_wait_generation_time_and_client(state, monkeypatch):
    from app import request_timing
    real = state.nai.request

    async def slow_request(*args, **kwargs):
        await asyncio.sleep(0.05)
        request_timing.mark_sent()
        await asyncio.sleep(0.05)
        return await real(*args, **kwargs)
    monkeypatch.setattr(state.nai, "request", slow_request)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app),
                                 base_url="http://fixture.invalid") as client:
        r = await client.post("/ai/generate-image", json=image_body(),
                              headers={"Authorization": "Bearer fixture-1", "User-Agent": "BaiBaoHui/2.3"})
    assert r.status_code == 200
    await asyncio.sleep(0)
    _args, kwargs = state.db.logs[-1]
    assert kwargs["client"] == "BaiBaoHui/2.3"
    assert kwargs["wait_ms"] >= 40 and kwargs["dur_ms"] >= 40


@pytest.mark.asyncio
async def test_rejection_without_dispatch_has_no_generation_time(state):
    state.db.keys["fixture-1"]["enabled"] = 0
    assert (await post("/ai/generate-image", image_body())).status_code == 403
    await asyncio.sleep(0)
    rejected = [kw for args, kw in state.db.logs if args[4] == "rejected"]
    assert rejected and rejected[-1]["dur_ms"] == 0


@pytest.mark.asyncio
async def test_usage_log_timing_columns_round_trip(tmp_path):
    db = Database(str(tmp_path / "t.db"))
    await db.connect()
    try:
        await db.add_log(1, "k", "image", "m", "ok", wait_ms=1500, dur_ms=8200, client="X" * 100)
        await db.record_success(1, "k", "image", "m", "2026-10-09", images=1, wait_ms=-5, dur_ms=10, client="c")
        rows = await db.list_logs(limit=5, offset=0)
        by_wait = {r["wait_ms"]: dict(r) for r in rows}
        assert by_wait[1500]["dur_ms"] == 8200 and len(by_wait[1500]["client"]) == 60
        assert by_wait[0]["client"] == "c" and by_wait[0]["dur_ms"] == 10
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_old_usage_log_gets_timing_columns(tmp_path):
    import sqlite3
    path = tmp_path / "old.db"
    con = sqlite3.connect(path)
    con.execute("""CREATE TABLE usage_log (id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, key_id INTEGER,
                   key_name TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
                   status TEXT NOT NULL, images INTEGER NOT NULL DEFAULT 0, anlas REAL NOT NULL DEFAULT 0,
                   tokens INTEGER NOT NULL DEFAULT 0, detail TEXT NOT NULL DEFAULT '')""")
    con.execute("INSERT INTO usage_log (ts, kind, status) VALUES (1, 'image', 'ok')")
    con.commit(); con.close()
    db = Database(str(path))
    await db.connect()
    try:
        await db.add_log(1, "k", "image", "m", "ok", wait_ms=3, dur_ms=4, client="c")
        rows = await db.list_logs(limit=5, offset=0)
        assert {r["wait_ms"] for r in rows} == {0, 3}
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_favicon_served_as_svg(state):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://fixture.invalid") as c:
        for path in ("/favicon.ico", "/favicon.svg"):
            r = await c.get(path)
            assert r.status_code == 200 and r.headers["content-type"].startswith("image/svg+xml")
            assert r.content.startswith(b"<svg")
    for page in ("landing.html", "index.html"):
        assert 'rel="icon"' in (Path(main.__file__).parent / "static" / page).read_text()


def test_bot_command_set_is_trimmed():
    import ast
    src = (Path(__file__).parent.parent / "integration" / "discord_bot.py").read_text()
    names = set()
    for node in ast.walk(ast.parse(src)):
        if isinstance(node, ast.Call) and getattr(node.func, "attr", "") == "command":
            names |= {kw.value.value for kw in node.keywords if kw.arg == "name"}
    assert names == {"register", "quota", "resetkey", "help", "open", "limit", "ban", "unban", "slots", "revoke"}


# ---------------------------------------------------------------- v1.4：V5 名额提醒、上游表现分析

@pytest.mark.asyncio
async def test_v5_capacity_warns_when_seats_exceed_global(tmp_path):
    from types import SimpleNamespace
    from app import ops
    db = Database(str(tmp_path / "c.db"))
    await db.connect()
    try:
        settings = SimpleNamespace(global_daily_v5=150)
        await db.set_settings_bulk({"register_max_users": 10, "register_daily_v5": 15, "register_image_scope": "all"})
        cap = await ops.v5_capacity(db, settings, None)
        assert cap == {"need": 150, "global": 150, "short": False, "message": ""}
        await db.set_setting("register_max_users", 12)
        cap = await ops.v5_capacity(db, settings, None)
        assert cap["short"] and cap["need"] == 180 and "180" in cap["message"]
        await db.set_setting("register_image_scope", "legacy")          # 不含 V5 时不提醒
        assert not (await ops.v5_capacity(db, settings, None))["short"]
        await db.set_settings_bulk({"register_image_scope": "all", "global_daily_v5": 0})   # 0 = 不限
        assert not (await ops.v5_capacity(db, settings, None))["short"]
    finally:
        await db.close()


def _perf_row(ts, model="nai-diffusion-4-5-full", status="ok", dur=8000, wait=500, up=200, detail=""):
    return (ts, model, status, 1 if status == "ok" else 0, wait, dur, up, detail)


def test_perf_family_split_and_capacity():
    from app import perf
    now = 1_000_000.0
    rows = [_perf_row(now - 60 * i) for i in range(10)] + [_perf_row(now - 30, "nai-diffusion-5", dur=20000)]
    r = perf.analyze(rows, now, slots=1, site_interval=15, key_interval=15)
    v45, v5 = r["families"]["V4.5"]["h1"], r["families"]["V5"]["h1"]
    assert v45["requests"] == 10 and v45["p50_ms"] == 8000 and v45["success_rate"] == 1.0
    assert v45["capacity_per_hour"] == 240 and v45["member_capacity_per_hour"] == 240   # 受 15s 间隔限制
    assert v5["capacity_per_hour"] == 180                                               # 受 20s 生成耗时限制
    assert r["flags"] == []


def test_perf_flags_slowdown_throttle_failures_and_account():
    from app import perf
    now = 2_000_000.0
    base = [_perf_row(now - 3 * 86400 - 60 * i, dur=8000) for i in range(20)]
    slow = [_perf_row(now - 60 * i, dur=16000) for i in range(6)]
    throttled = [_perf_row(now - 100 - i, status="error", dur=300, up=429) for i in range(3)]
    old_text_429 = [_perf_row(now - 200, status="error", dur=0, up=0, detail="上游限流(429)，全站图片生成已进入冷却")]
    account = [_perf_row(now - 3600 * 5, status="error", up=403)]
    r = perf.analyze(base + slow + throttled + old_text_429 + account, now)
    codes = {f["code"] for f in r["flags"]}
    assert {"slow", "throttle", "account"} <= codes
    throttle = next(f for f in r["flags"] if f["code"] == "throttle")
    assert "4 次" in throttle["text"] and throttle["family"] == "V4.5"
    assert r["families"]["V4.5"]["baseline_ready"]


def test_perf_needs_samples_before_judging_slowdown():
    from app import perf
    now = 3_000_000.0
    rows = [_perf_row(now - 3 * 86400, dur=1000), _perf_row(now - 60, dur=60000)]
    r = perf.analyze(rows, now)
    assert not any(f["code"] == "slow" for f in r["flags"])
    assert not r["families"]["V4.5"]["baseline_ready"]


def test_perf_hourly_buckets_cover_24h():
    from app import perf
    now = 3600 * 1000 + 1800.0
    rows = [_perf_row(now - 10), _perf_row(now - 10, status="error", up=429), _perf_row(now - 23 * 3600 - 1700)]
    hours = perf.analyze(rows, now)["families"]["V4.5"]["hourly"]
    assert len(hours) == 24 and hours[-1]["ok"] == 1 and hours[-1]["r429"] == 1 and hours[0]["ok"] == 1


@pytest.mark.asyncio
async def test_upstream_status_is_logged(state, monkeypatch):
    from app import request_timing
    real = state.nai.request

    async def tagged(*args, **kwargs):
        request_timing.mark_sent()
        request_timing.mark_status(200)
        return await real(*args, **kwargs)
    monkeypatch.setattr(state.nai, "request", tagged)
    assert (await post("/ai/generate-image", image_body())).status_code == 200
    await asyncio.sleep(0)
    assert state.db.logs[-1][1]["up_status"] == 200


@pytest.mark.asyncio
async def test_image_perf_rows_reads_usage_log(tmp_path):
    db = Database(str(tmp_path / "p.db"))
    await db.connect()
    try:
        await db.add_log(1, "k", "image", "nai-diffusion-5", "error", wait_ms=10, dur_ms=20, up_status=429)
        await db.add_log(1, "k", "tags", "", "ok")
        await db.add_log(1, "k", "image", "nai-diffusion-5", "rejected")
        rows = await db.image_perf_rows(0)
        assert len(rows) == 1 and rows[0][1:] == ("nai-diffusion-5", "error", 0, 10, 20, 429, "")
    finally:
        await db.close()
