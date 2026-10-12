"""「实际每小时出图 ≤ 上限」这条校验不该在上限刚降下来时误报（2026-10-13 线上实测）。

限流器 `Guard._window(..., HOUR)` 用的是**滚动 60 分钟**窗口，而上限（hourly_cap）本身会变：

- 上游 429 → 立刻减半（`on_upstream_429`，下限 100）
- 平稳一天没有 429 → +10（`adapt_daily`，上限 160/200）
- 进出安静时段 → 在 account_hourly_cap 与 quiet_hourly_cap 之间切换

上限一降，窗口里那些「按当时更高上限放行」的图就显得超限。线上看到 112 张 / 上限 100 就是这么来的。
比对时用窗口内出现过的最高上限（`hourly_cap_ceiling`）即可，不需要放宽阈值。
"""
import time

from app.guard import Guard


def test_ceiling_follows_cap_increases_and_decreases():
    g = Guard()
    now = time.time()
    g.values["account_hourly_cap"] = 160
    g.values["quiet_hourly_cap"] = 20
    g.values["quiet_start"] = g.values["quiet_end"] = 0        # 关掉安静时段，避免时间相关
    assert g.hourly_cap(now) == 160
    assert g.hourly_cap_ceiling(now) == 160

    g.values["account_hourly_cap"] = 100                       # 上游 429 减半
    assert g.hourly_cap(now) == 100
    assert g.hourly_cap_ceiling(now) == 160, "窗口内出现过 160，就不该用 100 去卡这一小时"


def test_ceiling_drops_after_the_window_passes():
    g = Guard()
    now = time.time()
    g.values["quiet_start"] = g.values["quiet_end"] = 0
    g.values["account_hourly_cap"] = 160
    g.hourly_cap(now)
    g.values["account_hourly_cap"] = 100
    g.hourly_cap(now)
    assert g.hourly_cap_ceiling(now) == 160
    later = now + 3601                                          # 一小时之后再比：旧的高值已滑出窗口
    assert g.hourly_cap_ceiling(later) == 100


def test_quiet_window_switch_is_not_reported_as_overshoot():
    """夜间切到安静档（20）时，前一小时的高峰流量不该被算成超限。"""
    g = Guard()
    now = time.time()
    g.values["quiet_start"] = g.values["quiet_end"] = 0
    g.values["account_hourly_cap"] = 120
    g.hourly_cap(now)
    g.values["quiet_start"] = 0                                # 进入安静时段（0 点 ~ 7 点）
    g.values["quiet_end"] = 7
    hour = time.localtime(now).tm_hour
    if 0 <= hour < 7:
        assert g.hourly_cap_ceiling(now) == 120, "安静档只影响之后的放行，不该回头卡已经出掉的图"
    else:
        assert g.hourly_cap_ceiling(now) == 120                # 白天同理：ceiling 至少是窗口内的最高值


def test_ceiling_is_marked_even_without_explicit_calls():
    """hourly_cap() 热路径顺带记录变化，ceiling 不需要额外调用一次 hourly_cap。"""
    g = Guard()
    now = time.time()
    g.values["quiet_start"] = g.values["quiet_end"] = 0
    g.values["account_hourly_cap"] = 150
    ceiling_before = g.hourly_cap_ceiling(now)
    g.values["account_hourly_cap"] = 200
    assert g.hourly_cap_ceiling(now) == 200 and ceiling_before == 150
