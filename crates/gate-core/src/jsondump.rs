//! `json.dumps(obj, ensure_ascii=False)` 的逐字节复刻。
//!
//! 额度 / 自动驾驶把状态序列化成字符串写进 `site_settings`，下一轮再 `json.loads` 读回来；
//! 切换期间新旧两边要写出**完全相同**的字符串，所以不能用 serde_json 默认的紧凑格式：
//! - 分隔符是 `", "` 与 `": "`（Python 默认），serde_json 默认没有空格；
//! - 浮点是 `repr(float)`（`1e+16`、`1e-05`、整数值带 `.0`），serde_json 用 ryu（`1e16`）；
//! - 其余（转义规则：`"` `\` 与 < 0x20 的控制字符，非 ASCII 原样输出；对象键保持插入顺序）两边一致。
//!
//! 不支持 NaN / Infinity（`serde_json::Value` 本身放不下）；旧代码里写进库的值都是有限数。

use serde_json::Value;

/// `json.dumps(v, ensure_ascii=False)`。
/// 实现统一在 [`crate::pyjson`]（同一个格式化器，只是参数组合不同）。
pub fn dumps(v: &Value) -> String {
    let style = crate::pyjson::Style { compact: false, ensure_ascii: false };
    String::from_utf8(crate::pyjson::dumps_with(v, style)).expect("serde_json 只输出合法 UTF-8")
}

/// `json.loads(s)`：解析失败返回 None（Python 抛 `JSONDecodeError`）。
/// 与 Python 的差异：不接受 `NaN` / `Infinity` 字面量（旧代码从不写出它们）。
pub fn loads(s: &str) -> Option<Value> {
    serde_json::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_style_separators_and_floats() {
        let v =
            json!({"a": [1, 2.0, 1e16, 1e-5], "b": {"c": null, "d": true}, "e": "猫\n\"头\u{1}"});
        assert_eq!(
            dumps(&v),
            "{\"a\": [1, 2.0, 1e+16, 1e-05], \"b\": {\"c\": null, \"d\": true}, \"e\": \"猫\\n\\\"头\\u0001\"}"
        );
        assert_eq!(dumps(&json!([])), "[]");
        assert_eq!(dumps(&json!({})), "{}");
    }
}
