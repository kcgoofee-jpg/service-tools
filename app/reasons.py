"""拒绝原因码：成员看到的是中文说明，统计 / 自动驾驶 / AIMD 按这里的码数。

以前这些地方用 `detail LIKE '%排队的人太多%'` 数中文，改一个字就静默失效。现在：
guard / 鉴权返回 Reason（就是带 .code 的 str，旧调用方照常当字符串用）→ err(..., code) → usage_log.reason。
"""
from __future__ import annotations

KEY_PAUSED = "key_paused"     # Key 被防分享 / 自动驾驶暂停
QUEUE_FULL = "queue_full"     # 全站排队满（节约模式按它判断拥挤）
KEY_BUSY = "key_busy"         # 这把 Key 上一张还没出完
HOURLY_CAP = "hourly_cap"     # 账号每小时上限（AIMD 按它判断「顶到过上限」）
QUIET_CAP = "quiet_cap"       # 安静时段的每小时上限
CAP_3H = "cap_3h"             # 账号 3 小时上限
DAILY_CAP = "daily_cap"       # 账号每日上限
BREAKER = "breaker"           # 全站熔断


class Reason(str):
    """带原因码的拒绝说明。"""
    code: str = ""

    def __new__(cls, text: str, code: str) -> "Reason":
        obj = super().__new__(cls, text)
        obj.code = code
        return obj


def code_of(text) -> str:
    return getattr(text, "code", "") or ""


# 只用于数据库升级时回填原因码上线之前的旧记录
BACKFILL = (
    (QUEUE_FULL, "%排队的人太多%"),
    (HOURLY_CAP, "%本小时出图量已达上限%"),
    (KEY_PAUSED, "%暂停%"),
)
