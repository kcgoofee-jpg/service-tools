mod common;

use gate_core::client_views::{SubscriptionData, SubscriptionError, subscription_payload};
use gate_golden::{as_f64, assert_eq_py_ordered, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn subscription_payload_golden() {
    let (mut ok, mut missing, mut admin) = (0, 0, 0);
    for c in load(M, "client/subscription") {
        let d = &c.raw["data"];
        let data = SubscriptionData {
            month_anlas_used: as_f64(&d["month_anlas_used"]).unwrap(),
            daily_v5_used: d["daily_v5_used"].as_i64().unwrap(),
            global_v5_limit: d["global_v5_limit"].as_i64().unwrap(),
            global_v5_total: d["global_v5_total"].as_i64().unwrap(),
        };
        let got = subscription_payload(&c.raw["key"], as_f64(&c.raw["now"]).unwrap(), &data);
        match c.raw["err"].as_str() {
            None => {
                ok += 1;
                let v = got.unwrap_or_else(|e| {
                    panic!(
                        "golden 第 {} 行：Python 成功，Rust 报 {e:?}\n{}",
                        c.line, c.raw
                    )
                });
                admin += usize::from(v["naiGate"]["isAdmin"] == json!(true));
                assert_eq_py_ordered(&c, &v, &c.raw["out"]);
            }
            Some("KeyError") => {
                missing += 1;
                assert!(
                    matches!(got, Err(SubscriptionError::MissingKey(_))),
                    "golden 第 {} 行：Python 报 KeyError，Rust 得到 {got:?}\n{}",
                    c.line,
                    c.raw
                );
            }
            Some(other) => panic!("golden 第 {} 行：意外的 Python 异常 {other}", c.line),
        }
    }
    assert!(
        ok > 600 && missing > 20 && admin > 50,
        "{ok} / {missing} / {admin}"
    );
    let _: Option<Value> = None;
}
