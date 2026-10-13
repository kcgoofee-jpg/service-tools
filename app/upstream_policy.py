"""上游响应策略：Retry-After 解析、403 双义判别、状态码分类（2026-10-14 自 nai.py 平移）。

纯函数模块：不做任何标记 / 记账 / 抛错。429/403/401 的后续动作按通道各不相同
（图片 / 图片流 / 文本三处的消息、冷却与熔断策略都在 nai.py 调用点），这里只提供
统一的无副作用判定，供三处共享同一套判据。
"""
from __future__ import annotations

import time
from typing import Any, NamedTuple, Optional


RETRY_AFTER_MIN = 5.0
RETRY_AFTER_MAX = 3600.0


def clamp_retry_after(value: Any) -> float:
    """任何来源的冷却秒数都钳制到 5..3600 的有限值（inf/NaN/超大值不能造成永久停摆）。"""
    try:
        seconds = float(value)
    except (TypeError, ValueError):
        return 20.0
    if seconds != seconds:          # NaN
        return 20.0
    return min(RETRY_AFTER_MAX, max(RETRY_AFTER_MIN, seconds))


def parse_retry_after(header: Optional[str]) -> float:
    """十进制秒数或 RFC 7231 HTTP 日期（Cloudflare / WAF 常用后者）；其它格式按默认 20 秒处理。

    只认数字会让「Retry-After: Wed, 21 Oct 2026 07:28:00 GMT」退化成 20 秒，
    在真实限流窗口内提前重试 —— 这正是最容易被上游升级封控的行为。
    """
    value = (header or "").strip()
    if not value:
        return 20.0
    if value.isascii() and value.isdigit():
        return clamp_retry_after(int(value))
    # 允许多个日期值（RFC 允许逗号分隔），取第一个能解析的
    for part in value.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            from email.utils import parsedate_to_datetime
            when = parsedate_to_datetime(part)
        except (TypeError, ValueError, IndexError):
            continue
        if when is None:
            continue
        if when.tzinfo is None:
            from datetime import timezone
            when = when.replace(tzinfo=timezone.utc)
        return clamp_retry_after(when.timestamp() - time.time())
    return 20.0


# NovelAI 用 403 表示两类完全不同的东西：账号/风控被拒（要看账号状态）与「模型不存在」。
# 2026-10-13 实测：{"statusCode":403,"message":"model 'nai-glm-4-6' doesn't exist"}、
# {"statusCode":403,"message":"model 'glm-4-6' is only available via OpenAI-compatible API"}。
# 后者纯粹是成员填错模型，不能报「账号被隔离」，更不能污染账号健康度。
MODEL_403_MARKERS = ("doesn't exist", "does not exist", "is only available via", "model must be a valid enum")


def is_model_level_403(body_text: str) -> bool:
    low = (body_text or "").lower()
    return any(marker in low for marker in MODEL_403_MARKERS)


class Verdict(NamedTuple):
    kind: str            # rate_limited / forbidden_model / forbidden_account / unauthorized / other
    retry_after: float   # 仅 429 有意义（已钳制 5..3600）；其余类别为 0.0


def classify_upstream(status: int, retry_after_header: Optional[str] = None, peek: str = "") -> Verdict:
    """状态码 +（可选）Retry-After 头 +（仅 403 时）已窥探的响应体 → 统一判据。

    peek 只在调用点确定要窥探时才传入（图片/文本 403 会窥探 300 字节判「模型不存在」；
    其余通道按政策不读上游错误体）。消息与后续动作留在调用点。
    """
    if status == 429:
        return Verdict("rate_limited", parse_retry_after(retry_after_header))
    if status == 403:
        return Verdict("forbidden_model" if is_model_level_403(peek) else "forbidden_account", 0.0)
    if status == 401:
        return Verdict("unauthorized", 0.0)
    return Verdict("other", 0.0)
