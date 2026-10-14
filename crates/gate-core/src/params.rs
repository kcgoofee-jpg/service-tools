//! 私密参数 `P(name, default)` 的纯逻辑部分（对应 `app/params.py`）。
//!
//! 不在这里的（服务层负责）：
//! - `path()` / `_load()` 里的文件 IO 与按 mtime 缓存：读 `$DATA_DIR/private_params.json`
//!   （缺省 `data/private_params.json`），文件文本交给 [`Params::from_json_text`]；
//! - `update()` 里的原子写文件（临时文件 + `os.replace`，权限 0600，`json.dump(indent=2, sort_keys=True)`）：
//!   这里只给内存里的合并语义 [`Params::apply_update`]；Rust 写出的文件只要是合法 JSON，Python 就能读。
//! - `_DEFAULTS` 注册表（`P` 每被调用一次就 `setdefault` 登记起点值，后台对照用）：是全局状态，
//!   服务层自己维护，传给 [`Params::snapshot`]。
//!
//! 已知边界（与旧版不同，但只在手写出奇怪文件时才会碰到）：
//! - Python 的 `json.load` 接受裸 `NaN` / `Infinity` / `-Infinity`，serde_json 不接受，
//!   遇到时整个文件按「坏文件」处理（退回起点值）；
//! - 超出 i64 的整数 / 超大浮点：Python 的 `int(v)` 是任意精度，这里饱和到 i64 边界。

use crate::{pyc, pyval};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// 私密参数文件的内容（顶层 JSON 对象）。
#[derive(Debug, Clone, Default)]
pub struct Params {
    data: Map<String, Value>,
}

impl Params {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_map(data: Map<String, Value>) -> Self {
        Self { data }
    }

    /// `_load()` 读到文件文本之后的解析：不是合法 JSON、或顶层不是对象 → 空（文件坏了就退回起点值）。
    pub fn from_json_text(text: &str) -> Self {
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => Self { data: m },
            _ => Self::default(),
        }
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.data
    }

    /// `overridden(name)`
    pub fn overridden(&self, name: &str) -> bool {
        self.data.contains_key(name)
    }

    /// `P(name, default)`：私密文件里有同名键就用它，类型跟起点值一致；转不了就回到起点值。
    /// 起点值是 bool / int / float 之外的类型（字符串、列表……）时，文件里的值原样返回。
    pub fn get(&self, name: &str, default: &Value) -> Value {
        let v = self.data.get(name).unwrap_or(default);
        match default {
            // 注意顺序：Python 里 bool 是 int 的子类，先判 bool
            Value::Bool(_) => Value::Bool(pyval::truthy(v)),
            Value::Number(n) if !n.is_f64() => to_int(v)
                .map(Value::from)
                .unwrap_or_else(|| default.clone()),
            Value::Number(_) => to_float(v)
                .map(pyval::float)
                .unwrap_or_else(|| default.clone()),
            _ => v.clone(),
        }
    }

    /// `P(name, <int>)`
    pub fn int(&self, name: &str, default: i64) -> i64 {
        self.data
            .get(name)
            .map_or(Some(default), to_int)
            .unwrap_or(default)
    }

    /// `P(name, <float>)`
    pub fn float(&self, name: &str, default: f64) -> f64 {
        self.data
            .get(name)
            .map_or(Some(default), to_float)
            .unwrap_or(default)
    }

    /// `P(name, <bool>)`：`bool(v)`，所以文件里写 `"false"` 会得到 True（与旧版一致）。
    pub fn bool(&self, name: &str, default: bool) -> bool {
        self.data.get(name).map_or(default, pyval::truthy)
    }

    /// `snapshot()`：`defaults` 是 `_DEFAULTS` 注册表；键按字典序，`value` 缺省取起点值。
    pub fn snapshot(&self, defaults: &BTreeMap<String, Value>) -> Value {
        let mut keys: Vec<&String> = defaults.keys().chain(self.data.keys()).collect();
        keys.sort();
        keys.dedup();
        let mut out = Map::new();
        for k in keys {
            let default = defaults.get(k).cloned().unwrap_or(Value::Null);
            let value = self.data.get(k).cloned().unwrap_or_else(|| default.clone());
            let mut row = Map::new();
            row.insert("default".into(), default);
            row.insert("value".into(), value);
            row.insert("private".into(), Value::Bool(self.data.contains_key(k)));
            out.insert(k.clone(), Value::Object(row));
        }
        Value::Object(out)
    }

    /// `update(values)` 的内存部分：值为 None（JSON null）表示删除覆盖，否则写入。
    /// 保持其余键的顺序（文件写出时 Python 会 `sort_keys`，顺序本身不影响读取）。
    pub fn apply_update(&mut self, values: &Map<String, Value>) {
        for (k, v) in values {
            if v.is_null() {
                pyval::pop(&mut self.data, k);
            } else {
                self.data.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Python `int(v)`；`TypeError` / `ValueError` 对应 None（P 会回到起点值）。
fn to_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(u) = n.as_u64() {
                Some(i64::try_from(u).unwrap_or(i64::MAX))
            } else {
                // 浮点：向零截断；NaN 在 JSON 里不存在，无穷在 Python 里是 OverflowError（不会被 P 捕获）
                let f = n.as_f64()?;
                pyc::int_of(f)
            }
        }
        Value::String(s) => pyc::parse_int(s),
        _ => None,
    }
}

/// Python `float(v)`。
fn to_float(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => pyc::parse_float(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_like_default() {
        let p =
            Params::from_json_text(r#"{"a": "12", "b": 1.9, "c": "false", "d": null, "e": [1]}"#);
        assert_eq!(p.int("a", 5), 12);
        assert_eq!(p.int("b", 5), 1);
        assert!(p.bool("c", false)); // bool("false") 为真
        assert_eq!(p.int("d", 5), 5);
        assert_eq!(p.get("e", &json!([0])), json!([1]));
        assert_eq!(p.int("zz", 7), 7);
        assert_eq!(Params::from_json_text("[1]").as_map().len(), 0);
    }
}
