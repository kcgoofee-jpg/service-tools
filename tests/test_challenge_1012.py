"""2026-10-12 逐行挑战（challenge/line-by-line-1012）的回归测试。

四组：
1. 反封号：Retry-After HTTP 日期、GET 不带 Content-Type、403 退避档位不被一次成功清零、
   文本通道独立冷却、Anlas 订阅查询复用共享连接（继承出口代理）。
2. 算法口径：注水桶上限与首页文案同源、安全档默认值可用、集中度分母是全站、拥挤度分母不含 V5。
3. 冲突与日志：站点级拒绝带原因码、本地拒绝退还 Key 出图间隔、删除测试 Key 后历史用量不回流到成员统计。
4. 前端：落地页运行时元素存在、公告预览沙箱化、写死的上限数字已移除。
"""
from __future__ import annotations

import asyncio
import time
from pathlib import Path
from types import SimpleNamespace

import httpx
import pytest

from app import anlas_pool, quota_algo, reasons, risk_check
from app.database import NOT_TEST, Database
from app.guard import FIELDS, Guard
from app.nai import NaiClient, parse_retry_after
from app.policy import FREE_BURST, FREE_REFILL_PER_HOUR, free_paced_cap
from test_anti_ban import fake_db                                       # noqa: F401  （复用既有假 DB 夹具）

ROOT = Path(__file__).resolve().parent.parent
DAY = "2026-09-22"
KEY_BASE = {"daily_images": 60, "daily_anlas": 0, "daily_v5": 5, "monthly_anlas": 0,
            "daily_text_tokens": 0, "rpm": 10, "allow_anlas": False, "allow_img2img": False,
            "exclude_global_v5": False}


# ---------------------------------------------------------------- 反封号

def test_get_headers_drop_content_type():
    """GET / suggest-tags 不带 Content-Type：官方前端不会给无 body 的查询带上它。"""
    client = NaiClient(tokens=["tok-1"], image_host="https://image.novelai.net",
                       text_host="https://text.novelai.net", legacy_text_host="https://api.novelai.net",
                       db=None, day_fn=lambda: DAY, v5_daily_limits=[0], allow_anlas=[True])
    ts = client.pool[0]
    get_headers = client._headers(ts, "application/json", method="GET")
    post_headers = client._headers(ts, "application/json", method="POST")
    assert "Content-Type" not in get_headers
    assert post_headers["Content-Type"] == "application/json"
    assert get_headers["Authorization"] == "Bearer tok-1"


def test_guard_fallback_default_is_effective():
    """默认值必须让「上游 429 / 403 → 退回安全档」这条保险丝真的能用（0 < 安全档 < 每日上限）。"""
    daily, fallback = FIELDS["account_daily_cap"][0], FIELDS["daily_cap_fallback"][0]
    assert 0 < fallback < daily
    guard = Guard()
    guard.values.update({"account_daily_cap": 1000, "daily_cap_fallback": 1500})
    assert guard.fallback_effective() is False          # 历史默认组合 = 保险丝静默失效
    guard.values["daily_cap_fallback"] = 800
    assert guard.fallback_effective() is True


@pytest.mark.asyncio
async def test_text_429_cools_text_only(fake_db):
    """文本 429 不再完全静默：文本通道自己冷却，图片通道不受影响。"""
    async def handler(request):
        return httpx.Response(429, headers={"retry-after": "120"}, json={"message": "slow down"})

    client = NaiClient(tokens=["tok-1"], image_host="https://image.novelai.net",
                       text_host="https://text.novelai.net", legacy_text_host="https://api.novelai.net",
                       db=fake_db, day_fn=lambda: DAY, v5_daily_limits=[0], allow_anlas=[True],
                       image_min_interval=0, post_jitter_min=0, post_jitter_max=0)
    client.on_event = lambda *a, **k: None
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client._client = http
        with pytest.raises(Exception) as caught:
            await client.stream("https://offline.invalid/ai/generate-stream", {})
    assert "429" in str(caught.value) or "限流" in str(caught.value)
    ts = client.pool[0]
    assert ts.text_blocked_until > time.time() + 60
    # 图片通道照常可用
    assert await client.pick_token() is ts
    # 文本通道被自己的冷却挡住（不是冻结整把 Token）
    with pytest.raises(Exception):
        await client.pick_token(text_job=True)


@pytest.mark.asyncio
async def test_anlas_subscription_query_reuses_shared_client(monkeypatch):
    """Anlas 订阅查询必须能用共享 client：临时新建的客户端不走 UPSTREAM_PROXY，
    同一把 Token 会同时从代理 IP 和服务器 IP 出现（共享账号最明显的特征之一）。"""
    seen = {}

    async def handler(request):
        seen["url"] = str(request.url)
        return httpx.Response(200, json={
            "trainingStepsLeft": {"fixedTrainingStepsLeft": 100, "purchasedTrainingSteps": 5},
            "expiresAt": 1e9 + 1000})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as shared:
        data = await anlas_pool.fetch_account("https://image.novelai.net", "tok", shared)
    assert data["anlas"] == 105
    assert seen["url"].endswith("/user/subscription")


# ---------------------------------------------------------------- 算法口径

def test_free_paced_cap_shared_by_copy_and_check():
    """首页文案与限额判定必须用同一组常量：30 点底仓 + 4 张/小时 → 全天最多 126 张。"""
    assert (FREE_BURST, FREE_REFILL_PER_HOUR) == (30.0, 4.0)
    assert free_paced_cap(150) == 126
    assert free_paced_cap(91) == 91          # 上限比桶小的时候，桶不收紧
    assert free_paced_cap(0) == 0


def test_quota_notice_is_truthful():
    # 每人 150 张但注水桶全天只放 126 张：文案必须写出 126，不能再写 96 或「蓄水池」
    note = quota_algo.quota_notice(150, {"each": 3, "global": 130}, 47, today="2026-10-12",
                                   pct=89, review=None)
    assert "126" in note and "96" not in note and "蓄水池" not in note
    assert "先到先得" in note                 # 3 × 47 = 141 > 130：个人额度不是承诺
    # 上限比桶小：直接写上限，不提桶
    plain = quota_algo.quota_notice(91, {"each": 18, "global": 130}, 7, today="2026-10-12",
                                    pct=89, review=None)
    assert "91 张/天" in plain and "126" not in plain and "先到先得" not in plain


@pytest.mark.asyncio
async def test_quota_run_records_paced_blocks_and_budget(tmp_path):
    """每日复盘要留下「被注水桶卡住」的次数与拥挤度分母（budget = 账号日上限 − 全站 V5）。"""
    db = Database(str(tmp_path / "paced.sqlite"))
    await db.connect()
    try:
        row = await db.create_key(dict(name="成员", token="nai-paced", **KEY_BASE))
        start = time.mktime(time.strptime(DAY, "%Y-%m-%d")) + 1800      # 当天 00:30：覆盖度 ≥ 20 小时
        await db.add_log(row["id"], "成员", "image", "m", "rejected", detail="429 太快", reason=reasons.PACED)
        await db._db.execute("UPDATE usage_log SET ts=? WHERE key_id=?", (start, row["id"]))
        await db.bump_counters(row["id"], DAY, images=3, legacy_free_images=3)
        await db._db.commit()
        guard = Guard(db)

        class Allow:
            async def snapshot(self, pool):
                return {"accounts": [{"percent": 98, "recharge_per_day": 11.0}]}

        state = SimpleNamespace(
            db=db, guard=guard,
            settings=SimpleNamespace(global_daily_v5=130, global_monthly_anlas=9000,
                                     global_daily_anlas=200, global_daily_anlas_until=0),
            nai=SimpleNamespace(pool=[SimpleNamespace(usable=True)], allowance=Allow()))
        result = await quota_algo.run(state, now=time.mktime(time.strptime("2026-09-23", "%Y-%m-%d")) + 3600)
        review = result["review"]
        assert review is not None
        assert review["paced_blocks"] == 1
        # 拥挤度分母 = 账号日上限 − 当时生效的全站 V5（不是 cap，也不是新算出来的 V5）
        assert review["budget"] == review["cap"] - 130
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_risk_concentration_uses_station_total(tmp_path):
    """集中度分母必须是全站总量，不是 Top-6 之和（否则 47 人时正常成员会被误报「占全站 60%」）。"""
    db = Database(str(tmp_path / "conc.sqlite"))
    await db.connect()
    try:
        now = time.time()
        for i in range(8):
            row = await db.create_key(dict(name=f"成员{i}", token=f"nai-c{i}", **KEY_BASE))
            for _ in range(10):
                await db.add_log(row["id"], row["name"], "image", "m", "ok", images=1)
        await db._db.execute("UPDATE usage_log SET ts=?", (now - 60,))
        await db._db.commit()
        item = await risk_check._concentration_item(SimpleNamespace(db=db), now)
        detail = " ".join(item["evidence"])
        assert "13%" in detail or "12%" in detail          # 10 / 80
        assert item["status"] == "ok"
    finally:
        await db.close()


# ---------------------------------------------------------------- 冲突与日志

def test_site_level_reason_codes():
    """站点级拒绝带码，且 key_guard 的排除集合包含「排队超时」这类站点原因；
    个人额度用完后客户端反复重试仍要能被 key_guard 数到（这是它存在的意义）。"""
    for code in (reasons.MEMBER_QUOTA, reasons.SITE_QUOTA, reasons.KEY_RATE, reasons.QUEUE_TIMEOUT):
        assert code in reasons.LABELS
    assert reasons.QUEUE_TIMEOUT in reasons.SITE_LEVEL
    assert reasons.MEMBER_QUOTA not in reasons.SITE_LEVEL
    assert reasons.SITE_QUOTA in reasons.SITE_LEVEL


@pytest.mark.asyncio
async def test_refund_local_reject_returns_key_slot(monkeypatch):
    """本地拒绝（排队满 / 熔断 / 冷却 / 额度）要退还 Key 的 15 秒出图间隔。"""
    from app.routers import image_routes

    calls = []

    class Stub:
        async def refund_key_image_slot(self, key_id):
            calls.append(key_id)

    monkeypatch.setattr(image_routes, "_state", lambda: Stub())
    key = {"id": 7, "is_admin": 0}
    monkeypatch.setattr(image_routes.request_timing, "was_sent", lambda: False)
    await image_routes.refund_local_reject(key)
    assert calls == [7]
    # 已经发到上游：不能退（那张图真的占了间隔）
    monkeypatch.setattr(image_routes.request_timing, "was_sent", lambda: True)
    await image_routes.refund_local_reject(key)
    assert calls == [7]


@pytest.mark.asyncio
async def test_deleted_test_key_usage_does_not_revive_as_member_stats(tmp_path):
    """删除测试 Key 后，它历史的用量不能重新算进成员统计（墓碑表记下 is_test）。"""
    db = Database(str(tmp_path / "tomb.sqlite"))
    await db.connect()
    try:
        row = await db.create_key(dict(name="测试", token="nai-tomb", is_test=True, **KEY_BASE))
        await db.add_log(row["id"], "测试", "image", "m", "ok", images=4)
        assert (await db._db.execute_fetchall(f"SELECT COUNT(*) FROM usage_log WHERE {NOT_TEST}"))[0][0] == 0
        await db.delete_key(row["id"])
        assert (await db._db.execute_fetchall(
            "SELECT is_test FROM deleted_key_usage_flags WHERE key_id=?", (row["id"],)))[0][0] == 1
        assert (await db._db.execute_fetchall(f"SELECT COUNT(*) FROM usage_log WHERE {NOT_TEST}"))[0][0] == 0
        # 非测试 Key 删除后照旧计入（用于对账已删除 Key 的历史用量）
        member = await db.create_key(dict(name="成员", token="nai-live", **KEY_BASE))
        await db.add_log(member["id"], "成员", "image", "m", "ok", images=2)
        await db.delete_key(member["id"])
        assert (await db._db.execute_fetchall(f"SELECT COUNT(*) FROM usage_log WHERE {NOT_TEST}"))[0][0] == 1
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_retry_after_http_date_from_waf():
    """Cloudflare / WAF 会用 HTTP 日期形式，且事件里必须按真实差值冷却。"""
    from datetime import datetime, timedelta, timezone
    from email.utils import format_datetime
    far = format_datetime(datetime.now(timezone.utc) + timedelta(hours=2))
    assert parse_retry_after(far) == 3600.0
    soon = format_datetime(datetime.now(timezone.utc) + timedelta(seconds=45))
    assert 35 <= parse_retry_after(soon) <= 46


# ---------------------------------------------------------------- 前端

def test_landing_runtime_ids_exist():
    """JS 引用的 id 必须真的在页面里：以前 algoNote / audit / pills 整段不渲染。"""
    html = (ROOT / "app/static/landing.html").read_text(encoding="utf-8")
    for element_id in ("pills", "algoNote", "audit", "auditTxt", "liveTitle", "headKey"):
        assert f'id="{element_id}"' in html, element_id


def test_landing_no_hardcoded_caps():
    html = (ROOT / "app/static/landing.html").read_text(encoding="utf-8")
    assert "q.images_cap || 1700" not in html
    assert "q.v5_cap || 260" not in html
    assert "'不限'" in html


def test_admin_announce_preview_is_sandboxed():
    html = (ROOT / "app/static/index.html").read_text(encoding="utf-8")
    assert "renderAnnouncePreview" in html
    assert "f.setAttribute('sandbox', '')" in html
    assert "$('#announcePreview').innerHTML = $('#announce').value" not in html
