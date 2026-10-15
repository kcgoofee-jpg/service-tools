//! guard.rs（账号保护）的快照。
//!
//! - 设置的读写和校验（`guard/settings`）；
//! - 安静时段、按时段放行的网格（`guard/grids`）；
//! - 操作轨迹（`guard/traces`）：`GuardCore` 在固定时钟 / 固定随机数 / 假库下执行的一串操作，每步记返回值、
//!   写库内容和执行后的全部内存状态（时间戳队列用 FNV-1a 摘要）。
//!
//! 常量、`GuardCore::new` 的默认值和 `parse_setting_float` 是手写的表。

mod common;

use common::Tally;
use gate_core::guard::{
    self, AdaptPlan, FIELDS, GuardCore, GuardError, parse_setting_float, stamps_from_rows,
};
use gate_core::reasons::Reason;
use gate_snapshot::snapshot::check;
use gate_snapshot::{Case, f};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const M: &str = env!("CARGO_MANIFEST_DIR");

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn fnv_bytes(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 执行后的全部内存状态：设置值（按 `FIELDS` 顺序）、每个账号的时间戳队列（条数 + 摘要）、在途数、排队表、
/// 序号、熔断、每小时上限的标记。
fn state_json(g: &GuardCore) -> Value {
    let v: Vec<Value> = FIELDS.iter().map(|fl| json!(g.values[fl.name])).collect();
    let s: Map<String, Value> = g
        .starts
        .iter()
        .map(|(tok, q)| {
            let h = fnv_bytes(q.iter().flat_map(|x| x.to_le_bytes()));
            (tok.clone(), json!([q.len(), h]))
        })
        .collect();
    let fl: Vec<Value> = g
        .image_inflight
        .iter()
        .map(|(k, n)| json!([k, n]))
        .collect();
    let e: Vec<Value> = g
        .entries
        .iter()
        .map(|e| json!([e.id, e.key, f(e.since), e.running]))
        .collect();
    let marks_hash = fnv_bytes(g.cap_marks.iter().flat_map(|(t, c)| {
        let mut b = t.to_le_bytes().to_vec();
        b.extend_from_slice(&c.to_le_bytes());
        b
    }));
    let last = g
        .cap_marks
        .back()
        .map_or(Value::Null, |(t, c)| json!([f(*t), c]));
    json!({
        "v": v,
        "s": s,
        "f": fl,
        "e": e,
        "q": g.seq,
        "b": [f(g.breaker_until), g.breaker_reason],
        "m": [g.cap_marks.len(), last, marks_hash],
    })
}

fn pair(r: Option<Reason>) -> Value {
    r.map_or(Value::Null, |r| json!([r.text(), r.code()]))
}

fn err_type(e: &GuardError) -> &'static str {
    match e {
        GuardError::Value(_) => "ValueError",
        GuardError::Overflow(_) => "OverflowError",
    }
}

/// 异常的记法：类型，ValueError 再带上面向用户的消息。
fn err_json(e: &GuardError) -> Value {
    match e {
        GuardError::Value(m) => json!({"err": err_type(e), "msg": m}),
        GuardError::Overflow(_) => json!({"err": err_type(e)}),
    }
}

fn kv_writes(writes: &[(String, String)]) -> Value {
    Value::Array(writes.iter().map(|(k, v)| json!([k, v])).collect())
}

// ----------------------------------------------------------------------------- 手写的表

#[test]
fn constants() {
    assert_eq!(guard::HOUR, 3600);
    assert_eq!(guard::WINDOW_3H, 10800);
    assert_eq!(guard::HOURLY_MIN, 100);
    assert_eq!(guard::HOURLY_MAX, 160);
    assert_eq!(guard::HOURLY_STEP, 10);
    // (名字, 默认, 下限, 上限)；说明文字在 describe 的快照里
    let want: [(&str, i64, i64, i64); 14] = [
        ("account_daily_cap", 1000, 0, 20000),
        ("daily_burst", 150, 0, 20000),
        ("account_hourly_cap", 150, 0, 240),
        ("quiet_start", 0, 0, 23),
        ("quiet_end", 0, 0, 23),
        ("quiet_hourly_cap", 20, 0, 240),
        ("interval_jitter", 5, 0, 30),
        ("post_jitter_min", 1, 0, 30),
        ("post_jitter_max", 3, 0, 30),
        ("key_image_queue", 1, 0, 3),
        ("queue_per_account", 5, 0, 100),
        ("hourly_min", 100, 0, 240),
        ("hourly_max", 160, 0, 240),
        ("hourly_step", 10, 0, 100),
    ];
    let got: Vec<(&str, i64, i64, i64)> = FIELDS
        .iter()
        .map(|fl| (fl.name, fl.default, fl.min, fl.max))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn new_clamps_the_reset_hour_and_starts_from_defaults() {
    for (arg, want) in [
        (-5, 0),
        (-1, 0),
        (0, 0),
        (1, 1),
        (7, 7),
        (23, 23),
        (24, 23),
        (99, 23),
    ] {
        let g = GuardCore::new("Asia/Shanghai", arg);
        assert_eq!(g.reset_hour, want, "reset_hour_arg={arg}");
        for fl in &FIELDS {
            assert_eq!(g.values[fl.name], fl.default, "{}", fl.name);
        }
        assert_eq!(g.values_map().len(), FIELDS.len());
    }
}

#[test]
fn setting_float_table() {
    for (raw, want) in [
        (None, 0.0),
        (Some(""), 0.0),
        (Some("0"), 0.0),
        (Some("0.0"), 0.0),
        (Some("125"), 125.0),
        (Some("125.0"), 125.0),
        (Some(" 7 "), 7.0),
        (Some("1e3"), 1000.0),
        (Some("1_0"), 10.0),
        (Some("1790000000.123456"), 1790000000.123456),
        (Some("-5"), -5.0),
        (Some("５"), 5.0),
        (Some("inf"), f64::INFINITY),
        (Some("-inf"), f64::NEG_INFINITY),
    ] {
        assert_eq!(parse_setting_float(raw).unwrap(), want, "{raw:?}");
    }
    assert!(parse_setting_float(Some("nan")).unwrap().is_nan());
    for bad in ["abc", "0x1", "1,5"] {
        let e = parse_setting_float(Some(bad)).unwrap_err();
        assert_eq!(err_type(&e), "ValueError", "{bad}");
    }
}

// ----------------------------------------------------------------------------- 设置

#[test]
fn settings_snapshot() {
    let mut tally = Tally::default();
    check(M, "guard/settings", |c| match c.get("fn")?.as_str()? {
        "load" => {
            let mut g = GuardCore::new("Asia/Shanghai", 0);
            for (k, v) in obj(c.get("init")?) {
                g.values.insert(k, v.as_i64()?);
            }
            let stored: BTreeMap<String, String> = obj(c.get("stored")?)
                .into_iter()
                .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
                .collect();
            let r = g.load_settings(&stored);
            tally.add("load");
            tally.add_if(r.is_err(), "load_overflow");
            // 出错时前面的字段已更新（与旧版一致），所以无论成败都记最终的值
            Some(json!({
                "err": r.err().map(|e| err_type(&e)),
                "values": Value::Object(g.values_map()),
            }))
        }
        "save" => {
            let mut g = GuardCore::new("Asia/Shanghai", 0);
            let r = g.save(&obj(c.arg(0)));
            // to_settings 与 save 写库的格式一致：全部字段，按 FIELDS 顺序，整数不带 .0
            let all = g.to_settings();
            assert_eq!(all.len(), FIELDS.len());
            for (i, fl) in FIELDS.iter().enumerate() {
                assert_eq!(
                    all[i],
                    (format!("guard_{}", fl.name), g.values[fl.name].to_string())
                );
            }
            let values = Value::Object(g.values_map());
            Some(match r {
                Ok(saved) => {
                    tally.add("save");
                    json!({"writes": kv_writes(&saved.writes), "values": values})
                }
                Err(e) => {
                    tally.add("save_error");
                    let mut m = obj(&err_json(&e));
                    m.insert("values".into(), values);
                    Value::Object(m)
                }
            })
        }
        other => panic!("未知 {other}"),
    });
    tally.assert_at_least(&[
        ("load", 20),
        ("load_overflow", 3),
        ("save", 10),
        ("save_error", 20),
    ]);
}

// ----------------------------------------------------------------------------- 网格

/// 安静时段网格用的 24 个时刻（各小时一个，分钟数错开）。
const QUIET_NOWS: [f64; 24] = [
    1791907200.25,
    1791911777.25,
    1791916354.25,
    1791920931.25,
    1791921908.25,
    1791926485.25,
    1791931062.25,
    1791935639.25,
    1791936616.25,
    1791941193.25,
    1791945770.25,
    1791950347.25,
    1791951324.25,
    1791955901.25,
    1791960478.25,
    1791961455.25,
    1791966032.25,
    1791970609.25,
    1791975186.25,
    1791976163.25,
    1791980740.25,
    1791985317.25,
    1791989894.25,
    1791990871.25,
];

fn floats(v: &Value) -> Vec<f64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect()
}

#[test]
fn grids_snapshot() {
    let mut tally = Tally::default();
    check(M, "guard/grids", |c| match c.get("fn")?.as_str()? {
        // 开始 / 结束两个整点 × 24 个时刻：是否在安静时段内
        "in_quiet_grid" => {
            let mut g = GuardCore::new("Asia/Shanghai", 7);
            g.values.insert("quiet_start".into(), c.arg(0).as_i64()?);
            g.values.insert("quiet_end".into(), c.arg(1).as_i64()?);
            tally.add("in_quiet_grid");
            Some(json!(
                QUIET_NOWS
                    .iter()
                    .map(|t| g.in_quiet(*t))
                    .collect::<Vec<bool>>()
            ))
        }
        // (重置时刻, 每日上限, 每日先放出的张数) × 若干时刻 / 已用张数：当天已过去的秒数、按时段的放行额度、还要等几分钟
        "paced_grid" => {
            let mut g = GuardCore::new("Asia/Shanghai", c.arg(0).as_i64()?);
            g.values
                .insert("account_daily_cap".into(), c.arg(1).as_i64()?);
            g.values.insert("daily_burst".into(), c.arg(2).as_i64()?);
            let nows = floats(c.get("nows")?);
            let useds = floats(c.get("useds")?);
            tally.add("paced_grid");
            Some(json!({
                "elapsed": nows.iter().map(|t| f(g.day_elapsed(*t))).collect::<Vec<_>>(),
                "allowance": nows.iter().map(|t| f(g.paced_allowance(*t))).collect::<Vec<_>>(),
                "minutes": nows
                    .iter()
                    .zip(&useds)
                    .map(|(t, u)| json!(g.minutes_until_paced(*u, *t)))
                    .collect::<Vec<_>>(),
            }))
        }
        other => panic!("未知 {other}"),
    });
    tally.assert_at_least(&[("in_quiet_grid", 30), ("paced_grid", 30)]);
}

// ----------------------------------------------------------------------------- 轨迹

/// 一条轨迹的重放：每步 `{"out" | "err", "writes", "state"}`。
fn replay(c: &Case, tally: &mut Tally) -> Vec<Value> {
    let init = c.get("init").unwrap();
    let mut g = GuardCore::new(
        init["tz"].as_str().unwrap(),
        init["reset_hour_arg"].as_i64().unwrap(),
    );
    for (k, v) in obj(&init["values"]) {
        g.values.insert(k, v.as_i64().unwrap());
    }
    if let Some(x) = init.get("hourly_decrease").and_then(gate_snapshot::as_f64) {
        g.hourly_decrease = x;
    }
    let mut kv: BTreeMap<String, String> = obj(&init["kv"])
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
        .collect();

    let mut results = Vec::new();
    for (i, step) in c
        .get("steps")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let op = step["op"].as_str().unwrap();
        tally.add(op);
        let now = step["now"].as_f64().unwrap_or(0.0);
        let tok = step.get("token").and_then(Value::as_str).unwrap_or("");
        let mut writes: Vec<(String, String)> = Vec::new();

        // 结果：Ok(返回值) 或 Err(异常)
        let out: Result<Value, Value> = match op {
            "record_starts" => {
                for e in step["entries"].as_array().unwrap() {
                    g.record_start(tok, e[0].as_f64().unwrap(), e[1].as_i64().unwrap());
                }
                Ok(Value::Null)
            }
            "hourly_cap" => Ok(json!(g.hourly_cap(now))),
            "hourly_cap_ceiling" => Ok(json!(g.hourly_cap_ceiling(now))),
            "in_quiet" => Ok(json!(g.in_quiet(now))),
            "hour_count" => Ok(json!(g.hour_count(tok, now))),
            "minutes_until_free" => Ok(json!(g.minutes_until_free(tok, now))),
            "paced_allowance" => Ok(f(g.paced_allowance(now))),
            "minutes_until_paced" => Ok(json!(
                g.minutes_until_paced(step["used"].as_f64().unwrap(), now)
            )),
            "token_block_reason" => Ok(pair(g.token_block_reason(
                tok,
                step["used"].as_f64().unwrap(),
                now,
            ))),
            "on_upstream_429" => (|| -> Result<Value, GuardError> {
                let prev = parse_setting_float(kv.get("guard_429_cap_at").map(String::as_str))?;
                let r = g.on_upstream_429(now, prev)?;
                writes = r.writes;
                Ok(r.change.map_or(Value::Null, |(a, b)| json!([a, b])))
            })()
            .map_err(|e| err_json(&e)),
            "adapt_daily" => (|| -> Result<Value, GuardError> {
                let rd = |k: &str| parse_setting_float(kv.get(k).map(String::as_str));
                let last_429 = rd("guard_last_upstream_429")?;
                let last_step = rd("guard_hourly_adapted_at")?;
                let adapted = match GuardCore::adapt_daily_begin(now, last_429, last_step) {
                    AdaptPlan::Done(a) => a,
                    AdaptPlan::NeedUsage => {
                        let seen = rd("guard_429_cap_at")?;
                        let first = step["first"].as_f64();
                        g.adapt_daily_finish(now, step["hit"].as_bool().unwrap(), first, seen)?
                    }
                };
                writes = adapted.writes;
                Ok(adapted.change.map_or(Value::Null, |(a, b)| json!([a, b])))
            })()
            .map_err(|e| err_json(&e)),
            "set_kv" => {
                let key = step["key"].as_str().unwrap().to_string();
                match step["value"].as_str() {
                    Some(v) => {
                        kv.insert(key, v.to_string());
                    }
                    None => {
                        kv.remove(&key);
                    }
                }
                results.push(Value::Null);
                continue;
            }
            "trip_breaker" => g
                .trip_breaker(
                    step["seconds"].as_f64().unwrap(),
                    step["reason"].as_str().unwrap(),
                    now,
                )
                .map(|()| Value::Null)
                .map_err(|e| err_json(&e)),
            "jitter" => {
                let draws: Vec<f64> = step.get("draws").map(floats).unwrap_or_default();
                let mut used = 0usize;
                let mut rand = || {
                    let r = *draws
                        .get(used)
                        .unwrap_or_else(|| panic!("第 {i} 步：多取了随机数"));
                    used += 1;
                    r
                };
                let out = g.jitter(&mut rand);
                // 随机数消耗次数也是行为的一部分
                Ok(json!({"value": f(out), "draws_used": used}))
            }
            "admit_image" => Ok(pair(g.admit_image(
                step["key"].as_i64().unwrap(),
                step["accounts"].as_i64().unwrap(),
                now,
            ))),
            "mark_running" => {
                g.mark_running(step["key"].as_i64().unwrap(), step["entry_id"].as_i64());
                Ok(Value::Null)
            }
            "release_image" => {
                g.release_image(step["key"].as_i64().unwrap(), step["entry_id"].as_i64());
                Ok(Value::Null)
            }
            "queue_view" => Ok(g.queue_view(step["key"].as_i64())),
            "set_value" => {
                g.values.insert(
                    step["name"].as_str().unwrap().to_string(),
                    step["value"].as_i64().unwrap(),
                );
                results.push(json!({"state": state_json(&g)}));
                continue;
            }
            "save" => match g.save(&obj(&step["body"])) {
                Ok(saved) => {
                    writes = saved.writes;
                    Ok(Value::Object(g.values_map()))
                }
                Err(e) => Err(err_json(&e)),
            },
            "describe" => Ok(g.describe(now)),
            "seed_hour" => {
                let toks: Vec<String> = step["tokens"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect();
                let rows: Vec<(f64, i64)> = step["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| (r[0].as_f64().unwrap(), r[1].as_i64().unwrap()))
                    .collect();
                let stamps = stamps_from_rows(&rows);
                Ok(json!(g.seed_hour(&toks, &stamps)))
            }
            other => panic!("第 {i} 步：未知操作 {other}"),
        };

        tally.add_if(out.is_err(), "error");
        let writes_json = kv_writes(&writes);
        // 写库内容（含顺序），并同步到假库
        for (k, v) in writes {
            kv.insert(k, v);
        }
        let mut m = Map::new();
        match out {
            Ok(v) => m.insert("out".into(), v),
            Err(e) => m.insert("err".into(), e),
        };
        m.insert("writes".into(), writes_json);
        m.insert("state".into(), state_json(&g));
        results.push(Value::Object(m));
    }
    results
}

#[test]
fn traces_snapshot() {
    let mut tally = Tally::default();
    check(M, "guard/traces", |c| {
        tally.add("trace");
        Some(Value::Array(replay(c, &mut tally)))
    });
    // 快照要真的覆盖到各个分支
    tally.assert_at_least(&[
        ("trace", 8),
        ("record_starts", 50),
        ("token_block_reason", 50),
        ("on_upstream_429", 10),
        ("adapt_daily", 10),
        ("admit_image", 10),
        ("release_image", 5),
        ("jitter", 5),
        ("describe", 5),
        ("save", 5),
        ("seed_hour", 1),
        ("trip_breaker", 3),
    ]);
}
