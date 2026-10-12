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
PACED = "paced"               # 每日上限按时段放行：这个时段的份额用完了
OVER_BASE = "over_base"       # 用完保底后，全站不空闲 / 时段份额不多，暂时不放行（站点原因，不是成员客户端的问题）
BREAKER = "breaker"           # 全站熔断
COOLDOWN = "cooldown"         # 上游 429 后全站暂停出图
V5_EXHAUSTED = "v5_exhausted" # 账号 V5 免费额度用完（未开通 Anlas 的 Key 不能自动转付费）
QUEUE_TIMEOUT = "queue_timeout"  # 排队 / 预算核对等待超时（站点忙，不是成员的问题）
MEMBER_QUOTA = "member_quota"    # 这把 Key 自己的每日 / 每月额度用完
SITE_QUOTA = "site_quota"        # 全站共享额度（全站 V5 / 全站 Anlas / 月预算 / Vibe 池）用完
KEY_RATE = "key_rate"            # 这把 Key 的请求频率 / 在途请求数超限（客户端行为）
BODY_ABORT = "body_abort"        # 请求体传输中断（客户端或网络，不是上游）

# 全站层面的拒绝：原因在站点（排队满、账号上限、熔断、冷却），不是这个成员的客户端出了问题。
# 自动驾驶的「单 Key 被拒 ≥60 次 → 暂停」不能数这些，否则忙时自动重试的正常成员会被误判为死循环。
SITE_LEVEL = (QUEUE_FULL, HOURLY_CAP, QUIET_CAP, CAP_3H, DAILY_CAP, PACED, OVER_BASE, BREAKER, COOLDOWN,
              V5_EXHAUSTED, QUEUE_TIMEOUT, SITE_QUOTA)


class Reason(str):
    """带原因码的拒绝说明。"""
    code: str = ""

    def __new__(cls, text: str, code: str) -> "Reason":
        obj = super().__new__(cls, text)
        obj.code = code
        return obj


def code_of(text) -> str:
    return getattr(text, "code", "") or ""


# 只用于数据库升级时回填原因码上线之前的旧记录（按顺序匹配，先匹配到的生效）
BACKFILL = (
    (QUEUE_FULL, "%排队的人太多%"),
    (HOURLY_CAP, "%本小时出图量已达上限%"),
    (KEY_PAUSED, "%暂停%"),
    # 站点级在前：全站额度用完的文案里也含「额度已用完」，不能被个人额度那条先吃掉
    (SITE_QUOTA, "%全站今日 V5 额度已用完%"),
    (SITE_QUOTA, "%全站今日 Vibe 额度已抢完%"),
    (SITE_QUOTA, "%全站今日 Anlas 额度已抢完%"),
    (SITE_QUOTA, "%全站本月 Anlas 预算已耗尽%"),
    (MEMBER_QUOTA, "%今日 V4.5 免费图额度已用完%"),
    (MEMBER_QUOTA, "%已达今日 V5 额度%"),
    (MEMBER_QUOTA, "%今日 Anlas 额度不足%"),
    (MEMBER_QUOTA, "%本月 Anlas 额度不足%"),
    (MEMBER_QUOTA, "%今日文本额度%"),
    (QUEUE_TIMEOUT, "%排队超时%"),
    (QUEUE_TIMEOUT, "%等待超时%"),
    (KEY_RATE, "%请求过于频繁%"),
    (KEY_RATE, "%同时进行中的请求过多%"),
    (BODY_ABORT, "%请求体传输中断%"),
)


LABELS = {
    KEY_PAUSED: "Key 被暂停", QUEUE_FULL: "排队满", KEY_BUSY: "上一张没出完就重发", HOURLY_CAP: "本小时全站上限",
    QUIET_CAP: "安静时段上限", CAP_3H: "3 小时全站上限", DAILY_CAP: "全站每日上限", PACED: "全天平滑放行", OVER_BASE: "超出保底暂缓", BREAKER: "全站熔断",
    COOLDOWN: "上游限流暂停", V5_EXHAUSTED: "全站 V5 用完", QUEUE_TIMEOUT: "排队超时", MEMBER_QUOTA: "个人额度用完",
    SITE_QUOTA: "全站额度用完", KEY_RATE: "Key 请求过快", BODY_ABORT: "请求体中断",
}


def reason_label(code: str, detail: str = "") -> str:
    """拒绝原因的短说明：有原因码用码，没有就从 detail 里取第一句（去掉开头的 HTTP 状态码）。"""
    if code and code in LABELS:
        return LABELS[code]
    import re
    text = re.sub(r"^\d{3}\s*", "", str(detail or "")).strip()
    text = re.split(r"[（(，,。：:；;]", text, maxsplit=1)[0].strip()
    return (text[:18] + "…") if len(text) > 18 else (text or "其他")
