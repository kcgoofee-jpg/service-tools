"""每个请求的耗时与客户端信息：排队等待、上游生成耗时、客户端名称（User-Agent 摘要）。

用一个可变 dict 放在 ContextVar 里：即使发送上游的代码跑在子任务里（子任务复制的是上下文，
dict 本身是同一个对象），也能把“开始发送上游”的时间点写回来。
"""
from __future__ import annotations

import re
import secrets
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
    return _TIMING.set({"t0": time.monotonic(), "sent": None, "client": client_name(ua), "status": 0,
                        "rid": secrets.token_hex(4)})


def rid() -> str:
    """本请求的编号（8 位十六进制），写进响应头 X-Request-Id 和日志，成员报错时给站长看这个就能定位。"""
    holder = _TIMING.get()
    return holder["rid"] if holder is not None else ""


def end(token) -> None:
    _TIMING.reset(token)


def mark_sent() -> None:
    """第一次真正把请求发往上游时调用；重试不覆盖。"""
    holder = _TIMING.get()
    if holder is not None and holder["sent"] is None:
        holder["sent"] = time.monotonic()
        hook = holder.get("on_sent")
        if hook is not None:
            hook()


def on_sent(hook) -> None:
    """登记一个回调：本请求第一次发往上游时调用（实时架构图把这张图从「排队」移到「生成中」）。"""
    holder = _TIMING.get()
    if holder is not None:
        holder["on_sent"] = hook


def mark_status(code: int) -> None:
    """记录上游最后一次返回的 HTTP 状态码（429 / 5xx 等），用于分析上游是否在限流或封控。"""
    holder = _TIMING.get()
    if holder is not None:
        holder["status"] = int(code)


def snapshot() -> dict:
    """wait_ms：收到请求到发往上游；dur_ms：上游耗时（没发到上游为 0）；up_status：上游状态码（没收到为 0）。"""
    holder = _TIMING.get()
    if holder is None:
        return {"wait_ms": 0, "dur_ms": 0, "client": "", "up_status": 0, "rid": "", "src": ""}
    now = time.monotonic()
    sent = holder["sent"]
    if sent is None:
        wait, dur = now - holder["t0"], 0.0
    else:
        wait, dur = sent - holder["t0"], now - sent
    return {"wait_ms": int(wait * 1000), "dur_ms": int(dur * 1000), "client": holder["client"],
            "up_status": holder["status"], "rid": holder.get("rid", ""), "src": holder.get("src", "")}


def set_source(label: str) -> None:
    """记下本请求的来源网络打码标签，写进用量日志（防分享溯源）。"""
    holder = _TIMING.get()
    if holder is not None:
        holder["src"] = label or ""
