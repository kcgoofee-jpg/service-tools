use gate_core::perf::{self, PerfParams, PerfRow, PerfStats};
use gate_golden::{Case, as_f64, assert_eq_py, assert_eq_py_ordered, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn row_of(v: &Value) -> PerfRow {
    let a = v.as_array().expect("行是数组");
    PerfRow {
        ts: as_f64(&a[0]).unwrap(),
        model: opt_str(&a[1]),
        status: a[2].as_str().unwrap().to_string(),
        images: a[3].as_i64().unwrap(),
        wait_ms: a[4].as_i64().unwrap(),
        dur_ms: a[5].as_i64().unwrap(),
        up_status: a[6].as_i64().unwrap(),
        detail: opt_str(&a[7]),
    }
}

fn rows_of(v: &Value) -> Vec<PerfRow> {
    v.as_array().unwrap().iter().map(row_of).collect()
}

fn params_of(slots: &Value, site: &Value, key: &Value, cap: &Value) -> PerfParams {
    PerfParams {
        slots: slots.as_i64().unwrap(),
        site_interval: as_f64(site).unwrap(),
        key_interval: as_f64(key).unwrap(),
        hourly_cap: cap.as_i64().unwrap(),
    }
}

/// `_flags` 只看这几个字段。
fn stats_of(v: &Value) -> PerfStats {
    PerfStats {
        requests: v["requests"].as_u64().unwrap() as usize,
        ok: 0,
        images: 0,
        rate_limited: v["rate_limited"].as_u64().unwrap() as usize,
        auth: v["auth"].as_u64().unwrap() as usize,
        server: 0,
        other: 0,
        success_rate: v["success_rate"].as_f64(),
        samples: v["samples"].as_u64().unwrap() as usize,
        p50_ms: v["p50_ms"].as_i64(),
        p90_ms: None,
        avg_wait_ms: None,
        p90_wait_ms: None,
        actual_per_hour: None,
        capacity_per_hour: None,
        member_capacity_per_hour: None,
    }
}

#[test]
fn pure_functions_golden() {
    let cases = load(M, "perf/pure");
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for c in cases {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *counts.entry(name.clone()).or_default() += 1;
        match name.as_str() {
            "family" => assert_eq_py(&c, &json!(perf::family(c.arg(0).as_str())), c.out()),
            "_pct" => {
                let vals: Vec<i64> = c
                    .arg(0)
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_i64().unwrap())
                    .collect();
                let q = as_f64(c.arg(1)).unwrap();
                assert_eq_py(&c, &json!(perf::pct(&vals, q)), c.out());
            }
            "_kind" => {
                let k = perf::kind(
                    c.arg(0).as_str().unwrap(),
                    c.arg(1).as_i64().unwrap(),
                    c.arg(2).as_str(),
                );
                assert_eq_py(&c, &json!(k.as_str()), c.out());
            }
            "_stats" => {
                let rows = rows_of(c.arg(0));
                let refs: Vec<&PerfRow> = rows.iter().collect();
                let params = params_of(c.arg(1), c.arg(2), c.arg(3), c.arg(5));
                let got = perf::stats(&refs, &params, as_f64(c.arg(4)).unwrap());
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            "_hourly" => {
                let rows = rows_of(c.arg(0));
                let refs: Vec<&PerfRow> = rows.iter().collect();
                let got = perf::hourly(&refs, as_f64(c.arg(1)).unwrap());
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            "_flags" => {
                let fam = c.arg(0).as_str().unwrap();
                let got = perf::flags(
                    fam,
                    &stats_of(c.arg(1)),
                    &stats_of(c.arg(2)),
                    &stats_of(c.arg(3)),
                );
                assert_eq_py_ordered(&c, &serde_json::to_value(&got).unwrap(), c.out());
            }
            other => panic!("未知函数 {other}"),
        }
    }
    for (name, want) in [
        ("family", 20),
        ("_pct", 1000),
        ("_kind", 300),
        ("_stats", 300),
        ("_hourly", 100),
        ("_flags", 1000),
    ] {
        assert!(
            counts.get(name).copied().unwrap_or(0) >= want,
            "{name} 用例太少：{counts:?}"
        );
    }
}

#[test]
fn flags_cover_every_code() {
    // 夹具里每种提示都要真的出现过，否则 _flags 的对拍是空转
    let mut seen = std::collections::BTreeSet::new();
    for c in load(M, "perf/pure") {
        if c.raw["fn"] == "_flags" {
            for f in c.out().as_array().unwrap() {
                seen.insert(format!(
                    "{}/{}",
                    f["code"].as_str().unwrap(),
                    f["level"].as_str().unwrap()
                ));
            }
        }
    }
    for want in [
        "account/bad",
        "throttle/warn",
        "slow/warn",
        "fail/bad",
        "fail/warn",
    ] {
        assert!(seen.contains(want), "夹具没有覆盖提示 {want}：{seen:?}");
    }
}

fn analyze_case(c: &Case) -> perf::PerfReport {
    let rows = rows_of(c.arg(0));
    let kw = |k: &str| c.kwarg(k).unwrap_or_else(|| panic!("缺少关键字参数 {k}"));
    let params = params_of(
        kw("slots"),
        kw("site_interval"),
        kw("key_interval"),
        kw("hourly_cap"),
    );
    perf::analyze(&rows, as_f64(c.arg(1)).unwrap(), &params)
}

#[test]
fn analyze_golden() {
    let mut flag_total = 0;
    let mut families_total = 0;
    let cases = load(M, "perf/analyze");
    assert!(cases.len() > 200);
    for c in cases {
        let got = analyze_case(&c);
        let got_json = serde_json::to_value(&got).unwrap();
        assert_eq_py_ordered(&c, &got_json, c.out());
        flag_total += got.flags.len();
        families_total += got.families.0.len();
    }
    assert!(flag_total > 100, "analyze 夹具里的提示太少：{flag_total}");
    assert!(families_total > 300);
}

#[test]
fn constants_golden() {
    let c = &load(M, "analysis/consts")[0];
    let p = &c.raw["perf"];
    assert_eq!(
        perf::FAMILIES.to_vec(),
        p["FAMILIES"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(perf::HOUR, p["HOUR"].as_i64().unwrap());
    assert_eq!(perf::DAY, p["DAY"].as_i64().unwrap());
    assert_eq!(perf::BASELINE_DAYS, p["BASELINE_DAYS"].as_i64().unwrap());
    assert_eq!(perf::MIN_SAMPLES as u64, p["MIN_SAMPLES"].as_u64().unwrap());
    assert_eq!(
        perf::SLOW_RATIO.to_bits(),
        p["SLOW_RATIO"].as_f64().unwrap().to_bits()
    );
    assert_eq!(
        perf::THROTTLE_COUNT_1H as u64,
        p["THROTTLE_COUNT_1H"].as_u64().unwrap()
    );
    assert_eq!(
        perf::THROTTLE_SHARE.to_bits(),
        p["THROTTLE_SHARE"].as_f64().unwrap().to_bits()
    );
    assert_eq!(
        perf::FAIL_DROP.to_bits(),
        p["FAIL_DROP"].as_f64().unwrap().to_bits()
    );
    assert_eq!(
        perf::FAIL_FLOOR_1H.to_bits(),
        p["FAIL_FLOOR_1H"].as_f64().unwrap().to_bits()
    );
}
