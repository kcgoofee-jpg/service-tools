//! 对 `serde_json::Value` 复刻 Python 动态类型的判定（请求体是任意 JSON，旧代码直接操作 dict）。
//!
//! 坑：Python 里 `bool` 是 `int` 的子类——`isinstance(True, int)` 为真；
//! 旧代码多处写 `isinstance(x, (int, float)) and not isinstance(x, bool)`，移植时要逐处对照。

use serde_json::{Map, Value};

/// `bool(x)`：None / False / 0 / 0.0 / "" / [] / {} 为假。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `isinstance(v, int)`（含 bool）→ 整数值。
pub fn int_like(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        _ => None,
    }
}

/// `isinstance(v, int) and not isinstance(v, bool)`。
pub fn strict_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        _ => None,
    }
}

/// `isinstance(v, (int, float))`（含 bool）→ f64。
pub fn number_like(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `isinstance(v, (int, float)) and not isinstance(v, bool)`。
pub fn strict_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `isinstance(v, float)`。
pub fn is_float(v: &Value) -> bool {
    matches!(v, Value::Number(n) if n.is_f64())
}

/// `d.get(key)` 对任意 Value：不是 dict 时返回 None。
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|m| m.get(key))
}

/// `d.pop(key, None)` / `del d[key]`：**必须**保持其余键的顺序。
/// serde_json 开了 preserve_order 后 `Map::remove` 是 swap_remove，会打乱顺序——一律用这个。
pub fn pop(m: &mut Map<String, Value>, key: &str) -> Option<Value> {
    m.shift_remove(key)
}

/// `d.setdefault(key, default)`：已存在则返回现值，不改位置。
pub fn setdefault<'a>(m: &'a mut Map<String, Value>, key: &str, default: Value) -> &'a mut Value {
    m.entry(key.to_string()).or_insert(default)
}

/// `str(v)` 对 JSON 标量（用于日志 / 拼接文案）：int → "3"，float → Python repr，bool → "True"。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => crate::pyc::bool_str(*b).into(),
        Value::Number(n) if n.is_f64() => crate::pyc::float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python `float` → JSON 数（保持 float 类型，1.0 不会变成 1）。非有限值没有 JSON 表示，返回 Null。
pub fn float(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_truthiness() {
        assert!(!truthy(&json!(0.0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!([0])));
        assert_eq!(int_like(&json!(true)), Some(1));
        assert_eq!(strict_int(&json!(true)), None);
        assert_eq!(strict_int(&json!(2.0)), None);
    }

    #[test]
    fn pop_keeps_order() {
        let mut m = json!({"a": 1, "b": 2, "c": 3}).as_object().unwrap().clone();
        pop(&mut m, "a");
        assert_eq!(m.keys().collect::<Vec<_>>(), ["b", "c"]);
    }
}
