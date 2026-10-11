"""每日上限按剩余时间摊开（daily_pacing）：默认关；打开后上午用得多，每小时上限自动收紧，撑到 0 点。"""
from datetime import datetime
from zoneinfo import ZoneInfo

import pytest

from app.guard import Guard, PACING_FLOOR

TZ = ZoneInfo("Asia/Shanghai")


class _DB:
    def __init__(self, used):
        self.used = used

    async def get_upstream_counter(self, token_id, day):
        return {"images": self.used, "v5": 0, "units": float(self.used)}


def _at(h, m=0):
    return datetime(2026, 10, 11, h, m, tzinfo=TZ).timestamp()


def _fill(g, now, n):
    for i in range(n):
        g.record_start("t", now - 30 - i)


@pytest.mark.asyncio
async def test_pacing_off_by_default_keeps_old_behaviour():
    g = Guard()
    g.values["account_daily_cap"] = 1500
    now = _at(10)
    _fill(g, now, 50)
    assert g.values["daily_pacing"] == 0
    assert await g.token_block_reason(_DB(1100), "t", "d", now) is None


@pytest.mark.asyncio
async def test_pacing_spreads_remaining_until_midnight():
    g = Guard()
    g.values.update(account_daily_cap=1500, daily_pacing=1)
    now = _at(10)                                   # 剩 400 张、14 小时 → 每小时 29 张
    assert await g.paced_cap(_DB(1100), "t", "d", now) == 29
    _fill(g, now, 28)
    assert await g.token_block_reason(_DB(1100), "t", "d", now) is None
    _fill(g, now - 100, 1)
    r = await g.token_block_reason(_DB(1100), "t", "d", now)
    assert r and "29" in r and "0 点" in r


@pytest.mark.asyncio
async def test_pacing_never_looser_than_hourly_cap_and_has_floor():
    g = Guard()
    g.values.update(account_daily_cap=1500, account_hourly_cap=125, daily_pacing=1)
    assert await g.paced_cap(_DB(0), "t", "d", _at(23)) > 125      # 晚上剩很多：由普通每小时上限管
    _fill(g, _at(23), 125)
    assert "本小时出图量已达上限" in (await g.token_block_reason(_DB(0), "t", "d", _at(23)) or "")
    assert await g.paced_cap(_DB(1499), "t", "d", _at(8)) == PACING_FLOOR
    assert "已达上限" in (await g.token_block_reason(_DB(1500), "t", "d", _at(8)) or "")   # 每日上限仍优先
