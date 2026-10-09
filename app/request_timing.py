"""每个请求的耗时与客户端信息：排队等待、上游生成耗时、客户端名称（User-Agent 摘要）。

用一个可变 dict 放在 ContextVar 里：即使发送上游的代码跑在子任务里（子任务复制的是上下文，
dict 本身是同一个对象），也能把“开始发送上游”的时间点写回来。
"""
from __future__ import annotations

import re
import time
from contextvars import ContextVar
from typing import Optional

_TIMING: ContextVar[Optional[dict]] = ContextVar("gate_request_timing", default=None)
_UNSAFE = re.compile(r"[\x00-\x1f\x7f]+")
CLIENT_MAX = 60


def client_name(user_agent: str) -> str:
    """只保留 User-Agent 的前一段，去掉控制字符；它由客户端自己填写，仅作参考。"""
    text = _UNSAFE.sub(" ", user_agent or "").strip()
    return text[:CLIENT_MAX]


def begin(scope) -> object:
    ua = ""
    for name, value in scope.get("headers") or ():
        if name == b"user-agent":
            ua = value.decode("latin-1", "replace")
            break
    return _TIMING.set({"t0": time.monotonic(), "sent": None, "client": client_name(ua)})


def end(token) -> None:
    _TIMING.reset(token)


def mark_sent() -> None:
    """第一次真正把请求发往上游时调用；重试不覆盖。"""
    holder = _TIMING.get()
    if holder is not None and holder["sent"] is None:
        holder["sent"] = time.monotonic()


def snapshot() -> tuple[int, int, str]:
    """返回 (等待毫秒, 上游耗时毫秒, 客户端)。没发到上游的请求：等待=全程，上游耗时=0。"""
    holder = _TIMING.get()
    if holder is None:
        return 0, 0, ""
    now = time.monotonic()
    sent = holder["sent"]
    if sent is None:
        return int((now - holder["t0"]) * 1000), 0, holder["client"]
    return int((sent - holder["t0"]) * 1000), int((now - sent) * 1000), holder["client"]
