//! 额度算法的快照：纯函数（`quota/pure`）、取数之后的数学（`quota/sqlmath`）、整轮 `decide`（`quota/run`）。
//!
//! 额度只剩每人每日上限 A（V4.5）和每人 V5（全站池 ÷ max(10, 前 3 个完整服务日用过 V5 的人数)，当天固定）。
//! 规则本身另有手写的规则测试，这里记的是当前输出。

mod common;

use common::Tally;
use gate_core::clock::Clock;
use gate_core::quota::{self, *};
use gate_snapshot::snapshot::check;
use gate_snapshot::{Case, as_f64, f};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn opt_f(v: &Value) -> Option<f64> {
    if v.is_null() { None } else { as_f64(v) }
}

fn fv(x: Option<f64>) -> Value {
    x.map_or(Value::Null, f)
}

fn ints(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect()
}

fn clock(rh: i64) -> Clock {
    Clock::new("Asia/Shanghai", rh)
}

/// 异常的记法：`{"err": 类型, "msg": 说明}`。
fn err_json(e: &DecideError) -> Value {
    json!({"err": e.py_type(), "msg": e.to_string()})
}

/// 服务日 `day` 的查询窗口（服务层拿它查日志）。
fn window_json(ck: &Clock, day: &str) -> Value {
    match day_window(ck, day) {
        Ok((start, end)) => json!([f(start), f(end)]),
        Err(e) => err_json(&e),
    }
}

#[test]
fn pure_snapshot() {
    let mut tally = Tally::default();
    check(M, "quota/pure", |c| {
        let name = c.get("fn")?.as_str()?;
        tally.add(name);
        Some(match name {
            "water_fill" => json!(water_fill(&ints(c.arg(0)), as_f64(c.arg(1))?)),
            "simple" => {
                let prev_a = c.arg(0).get("a").and_then(Value::as_i64);
                let cfg: QuotaCfg = serde_json::from_value(c.kwarg("cfg")?.clone()).ok()?;
                let usage = ints(c.kwarg("usage")?);
                let budget = c.kwarg("budget")?.as_i64()?;
                let (n, why) = simple(prev_a, &usage, budget, &cfg);
                json!([n, why])
            }
            "choose_rate" => {
                let (rate, info) = choose_rate(opt_f(c.arg(0)), opt_f(c.arg(1)));
                json!([f(rate), Value::Object(info)])
            }
            "v5_plan" => {
                json!(v5_plan(opt_f(c.arg(0)), opt_f(c.arg(1)), c.arg(2).as_i64()?).to_map())
            }
            "quota_notice" => {
                let kw = |k: &str| c.kwarg(k).unwrap();
                match quota_notice(
                    c.arg(0).as_i64()?,
                    c.arg(1),
                    c.arg(2).as_i64()?,
                    kw("today").as_str()?,
                    Some(kw("review")).filter(|v| !v.is_null()),
                    kw("reset_hour").as_i64()?,
                    kw("anlas").as_i64()?,
                ) {
                    Ok(text) => json!(text),
                    Err(e) => err_json(&e),
                }
            }
            other => panic!("未知函数 {other}"),
        })
    });
    tally.assert_at_least(&[
        ("water_fill", 20),
        ("simple", 20),
        ("choose_rate", 20),
        ("v5_plan", 20),
        ("quota_notice", 20),
    ]);
}

fn yesterday_inputs(v: &Value) -> YesterdayInputs {
    serde_json::from_value(v.clone()).unwrap()
}

#[test]
fn sqlmath_snapshot() {
    let mut tally = Tally::default();
    check(M, "quota/sqlmath", |c| {
        let name = c.get("fn")?.as_str()?;
        tally.add(name);
        Some(match name {
            "day_coverage" => {
                let ck = clock(c.arg(2).as_i64()?);
                match day_coverage(&ck, c.arg(1).as_str()?, opt_f(c.arg(0))) {
                    Ok(x) => f(x),
                    Err(e) => err_json(&e),
                }
            }
            // 昨天的成员出图总数、顶格人数、被每小时上限拦的小时数……，以及服务层查日志用的时间窗口
            "_yesterday" => {
                let ck = clock(c.arg(2).as_i64()?);
                let day = c.arg(1).as_str()?;
                let stats = yesterday_stats(&yesterday_inputs(c.arg(0)));
                json!({"stats": stats.to_map(), "window": window_json(&ck, day)})
            }
            "_v5_used_equiv" => {
                let rows: Vec<V5Row> = serde_json::from_value(c.arg(0).clone()).ok()?;
                let ck = clock(c.arg(2).as_i64()?);
                json!({
                    "used": f(v5_used_equiv(&rows)),
                    "window": window_json(&ck, c.arg(1).as_str()?),
                })
            }
            // 安静时段测速，以及服务层该查的参数：快照下界、要数 V5 条数的相邻快照对
            "quiet_recovery" => {
                let now = as_f64(c.arg(0))?;
                let q = QuietInputs {
                    snaps: serde_json::from_value(c.arg(1).clone()).ok()?,
                    counts: ints(c.arg(2)),
                };
                let pairs: Vec<Value> = quiet_count_pairs(&q.snaps)
                    .into_iter()
                    .map(|i| json!([f(q.snaps[i].ts), f(q.snaps[i + 1].ts)]))
                    .collect();
                json!({
                    "result": quiet_recovery(&q).map_or(Value::Null, |x| x.to_value()),
                    "snaps_since": f(quiet_snaps_since(now)),
                    "pairs": pairs,
                })
            }
            "_cfg" => {
                let settings: std::collections::HashMap<String, String> =
                    serde_json::from_value(c.arg(0).clone()).ok()?;
                match quota_cfg(&settings) {
                    Ok(cfg) => serde_json::to_value(&cfg).unwrap(),
                    Err(e) => json!({"err": e.py_type()}),
                }
            }
            "_allowance" => {
                let accounts: Vec<AccountAllowance> =
                    serde_json::from_value(c.arg(0).clone()).ok()?;
                let (pct, rate) = allowance_summary(&accounts);
                json!([fv(pct), fv(rate)])
            }
            // 今天之前的 n 个完整服务日，从旧到新
            "_active_v5_days" => {
                json!(active_v5_days(
                    &clock(7),
                    as_f64(c.arg(0))?,
                    c.arg(1).as_u64()? as usize
                ))
            }
            other => panic!("未知函数 {other}"),
        })
    });
    tally.assert_at_least(&[
        ("day_coverage", 20),
        ("_cfg", 20),
        ("_yesterday", 20),
        ("_v5_used_equiv", 20),
        ("quiet_recovery", 20),
        ("_allowance", 20),
        ("_active_v5_days", 20),
    ]);
}

/// 一轮 `decide`：输入是 `run` 读到的一切（设置、计数、快照、成员 Key……）和重置小时；
/// 输出是按发生顺序的写入、返回值（同时是写进 `quota_algo_last` 的内容）、出错时的异常类名，
/// 以及服务层决定要不要做「每日微调」用的 `daily_adjust_due`。
fn decide_case(c: &Case, tally: &mut Tally) -> Option<Value> {
    let rh = c.get("reset_hour")?.as_i64()?;
    let ck = clock(rh);
    let inp: QuotaInputs = serde_json::from_value(c.get("inputs")?.clone())
        .unwrap_or_else(|e| panic!("第 {} 行输入解析失败: {e}", c.line));
    let out = decide(&inp, &ck);
    let today = ck.day(inp.now);
    tally.add("run");
    tally.add_if(out.error.is_some(), "error");
    if let Some(r) = &out.result {
        tally.add_if(r["enabled"] == true, "enabled");
        tally.add_if(!r["review"].is_null(), "review");
        tally.add_if(r["changed_keys"].as_i64().unwrap_or(0) > 0, "changed_keys");
        tally.add_if(!r["v5"]["oversell"].is_null(), "oversell");
    }
    Some(json!({
        "error": out.error,
        "effects": serde_json::to_value(&out.effects).unwrap(),
        "result": out.result,
        "daily_adjust_due": daily_adjust_due(
            inp.settings.get(quota::DAY_KEY).map(String::as_str),
            &today
        ),
    }))
}

#[test]
fn run_snapshot() {
    let mut tally = Tally::default();
    check(M, "quota/run", |c| decide_case(c, &mut tally));
    // 异常路径、每日微调、下发都要真的出现过
    tally.assert_at_least(&[
        ("run", 30),
        ("error", 5),
        ("enabled", 10),
        ("review", 10),
        ("changed_keys", 5),
    ]);
}
