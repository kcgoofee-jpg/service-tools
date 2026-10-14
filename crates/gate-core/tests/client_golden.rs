mod common;

use common::bytes_of;
use gate_core::client_views::{SubscriptionData, SubscriptionError, subscription_payload};
use gate_core::request_timing::{
    classify_client, client_name, model_label, refine_client_from_payload, snapshot_ms,
};
use gate_golden::{as_f64, assert_eq_py, assert_eq_py_ordered, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn request_timing_golden() {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    let mut changed = 0;
    for c in load(M, "client/name") {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *counts.entry(name.clone()).or_default() += 1;
        match name.as_str() {
            "client_name" => {
                // 参数可能是 None（旧版 `user_agent or ""`）
                let ua = c.arg(0).as_str().unwrap_or("");
                assert_eq_py(&c, &json!(client_name(ua)), c.out());
            }
            "begin_client" => {
                let headers: Vec<(Vec<u8>, Vec<u8>)> = c.raw["headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| (bytes_of(&h[0]), bytes_of(&h[1])))
                    .collect();
                let refs: Vec<(&[u8], &[u8])> = headers
                    .iter()
                    .map(|(a, b)| (a.as_slice(), b.as_slice()))
                    .collect();
                assert_eq_py(&c, &json!(classify_client(&refs)), &c.raw["out"]);
            }
            "refine" => {
                let current = c.raw["current"].as_str().unwrap();
                let got = refine_client_from_payload(current, &c.raw["body"]);
                changed += usize::from(got.is_some());
                assert_eq_py(
                    &c,
                    &json!(got.unwrap_or_else(|| current.to_string())),
                    &c.raw["out"],
                );
            }
            "snapshot_ms" => {
                let sent = c.raw["sent"].as_f64();
                let (wait, dur) = snapshot_ms(
                    as_f64(&c.raw["t0"]).unwrap(),
                    sent,
                    as_f64(&c.raw["now"]).unwrap(),
                );
                assert_eq!(
                    wait,
                    c.raw["wait_ms"].as_i64().unwrap(),
                    "第 {} 行 wait_ms",
                    c.line
                );
                assert_eq!(
                    dur,
                    c.raw["dur_ms"].as_i64().unwrap(),
                    "第 {} 行 dur_ms",
                    c.line
                );
            }
            "model_label" => {
                let model = c.raw["model"].as_str().unwrap_or("");
                assert_eq_py(&c, &json!(model_label(model)), &c.raw["out"]);
            }
            other => panic!("未知函数 {other}"),
        }
    }
    assert!(changed > 300, "refine 里被改写的用例太少：{changed}");
    for (name, want) in [
        ("client_name", 300),
        ("begin_client", 400),
        ("refine", 1500),
        ("snapshot_ms", 300),
        ("model_label", 5),
    ] {
        assert!(
            counts.get(name).copied().unwrap_or(0) >= want,
            "{name} 用例太少：{counts:?}"
        );
    }
}

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

#[test]
fn constants_golden() {
    let c = &load(M, "analysis/consts")[0];
    assert_eq!(
        gate_core::request_timing::CLIENT_MAX as u64,
        c.raw["request_timing"]["CLIENT_MAX"].as_u64().unwrap()
    );
}
