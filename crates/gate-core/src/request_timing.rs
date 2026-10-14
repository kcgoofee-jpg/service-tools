//! 请求耗时与客户端名称的纯逻辑（`app/request_timing.py`）。
//!
//! 不移植：用 `ContextVar` 存的每请求状态（`begin` / `end` / `mark_*` / `rid` 等）和 `secrets.token_hex(4)`
//! 生成请求编号——Rust 侧由请求上下文自己持有。这里只放判定与格式化：
//! - [`client_name`]：User-Agent 摘要（去控制字符、截断）；
//! - [`classify_client`]：`begin()` 里按 UA / 请求头识别客户端的那条 if-elif 链；
//! - [`refine_client_from_payload`]：没有自报 UA 的客户端，按请求体里的字段指纹识别；
//! - [`snapshot_ms`]：`wait_ms` / `dur_ms` 的换算；[`model_label`]：被拒请求也记下的模型名。

use serde_json::{Map, Value};

use crate::pyc;

/// 完整 User-Agent 的最大长度：前 60 个字符里 12 把 Windows Key 一模一样，区分不了客户端。
pub const CLIENT_MAX: usize = 256;
/// `set_model` 保留的模型名长度。
pub const MODEL_MAX: usize = 80;

/// `client_name(user_agent)`：只保留 User-Agent 的前一段，连续的控制字符（`\x00-\x1f` `\x7f`）换成一个空格，
/// 去掉首尾空白，截断到 [`CLIENT_MAX`] 个字符。它由客户端自己填写，仅作参考。
pub fn client_name(user_agent: &str) -> String {
    let mut text = String::with_capacity(user_agent.len());
    let mut in_ctrl = false;
    for c in user_agent.chars() {
        if c <= '\u{1f}' || c == '\u{7f}' {
            if !in_ctrl {
                text.push(' ');
            }
            in_ctrl = true;
        } else {
            in_ctrl = false;
            text.push(c);
        }
    }
    pyc::take_chars(pyc::strip(&text), CLIENT_MAX).to_string()
}

/// `(model or "")[:80]`
pub fn model_label(model: &str) -> String {
    pyc::take_chars(model, MODEL_MAX).to_string()
}

/// 识别链：关键字（小写，匹配 UA 的小写形式）→ 前缀（已带该前缀就不再加）→ 包装名。
const CLASSIFY: [(&[&str], &str, &str); 5] = [
    (&["dsh"], "DSH-Tavern", "DSH-Tavern-Image"),
    (&["luker"], "Luker", "Luker"),
    (&["tauri"], "Tauri", "TauriTavern"),
    (&["workbench"], "NAI-Workbench", "NAI-Workbench"),
    (
        &["littlewhitebox", "xiaobaihe"],
        "LittleWhiteBox",
        "LittleWhiteBox",
    ),
];

/// `begin(scope)` 里算客户端名的部分。`headers` 是 ASGI 的 `(名字, 值)` 字节对（名字已小写）：
/// 多个 `user-agent` 时取最后一个；带 `x-correlation-id` / `x-initiated-at` 头的当作 Aaalice_NAI_Launcher。
/// 值按 latin-1 解码（每个字节对应一个字符，永远成功）。
pub fn classify_client(headers: &[(&[u8], &[u8])]) -> String {
    let mut ua = String::new();
    let mut is_aalice = false;
    for (name, value) in headers {
        if *name == b"user-agent" {
            ua = value.iter().map(|&b| char::from(b)).collect();
        } else if *name == b"x-correlation-id" || *name == b"x-initiated-at" {
            is_aalice = true;
        }
    }
    let lower = ua.to_lowercase();
    let mut cname = client_name(&ua);
    let wrap = |cname: &str, wrapper: &str| {
        pyc::take_chars(&format!("{wrapper} ({cname})"), CLIENT_MAX).to_string()
    };
    if is_aalice || lower.contains("dart") {
        if !cname.starts_with("Aaalice") {
            cname = wrap(&cname, "Aaalice_NAI_Launcher");
        }
    } else if let Some((_, prefix, wrapper)) = CLASSIFY
        .iter()
        .find(|(keys, _, _)| keys.iter().any(|k| lower.contains(k)))
        && !cname.starts_with(prefix)
    {
        cname = wrap(&cname, wrapper);
    }
    cname
}

/// Python `v == n`（n 是小整数）：int / float / bool 都按数值比较（`True == 1`，`4.0 == 4`）。
fn py_eq_int(v: Option<&Value>, n: i64) -> bool {
    match v {
        Some(Value::Bool(b)) => i64::from(*b) == n,
        Some(Value::Number(x)) => {
            if let Some(i) = x.as_i64() {
                i == n
            } else if let Some(u) = x.as_u64() {
                i128::from(u) == i128::from(n)
            } else {
                x.as_f64() == Some(n as f64)
            }
        }
        _ => false,
    }
}

fn py_eq_str(v: Option<&Value>, s: &str) -> bool {
    matches!(v, Some(Value::String(x)) if x == s)
}

fn as_dict(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// 已经明确识别出来的客户端前缀：不再按请求体改写。
const KNOWN_PREFIXES: [&str; 6] = [
    "Aaalice",
    "Luker",
    "TauriTavern",
    "NAI-Workbench",
    "DSH-Tavern",
    "LittleWhiteBox",
];

/// `refine_client_from_payload(body)`：对无明确自报 UA 的客户端，用请求体参数指纹识别
/// （小白盒 LittleWhiteBox、dsh-tavern-image）。返回新的客户端名；不需要改写返回 `None`。
/// `current` 是请求当前的客户端名；`body` 不是 JSON 对象时也返回 `None`。
pub fn refine_client_from_payload(current: &str, body: &Value) -> Option<String> {
    let body = body.as_object()?;
    if KNOWN_PREFIXES.iter().any(|p| current.starts_with(p)) {
        return None;
    }
    let empty = Map::new();
    let params = as_dict(body.get("parameters")).unwrap_or(&empty);
    let v4_prompt = as_dict(params.get("v4_prompt")).unwrap_or(&empty);
    let suffix = if current.is_empty() {
        String::new()
    } else {
        format!(" ({current})")
    };

    // 小白盒 LittleWhiteBox（RT15548/LittleWhiteBox，酒馆插件）：2026-10-13 按它的源码加的指纹。
    // 它「前端直连」时用的是宿主浏览器 UA（和柏宝绘之类完全一样），只能靠请求体认：
    // · ucPresetId / qualityPresetId 是它自有的字段名（官方 SDK 用 ucPreset / qualityToggle）；
    // · 兜底组合：顶层 use_new_shared_trial + params_version 4 + stream msgpack + image_format png；
    // · 「测试连接」探针：input=test + nai-diffusion-3 + 64x64 + 1 step + 1 张。
    let lwb_marker = ["ucPresetId", "qualityPresetId"]
        .iter()
        .any(|m| params.contains_key(*m));
    let lwb_combo = body.get("use_new_shared_trial") == Some(&Value::Bool(true))
        && py_eq_int(params.get("params_version"), 4)
        && py_eq_str(params.get("stream"), "msgpack")
        && py_eq_str(params.get("image_format"), "png");
    let model_ok = matches!(params.get("model"), None | Some(Value::Null))
        || py_eq_str(params.get("model"), "nai-diffusion-3");
    let lwb_probe = py_eq_str(body.get("input"), "test")
        && py_eq_str(body.get("action"), "generate")
        && model_ok
        && py_eq_int(params.get("width"), 64)
        && py_eq_int(params.get("height"), 64)
        && py_eq_int(params.get("steps"), 1)
        && py_eq_int(params.get("n_samples"), 1);
    if lwb_marker || lwb_combo || lwb_probe {
        return Some(pyc::take_chars(&format!("LittleWhiteBox{suffix}"), CLIENT_MAX).to_string());
    }

    // dsh-tavern-image (weixinlll/dsh-tavern-image) 专有特征指纹
    if py_eq_str(body.get("action"), "generate")
        && py_eq_int(params.get("params_version"), 3)
        && py_eq_int(params.get("ucPreset"), 3)
        && v4_prompt.get("use_order") == Some(&Value::Bool(true))
        && v4_prompt.get("use_coords") == Some(&Value::Bool(false))
        && params.get("add_original_image") == Some(&Value::Bool(true))
        && py_eq_int(params.get("controlnet_strength"), 1)
    {
        return Some(pyc::take_chars(&format!("DSH-Tavern-Image{suffix}"), CLIENT_MAX).to_string());
    }
    None
}

/// `snapshot()` 里的耗时换算（单调时钟，秒）：返回 `(wait_ms, dur_ms)`。
/// `sent` 为 `None`（没发到上游）：等待 = `now - t0`，上游耗时 0。
pub fn snapshot_ms(t0: f64, sent: Option<f64>, now: f64) -> (i64, i64) {
    let (wait, dur) = match sent {
        None => (now - t0, 0.0),
        Some(s) => (s - t0, now - s),
    };
    ((wait * 1000.0) as i64, (dur * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_strips_control_chars() {
        assert_eq!(
            client_name("  Mozilla/5.0\r\n\t(X11)\x00\x7f x "),
            "Mozilla/5.0 (X11)  x"
        );
        assert_eq!(client_name(""), "");
        assert_eq!(client_name("\x01\x02"), "");
    }

    #[test]
    fn classify_by_header_and_ua() {
        let h: Vec<(&[u8], &[u8])> = vec![(b"user-agent", b"Foo/1.0"), (b"x-initiated-at", b"1")];
        assert_eq!(classify_client(&h), "Aaalice_NAI_Launcher (Foo/1.0)");
        let h: Vec<(&[u8], &[u8])> = vec![(b"user-agent", b"Mozilla Luker/2")];
        assert_eq!(classify_client(&h), "Luker (Mozilla Luker/2)");
        let h: Vec<(&[u8], &[u8])> = vec![(b"user-agent", b"Luker/2")];
        assert_eq!(classify_client(&h), "Luker/2");
    }

    #[test]
    fn payload_fingerprints() {
        let b = json!({"parameters": {"ucPresetId": 1}});
        assert_eq!(
            refine_client_from_payload("Mozilla", &b).as_deref(),
            Some("LittleWhiteBox (Mozilla)")
        );
        assert_eq!(refine_client_from_payload("Luker/1", &b), None);
        // True == 1：旧版的相等判断会放过布尔值
        let probe = json!({"input": "test", "action": "generate",
            "parameters": {"width": 64.0, "height": 64, "steps": true, "n_samples": 1}});
        assert!(refine_client_from_payload("", &probe).is_some());
    }

    #[test]
    fn timing() {
        assert_eq!(snapshot_ms(1.0, None, 3.5), (2500, 0));
        assert_eq!(snapshot_ms(1.0, Some(2.0), 5.0), (1000, 3000));
    }
}
