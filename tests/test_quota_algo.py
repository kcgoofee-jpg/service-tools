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
