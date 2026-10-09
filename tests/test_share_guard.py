"""防分享：一个人换网络 / 两台设备不处罚；多地同时使用逐级处罚（提醒 → 暂停 → 重置 → 停用）。"""
import tempfile
from pathlib import Path

import pytest
import pytest_asyncio

from app.database import Database
from app.share_guard import ShareGuard, RESET

KEY = {"id": 1, "name": "小明", "is_admin": 0, "is_test": 0}
WIN = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
AND = "Mozilla/5.0 (Linux; Android 14)"
IOS = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"


@pytest_asyncio.fixture
async def guard():
    tmp = tempfile.TemporaryDirectory()
    db = Database(str(Path(tmp.name) / "g.sqlite"))
    await db.connect()
    g = ShareGuard(db)
    yield g
    await db.close()
    tmp.cleanup()


class Calls:
    def __init__(self):
        self.member, self.admin, self.resets, self.bans = [], [], [], []

    def kw(self):
        async def member(k, t): self.member.append(t)
        async def reset(k): self.resets.append(k); return "nai-NEW"
        async def ban(k): self.bans.append(k)
        return {"member": member, "admin": self.admin.append, "reset": reset, "ban": ban}


@pytest.mark.asyncio
async def test_one_person_switching_networks_is_not_punished(guard):
    c, t = Calls(), 1_000_000.0
    # 家里 WiFi 一下午，傍晚换到手机流量，晚上回家；电脑 + 手机两台设备
    for i in range(30):
        await guard.observe(KEY, "120.235.*.*", 4, WIN, now=t + i * 120, **c.kw())
    for i in range(10):
        await guard.observe(KEY, "39.144.*.*", 4, AND, now=t + 4000 + i * 60, **c.kw())
    for i in range(10):
        await guard.observe(KEY, "120.235.*.*", 4, WIN, now=t + 9000 + i * 60, **c.kw())
    assert not c.member and not c.resets and not guard.paused_until(1, t + 9700)


@pytest.mark.asyncio
async def test_three_places_at_once_escalates_to_reset_and_ban(guard):
    c, t = Calls(), 2_000_000.0
    nets = ["120.235.*.*", "183.6.*.*", "112.97.*.*"]
    actions = []
    for burst in range(12):
        base = t + burst * 1900                       # 每 ~30 分钟一轮，三地轮流出图
        for i in range(9):
            a = await guard.observe(KEY, nets[i % 3], 4, [WIN, AND, IOS][i % 3], now=base + i * 30, **c.kw())
            if a:
                actions.append(a)
        if guard.paused_until(1, base + 300):
            assert "暂停" in c.member[-1] or "重置" in c.member[-1]
    # 三地 + 三种设备同时出现，第一轮就够暂停（跳过提醒）
    assert actions[0] in ("warn", "pause") and "pause" in actions and "reset" in actions
    assert c.resets and "nai-NEW" in next(m for m in c.member if "新的 Key" in m)
    ev = await guard.evidence(1)
    assert any(e["kind"] == "concurrent" for e in ev) and any(e["kind"] == "action" for e in ev)
    # 继续违规到第 3 次 → 停用并禁止再领取
    for burst in range(12, 80):
        base = t + burst * 1900
        for i in range(9):
            if await guard.observe(KEY, nets[i % 3], 4, WIN, now=base + i * 30, **c.kw()) == "ban":
                break
        if c.bans:
            break
    assert c.bans == [1]


@pytest.mark.asyncio
async def test_vpn_rotating_exits_same_client_is_not_punished(guard):
    """10-09 回测：代理每个请求换出口，但始终是同一个客户端 —— 一个人。"""
    c, t = Calls(), 5_000_000.0
    nodes = ["89.185.*.*", "34.92.*.*", "185.14.*.*", "85.237.*.*", "104.28.*.*"]
    for i in range(200):
        await guard.observe(KEY, nodes[i % 5], 4, WIN + " Chrome/154", now=t + i * 40, busy=i % 2 == 0, **c.kw())
    assert not c.member and not c.admin


@pytest.mark.asyncio
async def test_overlap_two_devices_two_places(guard):
    c, t = Calls(), 6_000_000.0
    a = b = None
    for r in range(6):
        base = t + r * 1900
        await guard.observe(KEY, "120.235.*.*", 4, WIN, now=base, **c.kw())
        a = await guard.observe(KEY, "183.6.*.*", 4, AND, now=base + 20, busy=True, **c.kw())
        b = a or b
    ev = await guard.evidence(1)
    assert any(e["kind"] == "overlap" for e in ev) and b in ("warn", "pause", "reset")


@pytest.mark.asyncio
async def test_observe_mode_and_clear(guard):
    c, t = Calls(), 3_000_000.0
    await guard.db.set_setting("share_guard_mode", "observe")
    nets = ["1.2.*.*", "3.4.*.*", "5.6.*.*"]
    for burst in range(6):
        for i in range(9):
            assert await guard.observe(KEY, nets[i % 3], 4, [WIN, AND, IOS][i % 3], now=t + burst * 1900 + i * 30,
                                       **c.kw()) is None
    assert not c.member and c.admin                    # 观察模式只告诉站长
    rep = await guard.report(now=t + 12000)
    assert rep and rep[0]["score"] >= RESET * 0.5
    await guard.clear(1)
    assert not await guard.report()


@pytest.mark.asyncio
async def test_test_and_admin_keys_skip(guard):
    c = Calls()
    for k in ({**KEY, "is_test": 1}, {**KEY, "is_admin": 1}):
        for i in range(30):
            assert await guard.observe(k, f"{i % 3}.0.*.*", 4, WIN, now=4e6 + i * 20, **c.kw()) is None
    assert not c.member and not c.admin
