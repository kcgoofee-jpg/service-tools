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
    assert autopilot.slots_rule(50, 3, 0.3, 0)[0] == 55
    assert autopilot.slots_rule(50, 3, 0.3, 12)[0] == 50          # 拥挤时不再加
    assert autopilot.slots_rule(100, 3, 0.1, 0)[0] == 100


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
