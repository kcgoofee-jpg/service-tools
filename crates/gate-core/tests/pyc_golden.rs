use gate_core::pyc;
use gate_snapshot::{as_f64, assert_eq_py, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn round_golden() {
    for c in load(M, "pyc/round") {
        let x = as_f64(c.arg(0)).unwrap();
        let got = match c.args().get(1) {
            None => json!(pyc::round0(x)),
            Some(n) => f(pyc::round(x, n.as_u64().unwrap() as usize)),
        };
        assert_eq_py(&c, &got, c.out());
    }
}

#[test]
fn sum_golden() {
    for c in load(M, "pyc/sum") {
        let xs: Vec<f64> = c
            .arg(0)
            .as_array()
            .unwrap()
            .iter()
            .map(|v| as_f64(v).unwrap())
            .collect();
        // sum([]) 在 Python 里是整数 0
        let got = if xs.is_empty() {
            json!(0)
        } else {
            f(pyc::sum(xs))
        };
        assert_eq_py(&c, &got, c.out());
    }
}

#[test]
fn repr_golden() {
    for c in load(M, "pyc/repr") {
        let x = as_f64(c.arg(0)).unwrap();
        assert_eq_py(&c, &json!(pyc::float_repr(x)), c.out());
    }
}

#[test]
fn parse_golden() {
    for c in load(M, "pyc/parse") {
        let s = c.arg(0).as_str().unwrap();
        let got: Value = match c.raw["fn"].as_str().unwrap() {
            "int" => pyc::parse_int(s).map(|v| json!(v)).unwrap_or(Value::Null),
            "float" => pyc::parse_float(s).map(f).unwrap_or(Value::Null),
            other => panic!("未知函数 {other}"),
        };
        if c.err().is_some() {
            assert!(
                got.is_null(),
                "第 {} 行：Python 抛异常，Rust 却解析出 {got}（输入 {s:?}）",
                c.line
            );
        } else {
            assert_eq_py(&c, &got, c.out());
        }
    }
}
