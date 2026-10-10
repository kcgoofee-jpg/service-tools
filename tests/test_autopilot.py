"""自动驾驶规则（纯函数）：闲置回收天数、名额、熔断、单个 Key 守护。"""
import time
from types import SimpleNamespace

import pytest

from app import autopilot
from app.database import Database


def test_idle_and_slots_rules():
    assert autopilot.idle_days_rule(50, 50, 0)[0] == 2
    assert autopilot.idle_days_rule(30, 50, 3)[0] == 2
    assert autopilot.idle_days_rule(20, 50, 0)[0] == 5
    assert autopilot.idle_days_rule(40, 50, 0)[0] == 3
    assert autopilot.slots_rule(50, 50, 3, 0.3, 0)[0] == 55       # 满员 + 候补 + 有余量
    assert autopilot.slots_rule(50, 48, 0, 0.3, 0)[0] == 55       # 快满（空位 ≤ 2）
    assert autopilot.slots_rule(50, 30, 0, 0.3, 0)[0] == 50       # 空位多，不加
    assert autopilot.slots_rule(50, 50, 3, 0.3, 3)[0] == 50       # 3 个小时被拦：不再加
    assert autopilot.slots_rule(50, 50, 3, 0.7, 0)[0] == 50       # 昨天用量高：不再加
    assert autopilot.slots_rule(100, 100, 3, 0.1, 0)[0] == 100    # 上限 100


def test_breaker_and_key_guard():
    assert autopilot.breaker_rule(6, 10)[0] and not autopilot.breaker_rule(4, 5)[0] and not autopilot.breaker_rule(5, 40)[0]
    assert autopilot.key_guard_rule(0, 1, 1, 0)[0] == "reset"
    assert autopilot.key_guard_rule(2, 0, 0, 0)[:2] == ("pause", 86400)
    assert autopilot.key_guard_rule(0, 0, 0, 61)[:2] == ("pause", 3600)
    assert autopilot.key_guard_rule(1, 1, 0, 10) is None


@pytest.mark.asyncio
async def test_run_observes_without_changing_keys(tmp_path):
    db = Database(str(tmp_path / "g.sqlite"))
    await db.connect()
    try:
        k = await db.create_key({"name": "m", "token": "nai-m", "daily_images": 150, "daily_anlas": 0, "daily_v5": 0,
                                 "monthly_anlas": 0, "daily_text_tokens": 0, "rpm": 10, "allow_anlas": False,
                                 "allow_img2img": False, "exclude_global_v5": False, "image_model_scope": "all"})
        now = time.time()
        src = SimpleNamespace(events=[(now - 60, k["id"], "alternate"), (now - 30, k["id"], "alternate")])
        st = SimpleNamespace(db=db, guard=None, sources=src)
        out = await autopilot.run(st, None, now=now)
        kg = out["rules"]["key_guard"]
        assert kg["mode"] == "observe" and kg["value"][0]["action"] == "pause"
        assert (await db.get_key(k["id"]))["enabled"] == 1                 # 观察模式不改任何东西
        assert (await db.list_admin_actions())[0]["action"] == "自动驾驶（观察）"
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_breaker_enforce_trips_site_pause(tmp_path):
    """breaker=enforce 且最近 15 分钟上游失败成簇 → guard 全站熔断，token_block_reason 给出暂停原因。"""
    from app.guard import Guard
    db = Database(str(tmp_path / "b.sqlite"))
    await db.connect()
    try:
        await db.set_setting("autopilot_breaker", "enforce")
        for _ in range(8):          # 本地拦截（冷却 / 上限，up_status=0）不算上游失败，不能触发熔断
            await db.add_log(None, "m", "image", "nai-diffusion-5-full", "error", detail="上游限流冷却中")
        assert not (await autopilot.run(SimpleNamespace(db=db, guard=Guard(db), share=None,
                                                        sources=SimpleNamespace(events=[])), None))["rules"]["breaker"]["value"]
        for _ in range(6):
            await db.add_log(None, "m", "image", "nai-diffusion-5-full", "error", up_status=500)
        guard = Guard(db)
        st = SimpleNamespace(db=db, guard=guard, share=None, sources=SimpleNamespace(events=[]))
        now = time.time()
        out = await autopilot.run(st, None, now=now)
        assert out["rules"]["breaker"]["value"] and out["rules"]["breaker"].get("applied")
        assert guard.breaker_until > now
        reason = await guard.token_block_reason(db, "tok", "2026-10-10", now)
        assert reason and "暂停出图" in reason
        assert (await db.list_admin_actions())[0]["action"] == "自动驾驶：熔断"
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_key_guard_enforce_pauses_only_rejects_branch(tmp_path):
    """key_guard=enforce 只执行「1 小时内被拒 ≥60 次」(pause 3600)；换网段 (pause 86400) 仍只观察。"""
    from app.share_guard import ShareGuard
    db = Database(str(tmp_path / "kg.sqlite"))
    await db.connect()
    try:
        await db.set_setting("autopilot_key_guard", "enforce")
        base = {"daily_images": 150, "daily_anlas": 0, "daily_v5": 0, "monthly_anlas": 0,
                "daily_text_tokens": 0, "rpm": 10, "allow_anlas": False, "allow_img2img": False,
                "exclude_global_v5": False, "image_model_scope": "all"}
        loop_key = await db.create_key({"name": "死循环", "token": "nai-loop", **base})
        alt_key = await db.create_key({"name": "换网段", "token": "nai-alt", **base})
        for _ in range(61):
            await db.add_log(loop_key["id"], "死循环", "image", "x", "rejected")
        now = time.time()
        src = SimpleNamespace(events=[(now - 60, alt_key["id"], "alternate"), (now - 30, alt_key["id"], "alternate")])
        share = ShareGuard(db)
        st = SimpleNamespace(db=db, guard=None, share=share, sources=src)
        out = await autopilot.run(st, None, now=now)
        applied = {d["key"]: d for d in out["rules"]["key_guard"]["value"]}
        assert applied[loop_key["id"]].get("applied") and share.paused_until(loop_key["id"], now)
        assert "反复重试" in share.pause_reasons.get(loop_key["id"], "")
        # 换网段那把只观察，不暂停
        assert not applied[alt_key["id"]].get("applied") and not share.paused_until(alt_key["id"], now)
    finally:
        await db.close()


def test_economy_rule():
    assert autopilot.economy_rule(False, 8, 8)[0] is True       # 拥挤 → 开
    assert autopilot.economy_rule(False, 3, 3)[0] is False      # 不够拥挤 → 保持关
    assert autopilot.economy_rule(True, 5, 0)[0] is False       # 空闲 → 关
    assert autopilot.economy_rule(True, 5, 5)[0] is True        # 还拥挤 → 保持开
    assert autopilot.economy_rule(False, 20, 20, keys_15m=1)[0] is False   # 一个客户端刷出来的拒绝不算全站拥挤


@pytest.mark.asyncio
async def test_economy_enforce_flips_and_announces(tmp_path):
    db = Database(str(tmp_path / "e.sqlite"))
    await db.connect()
    try:
        await db.set_setting("autopilot_economy", "enforce")
        for i in range(8):
            await db.add_log(1 + i % 3, "m", "image", "x", "rejected",
                             detail="当前排队的人太多（全站最多同时排 8 张），请稍后再试", reason="queue_full")
        posts = []
        ann = SimpleNamespace(post=lambda t: posts.append(t))
        st = SimpleNamespace(db=db, guard=None, share=None, announcer=ann, sources=SimpleNamespace(events=[]))
        now = time.time()
        out = await autopilot.run(st, None, now=now)
        assert out["rules"]["economy"]["value"] is True and out["rules"]["economy"].get("applied")
        assert (await db.get_setting("economy_mode")) == "on"
        assert posts and "节约模式已开启" in posts[0]
        # 1 小时内不重复切换（避免公告刷屏）
        out2 = await autopilot.run(st, None, now=now + 60)
        assert not out2["rules"]["economy"].get("applied")
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_upgrade_backfills_reason_codes_for_recent_rejections(tmp_path):
    # 原因码上线前的拒绝只有中文 detail：升级时回填最近 2 天，节约模式 / Key 限流 / AIMD 不会因为升级漏数
    path = str(tmp_path / "r.sqlite")
    db = Database(path)
    await db.connect()
    await db.add_log(1, "m", "image", "x", "rejected", detail="429 当前排队的人太多（全站最多同时排 8 张），请稍后再试")
    await db.add_log(1, "m", "image", "x", "rejected", detail="403 你的 Key 因检测到多人共用已暂停，10-11 自动恢复")
    await db.add_log(1, "m", "image", "x", "rejected", detail="429 本小时出图量已达上限（每小时 150 张）")
    await db.close()
    db = Database(path)
    await db.connect()
    try:
        got = [r[0] for r in await db._db.execute_fetchall("SELECT reason FROM usage_log ORDER BY id")]
        assert got == ["queue_full", "key_paused", "hourly_cap"]
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_key_guard_ignores_site_level_rejections(tmp_path):
    # 审查 F4：全站排队满 / 每小时上限 / 冷却造成的拒绝，不能让自动重试的成员被暂停
    db = Database(str(tmp_path / "k.sqlite"))
    await db.connect()
    try:
        key = await db.create_key({"name": "m", "token": "nai-x", "daily_images": 10, "daily_v5": 0, "features": None,
                                   "daily_anlas": 0, "monthly_anlas": 0, "daily_text_tokens": 0, "rpm": 5,
                                   "allow_anlas": False, "allow_img2img": False, "exclude_global_v5": False,
                                   "image_model_scope": "legacy", "expires_at": None})
        for code in ("queue_full", "hourly_cap", "cooldown", "breaker") * 20:
            await db.add_log(key["id"], "m", "image", "x", "rejected", detail="429", reason=code)
        st = SimpleNamespace(db=db, guard=None, share=None, announcer=None, sources=SimpleNamespace(events=[]))
        out = await autopilot.run(st, None, now=time.time())
        assert out["rules"]["key_guard"]["value"] == []
        for _ in range(60):
            await db.add_log(key["id"], "m", "image", "x", "rejected", detail="429 key busy", reason="key_busy")
        out = await autopilot.run(st, None, now=time.time())
        assert [d["key"] for d in out["rules"]["key_guard"]["value"]] == [key["id"]]
    finally:
        await db.close()
