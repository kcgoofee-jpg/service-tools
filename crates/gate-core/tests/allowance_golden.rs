//! allowance.rs 与旧 Python 对拍（夹具：tools/parity/golden/allowance.py）。
//!
//! 每条夹具是一条操作轨迹：真实 `AllowanceCache` 在假时钟 / 假 HTTP 客户端下执行的 resolve / note_period /
//! period / snapshot / 改阈值。这里用 `AllowanceCore` 的 check_cache → finish_resolve → note_period 重放，
//! 逐步比较返回值、缓存行、写库内容、告警和「是否发了 HTTP 请求」。

use base64::{Engine, engine::general_purpose::STANDARD};
use gate_core::allowance::{
    self, AllowanceCore, Begin, Fetched, MAX_BODY, PERIOD_KEY, SETTING, UNAVAILABLE_MSG,
};
use gate_golden::{Case, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn threshold_golden() {
    for c in load(M, "allowance/threshold") {
        let present = c.raw["present"].as_bool().unwrap();
        let v = if present {
            c.arg(0).clone()
        } else {
            Value::Null
        };
        assert_eq_py(
            &c,
            &json!(allowance::normalize_alert_threshold(&v)),
            c.out(),
        );
    }
}

/// 假 HTTP 的结果（拥有数据，好借给 `Fetched`）。
enum Plan {
    Failed,
    Response {
        status: u16,
        retry_after: String,
        body: Option<Vec<u8>>,
    },
}

fn plan_of(p: &Value) -> Plan {
    match p["kind"].as_str().unwrap() {
        "error" | "timeout" => Plan::Failed,
        kind => {
            let bytes: Vec<u8> = if let Some(n) = p.get("body_size").and_then(Value::as_u64) {
                vec![b'x'; n as usize]
            } else if let Some(b) = p.get("body_b64").and_then(Value::as_str) {
                STANDARD.decode(b).unwrap()
            } else {
                p["body"].as_str().unwrap().as_bytes().to_vec()
            };
            Plan::Response {
                status: p["status"].as_u64().unwrap() as u16,
                retry_after: p["retry_after"].as_str().unwrap_or("").to_string(),
                // 响应体最多读 MAX_BODY 字节，超过或读取中途出错都是 None
                body: (kind != "read_error" && bytes.len() <= MAX_BODY).then_some(bytes),
            }
        }
    }
}

fn row_json(core: &AllowanceCore, token: &str) -> Value {
    let mut m = Map::new();
    if let Some(r) = core.rows.get(token) {
        if let Some(v) = r.percent {
            m.insert("percent".into(), json!(v));
        }
        if let Some(v) = r.is_negative {
            m.insert("is_negative".into(), json!(v));
        }
        if let Some(v) = r.checked_at {
            m.insert("checked_at".into(), f(v));
        }
        if let Some(v) = r.at {
            m.insert("at".into(), f(v));
        }
        if let Some(v) = &r.error {
            m.insert("error".into(), json!(v));
        }
        if r.retry_at != 0.0 {
            m.insert("retry_at".into(), f(r.retry_at));
        }
        if let Some(v) = r.next_percent_seconds {
            m.insert("next_percent_seconds".into(), json!(v));
        }
    }
    Value::Object(m)
}

fn write_json(key: &str, text: &str) -> Value {
    json!({
        "key": key,
        "len": text.chars().count(),
        "sha256": hex::encode(Sha256::digest(text.as_bytes())),
        "text": if text.chars().count() < 700 { json!(text) } else { Value::Null },
    })
}

fn replay(c: &Case) -> usize {
    let mut core = AllowanceCore::new();
    let mut kv: BTreeMap<String, String> = c.raw["init"]["kv"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    let threshold_of = |raw: &Value| {
        allowance::normalize_alert_threshold(&match raw {
            Value::String(_) => raw.clone(),
            _ => Value::Null,
        })
    };
    let mut n_fetch = 0;
    for (i, step) in c.raw["steps"].as_array().unwrap().iter().enumerate() {
        let ctx = format!("轨迹 {} 第 {i} 步 {}", c.raw["name"], step["op"]);
        let op = step["op"].as_str().unwrap();
        match op {
            "set_threshold" => match step["raw"].as_str() {
                Some(s) => {
                    kv.insert(SETTING.into(), s.into());
                }
                None => {
                    kv.remove(SETTING);
                }
            },
            "resolve" => {
                let token = step["token"].as_str().unwrap();
                let (mono, wall) = (
                    step["mono"].as_f64().unwrap(),
                    step["wall"].as_f64().unwrap(),
                );
                // 夹具记录的阈值原文就是库里当时的值
                let threshold = threshold_of(&step["threshold_raw"]);
                let (mono_after, wall_after) = (
                    step["mono_after"].as_f64().unwrap(),
                    step["wall_after"].as_f64().unwrap(),
                );
                let _ = wall;
                let mut calls = 0;
                let mut alerts = Vec::new();
                let mut writes = Vec::new();
                let out = match core.check_cache(token, mono, threshold) {
                    Begin::Unavailable => {
                        json!({"err": "AllowanceUnavailable", "msg": UNAVAILABLE_MSG})
                    }
                    Begin::Cached => json!({"ok": false}),
                    Begin::Fetch => {
                        calls = 1;
                        n_fetch += 1;
                        let plan = plan_of(&step["plan"]);
                        let fetched = match &plan {
                            Plan::Failed => Fetched::Failed,
                            Plan::Response {
                                status,
                                retry_after,
                                body,
                            } => Fetched::Response {
                                status: *status,
                                retry_after,
                                body: body.as_deref(),
                            },
                        };
                        let r =
                            core.finish_resolve(token, &fetched, wall_after, mono_after, threshold);
                        if let Some(a) = r.low_alert {
                            alerts.push(json!(["v5_low", a.message, a.cooldown_secs]));
                        }
                        if let Some(secs) = r.note_period {
                            core.load_periods(kv.get(PERIOD_KEY).map(String::as_str));
                            let text = core.note_period(token, secs, wall_after).unwrap();
                            writes.push(write_json(PERIOD_KEY, &text));
                            kv.insert(PERIOD_KEY.into(), text);
                        }
                        match r.result {
                            Ok(neg) => json!({"ok": neg}),
                            Err(()) => {
                                json!({"err": "AllowanceUnavailable", "msg": UNAVAILABLE_MSG})
                            }
                        }
                    }
                };
                let want = &step["out"];
                // Python 侧的异常消息只在 err 里；ok 的比较见下
                assert_eq_py(c, &out, want);
                assert_eq!(
                    calls,
                    step["http_calls"].as_i64().unwrap(),
                    "{ctx}：HTTP 调用次数"
                );
                assert_eq_py(c, &Value::Array(alerts), &step["alerts"]);
                assert_eq_py(c, &row_json(&core, token), &step["row"]);
                assert_eq_py(c, &Value::Array(writes), &step["writes"]);
            }
            "note_period" => {
                let token = step["token"].as_str().unwrap();
                core.load_periods(kv.get(PERIOD_KEY).map(String::as_str));
                let text = core
                    .note_period(
                        token,
                        step["seconds"].as_i64().unwrap(),
                        step["wall"].as_f64().unwrap(),
                    )
                    .unwrap();
                assert_eq_py(
                    c,
                    &Value::Array(vec![write_json(PERIOD_KEY, &text)]),
                    &step["writes"],
                );
                kv.insert(PERIOD_KEY.into(), text);
            }
            "period" => {
                let got = core.period(
                    step["token"].as_str().unwrap(),
                    step["wall"].as_f64().unwrap(),
                );
                assert_eq_py(c, &got.map_or(Value::Null, |v| json!(v)), &step["out"]);
            }
            "snapshot" => {
                let tokens: Vec<&str> = step["tokens"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap())
                    .collect();
                let got = core.snapshot(
                    tokens,
                    threshold_of(&step["threshold_raw"]),
                    step["wall"].as_f64().unwrap(),
                    step["mono"].as_f64().unwrap(),
                );
                assert_eq_py_ordered(c, &got, &step["out"]);
            }
            other => panic!("{ctx}：未知操作 {other}"),
        }
    }
    // 终态：periods 整体（含账号插入顺序）
    let got = core
        .periods
        .as_ref()
        .map_or(Value::Null, |p| serde_json::from_str(&p.to_json()).unwrap());
    assert_eq_py_ordered(c, &got, &c.raw["final_periods"]);
    n_fetch
}

fn replay_file(name: &str, min_cases: usize) -> usize {
    let cases = load(M, name);
    assert!(cases.len() >= min_cases);
    cases.iter().map(replay).sum()
}

#[test]
fn bodies_golden() {
    assert!(replay_file("allowance/bodies", 70) >= 70);
}

#[test]
fn status_and_retry_after_golden() {
    assert!(replay_file("allowance/status", 300) >= 300);
}

#[test]
fn traces_golden() {
    // 随机轨迹里要真的发生过请求（不能全是缓存命中 / 退避）
    assert!(replay_file("allowance/traces", 39) > 1000);
}
