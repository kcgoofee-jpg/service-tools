//! 与 Python `json.dumps` 逐字节一致的序列化。
//!
//! 用在三处，都要求与旧版字节相同：
//! - 转发给上游的请求体（httpx：`ensure_ascii=False, separators=(",", ":")`）——请求体本身就是指纹的一部分；
//! - 返回给客户端的 JSON（Starlette `JSONResponse`：同上）；
//! - 写进 `site_settings` 的 JSON（`json.dumps(x)` 默认：`ensure_ascii=True`，分隔符 `", "` / `": "`）。
//!
//! 浮点用 Python 的 repr（`1e-05`、`1e+16`），不是 Rust 的 `1e-5`。

use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};

/// Python `json.dumps` 的参数组合。
#[derive(Debug, Clone, Copy)]
pub struct Style {
    /// `separators=(",", ":")`；否则用默认的 `(", ", ": ")`。
    pub compact: bool,
    /// `ensure_ascii=True`：非 ASCII 字符写成 `\uXXXX`（码点超过 BMP 时写成代理对）。
    pub ensure_ascii: bool,
}

/// httpx 发请求 / Starlette 回响应用的格式。
pub const WIRE: Style = Style {
    compact: true,
    ensure_ascii: false,
};
/// `json.dumps(x)` 不带参数时的格式。
pub const DEFAULT: Style = Style {
    compact: false,
    ensure_ascii: true,
};

struct PyFormatter {
    style: Style,
}

impl serde_json::ser::Formatter for PyFormatter {
    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        writer.write_all(crate::pyc::float_repr(value).as_bytes())
    }

    fn write_f32<W: ?Sized + Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        self.write_f64(writer, f64::from(value))
    }

    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else if self.style.compact {
            writer.write_all(b",")
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.begin_array_value(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if self.style.compact {
            writer.write_all(b":")
        } else {
            writer.write_all(b": ")
        }
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        if !self.style.ensure_ascii {
            return writer.write_all(fragment.as_bytes());
        }
        let mut buf = [0u16; 2];
        for c in fragment.chars() {
            // Python 的 ESCAPE_ASCII 只放过 0x20–0x7E：DEL（0x7F）也要转义
            if c.is_ascii() && c != '\x7f' {
                writer.write_all(&[c as u8])?;
            } else {
                for unit in c.encode_utf16(&mut buf) {
                    write!(writer, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

/// 序列化任意值（通常是 `serde_json::Value`）。值里不可能有 NaN / Infinity（`Value` 表示不了），
/// 与 `allow_nan=False` 的行为一致。
pub fn dumps_with<T: Serialize + ?Sized>(value: &T, style: Style) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PyFormatter { style });
    value.serialize(&mut ser).expect("序列化到内存不会失败");
    out
}

/// 线上格式（紧凑、保留 UTF-8）。
pub fn wire(value: &Value) -> Vec<u8> {
    dumps_with(value, WIRE)
}

/// `json.dumps(x)` 默认格式，返回字符串（存设置用）。
pub fn default_str(value: &Value) -> String {
    String::from_utf8(dumps_with(value, DEFAULT)).expect("ASCII 输出")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_formats() {
        let v = json!({"b": 1, "a": [1.0, "x", 1e-5, 1e16], "中": "猫\n\u{1F989}"});
        assert_eq!(
            String::from_utf8(wire(&v)).unwrap(),
            r#"{"b":1,"a":[1.0,"x",1e-05,1e+16],"中":"猫\n🦉"}"#
        );
        assert_eq!(
            default_str(&v),
            r#"{"b": 1, "a": [1.0, "x", 1e-05, 1e+16], "\u4e2d": "\u732b\n\ud83e\udd89"}"#
        );
        assert_eq!(
            String::from_utf8(wire(&json!("\u{1f}\u{7f}"))).unwrap(),
            "\"\\u001f\u{7f}\""
        );
    }
}
