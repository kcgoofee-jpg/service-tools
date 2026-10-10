"""模块内核：整个系统的两个核心原理落在这里。

━━ 1. 模块化 ━━
每个模块只回答一个问题（容量 / 分配 / 调度 / 诚信 / 注册 / ……），声明自己的：
  · question  回答什么问题      · reality  依据哪些现实数据      · principle  用什么数学原理
  · params    关键参数（值 + 依据等级 A 拍脑袋 / B 有参考 / C 实测 / D 多指标互证 + 来源）
  · tick()    周期任务（每 10 分钟）· checks()  交叉校验
模块可以在后台单独启用 / 关闭（设置 module_<名字>），新增模块只要写一个类并 register。
关闭一个模块 = 停止它的自动调整，已经生效的数值保持不变；保护类的硬限制（每小时上限等）不随开关失效。

━━ 2. 多重相互校验 ━━
每轮 tick 之后跑所有模块的 checks()：用另一个独立来源核对这个模块的结论，比如
  · 内存里的每小时计数 ↔ 用量日志（10-10 发现重启会清零，就是这类检查本该抓到的）
  · 分配给所有人的 V5 总和 ↔ 容量模块给出的全站 V5
  · 领 Key 默认额度 ↔ 算法当前给出的额度
不一致就记进 Bug 追踪并在后台标红。会处罚成员的决定要有强证据：由 share_guard 落实（辅助证据只在 72 小时内
出现过「网络 + 客户端」强证据时才计分），诚信模块的校验「每个处罚都有强证据」再独立核对一遍。
"""
from __future__ import annotations

import json
import time
import traceback
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

BASIS = {"A": "拍脑袋", "B": "有参考", "C": "实测", "D": "多指标互证"}
STATE_KEY = "kernel_last"


@dataclass
class Param:
    name: str
    value: Callable[[], Any]          # 读当前生效值（可能来自设置或常量）
    basis: str                        # A / B / C / D
    source: str = ""                  # 依据：文档链接、实测日期、互证的指标
    note: str = ""
    key: str = ""                     # 私密参数键名（params.py）；有私密值时后台标「私密」


@dataclass
class Check:
    name: str
    ok: bool
    detail: str = ""


@dataclass
class Module:
    name: str
    title: str
    question: str
    reality: str
    principle: str
    default_enabled: bool = True
    params: list[Param] = field(default_factory=list)
    tick: Optional[Callable[["Kernel"], Any]] = None
    checks: Optional[Callable[["Kernel"], Any]] = None
    # 有的模块的开关就是已有的设置（例如领 Key 开关 = register_open），用这两个钩子对接
    get_enabled: Optional[Callable[["Kernel"], Any]] = None
    set_enabled: Optional[Callable[["Kernel", bool], Any]] = None
    hard_note: str = ""               # 关闭后哪些东西仍然生效（给站长看）


class Kernel:
    def __init__(self, state, bug: Callable[..., None] = lambda *a, **k: None):
        self.state, self.db, self.bug = state, state.db, bug
        self.modules: dict[str, Module] = {}
        self.last: dict[str, dict] = {}
        self.extra: dict[str, Any] = {}       # 模块需要的外部对象（registrar、通知函数等）

    def register(self, module: Module) -> None:
        self.modules[module.name] = module

    async def enabled(self, name: str) -> bool:
        m = self.modules[name]
        if m.get_enabled is not None:
            return bool(await m.get_enabled(self))
        raw = await self.db.get_setting("module_" + name, None)
        return m.default_enabled if raw is None else str(raw) == "1"

    async def set_enabled(self, name: str, on: bool) -> None:
        m = self.modules[name]
        if m.set_enabled is not None:
            await m.set_enabled(self, on)
        else:
            await self.db.set_setting("module_" + name, "1" if on else "0")

    async def tick_all(self) -> dict[str, dict]:
        """周期任务：按登记顺序运行启用的模块，然后做交叉校验。单个模块出错不影响其他模块。"""
        for name, m in self.modules.items():
            entry = self.last.setdefault(name, {})
            on = await self.enabled(name)
            entry["enabled"] = on
            if not on or m.tick is None:
                continue
            t0 = time.time()
            try:
                result = await m.tick(self)
                entry.update(at=t0, ok=True, ms=int((time.time() - t0) * 1000),
                             summary=str(result)[:300] if result is not None else "", error="")
            except Exception as exc:
                entry.update(at=t0, ok=False, error=f"{type(exc).__name__}: {exc}"[:300])
                self.bug("module:" + name, exc)
        await self.check_all()
        await self.db.set_setting(STATE_KEY, json.dumps(self.last, ensure_ascii=False, default=str))
        return self.last

    async def check_all(self) -> list[tuple[str, Check]]:
        bad: list[tuple[str, Check]] = []
        for name, m in self.modules.items():
            if m.checks is None or not await self.enabled(name):
                continue
            try:
                checks = list(await m.checks(self))
            except Exception as exc:
                checks = [Check("校验本身出错", False, f"{type(exc).__name__}: {exc}"[:200])]
                traceback.print_exc()
            self.last.setdefault(name, {})["checks"] = [c.__dict__ for c in checks]
            for c in checks:
                if not c.ok:
                    bad.append((name, c))
                    self.bug(f"check:{name}:{c.name}", title=f"交叉校验不一致：{m.title} · {c.name}：{c.detail}"[:200],
                             level="warn")
        return bad

    async def snapshot(self) -> list[dict[str, Any]]:
        out = []
        for name, m in self.modules.items():
            params = []
            for p in m.params:
                try:
                    v = p.value()
                    v = await v if hasattr(v, "__await__") else v
                except Exception as exc:
                    v = f"读取失败：{exc}"
                from . import params as _params
                private = bool(p.key) and _params.overridden(p.key)
                params.append({"name": p.name, "value": v, "basis": p.basis, "basis_label": BASIS.get(p.basis, p.basis),
                               "source": p.source, "note": p.note, "private": private})
            out.append({"name": name, "title": m.title, "question": m.question, "reality": m.reality,
                        "principle": m.principle, "enabled": await self.enabled(name), "hard_note": m.hard_note,
                        "periodic": m.tick is not None, "last": self.last.get(name, {}), "params": params})
        return out
