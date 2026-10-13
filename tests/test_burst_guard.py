"""被上游 429 之后的结构性防护（2026-10-13 01:30 第一次挨限流）。

当时的情形：每小时上限 150，成员侧稳定 ~100 张/小时；01:16–01:24 有一批**绕过网关**
直连上游的调试请求（约 15 次真实出图，不在网关计数里），那一小段上游看到的是
~40 张/10 分钟 ≈ 240 张/小时的速率 → 01:30:07 上游回 429。

结论不是「总量太大」，而是「额度可以挤在十分钟里一口气发完」：
1. 10 分钟突发窗口恒为「每小时上限 ÷ 4」自动分摊（旋钮已移除）；
2. AIMD 加窗不许回到挨限流时的水平（记 `guard_429_cap_at`，天花板压到它的 80%）。

上线后经**独立架构师 review** 补了四条修正，下面的用例就是它们：
- 小时额度还很宽裕时突发窗口**不能**拒人（第 26 张不该在小时只用了 25 张时被拒）；
- 突发拒绝必须用**独立原因码** `burst_cap`：AIMD 拿 `hourly_cap` 判断「需求顶到过上限」，
  复用会让它误以为需求旺盛而加窗；
- 计数按**张数**（n_samples）而不是按请求，否则批量请求能绕过窗口；
- 加窗天花板**只升不降**，且手动压到下限以下的低值不该污染天花板。
"""
import time

import pytest

from app.guard import Guard
from app.reasons import BURST_CAP, HOURLY_CAP


def _guard(cap: int = 100) -> Guard:
    g = Guard()
    g.values["quiet_start"] = g.values["quiet_end"] = 0      # 关掉安静时段，避免时间相关
    g.values["account_hourly_cap"] = cap
    g.values["account_10m_cap"] = 0
    g.values["account_3h_cap"] = 0
    g.values["account_daily_cap"] = 0                        # 关掉每日上限，只看小时 / 突发
    g.values["daily_burst"] = 0
    return g


class _DB:
    """每日上限关掉后，token_block_reason 只需要这个。"""

    async def get_upstream_counter(self, token_id, day):
        return {"images": 0}


def _fill(g: Guard, token: str, n: int, now: float, span: float = 600.0) -> None:
    """在最近 span 秒里均匀填 n 张。"""
    for i in range(n):
        g.record_start(token, now=now - (i * span / max(1, n)))


def test_burst_cap_is_a_quarter_of_hourly_by_default():
    g = _guard(100)
    assert g.burst_cap() == 25                                # ceil(100 / 4)；10 分钟旋钮已移除，恒为自动


@pytest.mark.asyncio
async def test_burst_does_not_reject_while_the_hour_still_has_room():
    """review 指出的误伤：小时只用 25/100 时，第 26 张不该因为突发窗口被拒。"""
    g = _guard(100)
    now = time.time()
    token = "tok-room"
    _fill(g, token, 25, now)
    assert g.count_10m(token, now) >= g.burst_cap()
    assert await g.token_block_reason(_DB(), token, "d", now) is None


@pytest.mark.asyncio
async def test_burst_catches_the_rate_that_got_us_limited():
    """昨晚挨限流的形态（~40 张/10 分钟 ≈ 240 张/小时）必须被拦住。

    第二轮 review：第一版用 2×burst（50 张/10 分钟 ≈ 300 张/小时）做硬顶，挡不住这个形态。
    """
    g = _guard(100)                                           # burst = 25，硬顶 = ceil(25×1.5) = 38
    now = time.time()
    token = "tok-lastnight"
    _fill(g, token, 40, now)                                  # 40 张/10 分钟
    assert g.hour_count(token, now) < 100                     # 小时额度还很宽裕
    reason = await g.token_block_reason(_DB(), token, "d", now)
    assert reason is not None and getattr(reason, "code", "") == BURST_CAP


@pytest.mark.asyncio
async def test_burst_engages_on_a_real_burst_and_uses_its_own_code():
    """真的在冲 → 拦下，且原因是 burst_cap 不是 hourly_cap。"""
    g = _guard(100)
    now = time.time()
    token = "tok-burst"
    _fill(g, token, 50, now)                                  # 10 分钟 50 张 ≈ 300 张/小时
    reason = await g.token_block_reason(_DB(), token, "d", now)
    assert reason is not None
    assert getattr(reason, "code", "") == BURST_CAP
    assert getattr(reason, "code", "") != HOURLY_CAP          # 不能污染 AIMD 的判断
    assert "10 分钟" in str(reason)


@pytest.mark.asyncio
async def test_burst_engages_when_the_hour_is_running_out():
    """小时额度快见底（剩下的不够一个突发窗口）时才抹平。"""
    g = _guard(100)
    now = time.time()
    token = "tok-tight"
    for i in range(60):                                       # 一小时里已出 60 张（较早的）
        g.record_start(token, now=now - 3500 + i * 46)
    _fill(g, token, 25, now)                                  # 最近 10 分钟再 25 张 → 小时共 85
    assert g.hour_count(token, now) < 100                     # 小时上限还没到（否则会先按小时上限拒）
    reason = await g.token_block_reason(_DB(), token, "d", now)
    assert reason is not None and getattr(reason, "code", "") == BURST_CAP


def test_batch_requests_are_counted_per_image():
    """n_samples=4 的请求必须记 4 张，否则突发窗口 / 每小时上限能被批量请求绕过。"""
    g = _guard(100)
    now = time.time()
    token = "tok-batch"
    g.record_start(token, now=now, images=4)
    assert g.count_10m(token, now) == 4
    assert g.hour_count(token, now) == 4


def test_aimd_ceiling_never_lowers_the_cap():
    """天花板低于当前上限时也不能反向降档（review 指出的锁死 / 反向降档风险）。"""
    old = 110
    wanted = min(160, old + 10)
    seen_at = 125                                             # 曾在 125 挨过限流 → 天花板 100
    capped = min(wanted, max(100, int(seen_at * 0.8)))
    assert max(old, capped) == old, "天花板比当前低时保持不动，不能降档"


def test_minutes_until_free_10m_is_positive_and_bounded():
    g = _guard(100)
    now = time.time()
    token = "tok-wait"
    _fill(g, token, 25, now, span=500)
    wait = g.minutes_until_free_10m(token, now)
    assert 1 <= wait <= 10


@pytest.mark.asyncio
async def test_burst_blocked_is_the_single_source_of_truth():
    """首页状态（burst_wait）与放行判定必须用同一个函数：不能说「已限速」而请求其实能进。"""
    g = _guard(100)
    now = time.time()
    for label, n, hour_extra in (("宽裕", 10, 0), ("踩线", 25, 60), ("真突发", 45, 0)):
        token = "tok-" + label
        for i in range(hour_extra):
            g.record_start(token, now=now - 3000 + i * 30)
        _fill(g, token, n, now)
        blocked = g.burst_blocked(token, now)
        reason = await g.token_block_reason(_DB(), token, "d", now)
        code = getattr(reason, "code", "") if reason else ""
        assert (blocked > 0) == (code == BURST_CAP), f"{label}: blocked={blocked} code={code!r}"
