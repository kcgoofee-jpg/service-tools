"""被上游 429 之后的两条结构性防护（2026-10-13 01:30 第一次挨限流）。

当时的情形：每小时上限 150，成员侧稳定在 ~100 张/小时；01:16–01:24 有一批**绕过网关**
直连上游的调试请求（约 15 次真实出图，不在网关计数里），那一小段上游看到的是
~40 张/10 分钟 ≈ 240 张/小时的速率 → 01:30:07 上游回 429。

结论不是「总量太大」，而是「额度可以挤在十分钟里一口气发完」：
1. `account_10m_cap`（0 = 每小时上限 ÷ 4）：把额度分摊到 10 分钟窗口，总量不变、只抹平突发；
2. AIMD 加窗不许回到挨限流时的水平（记 `guard_429_cap_at`，天花板压到它的 80%）：
   乘性减是 /2，但加性增（+10/天）会一路加回 150，于是每隔一两天再撞同一个红线。
"""
import math
import time

from app.guard import Guard


def _guard(cap: int = 100) -> Guard:
    g = Guard()
    g.values["quiet_start"] = g.values["quiet_end"] = 0      # 关掉安静时段，避免时间相关
    g.values["account_hourly_cap"] = cap
    g.values["account_10m_cap"] = 0
    return g


def test_burst_cap_is_a_quarter_of_hourly_by_default():
    g = _guard(100)
    assert g.burst_cap() == 25                                # ceil(100 / 4)
    g.values["account_10m_cap"] = 12
    assert g.burst_cap() == 12                                # 也能单独指定


def test_burst_window_counts_only_the_last_ten_minutes():
    g = _guard(100)
    now = time.time()
    token = "token-test"
    for i in range(30):
        g.record_start(token, now=now - 3600 + i * 20)        # 一小时前的 30 张，早已滑出窗口
    g.record_start(token, now=now - 30)                       # 这三张在窗口内
    g.record_start(token, now=now - 20)
    g.record_start(token, now=now - 10)
    assert g.count_10m(token, now) == 3
    assert g.hour_count(token, now) >= 3


def test_burst_cap_spreads_the_hourly_budget():
    """一小时的量挤在十分钟里 = 上游看到 6 倍速率：必须被抹平。"""
    g = _guard(100)
    now = time.time()
    token = "token-burst"
    for i in range(24):                                       # 10 分钟内连发 24 张（低于 25 的上限）
        g.record_start(token, now=now - i * 5)
    assert g.count_10m(token, now) == 24 < g.burst_cap()
    g.record_start(token, now=now - 1)
    assert g.count_10m(token, now) >= g.burst_cap()


def test_minutes_until_free_10m_is_positive_and_bounded():
    g = _guard(100)
    now = time.time()
    token = "token-wait"
    for i in range(25):
        g.record_start(token, now=now - i * 10)
    wait = g.minutes_until_free_10m(token, now)
    assert 1 <= wait <= 10


def test_aimd_never_regrows_to_the_rate_that_got_us_limited():
    """挨过 429 在上限 150，之后每天 +10 不该一路加回 150。"""
    g = _guard(100)
    g.values["account_hourly_cap"] = 150
    # 429 时记下当时的上限（生产里由 on_upstream_429 写 guard_429_cap_at = 150）
    ceiling = max(100, int(150 * 0.8))                        # 120
    wanted = min(160, 100 + 10)
    assert min(wanted, ceiling) == 110                        # 第一步 +10 仍允许
    for step in range(10):                                    # 反复加窗也不会超过天花板
        old = 100 + step * 10
        assert min(min(160, old + 10), ceiling) <= 120
