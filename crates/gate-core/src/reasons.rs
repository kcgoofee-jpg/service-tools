//! 拒绝原因码（移植自 `app/reasons.py`）：成员看到的是中文说明，统计 / 自动驾驶 / AIMD 按这里的码数。
//!
//! 以前这些地方用 `detail LIKE '%排队的人太多%'` 数中文，改一个字就静默失效。现在 guard / 鉴权返回 [`Reason`]
//! （带 `.code` 的说明文字）→ `err(..., code)` → `usage_log.reason`。所有中文文案是面向成员的，逐字保持。

use crate::pyc;
use crate::pyext::is_nd;

pub const KEY_PAUSED: &str = "key_paused"; // Key 被自动驾驶临时暂停
pub const QUEUE_FULL: &str = "queue_full"; // 全站排队满（节约模式按它判断拥挤）
pub const KEY_BUSY: &str = "key_busy"; // 这把 Key 上一张还没出完
pub const HOURLY_CAP: &str = "hourly_cap"; // 账号每小时上限（AIMD 按它判断「顶到过上限」）
pub const QUIET_CAP: &str = "quiet_cap"; // 安静时段的每小时上限
/// 账号 3 小时上限。这个功能已经删除，但 `usage_log.reason` 里还留着历史记录，
/// 旧版的 `LABELS` / `SITE_LEVEL` 仍然认它，所以常量保留（`reason_label("cap_3h")` 的结果依赖它）。
pub const CAP_3H: &str = "cap_3h";
/// 账号每 10 分钟突发窗口。这个功能 3.0 已删除（docs/v3-deviations.md #64），与 [`CAP_3H`] 一样只为
/// `usage_log.reason` 里的历史记录保留（短说明、回填、单 Key 限流的排除项都还认它）。
pub const BURST_CAP: &str = "burst_cap";
pub const DAILY_CAP: &str = "daily_cap"; // 账号每日上限
pub const PACED: &str = "paced"; // 全站每日上限按时段放行：这个时段的份额用完了
pub const BREAKER: &str = "breaker"; // 全站熔断
pub const COOLDOWN: &str = "cooldown"; // 上游 429 后全站暂停出图
pub const V5_EXHAUSTED: &str = "v5_exhausted"; // 账号 V5 免费额度用完（未开通 Anlas 的 Key 不能自动转付费）
pub const QUEUE_TIMEOUT: &str = "queue_timeout"; // 排队 / 预算核对等待超时（站点忙，不是成员的问题）
pub const MEMBER_QUOTA: &str = "member_quota"; // 这把 Key 自己的每日 / 每月额度用完
pub const SITE_QUOTA: &str = "site_quota"; // 全站共享额度（全站 V5 / 全站 Anlas / 月预算 / Vibe 池）用完
pub const KEY_RATE: &str = "key_rate"; // 这把 Key 的请求频率 / 在途请求数超限（客户端行为）
pub const BODY_ABORT: &str = "body_abort"; // 请求体传输中断（客户端或网络，不是上游）
pub const FEATURE_OFF: &str = "feature_off"; // 该功能没给这把 Key 开通 / 全局关闭（成员可控）
pub const PARAM: &str = "param"; // 参数不被支持（模型、采样器、角色数、权重写法……），成员按提示改就好

/// 全站层面的拒绝：原因在站点（排队满、账号上限、熔断、冷却），不是这个成员的客户端出了问题。
/// 自动驾驶的「单 Key 被拒 ≥60 次 → 暂停」不能数这些，否则忙时自动重试的正常成员会被误判为死循环。
pub const SITE_LEVEL: [&str; 12] = [
    QUEUE_FULL,
    HOURLY_CAP,
    QUIET_CAP,
    BURST_CAP,
    CAP_3H,
    DAILY_CAP,
    PACED,
    BREAKER,
    COOLDOWN,
    V5_EXHAUSTED,
    QUEUE_TIMEOUT,
    SITE_QUOTA,
];

/// 带原因码的拒绝说明（旧版是 `str` 子类，旧调用方照常当字符串用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    text: String,
    code: String,
}

impl Reason {
    pub fn new(text: impl Into<String>, code: &str) -> Self {
        Self {
            text: text.into(),
            code: code.to_string(),
        }
    }

    /// 没有原因码的普通说明（对应旧版里直接传 `str`）。
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, "")
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

impl std::ops::Deref for Reason {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// `getattr(text, "code", "") or ""`
pub fn code_of(reason: &Reason) -> &str {
    reason.code()
}

/// 只用于数据库升级时回填原因码上线之前的旧记录（按顺序匹配，先匹配到的生效；模式是 SQL `LIKE`）。
pub const BACKFILL: &[(&str, &str)] = &[
    (QUEUE_FULL, "%排队的人太多%"),
    (HOURLY_CAP, "%本小时出图量已达上限%"),
    (BURST_CAP, "%本时段出图已达上限%"),
    (KEY_PAUSED, "%暂停%"),
    // 站点级在前：全站额度用完的文案里也含「额度已用完」，不能被个人额度那条先吃掉
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
    // 免费图日额（V5 那条在上面的 SITE_QUOTA 之后匹配，文案含「及以下」）
    (MEMBER_QUOTA, "%免费图额度已用完%"),
    (FEATURE_OFF, "%暂无权使用%"),
    (FEATURE_OFF, "%站长暂未开放此功能%"),
    (FEATURE_OFF, "%本站只提供免费出图%"),
    (KEY_BUSY, "%上一张图还没出完%"),
    // 功能权限：模型范围 / 图生图 / 白名单这类「没给这把 Key 开」的拒绝
    (FEATURE_OFF, "%模型权限：仅允许%"),
    (FEATURE_OFF, "%img2img 未开放%"),
    (FEATURE_OFF, "%未列入本站图片模型白名单%"),
    // 参数问题：文案千变万化，只能按几个明确的片段匹配（成员按提示改客户端设置即可）
    (PARAM, "%不支持 V4/V4.5/V5 模型%"),
    (PARAM, "%角色太多%"),
    (PARAM, "%异常权重%"),
    (PARAM, "%必须是非负整数%"),
    (PARAM, "%权重括号嵌套过深%"),
];

/// 原因码 → 给站长看的短说明（`LABELS`，顺序同旧版）。
pub const LABELS: &[(&str, &str)] = &[
    (KEY_PAUSED, "Key 被暂停"),
    (QUEUE_FULL, "排队满"),
    (KEY_BUSY, "上一张没出完就重发"),
    (HOURLY_CAP, "本小时全站上限"),
    (BURST_CAP, "突发窗口限速"),
    (QUIET_CAP, "安静时段上限"),
    (CAP_3H, "3 小时全站上限"),
    (DAILY_CAP, "全站每日上限"),
    (PACED, "全天平滑放行"),
    (BREAKER, "全站熔断"),
    (COOLDOWN, "上游限流暂停"),
    (V5_EXHAUSTED, "全站 V5 用完"),
    (QUEUE_TIMEOUT, "排队超时"),
    (MEMBER_QUOTA, "个人额度用完"),
    (SITE_QUOTA, "全站额度用完"),
    (KEY_RATE, "Key 请求过快"),
    (BODY_ABORT, "请求体中断"),
];

/// `^\d{3}\s*`：开头三个十进制数字（HTTP 状态码）加后面的空白。
fn strip_status_prefix(detail: &str) -> &str {
    let mut it = detail.char_indices();
    for _ in 0..3 {
        match it.next() {
            Some((_, c)) if is_nd(c) => {}
            _ => return detail,
        }
    }
    let rest = it.next().map_or("", |(i, _)| &detail[i..]);
    rest.trim_start_matches(pyc::is_space)
}

/// 拒绝原因的短说明：有原因码用码，没有就从 detail 里取第一句（去掉开头的 HTTP 状态码）。
pub fn reason_label(code: &str, detail: &str) -> String {
    if !code.is_empty()
        && let Some((_, label)) = LABELS.iter().find(|(c, _)| *c == code)
    {
        return (*label).to_string();
    }
    let text = pyc::strip(strip_status_prefix(detail));
    // re.split(r"[（(，,。：:；;]", text, maxsplit=1)[0].strip()
    let first = text
        .split(['（', '(', '，', ',', '。', '：', ':', '；', ';'])
        .next()
        .unwrap_or("");
    let text = pyc::strip(first);
    if pyc::len_chars(text) > 18 {
        format!("{}…", pyc::take_chars(text, 18))
    } else if text.is_empty() {
        "其他".to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_by_code_or_first_sentence() {
        assert_eq!(reason_label(QUEUE_FULL, "429 whatever"), "排队满");
        // 已删除功能的历史记录仍然显示旧文案
        assert_eq!(reason_label(CAP_3H, ""), "3 小时全站上限");
        assert_eq!(
            reason_label("", "429 排队的人太多（请稍后）"),
            "排队的人太多"
        );
        assert_eq!(reason_label("", ""), "其他");
        assert_eq!(
            reason_label("unknown", "403 暂无权使用，联系站长"),
            "暂无权使用"
        );
        assert_eq!(
            reason_label("", &"字".repeat(19)),
            format!("{}…", "字".repeat(18))
        );
    }

    #[test]
    fn reason_carries_code_like_a_str_subclass() {
        let r = Reason::new("排队的人太多", QUEUE_FULL);
        assert_eq!(code_of(&r), "queue_full");
        assert_eq!(&*r, "排队的人太多");
        assert_eq!(code_of(&Reason::plain("x")), "");
    }
}
