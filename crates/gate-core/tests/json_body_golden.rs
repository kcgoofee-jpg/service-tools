mod common;

use common::{Raw, depth_of, raw_of, tag};
use gate_core::json_body::{self, BodyJsonError, Lossy, MAX_JSON_DEPTH};
use gate_core::pyloads::{self, JsonError, Options};
use gate_golden::{Case, assert_eq_py, assert_eq_py_ordered, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn lossy_json(l: &Lossy) -> Value {
    json!({
        "lone_surrogate": l.lone_surrogate,
        "big_int": l.big_int,
        "float_overflow": l.float_overflow,
    })
}

/// 错误必须同一类型；Python 记了消息的（`不支持 NaN` / `JSON 嵌套过深`）也要一致。
fn check_err(c: &Case, err: &JsonError) {
    let want = c.get("err").and_then(Value::as_str).unwrap();
    assert_eq!(
        err.py_type(),
        want,
        "golden 第 {} 行：Rust 报 {err:?}，Python 报 {want}\n{}",
        c.line,
        c.raw
    );
    if let Some(msg) = c.get("msg").and_then(Value::as_str) {
        assert_eq!(&err.to_string(), msg, "golden 第 {} 行消息不一致", c.line);
    }
}

#[test]
fn loads_golden() {
    let cases = load(M, "json_body/loads");
    assert!(cases.len() > 2000);
    let (mut ok, mut bad) = (0, 0);
    for c in cases {
        let raw = raw_of(&c.raw["raw"]);
        let got = match &raw {
            Raw::Bytes(b) => pyloads::loads_bytes(b, Options::LOADS),
            Raw::Str(s) => pyloads::loads_str(s, Options::LOADS),
        };
        if c.get("ok").and_then(Value::as_bool).unwrap() {
            ok += 1;
            let loaded = got.unwrap_or_else(|e| {
                panic!(
                    "golden 第 {} 行：Python 接受，Rust 报 {e:?}\n{}",
                    c.line, c.raw
                )
            });
            if let Some(depth) = c.get("depth").and_then(Value::as_u64) {
                assert_eq!(
                    depth_of(&loaded.value) as u64,
                    depth,
                    "第 {} 行深度",
                    c.line
                );
                assert_eq!(loaded.max_depth as u64, depth);
            } else {
                assert_eq_py(&c, &tag(&loaded.value), &c.raw["value"]);
                assert_eq!(
                    loaded.lone_surrogate,
                    c.get("lone").and_then(Value::as_bool).unwrap(),
                    "golden 第 {} 行：孤立代理标记不一致",
                    c.line
                );
            }
        } else {
            bad += 1;
            match got {
                Ok(l) => panic!(
                    "golden 第 {} 行：Python 拒绝（{}），Rust 接受 {:?}\n{}",
                    c.line,
                    c.get("err").unwrap(),
                    tag(&l.value),
                    c.raw
                ),
                Err(e) => check_err(&c, &e),
            }
        }
    }
    assert!(
        ok > 800 && bad > 800,
        "接受 / 拒绝用例都要足够多：{ok} / {bad}"
    );
}

#[test]
fn parse_json_golden() {
    let cases = load(M, "json_body/parse_json");
    assert!(cases.len() > 2000);
    let (mut ok, mut bad, mut lossy) = (0, 0, 0);
    for c in cases {
        let got = match raw_of(&c.raw["raw"]) {
            Raw::Bytes(b) => json_body::parse_json(&b),
            Raw::Str(s) => json_body::parse_json_str(&s),
        };
        if c.get("ok").and_then(Value::as_bool).unwrap() {
            ok += 1;
            let parsed = got.unwrap_or_else(|e| {
                panic!(
                    "golden 第 {} 行：Python 接受，Rust 报 {e:?}\n{}",
                    c.line, c.raw
                )
            });
            // 键顺序也要一致：它会原样转发给上游
            assert_eq_py_ordered(&c, &parsed.value, &c.raw["value"]);
            assert_eq_py(&c, &lossy_json(&parsed.lossy), &c.raw["lossy"]);
            lossy += usize::from(parsed.lossy.any());
        } else {
            bad += 1;
            match got {
                Ok(p) => panic!(
                    "golden 第 {} 行：Python 拒绝（{}），Rust 接受 {}\n{}",
                    c.line,
                    c.get("err").unwrap(),
                    p.value,
                    c.raw
                ),
                Err(e) => check_err(&c, &e),
            }
        }
    }
    assert!(
        ok > 800 && bad > 800,
        "接受 / 拒绝用例都要足够多：{ok} / {bad}"
    );
    assert!(
        lossy > 20,
        "不可约差异（孤立代理 / 大整数 / 溢出浮点）要有覆盖：{lossy}"
    );
}

#[test]
fn parse_json_body_golden() {
    let cases = load(M, "json_body/body");
    assert!(cases.len() > 2000);
    for c in cases {
        let Raw::Bytes(raw) = raw_of(&c.raw["raw"]) else {
            panic!("body 夹具只有 bytes")
        };
        let got = json_body::parse_json_body(&raw);
        if c.get("ok").and_then(Value::as_bool).unwrap() {
            let (map, lossy) = got.unwrap_or_else(|e| {
                panic!(
                    "golden 第 {} 行：Python 接受，Rust 报 {e:?}\n{}",
                    c.line, c.raw
                )
            });
            assert_eq_py_ordered(&c, &Value::Object(map), &c.raw["value"]);
            assert_eq_py(&c, &lossy_json(&lossy), &c.raw["lossy"]);
        } else {
            let e = got.expect_err("Python 抛 HTTPException，Rust 却成功");
            assert_eq!(c.get("status").and_then(Value::as_u64), Some(400));
            assert_eq!(
                Some(e.message()),
                c.get("detail").and_then(Value::as_str),
                "golden 第 {} 行文案不一致",
                c.line
            );
            assert!(matches!(
                e,
                BodyJsonError::Invalid | BodyJsonError::NotObject
            ));
        }
    }
}

fn build_deep(shape: &str, n: usize, tail: &str) -> Vec<u8> {
    let (head, leaf, close): (Vec<u8>, &[u8], Vec<u8>) = match shape {
        "arr" => (b"[".repeat(n), b"1", b"]".repeat(n)),
        "obj" => (b"{\"a\":".repeat(n), b"1", b"}".repeat(n)),
        "arr_empty_leaf" => (b"[".repeat(n - 1), b"[]", b"]".repeat(n - 1)),
        other => panic!("未知形状 {other}"),
    };
    let mut out = head;
    out.extend_from_slice(leaf);
    if tail != "open" {
        out.extend_from_slice(&close);
        if tail == "x" {
            out.extend_from_slice(b" x");
        }
    }
    out
}

#[test]
fn deep_nesting_golden() {
    let cases = load(M, "json_body/deep");
    let mut checked = 0;
    for c in cases {
        let fname = c.get("fn").and_then(Value::as_str).unwrap();
        if fname == "recursion_probe" {
            // 本机 Python 的 RecursionError 起点必须等于我们复刻的常数
            let first = c
                .get("first_recursion_depth")
                .and_then(Value::as_u64)
                .unwrap();
            assert_eq!(pyloads::RECURSION_DEPTH as u64, first);
            continue;
        }
        let shape = c.get("shape").and_then(Value::as_str).unwrap();
        let n = c.get("n").and_then(Value::as_u64).unwrap() as usize;
        let tail = c.get("tail").and_then(Value::as_str).unwrap();
        let raw = build_deep(shape, n, tail);
        let want_ok = c.get("ok").and_then(Value::as_bool).unwrap();
        match fname {
            "loads" => {
                let got = pyloads::loads_bytes(&raw, Options::LOADS);
                if want_ok {
                    let l = got.unwrap_or_else(|e| panic!("第 {} 行：Rust 报 {e:?}", c.line));
                    assert_eq!(
                        l.max_depth as u64,
                        c.get("depth").and_then(Value::as_u64).unwrap()
                    );
                    assert_eq!(depth_of(&l.value), l.max_depth);
                } else {
                    check_err(&c, &got.expect_err("Python 拒绝"));
                }
            }
            "parse_json" => {
                let got = json_body::parse_json(&raw);
                if want_ok {
                    let p = got.unwrap_or_else(|e| panic!("第 {} 行：Rust 报 {e:?}", c.line));
                    assert!(n <= MAX_JSON_DEPTH + 1);
                    let _ = p;
                } else {
                    check_err(&c, &got.expect_err("Python 拒绝"));
                }
            }
            other => panic!("未知函数 {other}"),
        }
        checked += 1;
    }
    assert!(checked > 150);
}
