use std::collections::HashMap;

use gate_core::shadow::{self, Req, ShadowOptions, ShadowRow};
use gate_golden::{as_f64, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn row_of(v: &Value) -> ShadowRow {
    let a = v.as_array().unwrap();
    ShadowRow {
        ts: as_f64(&a[0]).unwrap(),
        key_id: a[1].as_i64(),
        name: opt_str(&a[2]),
        status: a[3].as_str().unwrap().to_string(),
        images: a[4].as_i64().unwrap(),
        wait_ms: a[5].as_i64().unwrap(),
        dur_ms: a[6].as_i64().unwrap(),
    }
}

fn rows_of(v: &Value) -> Vec<ShadowRow> {
    v.as_array().unwrap().iter().map(row_of).collect()
}

fn req_of(v: &Value) -> Req {
    Req {
        key: v["key"].as_i64().unwrap(),
        name: opt_str(&v["name"]),
        arrival: as_f64(&v["arrival"]).unwrap(),
        start: as_f64(&v["start"]).unwrap(),
        dur: as_f64(&v["dur"]).unwrap(),
        wait: as_f64(&v["wait"]).unwrap(),
        ok: v["ok"].as_bool().unwrap(),
        images: v["images"].as_i64().unwrap(),
    }
}

fn reqs_of(v: &Value) -> Vec<Req> {
    v.as_array().unwrap().iter().map(req_of).collect()
}

/// `{"$map": [[k, v], ...]}`（int 键的 dict）或空对象 `{}`。
fn int_map(v: &Value) -> Vec<(i64, Value)> {
    if let Some(pairs) = v.get("$map").and_then(Value::as_array) {
        pairs
            .iter()
            .map(|p| (p[0].as_i64().unwrap(), p[1].clone()))
            .collect()
    } else {
        assert!(
            v.as_object().is_some_and(|o| o.is_empty()),
            "不是 int 键 dict：{v}"
        );
        Vec::new()
    }
}

#[test]
fn pure_functions_golden() {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for c in load(M, "shadow/pure") {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *counts.entry(name.clone()).or_default() += 1;
        match name.as_str() {
            "_pct" => {
                let vals: Vec<f64> = c
                    .arg(0)
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| as_f64(x).unwrap())
                    .collect();
                let got = shadow::pct(&vals, as_f64(c.arg(1)).unwrap());
                assert_eq_py(&c, &got.map_or(Value::Null, f), c.out());
            }
            "_requests" => {
                let got = shadow::requests(&rows_of(c.arg(0)));
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            "contention" => {
                let got = shadow::contention(&reqs_of(c.arg(0)));
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            "busy_windows" => {
                let slots = c.kwarg("slots").and_then(Value::as_i64).unwrap();
                let interval = as_f64(c.kwarg("interval").unwrap()).unwrap();
                let got = shadow::busy_windows(&reqs_of(c.arg(0)), slots, interval);
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            "per_member" => {
                let day_images: Vec<(i64, i64)> = int_map(c.arg(0))
                    .into_iter()
                    .map(|(k, v)| (k, v.as_i64().unwrap()))
                    .collect();
                let names: HashMap<i64, Option<String>> = int_map(c.arg(1))
                    .into_iter()
                    .map(|(k, v)| (k, opt_str(&v)))
                    .collect();
                let kw = |k: &str| c.kwarg(k).and_then(Value::as_i64).unwrap();
                let got = shadow::per_member(
                    &day_images,
                    &names,
                    kw("accounts"),
                    kw("per_person"),
                    kw("account_cap"),
                );
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            other => panic!("未知函数 {other}"),
        }
    }
    for (name, want) in [
        ("_pct", 500),
        ("_requests", 100),
        ("contention", 100),
        ("busy_windows", 100),
        ("per_member", 200),
    ] {
        assert!(
            counts.get(name).copied().unwrap_or(0) >= want,
            "{name} 用例太少：{counts:?}"
        );
    }
}

#[test]
fn analyze_golden() {
    let (mut busy, mut contended, mut full) = (0, 0, 0);
    let cases = load(M, "shadow/analyze");
    assert!(cases.len() > 200);
    for c in cases {
        let kw = |k: &str| c.kwarg(k).unwrap_or_else(|| panic!("缺少关键字参数 {k}"));
        let opts = ShadowOptions {
            slots: kw("slots").as_i64().unwrap(),
            interval: as_f64(kw("interval")).unwrap(),
            key_interval: as_f64(kw("key_interval")).unwrap(),
            accounts: kw("accounts").as_i64().unwrap(),
            per_person: kw("per_person").as_i64().unwrap(),
            account_cap: kw("account_cap").as_i64().unwrap(),
        };
        let got = shadow::analyze(&rows_of(c.arg(0)), &opts);
        busy += i64::from(got.load.busy_minutes > 0);
        contended += i64::from(got.contention.peak_waiting_members > 0);
        full += i64::from(got.quota.full > 0);
        // 夹具已去掉 fifo / drr / by_accounts；其余字段（含键顺序）必须一致
        let got_json = serde_json::to_value(&got).unwrap();
        assert_eq_py_ordered(&c, &got_json, c.out());
        let keys: Vec<&str> = got_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["samples", "actual", "contention", "load", "quota", "params"]
        );
    }
    assert!(
        busy > 5 && contended > 30 && full > 5,
        "覆盖不足：繁忙 {busy} 排队 {contended} 用满 {full}"
    );
    let _ = json!(null);
}

#[test]
fn constants_golden() {
    let c = &load(M, "analysis/consts")[0];
    let s = &c.raw["shadow"];
    assert_eq!(shadow::BASE_QUOTA, s["BASE_QUOTA"].as_i64().unwrap());
    assert_eq!(shadow::CEIL_QUOTA, s["CEIL_QUOTA"].as_i64().unwrap());
    assert_eq!(
        shadow::ACCOUNT_DAILY_CAP,
        s["ACCOUNT_DAILY_CAP"].as_i64().unwrap()
    );
    assert_eq!(
        shadow::BUSY_UTIL.to_bits(),
        s["BUSY_UTIL"].as_f64().unwrap().to_bits()
    );
    assert_eq!(
        shadow::IDLE_UTIL.to_bits(),
        s["IDLE_UTIL"].as_f64().unwrap().to_bits()
    );
    assert_eq!(shadow::UTIL_WINDOW, s["UTIL_WINDOW"].as_i64().unwrap());
    assert_eq!(
        shadow::LIGHT_USER_MAX,
        s["LIGHT_USER_MAX"].as_i64().unwrap()
    );
    assert_eq!(shadow::MAX_ROWS as u64, s["MAX_ROWS"].as_u64().unwrap());
}
