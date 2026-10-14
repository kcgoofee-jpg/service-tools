"""场景类 golden（quota / autopilot / modules）共用的假 db：
给旧代码一个能回答查询、并记录每一次写入的 db，不连真实 SQLite。

- 读设置：`get_setting` 按场景给的 dict 回答；写设置会更新这个 dict（旧代码会在同一轮里读回自己刚写的值）。
- 查询：`_db.execute_fetchall(sql, params)` 把 SQL 压成单行空白后交给场景提供的 `fetch(norm_sql, params)`；
  每次查询都记在 `queries` 里（Rust 侧用它核对「服务层该传的参数」）。
- 写入：全部按发生顺序记进 `effects`（与 `gate_core::quota::Effect` 的 JSON 形状一致）。
"""
from __future__ import annotations

import asyncio
from datetime import datetime
from typing import Any, Callable
from zoneinfo import ZoneInfo

TZ = ZoneInfo("Asia/Shanghai")


def norm(sql: str) -> str:
    return " ".join(sql.split())


class FakeConn:
    """`fetch(norm_sql, params) -> (tag, rows)`；tag 是场景给查询起的短名，记进 `queries`（不存 SQL 原文，省体积）。
    不认识的 SQL 直接抛 AssertionError——旧代码新增查询时，生成脚本会立刻报错而不是悄悄漏掉。"""

    def __init__(self, fetch: Callable[[str, tuple], tuple[str, list]],
                 execute: Callable[[str, tuple], str] | None = None):
        self._fetch, self._execute = fetch, execute
        self.queries: list[list[Any]] = []

    async def execute_fetchall(self, sql: str, params: Any = ()) -> list:
        n = norm(sql)
        p = tuple(params)
        tag, rows = self._fetch(n, p)
        self.queries.append([tag, list(p)])
        return rows

    async def execute(self, sql: str, params: Any = ()) -> None:
        n = norm(sql)
        p = tuple(params)
        if self._execute is None:
            raise AssertionError(f"未预期的写 SQL: {n}")
        tag = self._execute(n, p)
        self.queries.append([tag, list(p)])

    async def commit(self) -> None:
        return None


class FakeDb:
    def __init__(self, settings: dict[str, str], effects: list[dict], fetch, execute=None):
        self.settings: dict[str, str] = dict(settings)
        self.effects = effects
        self._db = FakeConn(fetch, execute)

    async def get_setting(self, key: str, default: Any = None) -> Any:
        v = self.settings.get(key)
        return default if v is None else v

    async def set_setting(self, key: str, value: Any) -> None:
        self.settings[key] = str(value)
        self.effects.append({"op": "set", "key": key, "value": str(value)})

    async def set_settings_bulk(self, values: dict[str, Any]) -> None:
        for k, v in values.items():
            self.settings[k] = str(v)
        self.effects.append({"op": "bulk", "pairs": [[k, str(v)] for k, v in values.items()]})


def fake_log(effects: list[dict]):
    async def log_action(db, actor, action, target="", detail="", ok=True):
        effects.append({"op": "log", "actor": actor, "action": action, "target": str(target), "detail": str(detail)})
    return log_action


def make_day(reset_hour: int):
    """复刻 `State.day`（含 `ts or time.time()` 的写法，这里 ts 永远非 0）。"""
    def day(ts=None):
        import time
        ts = (ts or time.time()) - reset_hour * 3600
        return datetime.fromtimestamp(ts, TZ).strftime("%Y-%m-%d")
    return day


def run(coro):
    return asyncio.run(coro)
