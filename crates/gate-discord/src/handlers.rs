//! 命令回复文案：网关桥接响应 → 回复文本。全是纯函数，不碰 Discord / 网络，方便单测。
//!
//! 与 `integration/discord_bot.py` / `discord_registration.py` 逐分支对应，文案逐字照抄。
//! Python 里 `data["x"]` 缺键会抛 `KeyError`、类型不对会抛 `TypeError` / `AttributeError`，
//! 这些异常最终被 `tree.error` 接住并回「出了点问题……」；这里统一返回 `HandlerError`，由上层回同一句话。
//! 注意保持 Python 的求值短路：某个分支没走到，就不能去要那个键。

use gate_core::{pyc, pyval};
use serde_json::{Map, Value};

use crate::events::Interaction;
use crate::pystr::py_str;

// ---------- 文案常量（测试里与 Python 源码里的字符串逐字比对） ----------

/// `tree.error`：命令抛出任何异常时回给成员的话。
pub const GENERIC_ERROR: &str = "出了点问题，已自动记录，请稍后再试；一直不行请在频道里告诉站长。";
/// `backend()` 连不上网关 / 返回体不是 JSON。
pub const BACKEND_UNAVAILABLE: &str = "服务暂时不可用，请稍后再试。";
/// `backend()` 非 200 且响应里没有 `detail`。
pub const BACKEND_FAILED: &str = "请求失败。";
pub const REGISTER_WRONG_GUILD: &str = "请在指定服务器使用 /register。";
pub const REGISTER_NOT_CONFIGURED: &str = "领 Key 功能尚未配置完成。";
pub const REGISTER_UNAVAILABLE: &str = "领 Key 暂不可用，请稍后再试。";
pub const REGISTER_DONE: &str = "领取成功。";
pub const REGISTER_NOT_ELIGIBLE: &str = "未通过资格检查。";
pub const FEEDBACK_DEFAULT_RECEIPT: &str = "收到啦～";
// /反馈 在 3.0 改成一段文字（docs/v3-deviations.md #52）：下面这几句是 3.0 新写的，不在旧 Python 里。
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
pub const HELP_COMMANDS: &str = "`/register` 领取 Key（结果仅你可见，含配置教程）· `/quota` 额度和服务状态 · `/resetkey` 换新 Key · `/反馈` 写几句话给站长";
pub const HELP_CLOSED: &str = "⚠ 领 Key 当前已关闭，开放时间看站长公告。";
pub const QUOTA_DISABLED: &str = "⚠ 这把 Key 已被停用，请联系站长。";

/// 全部「整句常量」，供文案测试遍历。
pub const ALL_COPY: &[&str] = &[
    GENERIC_ERROR,
    BACKEND_UNAVAILABLE,
    BACKEND_FAILED,
    REGISTER_WRONG_GUILD,
    REGISTER_NOT_CONFIGURED,
    REGISTER_UNAVAILABLE,
    REGISTER_DONE,
    REGISTER_NOT_ELIGIBLE,
    FEEDBACK_DEFAULT_RECEIPT,
    FEEDBACK_MODAL_TITLE,
    FEEDBACK_LABEL,
    FEEDBACK_PLACEHOLDER,
    HELP_COMMANDS,
    HELP_CLOSED,
    QUOTA_DISABLED,
];

/// 上游状态 → 文案（`STATUS_TEXT`）。
const STATUS_TEXT: &[(&str, &str)] = &[
    ("ok", "🟢 正常"),
    ("idle", "🟢 正常（近期无请求）"),
    ("degraded", "🟠 不稳定（近期失败较多）"),
];

/// 对应 Python 里会被 `tree.error` 接住的 `KeyError` / `TypeError` / `AttributeError`。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandlerError {
    #[error("KeyError: '{0}'")]
    Key(String),
    #[error("TypeError: {0}")]
    Type(String),
}

fn obj(v: &Value) -> Result<&Map<String, Value>, HandlerError> {
    v.as_object()
        .ok_or_else(|| HandlerError::Type("response is not a JSON object".into()))
}

/// `d["k"]`
fn key<'a>(d: &'a Map<String, Value>, k: &str) -> Result<&'a Value, HandlerError> {
    d.get(k).ok_or_else(|| HandlerError::Key(k.to_owned()))
}

fn truthy_key(d: &Map<String, Value>, k: &str) -> Result<bool, HandlerError> {
    Ok(pyval::truthy(key(d, k)?))
}

/// `d.get(k)` 的真值（缺键 = None = 假）。
fn get_truthy(d: &Map<String, Value>, k: &str) -> bool {
    d.get(k).is_some_and(pyval::truthy)
}

// ---------- backend() 结果 ----------

/// `backend()` 的返回值 `(status, data)`：200 时 `data` 是网关的 JSON；
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

    /// `str(data)`：非 200 时发给成员的就是它。
    pub fn text(&self) -> String {
        py_str(&self.data)
    }
}

/// 把网关的 HTTP 响应翻译成 `Backend`。
/// 先解析 JSON（失败 = 服务暂时不可用，哪怕状态码是 502），再看状态码；
/// 非 200 时 Python 对解析结果调 `.get("detail", ...)`，非对象会 AttributeError → `Err`。
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

fn status_line(status: &Value) -> Result<String, HandlerError> {
    if matches!(status, Value::Array(_) | Value::Object(_)) {
        // dict.get(unhashable) → TypeError
        return Err(HandlerError::Type("unhashable type".into()));
    }
    let text = status
        .as_str()
        .and_then(|s| {
            STATUS_TEXT
                .iter()
                .find(|(k, _)| *k == s)
                .map(|(_, v)| (*v).to_owned())
        })
        .unwrap_or_else(|| py_str(status));
    Ok(format!("服务状态：{text}"))
}

/// `/quota` 的回复。`info` 是 `/self-register/info` 返回 200 时的 JSON（没拿到就是 `None`，不显示服务状态）。
/// `now` 是 `time.time()`。
pub fn quota_text(data: &Value, info: Option<&Value>, now: f64) -> Result<String, HandlerError> {
    let d = obj(data)?;
    let images = key(d, "images")?; // Python 在求 get 的默认值时就会取 data['images']
    let legacy = d.get("legacy").unwrap_or(images);
    let mut first = format!(
        "今日 V4.5：{} / {}",
        py_str(legacy),
        py_str(key(d, "daily_images")?)
    );
    if let Some(hour) = d.get("reset_hour").filter(|v| !v.is_null()) {
        first.push_str(&format!("（每天 {} 点重置）", py_str(hour)));
    }
    let mut lines = vec![first];
    if truthy_key(d, "daily_v5")? {
        lines.push(format!(
            "今日 V5：{} / {}",
            py_str(key(d, "v5")?),
            py_str(key(d, "daily_v5")?)
        ));
    }
    if get_truthy(d, "daily_anlas") {
        let anlas = d.get("anlas").cloned().unwrap_or_else(|| Value::from(0));
        lines.push(format!(
            "今日 Anlas：{} / {}",
            py_str(&anlas),
            py_str(key(d, "daily_anlas")?)
        ));
    }
    if get_truthy(d, "quiet_now") {
        let window = d
            .get("quiet_window")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        lines.push(format!(
            "🌙 安静时段（{}）暂停出图，到点自动恢复",
            py_str(&window)
        ));
    }
    let scope = if key(d, "image_model_scope")? == &Value::String("all".into()) {
        "含 V5"
    } else {
        "仅 V4.5 及以下"
    };
    lines.push(format!("可用模型：{scope}"));

    let features = key(d, "features")?
        .as_array()
        .ok_or_else(|| HandlerError::Type("features is not a list".into()))?;
    let mut opened: Vec<&str> = Vec::new();
    let mut closed: Vec<&str> = Vec::new();
    for f in features {
        let f = obj(f)?;
        // "、".join(...) 只接受 str，label 不是字符串会 TypeError；生成器按 on 的真值分两组各取一次 label
        let on = truthy_key(f, "on")?;
        let label = key(f, "label");
        if on {
            opened.push(
                label?
                    .as_str()
                    .ok_or_else(|| HandlerError::Type("label is not a string".into()))?,
            );
        } else {
            closed.push(
                label?
                    .as_str()
                    .ok_or_else(|| HandlerError::Type("label is not a string".into()))?,
            );
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
    if pyval::truthy(expires) {
        let at = pyval::number_like(expires)
            .ok_or_else(|| HandlerError::Type("expires_at is not a number".into()))?;
        let days = pyc::int_of(pyc::floordiv_f(at - now, 86400.0))
            .ok_or_else(|| HandlerError::Type("expires_at is not finite".into()))?;
        lines.push(format!("Key 剩余有效期：约 {} 天", days.max(0)));
    }
    if !truthy_key(d, "enabled")? {
        lines.push(QUOTA_DISABLED.to_owned());
    }

    if let Some(info) = info {
        let up = key(obj(info)?, "upstream")?;
        let up = obj(up)?;
        let mut state = status_line(key(up, "status")?)?;
        if truthy_key(up, "recent")? {
            state.push_str(&format!(
                "（近 10 分钟 {} 次请求，失败 {} 次）",
                py_str(key(up, "recent")?),
                py_str(key(up, "failed")?)
            ));
        }
        lines.push(state);
        if truthy_key(up, "image_cooldown_seconds")? {
            lines.push(format!(
                "⏳ 上游限流冷却中，约 {} 秒后恢复生图",
                py_str(key(up, "image_cooldown_seconds")?)
            ));
        }
    }
    Ok(lines.join("\n"))
}

// ---------- /resetkey /help /slots /revoke / 管理命令 ----------

pub fn resetkey_text(data: &Value) -> Result<String, HandlerError> {
    let k = key(obj(data)?, "key")?;
    Ok(format!(
        "新的 Key（只有你能看到这条消息，请妥善保存）：\n`{}`\n旧 Key 已失效。",
        py_str(k)
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
        if pyval::truthy(notice) {
            // "📢 " + data["notice"]：不是字符串会 TypeError
            let notice = notice
                .as_str()
                .ok_or_else(|| HandlerError::Type("notice is not a string".into()))?;
            lines.push(format!("📢 {notice}"));
        }
    }
    Ok(lines.join("\n"))
}

/// 管理命令（/open /limit /ban /unban）统一回复：200 取 `data["message"]`，否则 `str(data)`。
pub fn ops_text(b: &Backend) -> Result<String, HandlerError> {
    if b.is_ok() {
        Ok(py_str(key(obj(&b.data)?, "message")?))
    } else {
        Ok(b.text())
    }
}

pub fn slots_text(data: &Value) -> Result<String, HandlerError> {
    let d = obj(data)?;
    let max = key(d, "max")?;
    let cap = if pyval::truthy(max) {
        py_str(max)
    } else {
        "不限".to_owned()
    };
    let reset_at = key(d, "reset_at")?;
    let reset = if pyval::truthy(reset_at) {
        format!("，每天 {} 自动清空", py_str(reset_at))
    } else {
        String::new()
    };
    let open = d.get("open").is_none_or(pyval::truthy);
    let state = if open { "开放中" } else { "已关闭" };
    Ok(format!(
        "已领取 {} / {cap}{reset}；领 Key {state}",
        py_str(key(d, "active")?)
    ))
}

/// `/revoke`：200 → 「已撤销 <@id> 的 Key。」（`member.mention`），否则 `str(data)`。
pub fn revoke_text(member_id: &str, b: &Backend) -> String {
    if b.is_ok() {
        let mention = format!("<@{member_id}>");
        format!("已撤销 {mention} 的 Key。")
    } else {
        b.text()
    }
}

/// 有人 @奶妹 时的回复，`<@主人ID>` 会真的 @ 到服务器主人。
pub fn mention_reply_text(owner: u64) -> String {
    format!("我在床上陪主人呢 <@{owner}>")
}

// ---------- /register ----------

/// `/register` 发给 `/self-register/issue` 的 JSON（键顺序与 Python 一致）。
/// `invite_key` 是已经 `.strip()` 过的 /register 选项。
pub fn register_payload(it: &Interaction, invite_key: &str) -> Result<Value, HandlerError> {
    let user = it
        .actor()
        .ok_or_else(|| HandlerError::Type("interaction has no user".into()))?;
    let name = pyc::take_chars(&user.username, 80);
    // getattr(user, "global_name", "") or getattr(user, "display_name", "") or ""
    // Member.display_name = nick or global_name or name
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
        "global_name": pyc::take_chars(global_name, 80),
        "avatar": pyc::take_chars(avatar, 120),
        "invite_key": pyc::take_chars(invite_key, 80),
    }))
}

/// `/register` 按网关 HTTP 响应决定回复。网络层失败（连不上 / 超时）由调用方直接用 [`REGISTER_UNAVAILABLE`]。
pub fn register_message(status: u16, body: &[u8]) -> Result<String, HandlerError> {
    match status {
        200 | 403 => {
            // 状态码是 200 / 403 时 Python 才会解析 JSON；解析失败是 ValueError，被捕获后回「暂不可用」
            let Ok(data) = serde_json::from_slice::<Value>(body) else {
                return Ok(REGISTER_UNAVAILABLE.to_owned());
            };
            let d = obj(&data)?; // 非对象：AttributeError，不在 except 里 → 走 tree.error
            if status == 200 {
                Ok(match d.get("message").filter(|v| pyval::truthy(v)) {
                    Some(m) => py_str(m),
                    None => REGISTER_DONE.to_owned(),
                })
            } else {
                Ok(d.get("detail")
                    .map_or_else(|| REGISTER_NOT_ELIGIBLE.to_owned(), py_str))
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

/// 提交后的回执：`result.get("message", "收到啦～")`（200 且是对象）否则 `str(result)`。
pub fn feedback_receipt(b: &Backend) -> String {
    match (&b.data, b.is_ok()) {
        (Value::Object(o), true) => o
            .get("message")
            .map_or_else(|| FEEDBACK_DEFAULT_RECEIPT.to_owned(), py_str),
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
            "reset_hour": 7, "quiet_now": false, "quiet_window": "",
            "features": [{"id": "image", "label": "文生图", "on": true}, {"id": "upscale", "label": "放大", "on": false}],
        })
    }

    fn info(status: &str, recent: i64, failed: i64, cooldown: i64) -> Value {
        json!({"open": true, "notice": "", "upstream": {
            "status": status, "recent": recent, "failed": failed, "image_cooldown_seconds": cooldown}})
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
        d["image_model_scope"] = json!("legacy");
        d["expires_at"] = json!(NOW + 86400.0 * 3.5);
        d["enabled"] = json!(false);
        let text = quota_text(&d, Some(&info("degraded", 12, 7, 33)), NOW).unwrap();
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
             服务状态：🟠 不稳定（近期失败较多）（近 10 分钟 12 次请求，失败 7 次）\n\
             ⏳ 上游限流冷却中，约 33 秒后恢复生图"
        );
    }

    #[test]
    fn quota_no_open_features_and_expired_key() {
        let mut d = quota_data();
        d["features"] = json!([{"id": "image", "label": "文生图", "on": false}]);
        d["expires_at"] = json!(NOW - 86400.0 * 10.0);
        let text = quota_text(&d, Some(&info("idle", 0, 0, 0)), NOW).unwrap();
        assert!(text.contains("已开通功能：无\n未开通：文生图\n"), "{text}");
        assert!(text.contains("Key 剩余有效期：约 0 天\n"), "{text}");
        assert!(text.ends_with("服务状态：🟢 正常（近期无请求）"), "{text}");
    }

    #[test]
    fn quota_unknown_upstream_status_is_printed_raw() {
        let text = quota_text(&quota_data(), Some(&info("weird", 0, 0, 0)), NOW).unwrap();
        assert!(text.ends_with("服务状态：weird"), "{text}");
        let ok = quota_text(&quota_data(), Some(&info("ok", 3, 0, 0)), NOW).unwrap();
        assert!(
            ok.ends_with("服务状态：🟢 正常（近 10 分钟 3 次请求，失败 0 次）"),
            "{ok}"
        );
    }

    #[test]
    fn quota_missing_keys_are_errors_like_keyerror() {
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("daily_images");
        assert_eq!(
            quota_text(&d, None, NOW),
            Err(HandlerError::Key("daily_images".into()))
        );
        // legacy 存在也要求 images 存在（Python 先求默认值）
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("images");
        assert_eq!(
            quota_text(&d, None, NOW),
            Err(HandlerError::Key("images".into()))
        );
        // daily_v5 为假时不会去取 v5
        let mut d = quota_data();
        d.as_object_mut().unwrap().remove("v5");
        assert!(quota_text(&d, None, NOW).is_ok());
        assert!(quota_text(&json!([1]), None, NOW).is_err());
    }

    #[test]
    fn resetkey_and_slots() {
        assert_eq!(
            resetkey_text(&json!({"key": "nai-abc"})).unwrap(),
            "新的 Key（只有你能看到这条消息，请妥善保存）：\n`nai-abc`\n旧 Key 已失效。"
        );
        assert!(resetkey_text(&json!({})).is_err());
        assert_eq!(
            slots_text(&json!({"active": 12, "max": 50, "open": true, "reset_at": "07:00"}))
                .unwrap(),
            "已领取 12 / 50，每天 07:00 自动清空；领 Key 开放中"
        );
        assert_eq!(
            slots_text(&json!({"active": 3, "max": 0, "open": false, "reset_at": ""})).unwrap(),
            "已领取 3 / 不限；领 Key 已关闭"
        );
        // open 缺省按开放；reset_at 为 null 不显示
        assert_eq!(
            slots_text(&json!({"active": 3, "max": 9, "reset_at": null})).unwrap(),
            "已领取 3 / 9；领 Key 开放中"
        );
        assert!(slots_text(&json!({"active": 3, "max": 9})).is_err());
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
    fn ops_and_revoke() {
        let ok = Backend {
            status: 200,
            data: json!({"message": "已开放领 Key。"}),
        };
        assert_eq!(ops_text(&ok).unwrap(), "已开放领 Key。");
        assert!(
            ops_text(&Backend {
                status: 200,
                data: json!({})
            })
            .is_err()
        );
        let denied = Backend {
            status: 403,
            data: json!("没有管理员权限。"),
        };
        assert_eq!(ops_text(&denied).unwrap(), "没有管理员权限。");
        assert_eq!(
            revoke_text("900000000000000077", &ok),
            "已撤销 <@900000000000000077> 的 Key。"
        );
        let missing = Backend {
            status: 404,
            data: json!("该用户没有已领取的 Key。"),
        };
        assert_eq!(
            revoke_text("900000000000000077", &missing),
            "该用户没有已领取的 Key。"
        );
        assert_eq!(
            ops_text(&Backend::unavailable()).unwrap(),
            BACKEND_UNAVAILABLE
        );
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
        let denied =
            interpret_backend(403, "{\"detail\":\"没有管理员权限。\"}".as_bytes()).unwrap();
        assert_eq!(denied.text(), "没有管理员权限。");
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
        // 非 200 且 JSON 不是对象：.get 抛 AttributeError
        assert!(interpret_backend(422, b"[1]").is_err());
        // 422 校验错误的 detail 是列表 → str(list)
        let v = interpret_backend(422, br#"{"detail":[{"msg":"x"}]}"#).unwrap();
        assert_eq!(v.text(), "[{'msg': 'x'}]");
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
        assert_eq!(feedback_receipt(&list), "[1]");
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
        // 没有 global_name：display_name = nick or name
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
