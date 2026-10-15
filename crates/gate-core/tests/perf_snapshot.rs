mod common;

use common::Tally;
use gate_core::perf::{self, PerfParams, PerfRow, PerfStats};
use gate_snapshot::snapshot::check;
use gate_snapshot::{Case, as_f64};
use serde_json::{Value, json};
use std::collections::BTreeSet;

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
fn pure_functions_snapshot() {
    let mut tally = Tally::default();
    // 每种提示都要真的出现过，否则 _flags 的快照是空转
    let mut seen = BTreeSet::new();
    check(M, "perf/pure", |c| {
        let name = c.get("fn")?.as_str()?;
        tally.add(name);
        Some(match name {
            "family" => json!(perf::family(c.arg(0).as_str())),
            "_pct" => {
                let vals: Vec<i64> = c
                    .arg(0)
                    .as_array()?
                    .iter()
                    .map(|x| x.as_i64().unwrap())
                    .collect();
                json!(perf::pct(&vals, as_f64(c.arg(1))?))
            }
            "_kind" => {
                let k = perf::kind(c.arg(0).as_str()?, c.arg(1).as_i64()?, c.arg(2).as_str());
                json!(k.as_str())
            }
            "_stats" => {
                let rows = rows_of(c.arg(0));
                let refs: Vec<&PerfRow> = rows.iter().collect();
                let params = params_of(c.arg(1), c.arg(2), c.arg(3), c.arg(5));
                let got = perf::stats(&refs, &params, as_f64(c.arg(4))?);
                serde_json::to_value(&got).unwrap()
            }
            "_hourly" => {
                let rows = rows_of(c.arg(0));
                let refs: Vec<&PerfRow> = rows.iter().collect();
                serde_json::to_value(perf::hourly(&refs, as_f64(c.arg(1))?)).unwrap()
            }
            "_flags" => {
                let got = perf::flags(
                    c.arg(0).as_str()?,
                    &stats_of(c.arg(1)),
                    &stats_of(c.arg(2)),
                    &stats_of(c.arg(3)),
                );
                let v = serde_json::to_value(&got).unwrap();
                for f in v.as_array().unwrap() {
                    seen.insert(format!(
                        "{}/{}",
                        f["code"].as_str().unwrap(),
                        f["level"].as_str().unwrap()
                    ));
                }
                v
            }
            other => panic!("未知函数 {other}"),
        })
    });
    tally.assert_at_least(&[
        ("family", 10),
        ("_pct", 30),
        ("_kind", 20),
        ("_stats", 20),
        ("_hourly", 10),
        ("_flags", 30),
    ]);
    for want in [
        "account/bad",
        "throttle/warn",
        "slow/warn",
        "fail/bad",
        "fail/warn",
    ] {
        assert!(seen.contains(want), "快照没有覆盖提示 {want}：{seen:?}");
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
fn analyze_snapshot() {
    let (mut n, mut flag_total, mut families_total) = (0, 0, 0);
    check(M, "perf/analyze", |c| {
        let got = analyze_case(c);
        n += 1;
        flag_total += got.flags.len();
        families_total += got.families.0.len();
        Some(serde_json::to_value(&got).unwrap())
    });
    assert!(n >= 20, "analyze 用例太少：{n}");
    assert!(flag_total >= 10, "analyze 快照里的提示太少：{flag_total}");
    assert!(families_total >= 30);
}

#[test]
fn constants() {
    assert_eq!(perf::FAMILIES, ["V4.5", "V5", "其他"]);
    assert_eq!(perf::HOUR, 3600);
    assert_eq!(perf::DAY, 86400);
    assert_eq!(perf::BASELINE_DAYS, 7);
    assert_eq!(perf::MIN_SAMPLES, 5);
    assert_eq!(perf::SLOW_RATIO, 1.5);
    assert_eq!(perf::THROTTLE_COUNT_1H, 3);
    assert_eq!(perf::THROTTLE_SHARE, 0.1);
    assert_eq!(perf::FAIL_DROP, 0.1);
    assert_eq!(perf::FAIL_FLOOR_1H, 0.8);
}
