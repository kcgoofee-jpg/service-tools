"""服务日起点 DAY_RESET_HOUR（默认 7 点）：day() 与账号注水桶都按它划分，而不是本地 0 点。

2026-10-13 站长：0 点刷新把「满血复活 + 底仓 burst」砸在深夜高峰（0-2 点是全天最高），
改到早上 7 点——要么在上班路上没空用，要么周末没这么早起。
"""
import time
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

from app.config import Settings
from app.guard import Guard
from app.state import GateState


def _ts(hour: int, minute: int = 0) -> float:
    """今天本地时区 hour:minute 的时间戳。"""
    tz = ZoneInfo("Asia/Shanghai")
    return datetime.now(tz).replace(hour=hour, minute=minute, second=0, microsecond=0).timestamp()


def test_day_rolls_at_reset_hour_not_midnight(tmp_path):
    settings = Settings(data_dir=Path(tmp_path), nai_tokens=[], day_reset_hour=7)
    state = GateState(settings)
    now_hour = datetime.now(ZoneInfo(settings.tz)).hour
    if now_hour < 7:      # 本地还在服务日的"前半段"：0-7 点属于昨天的服务日
        assert state.day(_ts(6, 59)) == state.day(_ts(0, 1))
        assert state.day(_ts(7, 1)) != state.day(_ts(6, 59))
    else:                                        # 本地已过 7 点：6:59 是昨天、7:01 起是今天
        today = state.day(_ts(12))
        assert state.day(_ts(6, 59)) != today
        assert state.day(_ts(7, 1)) == today
    # 0 点重置的旧口径：两者相同
    old = Settings(data_dir=Path(tmp_path), nai_tokens=[], day_reset_hour=0)
    assert GateState(old).day(_ts(6, 59)) == GateState(old).day(_ts(6, 0))


def test_guard_paced_bucket_refills_from_reset_hour(tmp_path):
    g = Guard(reset_hour=7, tz="Asia/Shanghai")
    g.values["account_daily_cap"] = 2400         # 每小时补 100 张
    g.values["daily_burst"] = 100
    # 7:30：服务日刚开始半小时 → 应许量 = burst + 2400*0.5/24 = 150
    assert abs(g._day_elapsed(_ts(7, 30)) - 1800) < 5
    assert abs(g.paced_allowance(_ts(7, 30)) - 150) < 3
    # 6:30：还属于昨天的服务日，已经过去 23.5 小时 → 应许量贴近全天上限
    assert g._day_elapsed(_ts(6, 30)) > 23 * 3600 - 10
    assert g.paced_allowance(_ts(6, 30)) >= 2390
    # 0 点重置的旧口径对照：0:30 的 elapsed 是半小时
    g0 = Guard(reset_hour=0, tz="Asia/Shanghai")
    g0.values["account_daily_cap"] = 2400
    g0.values["daily_burst"] = 100
    assert abs(g0._day_elapsed(_ts(0, 30)) - 1800) < 5
