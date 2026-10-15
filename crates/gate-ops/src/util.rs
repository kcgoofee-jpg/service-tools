//! 各子命令共用的小工具：时间、路径、环境文件、巡检输出格式。

use jiff::{Timestamp, tz::TimeZone};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `time.time()`。
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 本机时区（Python 的 `time.localtime()` 看进程 TZ；探测失败按 UTC）。
pub fn local_tz() -> TimeZone {
    TimeZone::try_system().unwrap_or(TimeZone::UTC)
}

/// `time.strftime(fmt, time.localtime(ts))`；`tz` 由调用方传入，测试里固定成 Asia/Shanghai。
pub fn fmt_ts(ts: f64, tz: &TimeZone, fmt: &str) -> String {
    let secs = ts.floor() as i64;
    Timestamp::from_second(secs)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(tz.clone())
        .strftime(fmt)
        .to_string()
}

/// 近似 `du -h`：1024 进制，向上取整，小于 10 的保留一位小数（「3.2M」「120K」）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{:.1}{}", (v * 10.0).ceil() / 10.0, UNITS[i])
    } else {
        format!("{:.0}{}", v.ceil(), UNITS[i])
    }
}

/// 把文件路径放进 SQLite 的 `file:` URI：只需处理 `%` `?` `#`（其余字符 SQLite 原样接受）。
pub fn sqlite_uri(path: &Path, query: &str) -> String {
    let raw = path.to_string_lossy();
    let mut enc = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        match c {
            '%' => enc.push_str("%25"),
            '?' => enc.push_str("%3f"),
            '#' => enc.push_str("%23"),
            _ => enc.push(c),
        }
    }
    if query.is_empty() {
        format!("file:{enc}")
    } else {
        format!("file:{enc}?{query}")
    }
}

/// 数据库路径：`--db` > `NAI_GATE_DB` > `$DATA_DIR/nai_gate.db` > `data/nai_gate.db`。
pub fn resolve_db(flag: Option<&Path>) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    if let Some(p) = env_nonempty("NAI_GATE_DB") {
        return PathBuf::from(p);
    }
    data_dir().join("nai_gate.db")
}

/// `$DATA_DIR`（与应用一致，默认 `data`）。
pub fn data_dir() -> PathBuf {
    PathBuf::from(env_nonempty("DATA_DIR").unwrap_or_else(|| "data".into()))
}

pub fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `str(value)`：把 SQLite 取出的值按 Python 的写法转成文本。
pub fn sv_str(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value as Sv;
    match v {
        Sv::Null => "None".into(),
        Sv::Integer(i) => i.to_string(),
        Sv::Real(f) => gate_core::pyc::float_repr(*f),
        Sv::Text(s) => s.clone(),
        Sv::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// `json.dumps(v, ensure_ascii=False, indent=2)`：浮点用 Python 的 repr（`1e-05`、`2.0`）。
pub fn dumps_indent2(v: &Value) -> String {
    let mut out = Vec::with_capacity(512);
    let fmt = PyPretty(serde_json::ser::PrettyFormatter::with_indent(b"  "));
    let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
    v.serialize(&mut ser).expect("序列化到内存不会失败");
    String::from_utf8(out).expect("serde_json 只输出合法 UTF-8")
}

struct PyPretty<'a>(serde_json::ser::PrettyFormatter<'a>);

impl serde_json::ser::Formatter for PyPretty<'_> {
    fn write_f64<W: ?Sized + Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(gate_core::pyc::float_repr(value).as_bytes())
    }
    fn write_f32<W: ?Sized + Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        self.write_f64(w, f64::from(value))
    }
    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_array(w)
    }
    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_array(w)
    }
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.0.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_array_value(w)
    }
    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_object(w)
    }
    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_object(w)
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.0.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.0.end_object_value(w)
    }
}

/// 读取 `.env` 风格文件里的某个键（与旧版 patrol.py 的 `env_value` 一致，见标签 py-final：取第一处命中，
/// 去掉首尾空白与成对引号字符；文件读不到按空串）。
pub fn env_file_value(path: &Path, key: &str) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let prefix = format!("{key}=");
    for line in text.lines() {
        let line = gate_core::pyc::strip(line);
        if let Some(rest) = line.strip_prefix(&prefix) {
            let v = gate_core::pyc::strip(rest);
            // Python 的 str.strip('"') 会去掉两端所有连续的引号字符
            return v.trim_matches('"').trim_matches('\'').to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn indent2_matches_python_dumps() {
        let v = json!({"a": [1, 2.0, 1e-5], "b": {}, "c": [], "d": "猫"});
        assert_eq!(
            dumps_indent2(&v),
            "{\n  \"a\": [\n    1,\n    2.0,\n    1e-05\n  ],\n  \"b\": {},\n  \"c\": [],\n  \"d\": \"猫\"\n}"
        );
    }

    #[test]
    fn human_size_like_du() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(3 * 1024 * 1024 + 100), "3.1M");
        assert_eq!(human_size(120 * 1024), "120K");
    }

    #[test]
    fn env_file_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "  A=\"x y\"\nB='q'\nC=\nA=second\nD=a=b\n").unwrap();
        assert_eq!(env_file_value(&p, "A"), "x y");
        assert_eq!(env_file_value(&p, "B"), "q");
        assert_eq!(env_file_value(&p, "C"), "");
        assert_eq!(env_file_value(&p, "D"), "a=b");
        assert_eq!(env_file_value(&p, "NOPE"), "");
    }

    #[test]
    fn sqlite_uri_escapes_specials() {
        assert_eq!(
            sqlite_uri(Path::new("/a b/c?d#e%f.db"), "mode=ro"),
            "file:/a b/c%3fd%23e%25f.db?mode=ro"
        );
    }
}
