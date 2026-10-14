//! Python 的 `str(x)` / `repr(x)` 对 JSON 值的复刻。
//!
//! 旧机器人把网关返回的任意 JSON 直接塞进 f-string（`f"{data['images']}"`）或 `str(data)`；
//! 网关实际只返回整数 / 字符串，但「detail 是个列表」之类的 FastAPI 422 响应会走到 `str(list)`，
//! 此时 Python 输出的是 `repr`（单引号、`True`/`None`），这里照样输出。

use gate_core::{pyc, pyval};
use serde_json::Value;

/// `str(v)`：字符串原样，其余同 `repr`。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// `repr(v)`：dict / list 递归，字符串带引号。
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => str_repr(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, x)| format!("{}: {}", str_repr(k), py_repr(x)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        scalar => pyval::py_str(scalar),
    }
}

/// `repr(str)`：优先单引号；字符串里只有单引号时改用双引号；两种引号都有时用单引号并转义。
fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c => {
                let n = c as u32;
                if n < 0x100 {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n < 0x10000 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// 近似 `str.isprintable()`：控制字符与各类空白 / 格式字符不可打印（ASCII 空格除外）。
/// 只影响「detail 里夹着不可见字符」这种极端情况下的日志外观，不影响正常中文 / 表情。
fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() || (c.is_whitespace() && c != ' ') || pyc::is_space(c) {
        return false;
    }
    !matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' | '\u{180e}'
        | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e000}'..='\u{f8ff}')
}

/// `json.dumps` 把 dict 键转成字符串的规则（`answers[qid]` 的 qid 理论上可以是任意标量）。
/// 列表 / 字典作键在 Python 里是 `TypeError: unhashable type`，返回 None 让调用方当错误处理。
pub fn json_key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => Some("null".into()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.into()),
        Value::Number(_) => Some(pyval::py_str(v)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn str_of_scalars() {
        assert_eq!(py_str(&json!("abc")), "abc");
        assert_eq!(py_str(&json!(3)), "3");
        assert_eq!(py_str(&json!(3.0)), "3.0");
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
    }

    #[test]
    fn repr_of_containers_matches_python() {
        let v = json!([{"loc": ["body", "x"], "msg": "field required", "n": 1, "ok": false, "z": null}]);
        assert_eq!(
            py_str(&v),
            "[{'loc': ['body', 'x'], 'msg': 'field required', 'n': 1, 'ok': False, 'z': None}]"
        );
        assert_eq!(py_str(&json!({})), "{}");
        assert_eq!(py_str(&json!([])), "[]");
    }

    #[test]
    fn repr_quote_rules() {
        assert_eq!(py_repr(&json!("it's")), "\"it's\"");
        assert_eq!(py_repr(&json!("say \"hi\"")), "'say \"hi\"'");
        assert_eq!(py_repr(&json!("a'b\"c")), "'a\\'b\"c'");
        assert_eq!(py_repr(&json!("a\nb\\")), "'a\\nb\\\\'");
        assert_eq!(py_repr(&json!("中文")), "'中文'");
        assert_eq!(py_repr(&json!("\u{1}")), "'\\x01'");
    }

    #[test]
    fn dict_keys_like_json_dumps() {
        assert_eq!(json_key(&json!("a")).as_deref(), Some("a"));
        assert_eq!(json_key(&json!(7)).as_deref(), Some("7"));
        assert_eq!(json_key(&json!(true)).as_deref(), Some("true"));
        assert_eq!(json_key(&json!(null)).as_deref(), Some("null"));
        assert_eq!(json_key(&json!([1])), None);
    }
}
