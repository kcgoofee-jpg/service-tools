//! `upstream_policy` / `upstream_errors` 与旧 Python 对拍：夹具由旧 Python 生成（脚本见标签 py-final）。
//! 依赖时间的函数用夹具里记录的固定 `now`（Python 侧把 `upstream_policy.time` 打桩成同一个值）。

use gate_core::pyext::PyResult;
use gate_core::upstream_errors::upstream_error_message;
use gate_core::upstream_policy as up;
use gate_golden::{Case, as_f64, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");
const NOW: f64 = 1_791_900_000.0;

#[track_caller]
fn check(c: &Case, got: PyResult<Value>) {
    match (c.err(), got) {
        (Some(ty), Err(e)) => {
            assert_eq!(
                e.type_name(),
                ty,
                "第 {} 行异常类型不同：{e}\n输入：{}",
                c.line,
                c.raw
            );
            // OverflowError 的消息随场景变化，只比类型
            if ty != "OverflowError" {
                let want = c.raw["err"]["msg"].as_str().unwrap_or_default();
                assert_eq!(
                    e.message(),
                    want,
                    "第 {} 行异常消息不同\n输入：{}",
                    c.line,
                    c.raw
                );
            }
        }
        (Some(ty), Ok(v)) => panic!(
            "第 {} 行：Python 抛了 {ty}，Rust 却返回 {v}\n输入：{}",
            c.line, c.raw
        ),
        (None, Err(e)) => panic!(
            "第 {} 行：Python 正常返回，Rust 却报 {}: {e}\n输入：{}",
            c.line,
            e.type_name(),
            c.raw
        ),
        (None, Ok(v)) => assert_eq_py_ordered(c, &v, c.out()),
    }
}

#[test]
fn meta_matches_python() {
    for c in load(M, "upstream/meta") {
        let name = c.raw["name"].as_str().unwrap();
        let want = &c.raw["value"];
        let got = match name {
            "now" => f(NOW),
            "MODEL_403_MARKERS" => json!(up::MODEL_403_MARKERS),
            "RETRY_AFTER_MIN" => f(up::RETRY_AFTER_MIN),
            "RETRY_AFTER_MAX" => f(up::RETRY_AFTER_MAX),
            other => panic!("未覆盖的元数据 {other}"),
        };
        assert_eq_py_ordered(&c, &got, want);
    }
}

/// 只剩浮点入口 `clamp_retry_after_f64`：任意 JSON 值的入口 `clamp_retry_after` 没有生产调用方，已删除
/// （docs/v3-deviations.md #101），夹具里参数不是数字（字符串、None、bool、list、dict）的用例跳过。
#[test]
fn clamp_retry_after_golden() {
    let mut n = 0;
    for c in load(M, "upstream/clamp_retry_after") {
        let arg = c.arg(0);
        // 非有限浮点在夹具里是 {"$f": ...}
        if !(arg.is_number() || arg.get("$f").is_some()) {
            continue;
        }
        let x = as_f64(arg).unwrap();
        assert_eq_py_ordered(&c, &f(up::clamp_retry_after_f64(x)), c.out());
        n += 1;
    }
    assert!(n > 300, "数字用例太少：{n}");
}

#[test]
fn parse_retry_after_golden() {
    for c in load(M, "upstream/parse_retry_after") {
        let header = c.arg(0).as_str();
        check(&c, up::parse_retry_after(header, NOW).map(f));
    }
}

#[test]
fn is_model_level_403_golden() {
    for c in load(M, "upstream/is_model_level_403") {
        let body = c.arg(0).as_str().unwrap_or("");
        assert_eq_py_ordered(&c, &json!(up::is_model_level_403(body)), c.out());
    }
}

#[test]
fn upstream_error_message_golden() {
    for c in load(M, "upstream/upstream_error_message") {
        let status = c.arg(0).as_i64().unwrap();
        assert_eq_py_ordered(&c, &json!(upstream_error_message(status)), c.out());
    }
}
