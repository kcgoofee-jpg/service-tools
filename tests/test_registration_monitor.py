"""领 Key 模块的常驻校验：开注册前必须能回答「名额能不能兑现」。

2026-10-13 逐行挑战：注册链路的交叉校验只检查了「已领人数 ≤ 名额」，
但名额是 0（不限）时这条永远通过，而真正的约束是「每人额度 × 人数 ≤ 全站日容量」。
这里覆盖四种情况：链路未配置、承诺超容量、领取速度的边际需求、后台与环境变量漂移。
"""
import tempfile
from pathlib import Path

import pytest
import pytest_asyncio

from app import modules, site_flags
from app.config import Settings
from app.database import Database
from app.kernel import Kernel
from app.state import GateState


class FakeRegistrar:
    """最小可用的注册服务替身：只需要 settings() / count_active() 和三个环境默认值。"""

    def __init__(self, *, daily_images=30, daily_v5=0, max_users=10, active=0, cfg=None):
        # 这三个是「环境变量默认值」（configured_service 读 REGISTER_*），settings() 返回后台生效值
        self.key_daily_images, self.key_daily_v5, self.max_users = daily_images, daily_v5, max_users
        self._active, self._cfg = active, cfg or {}

    async def settings(self):
        return {"open": True, "max_users": self._cfg.get("max_users", self.max_users),
                "daily_images": self._cfg.get("daily_images", self.key_daily_images),
                "daily_v5": self._cfg.get("daily_v5", self.key_daily_v5),
                "features": ["image"], "image_scope": "legacy", "expires_days": 30}

    async def count_active(self):
        return self._active


class Tok:
    token_id, usable = "t1", True


@pytest_asyncio.fixture
async def state():
    tmp = tempfile.TemporaryDirectory()
    st = GateState(Settings())
    st.db = Database(str(Path(tmp.name) / "reg.sqlite"))
    await st.db.connect()
    st.guard.db = st.db
    st.share.db = st.db
    st.guard.values["account_daily_cap"] = 1700
    st.nai.pool = [Tok()]
    yield st
    await st.db.close()
    tmp.cleanup()


async def _registration_checks(state, kernel):
    module = kernel.modules["registration"]
    return {c.name: c for c in await module.checks(kernel)}


@pytest.mark.asyncio
async def test_unconfigured_service_blocks_open_switch(state):
    """服务没配起来时：开关关着没事，开着就是「开放了却领不到」。"""
    k = Kernel(state, bug=lambda *a, **kw: None)
    k.register(modules.registration(state))
    k.extra["registrar"] = None

    checks = await _registration_checks(state, k)
    assert checks["注册链路已配置"].ok                      # 默认关闭，没问题

    await site_flags.put(state.db, site_flags.REGISTER_OPEN, True)
    checks = await _registration_checks(state, k)
    assert not checks["注册链路已配置"].ok
    assert "暂不可用" in checks["注册链路已配置"].detail


@pytest.mark.asyncio
async def test_promise_vs_capacity_is_the_real_limit(state):
    """线上口径：每人 79 张 × 138 人 vs 1 个账号 1700 张/天 —— 必须标出来。"""
    reg = FakeRegistrar(daily_images=79, max_users=0, active=138, cfg={"daily_images": 79, "max_users": 0})
    k = Kernel(state, bug=lambda *a, **kw: None)
    k.register(modules.registration(state))
    k.extra["registrar"] = reg

    checks = await _registration_checks(state, k)
    assert checks["已领人数 ≤ 名额"].ok                     # 名额不限（0）时这条照旧通过
    promise = checks["每人承诺 ≤ 全站日容量"]
    assert not promise.ok and promise.level == "warn"
    assert "10902" in promise.detail and "1700" in promise.detail
    assert "只有前约 21 个名额能拿满 79 张" in promise.detail

    seats = next(p for p in k.modules["registration"].params if p.name == "容量能兑现的名额")
    assert "约 21 人" in await seats.value()


@pytest.mark.asyncio
async def test_ramp_limit_and_env_drift(state):
    """领取速度的边际需求；以及后台值被清掉会跳回环境变量的大数。"""
    # 每小时 12 个名额 × 79 张 = 948 张/小时，仍在 1700 张/天以内 → 通过
    reg = FakeRegistrar(daily_images=300, daily_v5=15, max_users=10, active=50,
                        cfg={"daily_images": 79, "daily_v5": 10, "max_users": 0})
    k = Kernel(state, bug=lambda *a, **kw: None)
    k.register(modules.registration(state))
    k.extra["registrar"] = reg

    checks = await _registration_checks(state, k)
    ramp = checks["每小时领取上限 ≤ 全站日容量"]
    assert ramp.ok and "948" in ramp.detail and "56%" in ramp.detail

    drift = checks["后台额度与环境变量一致"]
    assert not drift.ok and drift.level == "warn"
    assert "每人张数 后台 79 / 环境变量 300" in drift.detail
    assert "每人 V5 后台 10 / 环境变量 15" in drift.detail

    hourly = next(p for p in k.modules["registration"].params if p.name == "每小时最多发放")
    assert await hourly.value() == "12 个名额/小时"


@pytest.mark.asyncio
async def test_ramp_over_capacity_is_reported(state):
    """把每小时领取上限调大（模拟一次公告带来的领取高峰）→ 边际需求超过一天容量时报警。"""
    reg = FakeRegistrar(daily_images=79, max_users=0, active=100, cfg={"daily_images": 79, "max_users": 0})
    k = Kernel(state, bug=lambda *a, **kw: None)
    k.register(modules.registration(state))
    k.extra["registrar"] = reg
    await site_flags.put(state.db, site_flags.ISSUE_HOURLY_CAP, 100)      # 100 个名额/小时

    checks = await _registration_checks(state, k)
    ramp = checks["每小时领取上限 ≤ 全站日容量"]
    assert not ramp.ok                                            # level 由 kernel 归一（"" → bad）
    assert ramp.level in ("", "bad")
    assert "7900" in ramp.detail
