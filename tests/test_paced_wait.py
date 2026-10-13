"""guard_precheck 的时段放行等位：全是 PACED 时替成员短等，非 PACED 立即拒绝。

2026-10-13：PACED 份额是线性注水的，短等几分钟必有空位；立即 429 会诱发客户端
重试风暴（healings 事件）。每日 / 每小时 / 熔断等不会自愈，照旧立即拒绝。
"""
import pytest
from types import SimpleNamespace

from app import main, reasons
from app.routers import image_routes


def _state(reasons_seq):
    calls = {"n": 0}

    async def token_block_reason(db, token_id, day):
        r = reasons_seq[min(calls["n"], len(reasons_seq) - 1)]
        calls["n"] += 1
        return r

    tok = SimpleNamespace(token_id="t1", usable=True)
    return (SimpleNamespace(guard=SimpleNamespace(token_block_reason=token_block_reason),
                            nai=SimpleNamespace(pool=[tok]), day=lambda: "2026-10-13", db=None), calls)


@pytest.mark.asyncio
async def test_all_paced_becomes_free_within_cap_passes(monkeypatch):
    r_paced = reasons.Reason("份额已用完，约 1 分钟后有空位", reasons.PACED)
    state, _ = _state([r_paced, None])                    # 第一次全 PACED，第二次放行
    monkeypatch.setattr(main, "STATE", state, raising=False)
    monkeypatch.setattr(image_routes, "PACED_POLL_SECONDS", 0.01)
    await image_routes.guard_precheck()                   # 不抛 = 等到份额，继续准入


@pytest.mark.asyncio
async def test_paced_past_cap_raises_429(monkeypatch):
    r_paced = reasons.Reason("份额已用完，约 30 分钟后有空位", reasons.PACED)
    state, _ = _state([r_paced])
    monkeypatch.setattr(main, "STATE", state, raising=False)
    monkeypatch.setattr(image_routes, "PACED_POLL_SECONDS", 0.01)
    monkeypatch.setattr(image_routes, "PACED_WAIT_CAP", 0.05)
    with pytest.raises(main.GateError) as caught:
        await image_routes.guard_precheck()
    assert caught.value.status == 429 and caught.value.code == reasons.PACED


@pytest.mark.asyncio
async def test_non_paced_reason_rejects_immediately(monkeypatch):
    r_hourly = reasons.Reason("本小时出图量已达上限", reasons.HOURLY_CAP)
    state, calls = _state([r_hourly])
    monkeypatch.setattr(main, "STATE", state, raising=False)
    with pytest.raises(main.GateError):
        await image_routes.guard_precheck()
    assert calls["n"] == 1                                # 没进等待循环
