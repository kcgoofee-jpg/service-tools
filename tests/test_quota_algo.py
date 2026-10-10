"""动态额度算法：V5 按恢复速度分配、V4.5 上限 / 保底每日微调、只管理 quota_auto=1 的成员 Key。"""
import json
import time
from types import SimpleNamespace

import pytest

from app import quota_algo
from app.database import Database
from app.guard import Guard

CFG = dict(quota_algo.DEFAULTS)


def test_v5_plan_spends_surplus_and_tightens_when_low():
    full = quota_algo.v5_plan(98, 11.0, active=14)
    assert full["global"] == int(11 * 14.2 * 1.3) and full["each"] == full["global"] // 14
    low = quota_algo.v5_plan(15, 11.0, active=14)
    assert low["global"] < full["global"] and low["each"] >= 3            # 剩得少就收紧，但不低于下限
    few = quota_algo.v5_plan(98, 11.0, active=2)
    assert few["people"] == 10 and few["each"] <= 30                       # 至少按 10 人算，单人不超过 30
    assert quota_algo.v5_plan(None, None, 14)["rate"] == 11.0              # 拿不到实测值按 11%/天


def test_daily_adjust_rules():
    # 拥挤：上限和保底都下调，保底不高于上限
    a, b, why = quota_algo.daily_adjust(150, 100, used=900, cap=1000, hourly_blocks=0, ceiling_hits=0, base_blocks=0, cfg=CFG)
    assert (a, b) == (125, 90) and "拥挤" in why[0]
    # 3 个不同小时被每小时上限拦过才算拥挤；只在 1～2 个小时扎堆（几分钟内的连拦）不下调
    a, b, _ = quota_algo.daily_adjust(150, 100, used=100, cap=1000, hourly_blocks=3, ceiling_hits=3, base_blocks=0, cfg=CFG)
    assert (a, b) == (125, 90)
    a, b, why = quota_algo.daily_adjust(150, 100, used=200, cap=1000, hourly_blocks=1, ceiling_hits=0, base_blocks=0, cfg=CFG)
    assert (a, b) == (150, 100) and "无需调整" in why[0]
    # 有余量且有人顶格：放宽上限；有人被保底拦：抬保底
    a, b, _ = quota_algo.daily_adjust(150, 100, used=300, cap=1000, hourly_blocks=0, ceiling_hits=2, base_blocks=5, cfg=CFG)
    assert (a, b) == (175, 110)
    # 上下限
    a, b, _ = quota_algo.daily_adjust(300, 100, used=300, cap=1000, hourly_blocks=0, ceiling_hits=2, base_blocks=0, cfg=CFG)
    assert a == 300
    a, b, why = quota_algo.daily_adjust(150, 100, used=700, cap=1000, hourly_blocks=0, ceiling_hits=0, base_blocks=0, cfg=CFG)
    assert (a, b) == (150, 100) and "无需调整" in why[0]


@pytest.mark.asyncio
async def test_run_applies_same_quota_to_auto_members_only(tmp_path):
    db = Database(str(tmp_path / "g.sqlite"))
    await db.connect()
    try:
        base = {"daily_images": 300, "daily_anlas": 0, "daily_v5": 15, "monthly_anlas": 0, "daily_text_tokens": 0,
                "rpm": 10, "allow_anlas": False, "allow_img2img": False, "exclude_global_v5": False}
        old = await db.create_key({"name": "老成员", "token": "nai-old", "image_model_scope": "all", **base})
        new = await db.create_key({"name": "新成员", "token": "nai-new", "image_model_scope": "legacy", **{**base, "daily_images": 150, "daily_v5": 0}})
        manual = await db.create_key({"name": "手动", "token": "nai-man", "image_model_scope": "legacy", **base})
        test = await db.create_key({"name": "测试", "token": "nai-test", "image_model_scope": "legacy", **base})
        await db._db.execute("UPDATE api_keys SET quota_auto=-1 WHERE id=?", (manual["id"],))
        await db._db.execute("UPDATE api_keys SET is_test=1 WHERE id=?", (test["id"],))
        await db._db.commit()
        guard = Guard(db)

        class Allow:
            async def snapshot(self, pool):
                return {"accounts": [{"percent": 98, "recharge_per_day": 11.0}]}
        st = SimpleNamespace(db=db, guard=guard, nai=SimpleNamespace(pool=[SimpleNamespace(usable=True)], allowance=Allow()))
        r = await quota_algo.run(st, now=time.time())
        assert r["ceiling"] == 150 and r["base"] == 100 and r["review"] is not None
        for k in (old, new):
            row = await db.get_key(k["id"])
            assert row["daily_images"] == 150 and row["daily_v5"] == r["v5"]["each"] and row["image_model_scope"] == "all"
        assert (await db.get_key(manual["id"]))["image_model_scope"] == "legacy"       # 手动的不覆盖
        assert (await db.get_key(test["id"]))["daily_v5"] == 15                        # 测试 Key 不参与
        assert int(await db.get_setting("global_daily_v5", 0)) == r["v5"]["global"]
        assert await db.get_setting("register_image_scope", "") == "all"
        # 同一天第二次运行不再做每日微调
        assert (await quota_algo.run(st, now=time.time()))["review"] is None
        assert len(json.loads(await db.get_setting(quota_algo.HISTORY_KEY, "[]"))) == 1
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_economy_mode_doubles_v5_and_reverts(tmp_path):
    # 节约模式下免费档 14 步，每张只扣约一半额度：V5 每人 / 全站额度 ×2；关掉后回到基础值；账号剩余低时不放大
    from app import ops, site_flags
    db = Database(str(tmp_path / "e.sqlite"))
    await db.connect()
    try:
        k = await db.create_key({"name": "m", "token": "nai-m", "daily_images": 150, "daily_anlas": 0, "daily_v5": 0,
                                 "monthly_anlas": 0, "daily_text_tokens": 0, "rpm": 10, "allow_anlas": False,
                                 "allow_img2img": False, "exclude_global_v5": False, "image_model_scope": "all"})
        pct = {"v": 98}

        class Allow:
            async def snapshot(self, pool):
                return {"accounts": [{"percent": pct["v"], "recharge_per_day": 11.0}]}
        st = SimpleNamespace(db=db, guard=Guard(db), announcer=None,
                             nai=SimpleNamespace(pool=[SimpleNamespace(usable=True)], allowance=Allow()))
        base = (await quota_algo.run(st))["v5"]
        assert not base.get("economy")
        assert await ops.set_economy(db, True, st)                      # 开启时立刻重算
        on = json.loads(await db.get_setting(quota_algo.STATE_KEY, "{}"))["v5"]
        assert on["economy"] == 2 and on["each"] == base["each"] * 2 and on["global"] == base["global"] * 2
        assert (await db.get_key(k["id"]))["daily_v5"] == on["each"]
        assert int(await db.get_setting("global_daily_v5", 0)) == on["global"]
        assert "节约模式 ×2" in await db.get_setting(quota_algo.NOTICE_KEY, "")
        pct["v"] = 30                                                   # 账号剩余跌破 40%：安全优先，不放大
        assert not (await quota_algo.run(st))["v5"].get("economy")
        pct["v"] = 98
        await ops.set_economy(db, False, st)                            # 关掉：回到当天的基础方案（上面收紧过就是收紧后的值）
        day_plan = json.loads(await db.get_setting(quota_algo.V5_DAY_KEY, "{}"))["plan"]
        assert (await db.get_key(k["id"]))["daily_v5"] == day_plan["each"] and not day_plan.get("economy")
        assert await site_flags.get(db, site_flags.ECONOMY) is False
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_pinned_manual_v5_never_below_members_and_scales_with_economy(tmp_path):
    from app import ops
    db = Database(str(tmp_path / "p.sqlite"))
    await db.connect()
    try:
        base = {"daily_images": 150, "daily_anlas": 0, "daily_v5": 0, "monthly_anlas": 0, "daily_text_tokens": 0,
                "rpm": 10, "allow_anlas": True, "allow_img2img": False, "exclude_global_v5": True, "image_model_scope": "all"}
        low = await db.create_key({"name": "熟人-低", "token": "nai-low", **base})
        high = await db.create_key({"name": "熟人-高", "token": "nai-high", **base})
        await db._db.execute("UPDATE api_keys SET quota_auto=-1, v5_pinned=5 WHERE id=?", (low["id"],))
        await db._db.execute("UPDATE api_keys SET quota_auto=-1, v5_pinned=50 WHERE id=?", (high["id"],))
        await db._db.commit()

        class Allow:
            async def snapshot(self, pool):
                return {"accounts": [{"percent": 98, "recharge_per_day": 11.0}]}
        st = SimpleNamespace(db=db, guard=Guard(db), announcer=None,
                             nai=SimpleNamespace(pool=[SimpleNamespace(usable=True)], allowance=Allow()))
        each = (await quota_algo.run(st))["v5"]["each"]
        assert (await db.get_key(low["id"]))["daily_v5"] == each          # 手动定得比大家少：抬到普通成员的值
        assert (await db.get_key(high["id"]))["daily_v5"] == 50
        await ops.set_economy(db, True, st)
        assert (await db.get_key(low["id"]))["daily_v5"] == each * 2
        assert (await db.get_key(high["id"]))["daily_v5"] == 100          # 节约模式一样翻倍
        assert (await db.get_key(high["id"]))["v5_pinned"] == 50          # 基础值不被改写
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_daily_account_cap_counts_compute_units(tmp_path):
    # 节约模式的图算半张：1000 张上限下，2 张 14 步的图只占 1 份
    db = Database(str(tmp_path / "u.sqlite"))
    await db.connect()
    try:
        guard = Guard(db)
        await guard.save({"account_daily_cap": 2})
        await db.bump_upstream_image_counter("tok", "2026-10-10", 2, 0.5)
        c = await db.get_upstream_counter("tok", "2026-10-10")
        assert c["images"] == 2 and c["units"] == 1.0
        assert await guard.token_block_reason(db, "tok", "2026-10-10") is None
        await db.bump_upstream_image_counter("tok", "2026-10-10", 1)
        assert "已达上限" in (await guard.token_block_reason(db, "tok", "2026-10-10") or "")
    finally:
        await db.close()
