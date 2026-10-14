//! golden 对拍测试的公共小工具（各测试文件 `mod common;` 引入）。
#![allow(dead_code)]

use base64::Engine;
use gate_core::pyobj::{DictKey, Obj};
use serde_json::{Value, json};

/// 夹具里的 bytes：`{"$b": "<base64>"}`。
pub fn bytes_of(v: &Value) -> Vec<u8> {
    let s = v
        .get("$b")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("不是 bytes 编码：{v}"));
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_else(|e| panic!("base64 解码失败：{e}"))
}

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// 夹具里的 `raw` 字段：bytes 或 str。
pub enum Raw {
    Bytes(Vec<u8>),
    Str(String),
}

pub fn raw_of(v: &Value) -> Raw {
    match v {
        Value::String(s) => Raw::Str(s.clone()),
        other => Raw::Bytes(bytes_of(other)),
    }
}

/// Python 对象 → 带类型标记的 JSON（与 streams.py 的 `jenc_loads` 一一对应）：
/// `["n"]` `["b", bool]` `["i", "十进制串"]` `["f", float]` `["s", str]` `["B", b64]`
/// `["l", [...]]` `["d", [[key, value], ...]]` `["x", code, b64]` `["t", seconds, nanoseconds]`。
/// 递归实现，只用于深度不大的结果。
pub fn tag(o: &Obj) -> Value {
    match o {
        Obj::None => json!(["n"]),
        Obj::Bool(b) => json!(["b", b]),
        Obj::Int(i) => json!(["i", i.to_string()]),
        Obj::BigInt { neg, digits } => {
            json!(["i", format!("{}{digits}", if *neg { "-" } else { "" })])
        }
        Obj::Float(x) => json!(["f", gate_golden::f(*x)]),
        Obj::Str(s) => json!(["s", s]),
        Obj::Bytes(b) => json!(["B", b64(b)]),
        Obj::List(items) => json!(["l", items.iter().map(tag).collect::<Vec<_>>()]),
        Obj::Dict(d) => {
            let pairs: Vec<Value> = d
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        DictKey::Str(s) => json!(["s", s]),
                        DictKey::Bytes(b) => json!(["B", b64(b)]),
                    };
                    json!([key, tag(v)])
                })
                .collect();
            json!(["d", pairs])
        }
        Obj::Ext { code, data } => json!(["x", code, b64(data)]),
        Obj::Timestamp {
            seconds,
            nanoseconds,
        } => json!(["t", seconds, nanoseconds]),
    }
}

/// 容器嵌套深度（迭代，深层树也安全）。
pub fn depth_of(o: &Obj) -> usize {
    let mut best = 0;
    let mut stack = vec![(o, 0usize)];
    while let Some((x, d)) = stack.pop() {
        match x {
            Obj::List(items) => {
                best = best.max(d + 1);
                stack.extend(items.iter().map(|c| (c, d + 1)));
            }
            Obj::Dict(m) => {
                best = best.max(d + 1);
                stack.extend(m.iter().map(|(_, c)| (c, d + 1)));
            }
            _ => {}
        }
    }
    best
}
