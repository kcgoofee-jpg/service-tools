//! Discord 机器人（奶妹）的配置与运行状态（对应 `app/bot_config.py`）的纯逻辑部分。
//!
//! 不在这里的（服务层负责，都是 `site_settings` 表的键值读写）：
//! - `load(db)`：逐个 `get_setting("bot_<字段>", None)`，把读到的 `bot_*` 键值交给 [`load_from`]；
//! - `save(db, values)`：`set_settings_bulk`，写入的键值由 [`to_settings`] 给出（`bot_<字段>` → `str(值)`）；
//! - `report(db, status, event, now)`：状态心跳写 `bot_status`（[`status_to_store`]）；事件先读 `bot_events`
//!   （缺省 `"[]"`），[`append_event`] 算出新值再写回 `bot_events`；
//! - `snapshot(db, now)`：读 `bot_status`（缺省 `"{}"`）、`bot_events`（缺省 `"[]"`）和全部 `bot_*` 配置，交给 [`snapshot`]。
//!
//! 已知边界：`load` 的 `int(raw)` 在 Python 里是任意精度，这里超出 i64 的值按「读不懂」回到默认值
//! （配置值只可能是 0～500，不会碰到）。

use crate::{pyc, pyext, pyval};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// 字段类型；字符串的上下限是长度（按码点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Int,
    Str,
}

#[derive(Debug, Clone, Copy)]
pub enum FieldDefault {
    Int(i64),
    Str(&'static str),
}

impl FieldDefault {
    pub fn to_value(self) -> Value {
        match self {
            FieldDefault::Int(i) => Value::from(i),
            FieldDefault::Str(s) => Value::String(s.into()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub default: FieldDefault,
    pub kind: FieldKind,
    pub lo: i64,
    pub hi: i64,
}

/// 名称 → (默认值, 类型, 下限, 上限)；顺序与旧版 `FIELDS` 一致。
pub const FIELDS: [Field; 5] = [
    // 论坛频道名包含这几个字就算跑图分享
    Field {
        name: "gallery_forum",
        default: FieldDefault::Str("跑图分享"),
        kind: FieldKind::Str,
        lo: 1,
        hi: 50,
    },
    // 新帖自动点 ❤️（默认关：机器人自动互动是 Discord 标记的信号之一）
    Field {
        name: "gallery_like",
        default: FieldDefault::Int(0),
        kind: FieldKind::Int,
        lo: 0,
        hi: 1,
    },
    // 新帖 AI 看图写评论（默认关）
    Field {
        name: "gallery_ai",
        default: FieldDefault::Int(0),
        kind: FieldKind::Int,
        lo: 0,
        hi: 1,
    },
    // AI 评论每天最多几条
    Field {
        name: "gallery_ai_daily",
        default: FieldDefault::Int(30),
        kind: FieldKind::Int,
        lo: 0,
        hi: 500,
    },
    Field {
        name: "gallery_ai_model",
        default: FieldDefault::Str("LongCat-2.5-Preview"),
        kind: FieldKind::Str,
        lo: 1,
        hi: 80,
    },
];
pub const STATUS_KEY: &str = "bot_status";
pub const EVENTS_KEY: &str = "bot_events";
pub const EVENT_KINDS: [(&str, &str); 5] = [
    ("like", "点赞"),
    ("comment", "AI 评论"),
    ("shy", "捂眼睛（露骨）"),
    ("skip", "跳过"),
    ("error", "出错"),
];

/// 事件只留最近这么多条
pub const EVENTS_KEEP: usize = 50;
/// 心跳每分钟一次，3 分钟没收到算离线
pub const ONLINE_WINDOW: f64 = 180.0;

/// `_setting(name)`：存进 `site_settings` 的键名。
pub fn setting_key(name: &str) -> String {
    format!("bot_{name}")
}

/// 校验失败：旧版抛 `ValueError`，带中文原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BotConfigError {
    #[error("{0}")]
    Invalid(String),
    /// 库里的 JSON 坏了：旧版 `json.loads` 的 ValueError 没被捕获 → 500
    #[error("ValueError: {0}")]
    BadJson(String),
    /// 库里的 JSON 类型不对（要对象 / 数组）：旧版会 AttributeError
    #[error("AttributeError: {0}")]
    WrongType(String),
    /// 状态里的字段类型不对：旧版 TypeError
    #[error("TypeError: {0}")]
    Type(String),
}

/// `validate(body)`：后台提交的字段 → 合法值；不认识的字段忽略，不合法返回中文原因。
/// 结果的键按 `FIELDS` 的顺序排列（不是按请求体里的顺序）。
pub fn validate(body: &Map<String, Value>) -> Result<Map<String, Value>, BotConfigError> {
    let mut out = Map::new();
    for f in FIELDS {
        let Some(v) = body.get(f.name) else {
            continue;
        };
        let value = match f.kind {
            FieldKind::Int => {
                // bool 先转 int；float（哪怕是 1.0）不算整数
                let n = match v {
                    Value::Bool(b) => Some(i128::from(*b)),
                    Value::Number(n) if !n.is_f64() => n
                        .as_i64()
                        .map(i128::from)
                        .or_else(|| n.as_u64().map(i128::from)),
                    _ => None,
                };
                match n {
                    Some(n) if i128::from(f.lo) <= n && n <= i128::from(f.hi) => {
                        Value::from(n as i64)
                    }
                    _ => {
                        return Err(BotConfigError::Invalid(format!(
                            "{} 需要是 {}～{} 的整数",
                            f.name, f.lo, f.hi
                        )));
                    }
                }
            }
            FieldKind::Str => {
                let s = pyc::strip(&pyext::py_str(v)).to_string();
                let n = pyc::len_chars(&s) as i64;
                if !(f.lo <= n && n <= f.hi) {
                    return Err(BotConfigError::Invalid(format!(
                        "{} 长度需要 {}～{} 个字",
                        f.name, f.lo, f.hi
                    )));
                }
                Value::String(s)
            }
        };
        out.insert(f.name.to_string(), value);
    }
    Ok(out)
}

/// `load(db)` 的解析部分：`stored` 是 `site_settings` 里读到的 `bot_*` 键值（没有的键 = 没存）。
/// 读不懂的值回到默认值。
pub fn load_from(stored: &BTreeMap<String, String>) -> Map<String, Value> {
    let mut out = Map::new();
    for f in FIELDS {
        let v = match stored.get(&setting_key(f.name)) {
            None => f.default.to_value(),
            Some(raw) => match f.kind {
                FieldKind::Int => pyc::parse_int(raw)
                    .map(Value::from)
                    .unwrap_or_else(|| f.default.to_value()),
                FieldKind::Str => Value::String(raw.clone()),
            },
        };
        out.insert(f.name.to_string(), v);
    }
    out
}

/// `save(db, values)` 要写的键值（`values` 为空时旧版不写库，这里返回空列表）。
pub fn to_settings(values: &Map<String, Value>) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(k, v)| (setting_key(k), pyext::py_str(v)))
        .collect()
}

/// `report` 的心跳部分：只留白名单字段，整体覆盖 `bot_status`。`status` 必须是对象。
pub fn status_to_store(status: &Map<String, Value>, now: f64) -> String {
    let mut keep = Map::new();
    for k in [
        "user",
        "guild",
        "latency_ms",
        "ready_at",
        "ai_ready",
        "ai_today",
        "version",
    ] {
        if let Some(v) = status.get(k) {
            keep.insert(k.to_string(), v.clone());
        }
    }
    keep.insert("seen_at".into(), pyval::float(now));
    crate::jsondump::dumps(&Value::Object(keep))
}

fn loads(raw: Option<&str>, empty: &str) -> Result<Value, BotConfigError> {
    let text = match raw {
        Some(s) if !s.is_empty() => s,
        _ => empty,
    };
    serde_json::from_str(text).map_err(|e| BotConfigError::BadJson(e.to_string()))
}

/// `report` 的事件部分：`stored` 是 `bot_events` 现值（没有 = None）。
/// 事件类型不在 `EVENT_KINDS` 里就忽略（返回 `Ok(None)`，不写库）；否则返回新的 `bot_events` 文本（最近 50 条）。
pub fn append_event(
    stored: Option<&str>,
    event: &Map<String, Value>,
    now: f64,
) -> Result<Option<String>, BotConfigError> {
    let kind = match event.get("kind") {
        Some(Value::String(k)) if EVENT_KINDS.iter().any(|(e, _)| e == k) => k,
        // `x in dict` 对不可哈希的值（列表 / 对象）抛 TypeError
        Some(Value::Array(_) | Value::Object(_)) => {
            return Err(BotConfigError::Type("unhashable type".into()));
        }
        _ => return Ok(None),
    };
    let Value::Array(mut events) = loads(stored, "[]")? else {
        return Err(BotConfigError::WrongType(
            "object has no attribute 'append'".into(),
        ));
    };
    let mut item = Map::new();
    item.insert("at".into(), pyval::float(now));
    item.insert("kind".into(), Value::String(kind.clone()));
    for k in ["title", "author", "detail", "url"] {
        let s = match event.get(k) {
            Some(v) if pyval::truthy(v) => pyext::py_str(v),
            _ => String::new(),
        };
        let s = s.trim_start_matches(['#', ' ']);
        item.insert(k.into(), Value::String(pyc::take_chars(s, 120).to_string()));
    }
    events.push(Value::Object(item));
    let start = events.len().saturating_sub(EVENTS_KEEP);
    Ok(Some(crate::jsondump::dumps(&Value::Array(
        events[start..].to_vec(),
    ))))
}

/// `snapshot` 里的在线判断：`bool(seen and now - seen < 180)`。`seen` 是状态里的 `seen_at`（没有 = None）。
/// 为真但不是数字时旧版 `now - seen` 抛 TypeError。
pub fn online(seen_at: Option<&Value>, now: f64) -> Result<bool, BotConfigError> {
    let Some(seen) = seen_at else {
        return Ok(false);
    };
    if !pyval::truthy(seen) {
        return Ok(false);
    }
    match pyval::number_like(seen) {
        Some(s) => Ok(now - s < ONLINE_WINDOW),
        None => Err(BotConfigError::Type(
            "unsupported operand type(s) for -: 'float' and ...".into(),
        )),
    }
}

/// `list(reversed(x))`：数组倒序；对象倒序取键；字符串倒序取字符；其余类型 TypeError。
fn reversed(v: Value) -> Result<Vec<Value>, BotConfigError> {
    match v {
        Value::Array(mut a) => {
            a.reverse();
            Ok(a)
        }
        Value::Object(m) => Ok(m.keys().rev().cloned().map(Value::String).collect()),
        Value::String(s) => Ok(s
            .chars()
            .rev()
            .map(|c| Value::String(c.to_string()))
            .collect()),
        other => Err(BotConfigError::Type(format!(
            "'{}' object is not reversible",
            type_name(&other)
        ))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `snapshot(db, now)`：`config` 是 [`load_from`] 的结果；`status_raw` / `events_raw` 是库里的原文。
pub fn snapshot(
    config: Map<String, Value>,
    status_raw: Option<&str>,
    events_raw: Option<&str>,
    now: f64,
) -> Result<Value, BotConfigError> {
    let Value::Object(mut status) = loads(status_raw, "{}")? else {
        return Err(BotConfigError::WrongType(
            "object has no attribute 'get'".into(),
        ));
    };
    let is_online = online(status.get("seen_at"), now)?;
    status.insert("online".into(), Value::Bool(is_online));
    let events = reversed(loads(events_raw, "[]")?)?;
    let kinds: Map<String, Value> = EVENT_KINDS
        .iter()
        .map(|(k, v)| ((*k).to_string(), Value::String((*v).into())))
        .collect();
    let mut out = Map::new();
    out.insert("config".into(), Value::Object(config));
    out.insert("status".into(), Value::Object(status));
    out.insert("events".into(), Value::Array(events));
    out.insert("kinds".into(), Value::Object(kinds));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_like_python() {
        let body = json!({"gallery_like": false, "gallery_ai_daily": 10, "x": 1});
        let out = validate(body.as_object().unwrap()).unwrap();
        assert_eq!(
            Value::Object(out),
            json!({"gallery_like": 0, "gallery_ai_daily": 10})
        );
        for bad in [
            json!({"gallery_like": 2}),
            json!({"gallery_ai_daily": -1}),
            json!({"gallery_forum": "  "}),
            json!({"gallery_ai_daily": "5"}),
        ] {
            assert!(validate(bad.as_object().unwrap()).is_err());
        }
    }

    #[test]
    fn events_keep_last_50() {
        let ev = json!({"kind": "like", "title": "## 标题"});
        let mut stored = None::<String>;
        for i in 0..60 {
            let mut e = ev.as_object().unwrap().clone();
            e.insert("detail".into(), json!(format!("d{i}")));
            stored = append_event(stored.as_deref(), &e, 1000.0 + f64::from(i)).unwrap();
        }
        let v: Value = serde_json::from_str(&stored.unwrap()).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 50);
        assert_eq!(v[0]["title"], "标题");
    }
}
