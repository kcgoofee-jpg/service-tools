//! Golden 夹具读取与 Python 语义比较。
//!
//! 夹具由 `tools/parity/golden/*.py` 在旧 Python 代码上生成，一行一个用例：
//! `{"fn": "...", "args": [...], "kwargs": {...}, "out": ..., "err": null | {"type": "...", "msg": "..."}}`
//! 另可带 `"after": {...}`（原地修改型函数：调用后的入参）与 `"note"`。
//!
//! 编码约定（与 `tools/parity/golden/_golden.py` 的 `enc()` 一一对应）：
//! - int → JSON 整数，float → 带小数点或指数的 JSON 数（Python repr），二者类型不混；
//! - 非有限浮点 → `{"$f": "inf" | "-inf" | "nan"}`；
//! - bytes → `{"$b": "<base64>"}`；tuple / set / frozenset → 列表（set 已排序）；
//! - dict 的非 str 键 → `{"$map": [[k, v], ...]}`。

use serde_json::Value;
use std::path::{Path, PathBuf};

/// 一个用例。
#[derive(Debug, Clone)]
pub struct Case {
    pub line: usize,
    pub raw: Value,
}

impl Case {
    pub fn args(&self) -> &[Value] {
        self.raw
            .get("args")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn arg(&self, i: usize) -> &Value {
        &self.args()[i]
    }
    pub fn kwargs(&self) -> &serde_json::Map<String, Value> {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        self.raw
            .get("kwargs")
            .and_then(Value::as_object)
            .unwrap_or_else(|| EMPTY.get_or_init(Default::default))
    }
    pub fn kwarg(&self, name: &str) -> Option<&Value> {
        self.kwargs().get(name)
    }
    pub fn out(&self) -> &Value {
        self.raw.get("out").unwrap_or(&Value::Null)
    }
    pub fn after(&self) -> Option<&Value> {
        self.raw.get("after")
    }
    /// Python 抛了异常时的异常类型名。
    pub fn err(&self) -> Option<&str> {
        self.raw
            .get("err")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }
}

/// 夹具根目录：`crates/gate-core/tests/golden`（相对调用方 crate 的 CARGO_MANIFEST_DIR）。
pub fn dir(manifest_dir: &str) -> PathBuf {
    Path::new(manifest_dir).join("tests").join("golden")
}

/// 读取 `<dir>/<name>.jsonl`（或压缩后的 `<name>.jsonl.gz`）的全部用例；文件不存在直接 panic（夹具必须入库）。
pub fn load(manifest_dir: &str, name: &str) -> Vec<Case> {
    let base = dir(manifest_dir).join(format!("{name}.jsonl"));
    let gz = dir(manifest_dir).join(format!("{name}.jsonl.gz"));
    let (path, text) = if gz.exists() {
        let file = std::fs::File::open(&gz).unwrap_or_else(|e| panic!("读不到夹具 {}: {e}", gz.display()));
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(file), &mut text)
            .unwrap_or_else(|e| panic!("解压夹具 {} 失败: {e}", gz.display()));
        (gz, text)
    } else {
        let text = std::fs::read_to_string(&base).unwrap_or_else(|e| panic!("读不到夹具 {}: {e}", base.display()));
        (base, text)
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| Case {
            line: i + 1,
            raw: serde_json::from_str(l).unwrap_or_else(|e| panic!("{}:{} 不是合法 JSON: {e}", path.display(), i + 1)),
        })
        .collect()
}

/// 解码特殊浮点：`{"$f": "inf"}` → f64。普通数字原样返回。
pub fn as_f64(v: &Value) -> Option<f64> {
    if let Some(f) = v.as_f64() {
        return Some(f);
    }
    match v.get("$f").and_then(Value::as_str)? {
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    }
}

/// 按 Python 语义比较：int 与 float 类型必须一致；浮点逐位相等（NaN 视为相等）；
/// 对象默认不比较键顺序（`ordered=true` 时比较，用于转发给上游的 JSON）。
/// 不一致时返回第一处差异的路径说明。
pub fn diff(actual: &Value, expected: &Value, ordered: bool) -> Option<String> {
    fn walk(a: &Value, e: &Value, path: &str, ordered: bool) -> Option<String> {
        // 特殊浮点
        if e.get("$f").is_some() || a.get("$f").is_some() {
            return match (as_f64(a), as_f64(e)) {
                (Some(x), Some(y)) if x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()) => {
                    None
                }
                _ => Some(format!("{path}: 实际 {a} ≠ 期望 {e}")),
            };
        }
        match (a, e) {
            (Value::Number(x), Value::Number(y)) => {
                let xf = x.is_f64();
                let yf = y.is_f64();
                if xf != yf {
                    return Some(format!(
                        "{path}: 类型不同（实际 {} {x}，期望 {} {y}）",
                        if xf { "float" } else { "int" },
                        if yf { "float" } else { "int" }
                    ));
                }
                if xf {
                    let (p, q) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                    if p.to_bits() != q.to_bits() {
                        return Some(format!("{path}: 实际 {p:?} ≠ 期望 {q:?}"));
                    }
                    None
                } else if x != y {
                    Some(format!("{path}: 实际 {x} ≠ 期望 {y}"))
                } else {
                    None
                }
            }
            (Value::Array(xs), Value::Array(ys)) => {
                if xs.len() != ys.len() {
                    return Some(format!("{path}: 长度 {} ≠ {}", xs.len(), ys.len()));
                }
                xs.iter()
                    .zip(ys)
                    .enumerate()
                    .find_map(|(i, (x, y))| walk(x, y, &format!("{path}[{i}]"), ordered))
            }
            (Value::Object(xm), Value::Object(ym)) => {
                if ordered {
                    let xk: Vec<_> = xm.keys().collect();
                    let yk: Vec<_> = ym.keys().collect();
                    if xk != yk {
                        return Some(format!("{path}: 键顺序 {xk:?} ≠ {yk:?}"));
                    }
                }
                for k in ym.keys() {
                    if !xm.contains_key(k) {
                        return Some(format!("{path}: 缺少键 {k:?}"));
                    }
                }
                for k in xm.keys() {
                    if !ym.contains_key(k) {
                        return Some(format!("{path}: 多出键 {k:?}"));
                    }
                }
                ym.iter()
                    .find_map(|(k, y)| walk(&xm[k], y, &format!("{path}.{k}"), ordered))
            }
            _ if a == e => None,
            _ => Some(format!("{path}: 实际 {a} ≠ 期望 {e}")),
        }
    }
    walk(actual, expected, "$", ordered)
}

/// 断言相等；失败时带上用例行号和输入。
#[track_caller]
pub fn assert_eq_py(case: &Case, actual: &Value, expected: &Value) {
    if let Some(d) = diff(actual, expected, false) {
        panic!("golden 第 {} 行不一致：{d}\n输入：{}", case.line, case.raw);
    }
}

/// 断言相等且对象键顺序一致。
#[track_caller]
pub fn assert_eq_py_ordered(case: &Case, actual: &Value, expected: &Value) {
    if let Some(d) = diff(actual, expected, true) {
        panic!("golden 第 {} 行不一致：{d}\n输入：{}", case.line, case.raw);
    }
}

/// 把 f64 编成与夹具同样的 JSON（非有限值用 `$f` 标记）。
pub fn f(x: f64) -> Value {
    if x.is_finite() {
        serde_json::Number::from_f64(x)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else if x.is_nan() {
        serde_json::json!({"$f": "nan"})
    } else if x > 0.0 {
        serde_json::json!({"$f": "inf"})
    } else {
        serde_json::json!({"$f": "-inf"})
    }
}
