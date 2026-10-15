//! 命令回复文案：网关桥接响应 → 回复文本。全是纯函数，不碰 Discord / 网络，方便单测。
//!
//! 网关回的内容缺字段或形状不对时返回 [`HandlerError`]，由上层统一回成员「出了点问题……」并打 `[bug]` 日志。
//! 只在某个分支真的用到某个字段时才要求它存在（比如没有 V5 额度时不要求 `v5` 字段）。

use serde_json::{Map, Value};

use crate::events::Interaction;

// ---------- 文案常量 ----------

/// 命令出错时回给成员的话。
pub const GENERIC_ERROR: &str = "出了点问题，已自动记录，请稍后再试；一直不行请在频道里告诉站长。";
/// 连不上网关 / 返回体不是 JSON。
pub const BACKEND_UNAVAILABLE: &str = "服务暂时不可用，请稍后再试。";
/// 网关回了非 200，但响应里没有文字说明（`detail` 不是字符串，例如请求格式校验的错误列表）。
pub const BACKEND_FAILED: &str = "请求失败。";
pub const REGISTER_WRONG_GUILD: &str = "请在指定服务器使用 /register。";
pub const REGISTER_UNAVAILABLE: &str = "领 Key 暂不可用，请稍后再试。";
pub const REGISTER_DONE: &str = "领取成功。";
pub const REGISTER_NOT_ELIGIBLE: &str = "未通过资格检查。";
pub const FEEDBACK_DEFAULT_RECEIPT: &str = "收到啦～";
pub const FEEDBACK_MODAL_TITLE: &str = "猫头鹰公益站 · 反馈";
pub const FEEDBACK_LABEL: &str = "想对站长说的话";
pub const FEEDBACK_PLACEHOLDER: &str =
    "遇到的问题、建议都可以写。只有站长能看到，回执只有你自己可见。";
/// 弹窗和文字框的 custom_id（只有这一个弹窗，固定值就够了，机器人重启后提交也认得）。
pub const FEEDBACK_MODAL_ID: &str = "owl-feedback";
pub const FEEDBACK_TEXT_ID: &str = "text";
/// 文字框的字数范围，和网关一致（网关会再校验一遍）。
pub const FEEDBACK_MIN_LENGTH: u32 = 2;
pub const FEEDBACK_MAX_LENGTH: u32 = 1000;
pub const HELP_COMMANDS: &str = "`/register` 领取 Key（结果仅你可见，含配置教程）· `/quota` 查看今日额度 · `/resetkey` 换新 Key · `/反馈` 写几句话给站长";
pub const HELP_CLOSED: &str = "⚠ 领 Key 当前已关闭，开放时间看站长公告。";
pub const QUOTA_DISABLED: &str = "⚠ 这把 Key 已被停用，请联系站长。";

/// 网关响应读不懂：缺字段，或字段的形状不对。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandlerError {
    #[error("网关响应缺少字段 {0}")]
    Missing(String),
    #[error("网关响应格式不对：{0}")]
    Malformed(String),
}

/// 最多取前 `n` 个字符（按字符数，不会把汉字切半）。
pub fn clip(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

fn obj(v: &Value) -> Result<&Map<String, Value>, HandlerError> {
    v.as_object()
        .ok_or_else(|| HandlerError::Malformed("不是 JSON 对象".into()))
}

/// 必填字段。
fn key<'a>(d: &'a Map<String, Value>, k: &str) -> Result<&'a Value, HandlerError> {
    d.get(k).ok_or_else(|| HandlerError::Missing(k.to_owned()))
}

/// 值算不算「有内容」：null / false / 0 / 空串 / 空列表 / 空对象都算没有。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// 必填字段，并且要「有内容」。
fn truthy_key(d: &Map<String, Value>, k: &str) -> Result<bool, HandlerError> {
    Ok(truthy(key(d, k)?))
}

/// 可选字段的「有内容」（缺键 = 没有）。
fn get_truthy(d: &Map<String, Value>, k: &str) -> bool {
    d.get(k).is_some_and(truthy)
}

/// 把网关给的值拼进文案：字符串原样，其余（数字等）用 JSON 写法。
fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------- 网关响应 ----------

/// 一次网关调用的结果 `(status, data)`：200 时 `data` 是网关的 JSON；
/// 其他状态时 `data` 是 `detail`（取不到则「请求失败。」）；连不上 / 非 JSON 时是 `(0, 服务暂时不可用……)`。
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub status: u16,
    pub data: Value,
}

impl Backend {
    pub fn unavailable() -> Backend {
        Backend {
            status: 0,
            data: Value::String(BACKEND_UNAVAILABLE.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == 200
    }

    /// 非 200 时发给成员的文字：网关的 `detail` 是字符串就原样给，否则给固定的「请求失败。」。
    pub fn text(&self) -> String {
        match &self.data {
            Value::String(s) => s.clone(),
            _ => BACKEND_FAILED.to_owned(),
        }
    }
}

/// 把网关的 HTTP 响应翻译成 [`Backend`]。
/// 先解析 JSON（失败 = 服务暂时不可用，哪怕状态码是 502），再看状态码；
/// 非 200 时取 `detail`，响应不是 JSON 对象则算读不懂。
pub fn interpret_backend(status: u16, body: &[u8]) -> Result<Backend, HandlerError> {
    let Ok(data) = serde_json::from_slice::<Value>(body) else {
        return Ok(Backend::unavailable());
    };
    if status == 200 {
        return Ok(Backend { status, data });
    }
    let detail = obj(&data)?
        .get("detail")
        .cloned()
        .unwrap_or_else(|| Value::String(BACKEND_FAILED.into()));
    Ok(Backend {
        status,
        data: detail,
    })
}

// ---------- /quota ----------

/// 安静时段那一行：上限为 0 时完全暂停出图，大于 0 时只是放慢。
fn quiet_line(d: &Map<String, Value>) -> String {
    let window = d.get("quiet_window").map(show).unwrap_or_default();
    // 网关没给 quiet_cap 时按「暂停」写（旧网关就是这么回的）
    let paused = d
        .get("quiet_cap")
        .and_then(Value::as_f64)
        .is_none_or(|cap| cap <= 0.0);
    if paused {
        format!("🌙 安静时段（{window}）暂停出图，到点自动恢复")
    } else {
        format!("🌙 安静时段（{window}）出图会放慢，到点自动恢复")
    }
}

/// `/quota` 的回复。`info` 是 `/self-register/info` 返回 200 时的 JSON（没拿到就是 `None`，不显示限流冷却）。
/// `now` 是当前 Unix 秒（算 Key 剩余有效期）。
pub fn quota_text(data: &Value, info: Option<&Value>, now: f64) -> Result<String, HandlerError> {
    let d = obj(data)?;
    // 兼容只回 images 的旧网关：legacy 没有就用 images
    let used_legacy = match d.get("legacy") {
        Some(v) => v,
        None => key(d, "images")?,
    };
    let mut first = format!(
        "今日 V4.5：{} / {}",
        show(used_legacy),
        show(key(d, "daily_images")?)
    );
    if let Some(hour) = d.get("reset_hour").filter(|v| !v.is_null()) {
        first.push_str(&format!("（每天 {} 点重置）", show(hour)));
    }
    let mut lines = vec![first];
    if truthy_key(d, "daily_v5")? {
        lines.push(format!(
            "今日 V5：{} / {}",
            show(key(d, "v5")?),
            show(key(d, "daily_v5")?)
        ));
    }
    if get_truthy(d, "daily_anlas") {
        let anlas = d.get("anlas").cloned().unwrap_or_else(|| Value::from(0));
        lines.push(format!(
            "今日 Anlas：{} / {}",
            show(&anlas),
            show(key(d, "daily_anlas")?)
        ));
    }
    if get_truthy(d, "quiet_now") {
        lines.push(quiet_line(d));
    }
    let scope = if key(d, "image_model_scope")? == &Value::String("all".into()) {
        "含 V5"
    } else {
        "仅 V4.5 及以下"
    };
    lines.push(format!("可用模型：{scope}"));

    let features = key(d, "features")?
        .as_array()
        .ok_or_else(|| HandlerError::Malformed("features 不是列表".into()))?;
    let mut opened: Vec<&str> = Vec::new();
    let mut closed: Vec<&str> = Vec::new();
    for f in features {
        let f = obj(f)?;
        let label = key(f, "label")?
            .as_str()
            .ok_or_else(|| HandlerError::Malformed("label 不是字符串".into()))?;
        if truthy_key(f, "on")? {
            opened.push(label);
        } else {
            closed.push(label);
        }
    }
    let opened = if opened.is_empty() {
        "无".to_owned()
    } else {
        opened.join("、")
    };
    lines.push(format!("已开通功能：{opened}"));
    if !closed.is_empty() {
        lines.push(format!("未开通：{}", closed.join("、")));
    }

    let expires = key(d, "expires_at")?;
    if truthy(expires) {
        let at = expires
            .as_f64()
            .filter(|at| at.is_finite())
            .ok_or_else(|| HandlerError::Malformed("expires_at 不是数字".into()))?;
        let days = ((at - now) / 86400.0).floor() as i64;
        lines.push(format!("Key 剩余有效期：约 {} 天", days.max(0)));
    }
    if !truthy_key(d, "enabled")? {
        lines.push(QUOTA_DISABLED.to_owned());
    }

    if let Some(info) = info {
        let up = obj(key(obj(info)?, "upstream")?)?;
        if truthy_key(up, "image_cooldown_seconds")? {
            lines.push(format!(
                "⏳ 上游限流冷却中，约 {} 秒后恢复生图",
                show(key(up, "image_cooldown_seconds")?)
            ));
        }
    }
    Ok(lines.join("\n"))
}

// ---------- /resetkey /help ----------

pub fn resetkey_text(data: &Value) -> Result<String, HandlerError> {
    let k = key(obj(data)?, "key")?;
    Ok(format!(
        "新的 Key（只有你能看到这条消息，请妥善保存）：\n`{}`\n旧 Key 已失效。",
        show(k)
    ))
}

/// `/help`。`info` 是 `/self-register/info` 返回 200 时的 JSON；其他情况只显示前两行。
pub fn help_text(site: &str, info: Option<&Value>) -> Result<String, HandlerError> {
    let mut lines = vec![
        HELP_COMMANDS.to_owned(),
        format!(
            "客户端接口地址填 `{site}`（不加 /v1），Key 填 `nai-` 开头的整串。详细说明：{site}"
        ),
    ];
    if let Some(info) = info {
        let d = obj(info)?;
        if !truthy_key(d, "open")? {
            lines.push(HELP_CLOSED.to_owned());
        }
        let notice = key(d, "notice")?;
        if truthy(notice) {
            let notice = notice
                .as_str()
                .ok_or_else(|| HandlerError::Malformed("notice 不是字符串".into()))?;
            lines.push(format!("📢 {notice}"));
        }
    }
    Ok(lines.join("\n"))
}

// ---------- /register ----------

/// `/register` 发给 `/self-register/issue` 的 JSON。
/// `invite_key` 是已经去掉首尾空白的 /register 选项。
pub fn register_payload(it: &Interaction, invite_key: &str) -> Result<Value, HandlerError> {
    let user = it
        .actor()
        .ok_or_else(|| HandlerError::Malformed("交互里没有用户".into()))?;
    let name = clip(&user.username, 80);
    // 显示名：全局昵称，其次服务器昵称，最后用户名
    let global_name = user
        .global_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| it.nick().filter(|s| !s.is_empty()))
        .or(Some(user.username.as_str()).filter(|s| !s.is_empty()))
        .unwrap_or("");
    let avatar = user.avatar.as_deref().unwrap_or("");
    Ok(serde_json::json!({
        "discord_id": user.id,
        "guild_id": it.guild_id_str(),
        "name": name,
        "username": name,
        "global_name": clip(global_name, 80),
        "avatar": clip(avatar, 120),
        "invite_key": clip(invite_key, 80),
    }))
}

/// `/register` 按网关 HTTP 响应决定回复。网络层失败（连不上 / 超时）由调用方直接用 [`REGISTER_UNAVAILABLE`]。
pub fn register_message(status: u16, body: &[u8]) -> Result<String, HandlerError> {
    match status {
        200 | 403 => {
            // 只有 200 / 403 才解析 JSON；解析失败按「暂不可用」回
            let Ok(data) = serde_json::from_slice::<Value>(body) else {
                return Ok(REGISTER_UNAVAILABLE.to_owned());
            };
            let d = obj(&data)?;
            if status == 200 {
                Ok(match d.get("message").filter(|v| truthy(v)) {
                    Some(m) => show(m),
                    None => REGISTER_DONE.to_owned(),
                })
            } else {
                Ok(d.get("detail")
                    .and_then(Value::as_str)
                    .map_or_else(|| REGISTER_NOT_ELIGIBLE.to_owned(), str::to_owned))
            }
        }
        _ => Ok(REGISTER_UNAVAILABLE.to_owned()),
    }
}

// ---------- /反馈 ----------

/// 弹窗 JSON：一个必填的多行文字框（TextInput，style 2），包在 Label(18) 里。
/// 点 `/反馈` 立刻弹出，不先问网关（网关慢时「先取题目再弹窗」会超过 3 秒让弹窗失败）。
pub fn feedback_modal() -> Value {
    serde_json::json!({
        "custom_id": FEEDBACK_MODAL_ID,
        "title": FEEDBACK_MODAL_TITLE,
        "components": [{
            "type": 18,
            "label": FEEDBACK_LABEL,
            "component": {
                "type": 4,
                "custom_id": FEEDBACK_TEXT_ID,
                "style": 2,
                "min_length": FEEDBACK_MIN_LENGTH,
                "max_length": FEEDBACK_MAX_LENGTH,
                "required": true,
                "placeholder": FEEDBACK_PLACEHOLDER,
            },
        }],
    })
}

/// 弹窗提交里文字框的内容（`events::modal_values` 取出的 custom_id → 值列表）；没找到就是空串，交给网关拒绝。
pub fn feedback_text(submitted: &[(String, Vec<String>)]) -> String {
    submitted
        .iter()
        .find(|(id, _)| id == FEEDBACK_TEXT_ID)
        .and_then(|(_, v)| v.first())
        .cloned()
        .unwrap_or_default()
}

/// 提交后的回执：200 且是对象 → `message`（没有则「收到啦～」），否则网关的说明文字。
pub fn feedback_receipt(b: &Backend) -> String {
    match (&b.data, b.is_ok()) {
        (Value::Object(o), true) => o
            .get("message")
            .map_or_else(|| FEEDBACK_DEFAULT_RECEIPT.to_owned(), show),
        (_, true) => FEEDBACK_DEFAULT_RECEIPT.to_owned(),
        _ => b.text(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_760_000_000.0;

    fn quota_data() -> Value {
        json!({
            "enabled": true, "expires_at": null, "daily_images": 20, "images": 5, "legacy": 4,
            "anlas": 0, "daily_anlas": 0, "daily_v5": 0, "v5": 0, "image_model_scope": "all",
            "reset_hour": 7, "quiet_now": false, "quiet_window": "", "quiet_cap": 20,
            "features": [{"id": "image", "label": "文生图", "on": true}, {"id": "upscale", "label": "放大", "on": false}],
        })
    }

    fn info(cooldown: i64) -> Value {
        json!({"open": true, "notice": "", "upstream": {"image_cooldown_seconds": cooldown}})
    }

    #[test]
    fn quota_minimal() {
        let text = quota_text(&quota_data(), None, NOW).unwrap();
        assert_eq!(
            text,
            "今日 V4.5：4 / 20（每天 7 点重置）\n可用模型：含 V5\n已开通功能：文生图\n未开通：放大"
        );
    }

    #[test]
    fn quota_legacy_falls_back_to_images_and_reset_hour_null_is_hidden() {
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("legacy");
        d["reset_hour"] = Value::Null;
        let text = quota_text(&d, None, NOW).unwrap();
        assert!(text.starts_with("今日 V4.5：5 / 20\n"), "{text}");
    }

    #[test]
    fn quota_all_branches() {
        let mut d = quota_data();
        d["daily_v5"] = json!(3);
        d["v5"] = json!(1);
        d["daily_anlas"] = json!(100);
        d["anlas"] = json!(42);
        d["quiet_now"] = json!(true);
        d["quiet_window"] = json!("2:00–6:00");
        d["quiet_cap"] = json!(0);
        d["image_model_scope"] = json!("legacy");
        d["expires_at"] = json!(NOW + 86400.0 * 3.5);
        d["enabled"] = json!(false);
        let text = quota_text(&d, Some(&info(33)), NOW).unwrap();
        assert_eq!(
            text,
            "今日 V4.5：4 / 20（每天 7 点重置）\n\
             今日 V5：1 / 3\n\
             今日 Anlas：42 / 100\n\
             🌙 安静时段（2:00–6:00）暂停出图，到点自动恢复\n\
             可用模型：仅 V4.5 及以下\n\
             已开通功能：文生图\n\
             未开通：放大\n\
             Key 剩余有效期：约 3 天\n\
             ⚠ 这把 Key 已被停用，请联系站长。\n\
             ⏳ 上游限流冷却中，约 33 秒后恢复生图"
        );
    }

    #[test]
    fn quota_has_no_service_status_line() {
        // 服务状态那一行（docs/v3-deviations.md #74）已删除：info 里就算还带着 status / recent / failed 也不显示
        let info = json!({"open": true, "notice": "", "upstream": {
            "status": "degraded", "recent": 12, "failed": 7, "image_cooldown_seconds": 0}});
        let text = quota_text(&quota_data(), Some(&info), NOW).unwrap();
        assert!(
            !text.contains("服务状态") && !text.contains("近 10 分钟"),
            "{text}"
        );
        assert_eq!(text, quota_text(&quota_data(), None, NOW).unwrap());
    }

    #[test]
    fn quiet_hours_pause_only_when_the_cap_is_zero() {
        let mut d = quota_data();
        d["quiet_now"] = json!(true);
        d["quiet_window"] = json!("3:00–7:00");
        // 上限 > 0：放慢，不是暂停
        d["quiet_cap"] = json!(20);
        let slow = quota_text(&d, None, NOW).unwrap();
        assert!(
            slow.contains("🌙 安静时段（3:00–7:00）出图会放慢，到点自动恢复\n"),
            "{slow}"
        );
        assert!(!slow.contains("暂停出图"), "{slow}");
        // 上限 = 0：暂停出图
        d["quiet_cap"] = json!(0);
        let paused = quota_text(&d, None, NOW).unwrap();
        assert!(
            paused.contains("🌙 安静时段（3:00–7:00）暂停出图，到点自动恢复\n"),
            "{paused}"
        );
        // 网关没给 quiet_cap（旧网关）：按暂停写
        d.as_object_mut().unwrap().remove("quiet_cap");
        assert!(quota_text(&d, None, NOW).unwrap().contains("暂停出图"));
        // 不在安静时段：不显示这一行
        d["quiet_now"] = json!(false);
        assert!(!quota_text(&d, None, NOW).unwrap().contains("安静时段"));
    }

    #[test]
    fn quota_no_open_features_and_expired_key() {
        let mut d = quota_data();
        d["features"] = json!([{"id": "image", "label": "文生图", "on": false}]);
        d["expires_at"] = json!(NOW - 86400.0 * 10.0);
        let text = quota_text(&d, Some(&info(0)), NOW).unwrap();
        assert!(text.contains("已开通功能：无\n未开通：文生图\n"), "{text}");
        assert!(text.ends_with("Key 剩余有效期：约 0 天"), "{text}");
    }

    #[test]
    fn quota_missing_or_malformed_fields_are_errors() {
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("daily_images");
        assert_eq!(
            quota_text(&d, None, NOW),
            Err(HandlerError::Missing("daily_images".into()))
        );
        // legacy 和 images 都没有
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("legacy");
        d.as_object_mut().unwrap().remove("images");
        assert_eq!(
            quota_text(&d, None, NOW),
            Err(HandlerError::Missing("images".into()))
        );
        // 没有 V5 额度时不要求 v5 字段
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("v5");
        assert!(quota_text(&d, None, NOW).is_ok());
        // 有 V5 额度却没有 v5 字段
        d["daily_v5"] = json!(3);
        assert_eq!(
            quota_text(&d, None, NOW),
            Err(HandlerError::Missing("v5".into()))
        );
        assert!(quota_text(&json!([1]), None, NOW).is_err());
        // info 里缺 upstream
        assert_eq!(
            quota_text(&quota_data(), Some(&json!({"open": true})), NOW),
            Err(HandlerError::Missing("upstream".into()))
        );
    }

    #[test]
    fn resetkey_text_shows_the_key_once() {
        assert_eq!(
            resetkey_text(&json!({"key": "nai-abc"})).unwrap(),
            "新的 Key（只有你能看到这条消息，请妥善保存）：\n`nai-abc`\n旧 Key 已失效。"
        );
        assert!(resetkey_text(&json!({})).is_err());
    }

    #[test]
    fn help_variants() {
        let site = "https://site.test";
        let base = format!(
            "{HELP_COMMANDS}\n客户端接口地址填 `{site}`（不加 /v1），Key 填 `nai-` 开头的整串。详细说明：{site}"
        );
        assert_eq!(help_text(site, None).unwrap(), base);
        let open = json!({"open": true, "notice": ""});
        assert_eq!(help_text(site, Some(&open)).unwrap(), base);
        let closed = json!({"open": false, "notice": "今晚维护"});
        assert_eq!(
            help_text(site, Some(&closed)).unwrap(),
            format!("{base}\n⚠ 领 Key 当前已关闭，开放时间看站长公告。\n📢 今晚维护")
        );
        assert!(help_text(site, Some(&json!({"open": true}))).is_err());
        assert!(help_text(site, Some(&json!({"open": true, "notice": 5}))).is_err());
    }

    #[test]
    fn help_lists_exactly_the_member_commands() {
        for name in ["/register", "/quota", "/resetkey", "/反馈"] {
            assert!(HELP_COMMANDS.contains(name), "{name}");
        }
        for gone in ["/open", "/limit", "/ban", "/unban", "/slots", "/revoke"] {
            assert!(!HELP_COMMANDS.contains(gone), "{gone}");
        }
    }

    #[test]
    fn interpret_backend_branches() {
        let ok = interpret_backend(200, br#"{"a":1}"#).unwrap();
        assert_eq!(
            ok,
            Backend {
                status: 200,
                data: json!({"a": 1})
            }
        );
        let denied = interpret_backend(
            403,
            "{\"detail\":\"请在指定服务器使用该命令。\"}".as_bytes(),
        )
        .unwrap();
        assert_eq!(denied.text(), "请在指定服务器使用该命令。");
        // 非 200 又没有 detail
        assert_eq!(
            interpret_backend(500, b"{}").unwrap().text(),
            BACKEND_FAILED
        );
        // 不是 JSON：不管状态码，都是 (0, 服务暂时不可用)
        assert_eq!(
            interpret_backend(502, b"<html>").unwrap(),
            Backend::unavailable()
        );
        assert_eq!(interpret_backend(200, b"").unwrap(), Backend::unavailable());
        // 非 200 且 JSON 不是对象：读不懂
        assert!(interpret_backend(422, b"[1]").is_err());
        // 请求格式校验的错误：detail 是列表，不把内部结构给成员看，给固定文案
        let v = interpret_backend(422, br#"{"detail":[{"msg":"x"}]}"#).unwrap();
        assert_eq!(v.text(), BACKEND_FAILED);
    }

    #[test]
    fn register_messages() {
        assert_eq!(
            register_message(200, "{\"message\":\"Key: nai-x\"}".as_bytes()).unwrap(),
            "Key: nai-x"
        );
        assert_eq!(register_message(200, b"{}").unwrap(), REGISTER_DONE);
        assert_eq!(
            register_message(200, br#"{"message":""}"#).unwrap(),
            REGISTER_DONE
        );
        assert_eq!(
            register_message(403, "{\"detail\":\"账号太新\"}".as_bytes()).unwrap(),
            "账号太新"
        );
        assert_eq!(register_message(403, b"{}").unwrap(), REGISTER_NOT_ELIGIBLE);
        // detail 不是字符串：固定文案
        assert_eq!(
            register_message(403, br#"{"detail":[1]}"#).unwrap(),
            REGISTER_NOT_ELIGIBLE
        );
        assert_eq!(
            register_message(200, b"nope").unwrap(),
            REGISTER_UNAVAILABLE
        );
        assert_eq!(
            register_message(403, b"nope").unwrap(),
            REGISTER_UNAVAILABLE
        );
        assert_eq!(register_message(500, b"{}").unwrap(), REGISTER_UNAVAILABLE);
        assert_eq!(
            register_message(401, b"{\"detail\":\"x\"}").unwrap(),
            REGISTER_UNAVAILABLE
        );
        assert!(register_message(200, b"[1]").is_err());
    }

    #[test]
    fn feedback_receipts() {
        let ok = Backend {
            status: 200,
            data: json!({"message": "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"}),
        };
        assert_eq!(
            feedback_receipt(&ok),
            "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"
        );
        let bare = Backend {
            status: 200,
            data: json!({}),
        };
        assert_eq!(feedback_receipt(&bare), FEEDBACK_DEFAULT_RECEIPT);
        let list = Backend {
            status: 200,
            data: json!([1]),
        };
        assert_eq!(feedback_receipt(&list), FEEDBACK_DEFAULT_RECEIPT);
        let cd = Backend {
            status: 400,
            data: json!("刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。"),
        };
        assert_eq!(
            feedback_receipt(&cd),
            "刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。"
        );
        assert_eq!(
            feedback_receipt(&Backend::unavailable()),
            BACKEND_UNAVAILABLE
        );
    }

    #[test]
    fn feedback_modal_is_one_required_paragraph_in_a_label() {
        assert_eq!(
            feedback_modal().to_string(),
            r#"{"custom_id":"owl-feedback","title":"猫头鹰公益站 · 反馈","components":[{"type":18,"label":"想对站长说的话","component":{"type":4,"custom_id":"text","style":2,"min_length":2,"max_length":1000,"required":true,"placeholder":"遇到的问题、建议都可以写。只有站长能看到，回执只有你自己可见。"}}]}"#
        );
        // Discord 的长度限制：标题 / Label ≤ 45 字，占位符 ≤ 100 字
        assert!(FEEDBACK_MODAL_TITLE.chars().count() <= 45);
        assert!(FEEDBACK_LABEL.chars().count() <= 45);
        assert!(FEEDBACK_PLACEHOLDER.chars().count() <= 100);
    }

    #[test]
    fn feedback_text_comes_from_the_text_input_only() {
        let submitted = vec![
            ("other".to_string(), vec!["不相干".to_string()]),
            ("text".to_string(), vec!["排队有点久".to_string()]),
        ];
        assert_eq!(feedback_text(&submitted), "排队有点久");
        assert_eq!(feedback_text(&submitted[..1]), "");
        assert_eq!(feedback_text(&[]), "");
        assert_eq!(feedback_text(&[("text".to_string(), vec![])]), "");
    }

    fn interaction(user: Value, member_extra: Value) -> Interaction {
        serde_json::from_value(json!({
            "id": "900000000000000010", "application_id": "900000000000000003", "type": 2, "token": "t",
            "guild_id": "900000000000000001",
            "member": {"user": user, "nick": member_extra},
            "data": {"name": "register"},
        }))
        .unwrap()
    }

    #[test]
    fn register_payload_fields_and_order() {
        let it = interaction(
            json!({"id": "900000000000000005", "username": "alice", "global_name": "Alice", "avatar": "abc123"}),
            Value::Null,
        );
        let p = register_payload(&it, "邀请码").unwrap();
        assert_eq!(
            p.to_string(),
            r#"{"discord_id":"900000000000000005","guild_id":"900000000000000001","name":"alice","username":"alice","global_name":"Alice","avatar":"abc123","invite_key":"邀请码"}"#
        );
    }

    #[test]
    fn register_payload_global_name_fallbacks() {
        // 没有 global_name：用服务器昵称
        let it = interaction(
            json!({"id": "900000000000000005", "username": "alice", "global_name": null, "avatar": null}),
            json!("昵称"),
        );
        let p = register_payload(&it, "").unwrap();
        assert_eq!(p["global_name"], "昵称");
        assert_eq!(p["avatar"], "");
        let it = interaction(
            json!({"id": "900000000000000005", "username": "alice"}),
            Value::Null,
        );
        assert_eq!(register_payload(&it, "").unwrap()["global_name"], "alice");
    }

    #[test]
    fn register_payload_truncates_by_code_points() {
        let it = interaction(
            json!({"id": "900000000000000005", "username": "名".repeat(100), "avatar": "a".repeat(200)}),
            Value::Null,
        );
        let p = register_payload(&it, &"k".repeat(100)).unwrap();
        assert_eq!(p["name"].as_str().unwrap().chars().count(), 80);
        assert_eq!(p["avatar"].as_str().unwrap().len(), 120);
        assert_eq!(p["invite_key"].as_str().unwrap().len(), 80);
    }
}
