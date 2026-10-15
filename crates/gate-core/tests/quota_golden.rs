//! 额度算法对拍：旧 Python 生成的夹具（真实输出，生成脚本见标签 py-final）。

use gate_core::clock::Clock;
use gate_core::quota::{self, *};
use gate_golden::{Case, as_f64, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Map, Value, json};

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

fn params(v: &Value) -> QuotaParams {
    serde_json::from_value(v.clone()).unwrap()
}

fn clock(rh: i64) -> Clock {
    Clock::new("Asia/Shanghai", rh)
}

#[test]
fn pure_golden() {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for c in load(M, "quota/pure") {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *seen.entry(name.clone()).or_default() += 1;
        assert!(c.err().is_none(), "第 {} 行：纯函数不应抛异常", c.line);
        let got: Value = match name.as_str() {
            "water_fill" => {
                json!(water_fill(&ints(c.arg(0)), as_f64(c.arg(1)).unwrap()))
            }
            "simple" => {
                let prev_a = c.arg(0).get("a").and_then(Value::as_i64);
                let cfg: QuotaCfg =
                    serde_json::from_value(c.kwarg("cfg").unwrap().clone()).unwrap();
                let usage = ints(c.kwarg("usage").unwrap());
                let budget = c.kwarg("budget").unwrap().as_i64().unwrap();
                let (n, why) = simple(prev_a, &usage, budget, &cfg);
                json!([{"a": n, "b": n}, why])
            }
            "choose_rate" => {
                let (rate, info) = choose_rate(opt_f(c.arg(0)), opt_f(c.arg(1)));
                json!([f(rate), Value::Object(info)])
            }
            "free_paced_cap" => f(free_paced_cap(c.arg(0).as_i64().unwrap())),
            "v5_factor" => f(v5_factor(opt_f(c.arg(0)), &params(c.arg(1)))),
            "v5_plan" => {
                let plan = v5_plan(
                    opt_f(c.arg(0)),
                    opt_f(c.arg(1)),
                    c.arg(2).as_i64().unwrap(),
                    c.arg(3).as_i64().unwrap(),
                    c.arg(4).as_i64().unwrap(),
                    &params(c.arg(5)),
                )
                .unwrap();
                Value::Object(plan.to_map())
            }
            "quota_notice" => {
                let kw = |k: &str| c.kwarg(k).unwrap();
                json!(
                    quota_notice(
                        c.arg(0).as_i64().unwrap(),
                        c.arg(1),
                        c.arg(2).as_i64().unwrap(),
                        kw("today").as_str().unwrap(),
                        opt_f(kw("pct")),
                        Some(kw("review")).filter(|v| !v.is_null()),
                        kw("reset_hour").as_i64().unwrap(),
                        kw("anlas").as_i64().unwrap(),
                    )
                    .unwrap()
                )
            }
            other => panic!("未知函数 {other}"),
        };
        assert_eq_py_ordered(&c, &got, c.out());
    }
    for k in [
        "water_fill",
        "simple",
        "choose_rate",
        "free_paced_cap",
        "v5_factor",
        "v5_plan",
        "quota_notice",
    ] {
        assert!(seen.get(k).copied().unwrap_or(0) > 100, "{k} 用例太少");
    }
}

fn yesterday_inputs(v: &Value) -> YesterdayInputs {
    serde_json::from_value(v.clone()).unwrap()
}

/// 旧版查询日志里取某个 tag 的全部参数。
fn queries<'a>(c: &'a Case, tag: &str) -> Vec<&'a Vec<Value>> {
    c.get("queries")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter(|q| q[0] == tag)
                .map(|q| q[1].as_array().unwrap())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn sqlmath_golden() {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for c in load(M, "quota/sqlmath") {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *seen.entry(name.clone()).or_default() += 1;
        match name.as_str() {
            "day_coverage" => {
                let ck = clock(c.arg(2).as_i64().unwrap());
                let got = day_coverage(&ck, c.arg(1).as_str().unwrap(), opt_f(c.arg(0))).unwrap();
                assert_eq_py(&c, &f(got), c.out());
            }
            "_yesterday" => {
                let day = c.arg(1).as_str().unwrap();
                let ck = clock(c.arg(2).as_i64().unwrap());
                let stats = yesterday_stats(&yesterday_inputs(c.arg(0)));
                assert_eq_py_ordered(&c, &Value::Object(stats.to_map()), c.out());
                // 服务层该传的查询参数
                let (start, end) = day_window(&ck, day).unwrap();
                for tag in ["counters", "upstream"] {
                    assert_eq!(
                        queries(&c, tag),
                        vec![&vec![json!(day)]],
                        "第 {} 行 {tag}",
                        c.line
                    );
                }
                for (tag, pat) in [
                    ("hourly_hours", "%出图量已达上限%"),
                    ("base_blocks", "%保底%其他人在排队%"),
                    ("paced_blocks", "paced"),
                ] {
                    let want = vec![f(start), f(end), json!(pat)];
                    assert_eq!(queries(&c, tag), vec![&want], "第 {} 行 {tag}", c.line);
                }
            }
            "_v5_used_equiv" => {
                let rows: Vec<V5Row> = serde_json::from_value(c.arg(0).clone()).unwrap();
                assert_eq_py(&c, &f(v5_used_equiv(&rows)), c.out());
                let ck = clock(c.arg(2).as_i64().unwrap());
                let (start, end) = day_window(&ck, c.arg(1).as_str().unwrap()).unwrap();
                assert_eq!(queries(&c, "v5_rows"), vec![&vec![f(start), f(end)]]);
            }
            "quiet_recovery" => {
                let now = as_f64(c.arg(0)).unwrap();
                let q = QuietInputs {
                    snaps: serde_json::from_value(c.arg(1).clone()).unwrap(),
                    counts: ints(c.arg(2)),
                };
                let got = quiet_recovery(&q).map_or(Value::Null, |x| x.to_value());
                assert_eq_py_ordered(&c, &got, c.out());
                assert_eq!(
                    queries(&c, "snaps"),
                    vec![&vec![f(quiet_snaps_since(now))]],
                    "第 {} 行",
                    c.line
                );
                let want: Vec<Vec<Value>> = quiet_count_pairs(&q.snaps)
                    .into_iter()
                    .map(|i| vec![f(q.snaps[i].ts), f(q.snaps[i + 1].ts)])
                    .collect();
                let got_q: Vec<Vec<Value>> =
                    queries(&c, "v5_between").into_iter().cloned().collect();
                assert_eq!(got_q, want, "第 {} 行", c.line);
            }
            "_cfg" => {
                let settings: std::collections::HashMap<String, String> =
                    serde_json::from_value(c.arg(0).clone()).unwrap();
                match (quota_cfg(&settings), c.err()) {
                    (Ok(cfg), None) => {
                        assert_eq_py_ordered(&c, &serde_json::to_value(&cfg).unwrap(), c.out())
                    }
                    (Err(e), Some(t)) => assert_eq!(e.py_type(), t, "第 {} 行", c.line),
                    (got, want) => panic!("第 {} 行：{got:?} 与 {want:?} 不符", c.line),
                }
            }
            "_allowance" => {
                let accounts: Vec<AccountAllowance> =
                    serde_json::from_value(c.arg(0).clone()).unwrap();
                let (pct, rate) = allowance_summary(&accounts);
                assert_eq_py(&c, &json!([fv(pct), fv(rate)]), c.out());
            }
            "_active_v5_days" => {
                let days = active_v5_days(
                    &clock(7),
                    as_f64(c.arg(0)).unwrap(),
                    c.arg(1).as_u64().unwrap() as usize,
                );
                assert_eq_py(&c, &json!(days), c.out());
            }
            other => panic!("未知函数 {other}"),
        }
    }
    for k in [
        "day_coverage",
        "_cfg",
        "_yesterday",
        "_v5_used_equiv",
        "quiet_recovery",
        "_allowance",
        "_active_v5_days",
    ] {
        assert!(seen.get(k).copied().unwrap_or(0) > 100, "{k} 用例太少");
    }
}

#[test]
fn run_golden() {
    let mut errs = 0;
    let mut adjusted = 0;
    let mut applied = 0;
    for c in load(M, "quota/run") {
        let rh = c.raw["reset_hour"].as_i64().unwrap();
        let ck = clock(rh);
        let inp: QuotaInputs = serde_json::from_value(c.raw["inputs"].clone())
            .unwrap_or_else(|e| panic!("第 {} 行输入解析失败: {e}", c.line));
        let out = quota::decide(&inp, &ck);

        // 异常类型与异常之前已发生的写入都要一致
        let want_err = c.raw["err"].as_str();
        assert_eq!(out.error.as_deref(), want_err, "第 {} 行异常类型", c.line);
        let effects = serde_json::to_value(&out.effects).unwrap();
        assert_eq_py_ordered(&c, &effects, &c.raw["effects"]);
        match (&out.result, want_err) {
            (Some(got), None) => assert_eq_py_ordered(&c, got, c.out()),
            (None, Some(_)) => {}
            other => panic!("第 {} 行：结果与异常不匹配 {other:?}", c.line),
        }
        if want_err.is_some() {
            errs += 1;
        }
        if let Some(r) = &out.result {
            if !r["review"].is_null() {
                adjusted += 1;
            }
            if r["changed_keys"].as_i64().unwrap_or(0) > 0 {
                applied += 1;
            }
        }

        // ---- 服务层该传的查询参数（文档里的清单）----
        let now = inp.now;
        let today = ck.day(now);
        let yday = ck.day(now - 86400.0);
        let did_daily = !queries(&c, "counters").is_empty();
        if did_daily {
            assert!(
                daily_adjust_due(inp.settings.get(quota::DAY_KEY).map(String::as_str), &today),
                "第 {} 行：旧版做了每日微调但 daily_adjust_due 说不需要",
                c.line
            );
            assert_eq!(queries(&c, "counters"), vec![&vec![json!(yday)]]);
            let (start, end) = day_window(&ck, &yday).unwrap();
            assert_eq!(
                queries(&c, "hourly_hours"),
                vec![&vec![f(start), f(end), json!("%出图量已达上限%")]]
            );
        } else if out.error.is_none() && out.result.as_ref().is_some_and(|r| r["enabled"] == true) {
            assert!(
                !daily_adjust_due(inp.settings.get(quota::DAY_KEY).map(String::as_str), &today),
                "第 {} 行：旧版没做每日微调但 daily_adjust_due 说需要",
                c.line
            );
        }
        for q in queries(&c, "active_v5") {
            let want: Vec<Value> = active_v5_days(&ck, now, 3)
                .into_iter()
                .map(|d| json!(d))
                .collect();
            assert_eq!(*q, want, "第 {} 行 active_v5 日期", c.line);
        }
        for q in queries(&c, "snaps") {
            assert_eq!(*q, vec![f(quiet_snaps_since(now))]);
        }
        let pairs: Vec<Vec<Value>> = quiet_count_pairs(&inp.quiet.snaps)
            .into_iter()
            .map(|i| vec![f(inp.quiet.snaps[i].ts), f(inp.quiet.snaps[i + 1].ts)])
            .collect();
        let got_pairs: Vec<Vec<Value>> = queries(&c, "v5_between").into_iter().cloned().collect();
        if !got_pairs.is_empty()
            || out.error.is_none() && out.result.as_ref().is_some_and(|r| r["enabled"] == true)
        {
            assert_eq!(got_pairs, pairs, "第 {} 行 v5_between", c.line);
        }
        if let Some(r) = &out.result
            && r["enabled"] == true
        {
            let want = vec![r["ceiling"].clone(), r["v5"]["each"].clone()];
            assert_eq!(
                queries(&c, "count_managed"),
                vec![&want],
                "第 {} 行",
                c.line
            );
        }
        let _: Option<&Map<String, Value>> = None;
    }
    assert!(errs > 50, "异常路径用例太少: {errs}");
    assert!(adjusted > 200, "每日微调用例太少: {adjusted}");
    assert!(applied > 100, "下发用例太少: {applied}");
}
