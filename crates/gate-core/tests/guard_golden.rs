//! guard.rs 与旧 Python 对拍（夹具：tools/parity/golden/guard.py）。
//!
//! 轨迹夹具：真实 `Guard` 在假时钟 / 假随机 / 假数据库下执行的一串操作，每步记录返回值、写库内容和执行后的
//! 全部内存状态（时间戳队列用 FNV-1a 摘要）。这里用 `GuardCore` 重放，逐步比较，状态也要逐步一致。
//!
//! 3.0 删掉的（docs/v3-deviations.md #61）：设置 `base_daily_images`（V4.5 保底）、`site_idle`（空闲借用）、
//! `paced_headroom`、常量 `IDLE_SHARE`。夹具冻结不改，在期望一侧去掉它们：
//! - 设置表、`values`、`describe`、`save` 的写库内容里去掉 `base_daily_images`；轨迹状态里的 `v` 去掉最后一项；
//! - `load` 只因 `guard_base_daily_images` 是 `inf` 而抛 OverflowError 的用例，3.0 读不到这个键，不再报错；
//! - `save` 只因保底字段不合法而失败的用例跳过（旧版在保底字段处就停下，后面的字段没校验过）；
//!   轨迹里的这种 `save` 当作不改任何东西（与旧版一样）；
//! - 轨迹里的 `site_idle` 只重放它的副作用（`hourly_cap` 记上限、弹出 3 小时前的时间戳），不比返回值；
//!   `paced_headroom` 没有副作用，跳过。

use gate_core::guard::{
    self, AdaptPlan, FIELDS, GuardCore, GuardError, parse_setting_float, stamps_from_rows,
};
use gate_core::params::Params;
use gate_core::reasons::Reason;
use gate_golden::{Case, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const M: &str = env!("CARGO_MANIFEST_DIR");

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// 3.0 删掉的设置字段（docs/v3-deviations.md #61）。
const REMOVED_FIELD: &str = "base_daily_images";
/// 它的说明（旧版校验失败的消息以它开头）。
const REMOVED_LABEL: &str = "V4.5 每人每天保底张数";

/// 期望一侧：去掉 `values` 里已删除的字段。
fn without_removed(v: &Value) -> Value {
    let mut m = obj(v);
    m.shift_remove(REMOVED_FIELD);
    Value::Object(m)
}

/// 期望一侧：写库内容里去掉已删除字段的键。
fn writes_without_removed(v: &Value) -> Value {
    let key = format!("guard_{REMOVED_FIELD}");
    Value::Array(
        v.as_array()
            .unwrap()
            .iter()
            .filter(|w| w[0].as_str() != Some(key.as_str()))
            .cloned()
            .collect(),
    )
}

/// 期望一侧：`describe()` 去掉已删除的字段。
fn describe_without_removed(v: &Value) -> Value {
    let mut m = obj(v);
    for k in ["values", "fields"] {
        if let Some(x) = m.get(k) {
            let x = without_removed(x);
            m.insert(k.into(), x);
        }
    }
    Value::Object(m)
}

/// 期望一侧：轨迹状态里的 `v`（按旧版 FIELDS 顺序，已删除的 `base_daily_images` 在最后）。
fn state_without_removed(v: &Value) -> Value {
    let mut m = obj(v);
    if let Some(Value::Array(vals)) = m.get_mut("v") {
        assert_eq!(
            vals.len(),
            FIELDS.len() + 1,
            "夹具里的 v 应带着已删除的字段"
        );
        vals.pop();
    }
    Value::Object(m)
}

/// 旧版 `save` 是否因为已删除的字段不合法而失败（`err` 为夹具里的 `{"type", "msg"}`）。
fn failed_on_removed(err: &Value) -> bool {
    err["msg"]
        .as_str()
        .is_some_and(|m| m.starts_with(REMOVED_LABEL))
}

fn fnv_bytes(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

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

fn kv_writes(writes: &[(String, String)]) -> Value {
    Value::Array(writes.iter().map(|(k, v)| json!([k, v])).collect())
}

#[test]
fn constants_golden() {
    for c in load(M, "guard/constants") {
        match c.raw["fn"].as_str().unwrap() {
            "constants" => {
                let fields: Vec<Value> = FIELDS
                    .iter()
                    .map(|fl| json!([fl.name, fl.default, fl.min, fl.max, fl.label]))
                    .collect();
                let mut want = obj(c.out());
                want.shift_remove("IDLE_SHARE");
                if let Some(Value::Array(fs)) = want.get_mut("FIELDS") {
                    fs.retain(|f| f[0] != REMOVED_FIELD);
                }
                assert_eq_py(
                    &c,
                    &json!({
                        "HOUR": guard::HOUR, "WINDOW_3H": guard::WINDOW_3H, "WINDOW_10M": guard::WINDOW_10M,
                        "BURST_DIVISOR": guard::BURST_DIVISOR, "HOURLY_MIN": guard::HOURLY_MIN,
                        "HOURLY_MAX": guard::HOURLY_MAX, "HOURLY_STEP": guard::HOURLY_STEP,
                        "FIELDS": fields,
                    }),
                    &Value::Object(want),
                );
            }
            "new" => {
                let g = GuardCore::new("Asia/Shanghai", c.arg(0).as_i64().unwrap());
                let values = Value::Object(g.values_map());
                let want = json!({
                    "reset_hour": c.out()["reset_hour"],
                    "values": without_removed(&c.out()["values"]),
                });
                assert_eq_py(
                    &c,
                    &json!({"reset_hour": g.reset_hour, "values": values}),
                    &want,
                );
            }
            other => panic!("未知 {other}"),
        }
    }
}

#[test]
fn setting_float_golden() {
    for c in load(M, "guard/setting_float") {
        let raw = c.arg(0).as_str();
        match (parse_setting_float(raw), c.err()) {
            (Ok(x), None) => assert_eq_py(&c, &f(x), c.out()),
            (Err(e), Some(ty)) => assert_eq!(err_type(&e), ty, "第 {} 行", c.line),
            (got, want) => panic!("第 {} 行：Rust {got:?}，Python 异常 {want:?}", c.line),
        }
    }
}

#[test]
fn settings_golden() {
    let (mut n_load, mut n_save, mut n_save_err, mut n_overflow) = (0, 0, 0, 0);
    let mut n_removed = 0;
    for c in load(M, "guard/settings") {
        match c.raw["fn"].as_str().unwrap() {
            "load" => {
                let mut g = GuardCore::new("Asia/Shanghai", 0);
                for (k, v) in obj(&c.raw["init"]) {
                    g.values.insert(k, v.as_i64().unwrap());
                }
                let stored: BTreeMap<String, String> = obj(&c.raw["stored"])
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
                    .collect();
                let r = g.load_settings(&stored);
                // 旧版 FIELDS 的最后一项是已删除的保底：只因它是 inf 而抛的 OverflowError，3.0 不再出现
                let removed_inf = stored
                    .get(&format!("guard_{REMOVED_FIELD}"))
                    .and_then(|raw| gate_core::pyc::parse_float(raw))
                    .is_some_and(f64::is_infinite);
                match (&r, c.err()) {
                    (Ok(()), None) => {}
                    (Ok(()), Some("OverflowError")) if removed_inf => n_removed += 1,
                    (Err(e), Some(ty)) => {
                        assert_eq!(err_type(e), ty, "第 {} 行", c.line);
                        n_overflow += 1;
                    }
                    (got, want) => panic!("第 {} 行：Rust {got:?}，Python 异常 {want:?}", c.line),
                }
                // 出错时前面的字段已更新（与旧版一致），所以无论成败都比较最终的值
                assert_eq_py(
                    &c,
                    &Value::Object(g.values_map()),
                    &without_removed(c.out()),
                );
                n_load += 1;
            }
            "save" => {
                let mut g = GuardCore::new("Asia/Shanghai", 0);
                assert_eq_py(
                    &c,
                    &Value::Object(g.values_map()),
                    &without_removed(&c.raw["before"]),
                );
                if failed_on_removed(&c.raw["err"]) {
                    // 旧版在保底字段处就停下（后面的字段没校验过）；3.0 没有这个字段，无从对拍
                    n_removed += 1;
                    continue;
                }
                let r = g.save(&obj(c.arg(0)));
                match (&r, c.err()) {
                    (Ok(saved), None) => {
                        assert_eq_py(
                            &c,
                            &kv_writes(&saved.writes),
                            &writes_without_removed(&c.raw["writes"]),
                        );
                        // 返回值是更新后的全部设置
                        assert_eq_py(
                            &c,
                            &Value::Object(g.values_map()),
                            &without_removed(c.out()),
                        );
                        n_save += 1;
                    }
                    (Err(e), Some(ty)) => {
                        assert_eq!(err_type(e), ty, "第 {} 行", c.line);
                        let msg = c.raw["err"]["msg"].as_str().unwrap();
                        let GuardError::Value(got) = e else {
                            unreachable!()
                        };
                        assert_eq!(got, msg, "第 {} 行：消息不同", c.line);
                        n_save_err += 1;
                    }
                    (got, want) => panic!("第 {} 行：Rust {got:?}，Python 异常 {want:?}", c.line),
                }
                assert_eq_py(
                    &c,
                    &Value::Object(g.values_map()),
                    &without_removed(&c.raw["values"]),
                );
                // to_settings 与 save 写库的格式一致：全部字段，按 FIELDS 顺序，整数不带 .0
                let all = g.to_settings();
                assert_eq!(all.len(), FIELDS.len());
                for (i, fl) in FIELDS.iter().enumerate() {
                    assert_eq!(
                        all[i],
                        (format!("guard_{}", fl.name), g.values[fl.name].to_string())
                    );
                }
            }
            other => panic!("未知 {other}"),
        }
    }
    assert!(
        n_load > 300 && n_save > 200 && n_save_err > 300 && n_overflow > 5 && n_removed > 0,
        "{n_load} {n_save} {n_save_err} {n_overflow} {n_removed}"
    );
}

fn floats(v: &Value) -> Vec<f64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect()
}

#[test]
fn grids_golden() {
    let mut quiet_nows: Vec<f64> = Vec::new();
    let mut n = (0, 0);
    for c in load(M, "guard/grids") {
        match c.raw["fn"].as_str().unwrap() {
            "quiet_nows" => quiet_nows = floats(c.out()),
            "in_quiet_grid" => {
                let mut g = GuardCore::new("Asia/Shanghai", 7);
                g.values
                    .insert("quiet_start".into(), c.arg(0).as_i64().unwrap());
                g.values
                    .insert("quiet_end".into(), c.arg(1).as_i64().unwrap());
                let got: Vec<Value> = quiet_nows.iter().map(|t| json!(g.in_quiet(*t))).collect();
                assert_eq_py(&c, &Value::Array(got), c.out());
                n.0 += 1;
            }
            "paced_grid" => {
                let mut g = GuardCore::new("Asia/Shanghai", c.arg(0).as_i64().unwrap());
                g.values
                    .insert("account_daily_cap".into(), c.arg(1).as_i64().unwrap());
                g.values
                    .insert("daily_burst".into(), c.arg(2).as_i64().unwrap());
                let nows = floats(&c.raw["nows"]);
                let useds = floats(&c.raw["useds"]);
                let elapsed: Vec<Value> = nows.iter().map(|t| f(g.day_elapsed(*t))).collect();
                let allowance: Vec<Value> = nows.iter().map(|t| f(g.paced_allowance(*t))).collect();
                let minutes: Vec<Value> = nows
                    .iter()
                    .zip(&useds)
                    .map(|(t, u)| json!(g.minutes_until_paced(*u, *t)))
                    .collect();
                assert_eq_py(&c, &Value::Array(elapsed), &c.raw["elapsed"]);
                assert_eq_py(&c, &Value::Array(allowance), &c.raw["allowance"]);
                assert_eq_py(&c, &Value::Array(minutes), &c.raw["minutes"]);
                n.1 += 1;
            }
            other => panic!("未知 {other}"),
        }
    }
    assert!(n.0 == 576 && n.1 > 70, "{n:?}");
}

/// 一条轨迹的重放。返回各操作的执行次数，供上层断言覆盖面。
fn replay(c: &Case, counts: &mut BTreeMap<String, usize>) {
    let init = &c.raw["init"];
    let mut g = GuardCore::new(
        init["tz"].as_str().unwrap(),
        init["reset_hour_arg"].as_i64().unwrap(),
    );
    assert_eq!(g.reset_hour, init["reset_hour"].as_i64().unwrap());
    for (k, v) in obj(&init["values"]) {
        g.values.insert(k, v.as_i64().unwrap());
    }
    let mut kv: BTreeMap<String, String> = obj(&init["kv"])
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
        .collect();
    let params = Params::from_map(obj(&init["params"]));

    for (i, step) in c.raw["steps"].as_array().unwrap().iter().enumerate() {
        let ctx = format!("轨迹 {} 第 {i} 步 {}", c.raw["name"], step["op"]);
        let op = step["op"].as_str().unwrap();
        *counts.entry(op.to_string()).or_default() += 1;
        let now = step["now"].as_f64().unwrap();
        let tok = step.get("token").and_then(Value::as_str).unwrap_or("");
        let want_err = step
            .get("err")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str);
        let mut writes: Vec<(String, String)> = Vec::new();
        let mut draws_used = 0usize;

        // 结果：Ok(Python 的返回值) 或 Err(异常类型)
        let out: Result<Value, String> = match op {
            "record_starts" => {
                for e in step["entries"].as_array().unwrap() {
                    g.record_start(tok, e[0].as_f64().unwrap(), e[1].as_i64().unwrap());
                }
                Ok(Value::Null)
            }
            "hourly_cap" => Ok(json!(g.hourly_cap(now))),
            "hourly_cap_ceiling" => Ok(json!(g.hourly_cap_ceiling(now))),
            "in_quiet" => Ok(json!(g.in_quiet(now))),
            "burst_cap" => Ok(json!(g.burst_cap(now))),
            "hour_count" => Ok(json!(g.hour_count(tok, now))),
            "count_10m" => Ok(json!(g.count_10m(tok, now))),
            "burst_blocked" => Ok(json!(g.burst_blocked(tok, now))),
            "minutes_until_free" => Ok(json!(g.minutes_until_free(tok, now))),
            "minutes_until_free_10m" => Ok(json!(g.minutes_until_free_10m(tok, now))),
            "paced_allowance" => Ok(f(g.paced_allowance(now))),
            "minutes_until_paced" => Ok(json!(
                g.minutes_until_paced(step["used"].as_f64().unwrap(), now)
            )),
            "token_block_reason" => Ok(pair(g.token_block_reason(
                tok,
                step["used"].as_f64().unwrap(),
                now,
            ))),
            // 已删除（#61），没有副作用：不比返回值
            "paced_headroom" => Ok(step["out"].clone()),
            "on_upstream_429" => (|| -> Result<Value, String> {
                let prev = parse_setting_float(kv.get("guard_429_cap_at").map(String::as_str))
                    .map_err(|e| err_type(&e).to_string())?;
                let r = g
                    .on_upstream_429(now, prev, &params)
                    .map_err(|e| err_type(&e).to_string())?;
                writes = r.writes;
                Ok(r.change.map_or(Value::Null, |(a, b)| json!([a, b])))
            })(),
            "adapt_daily" => (|| -> Result<Value, String> {
                let rd = |k: &str| {
                    parse_setting_float(kv.get(k).map(String::as_str))
                        .map_err(|e| err_type(&e).to_string())
                };
                let last_429 = rd("guard_last_upstream_429")?;
                let last_step = rd("guard_hourly_adapted_at")?;
                let adapted = match GuardCore::adapt_daily_begin(now, last_429, last_step) {
                    AdaptPlan::Done(a) => a,
                    AdaptPlan::NeedUsage => {
                        let seen = rd("guard_429_cap_at")?;
                        let first = step["first"].as_f64();
                        g.adapt_daily_finish(
                            now,
                            step["hit"].as_bool().unwrap(),
                            first,
                            seen,
                            &params,
                        )
                        .map_err(|e| err_type(&e).to_string())?
                    }
                };
                writes = adapted.writes;
                Ok(adapted.change.map_or(Value::Null, |(a, b)| json!([a, b])))
            })(),
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
                assert!(step.get("state").is_none());
                continue;
            }
            "trip_breaker" => g
                .trip_breaker(
                    step["seconds"].as_f64().unwrap(),
                    step["reason"].as_str().unwrap(),
                    now,
                )
                .map(|()| Value::Null)
                .map_err(|e| err_type(&e).to_string()),
            "jitter" => {
                let draws: Vec<f64> = step.get("draws").map(floats).unwrap_or_default();
                let mut rand = || {
                    let r = *draws
                        .get(draws_used)
                        .unwrap_or_else(|| panic!("{ctx}：Rust 多取了随机数"));
                    draws_used += 1;
                    r
                };
                let out = g.jitter(&mut rand);
                assert_eq!(draws_used, draws.len(), "{ctx}：随机数消耗次数不同");
                Ok(f(out))
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
            "waiting_keys" => Ok(json!(g.waiting_keys())),
            // 已删除（#61）：只重放它的副作用——别人在排队就直接返回；否则记一次每小时上限，
            // 上限不为 0 时数每个账号的小时用量（弹出 3 小时前的时间戳）。不比返回值
            "site_idle" => {
                let key = step["key"].as_i64().unwrap();
                let busy = g.image_inflight.iter().any(|(k, n)| *k != key && *n > 0);
                if !busy {
                    let cap = g.hourly_cap(now);
                    if cap != 0 {
                        for t in step["tokens"].as_array().unwrap() {
                            g.hour_count(t.as_str().unwrap(), now);
                        }
                    }
                }
                Ok(step["out"].clone())
            }
            "set_value" => {
                g.values.insert(
                    step["name"].as_str().unwrap().to_string(),
                    step["value"].as_i64().unwrap(),
                );
                assert_eq_py(c, &state_json(&g), &state_without_removed(&step["state"]));
                continue;
            }
            // 旧版因保底字段不合法而整个失败、什么都没改：3.0 同样不改（#61）
            "save" if failed_on_removed(&step["err"]) => Err("ValueError".to_string()),
            "save" => match g.save(&obj(&step["body"])) {
                Ok(saved) => {
                    writes = saved.writes;
                    Ok(Value::Object(g.values_map()))
                }
                Err(e) => {
                    let GuardError::Value(m) = &e else {
                        unreachable!()
                    };
                    assert_eq!(m, step["err"]["msg"].as_str().unwrap(), "{ctx}：消息不同");
                    Err(err_type(&e).to_string())
                }
            },
            "describe" => {
                let got = g.describe(now);
                let want = describe_without_removed(&step["out"]);
                assert_eq_py_ordered(c, &got, &want);
                Ok(want)
            }
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
            other => panic!("{ctx}：未知操作 {other}"),
        };

        let want_out = match op {
            "save" => without_removed(&step["out"]),
            "describe" => describe_without_removed(&step["out"]),
            _ => step["out"].clone(),
        };
        match (out, want_err) {
            (Ok(v), None) => assert_eq_py(c, &v, &want_out),
            (Err(ty), Some(want)) => assert_eq!(ty, want, "{ctx}"),
            (Ok(v), Some(want)) => panic!("{ctx}：Python 抛 {want}，Rust 返回 {v}"),
            (Err(ty), None) => panic!("{ctx}：Python 成功，Rust 报 {ty}"),
        }
        // 写库内容（含顺序），并同步到假库
        assert_eq_py(
            c,
            &kv_writes(&writes),
            &writes_without_removed(&step["writes"]),
        );
        for (k, v) in writes {
            kv.insert(k, v);
        }
        assert_eq_py(c, &state_json(&g), &state_without_removed(&step["state"]));
    }
}

#[test]
fn traces_golden() {
    let cases = load(M, "guard/traces");
    assert!(cases.len() >= 39);
    let mut counts = BTreeMap::new();
    for c in &cases {
        replay(c, &mut counts);
    }
    // 夹具要真的覆盖到各个分支
    for (op, min) in [
        ("record_starts", 800),
        ("token_block_reason", 1000),
        ("on_upstream_429", 200),
        ("adapt_daily", 250),
        ("admit_image", 300),
        ("release_image", 180),
        ("jitter", 150),
        ("describe", 100),
        ("save", 120),
        ("seed_hour", 30),
        ("site_idle", 200),
        ("burst_blocked", 300),
        ("trip_breaker", 80),
        ("paced_headroom", 100),
    ] {
        assert!(
            counts.get(op).copied().unwrap_or(0) >= min,
            "{op} 只有 {:?} 次",
            counts.get(op)
        );
    }
}
