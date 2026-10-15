//! 额度算法对拍：`tools/parity/golden/quota.py` 生成的夹具（旧 Python 的真实输出）。
//!
//! 夹具冻结不改。3.0 有意改掉的行为在比较前从期望一侧改写（见 [`expect`]）：
//! - docs/v3-deviations.md #61：「保底」一层删除——输入里的 `guard.base_daily_images`、`base_blocks` 去掉；
//!   `_cfg` 不再有 `quota_base` / `quota_base_min` / `quota_base_step`；写入里没有 `quota_base_now`、
//!   `guard_base_daily_images`；复盘的 `from` / `to` 只记 `[A]`、没有 `base_blocks`；结果里没有 `base`；
//!   操作日志不再写「保底」；每小时上限被拦的小时数按原因码数（查询参数只比时间窗口）。

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

/// 期望一侧的改写（3.0 的有意差异，见文件头）。
mod expect {
    use gate_core::jsondump;
    use serde_json::{Map, Value};

    /// `_cfg` 里 3.0 删掉的键（#61）。
    pub const REMOVED_CFG: [&str; 3] = ["quota_base", "quota_base_min", "quota_base_step"];

    /// 旧版会因 3.0 不再读的保底设置抛异常（在任何写入之前）：`_cfg` 里的 `quota_base*` 是 `inf`，
    /// 或 `quota_base_now` 不是数字（#61）。
    pub fn removed_setting_error(settings: &std::collections::HashMap<String, String>) -> bool {
        let parse = |k: &str| settings.get(k).map(|s| gate_core::pyc::parse_float(s));
        REMOVED_CFG
            .iter()
            .any(|k| parse(k).flatten().is_some_and(f64::is_infinite))
            || parse("quota_base_now").is_some_and(|x| x.is_none_or(|f| !f.is_finite()))
    }

    /// 输入：去掉 3.0 删掉的字段（#61）。
    pub fn inputs(raw: &Value) -> Value {
        let mut v = raw.clone();
        if let Some(g) = v.get_mut("guard").and_then(Value::as_object_mut) {
            g.shift_remove("base_daily_images");
        }
        if let Some(y) = v
            .get_mut("daily")
            .and_then(|d| d.get_mut("yesterday"))
            .and_then(Value::as_object_mut)
        {
            y.shift_remove("base_blocks");
        }
        v
    }

    /// `_yesterday` 的结果：没有 `base_blocks`（#61）。
    pub fn yesterday(v: &Value) -> Value {
        let mut m = v.as_object().cloned().unwrap_or_default();
        m.shift_remove("base_blocks");
        Value::Object(m)
    }

    /// 一条复盘：`from` / `to` 只留每人上限 A，去掉 `base_blocks`（#61）。
    pub fn review(r: &mut Map<String, Value>) {
        for k in ["from", "to"] {
            if let Some(Value::Array(pair)) = r.get_mut(k) {
                pair.truncate(1);
            }
        }
        r.shift_remove("base_blocks");
    }

    /// `run` 的返回值（也是 `quota_algo_last` 的内容）。
    pub fn result(v: &Value) -> Value {
        let mut v = v.clone();
        if let Some(m) = v.as_object_mut() {
            m.shift_remove("base");
            if let Some(Value::Object(r)) = m.get_mut("review") {
                review(r);
            }
        }
        v
    }

    fn reparse(text: &str, f: impl FnOnce(&mut Value)) -> String {
        let mut v = jsondump::loads(text).expect("夹具里写进库的 JSON 合法");
        f(&mut v);
        jsondump::dumps(&v)
    }

    /// 复盘历史：最后一条是这一轮写的复盘，前面的是输入里原有的（不动）。
    fn history_text(text: &str) -> String {
        reparse(text, |v| {
            if let Some(Value::Object(r)) = v.as_array_mut().and_then(|a| a.last_mut()) {
                review(r);
            }
        })
    }

    fn setting_value(key: &str, value: &str) -> String {
        match key {
            "quota_algo_last" => reparse(value, |v| *v = result(v)),
            "quota_algo_history" => history_text(value),
            _ => value.to_string(),
        }
    }

    /// 这一轮的复盘是否只因为保底变了才算「调整」（`from` / `to` 只有第二项不同）：
    /// 3.0 没有保底，首页文案不再写「算法调整」那一句（#61）。
    fn only_base_changed(history_text: &str) -> bool {
        let v = jsondump::loads(history_text).unwrap();
        let Some(r) = v.as_array().and_then(|a| a.last()) else {
            return false;
        };
        let (from, to) = (&r["from"], &r["to"]);
        from != to && from.get(0) == to.get(0)
    }

    /// 期望的写入序列。
    pub fn effects(effects: &Value) -> Value {
        let removed_bulk_keys = ["quota_base_now"];
        let mut drop_review_note = false;
        let mut out = Vec::new();
        for e in effects.as_array().unwrap() {
            let mut e = e.clone();
            match e["op"].as_str().unwrap() {
                // 不再把保底同步进 Guard（#61）
                "guard_save_base" => continue,
                "set" => {
                    let key = e["key"].as_str().unwrap().to_string();
                    let raw = e["value"].as_str().unwrap().to_string();
                    let value = if key == "algo_notice" && drop_review_note {
                        let i = raw.find("今日额度：").expect("首页文案");
                        raw[i..].to_string()
                    } else {
                        setting_value(&key, &raw)
                    };
                    e["value"] = Value::String(value);
                }
                "bulk" => {
                    for p in e["pairs"].as_array().unwrap() {
                        if p[0] == "quota_algo_history" {
                            drop_review_note = only_base_changed(p[1].as_str().unwrap());
                        }
                    }
                    let pairs: Vec<Value> = e["pairs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|p| !removed_bulk_keys.contains(&p[0].as_str().unwrap()))
                        .map(|p| {
                            let k = p[0].as_str().unwrap();
                            serde_json::json!([k, setting_value(k, p[1].as_str().unwrap())])
                        })
                        .collect();
                    e["pairs"] = Value::Array(pairs);
                }
                "log" => match e["action"].as_str().unwrap() {
                    // 「上限 a→a2 · 保底 b→b2」→「上限 a→a2」；只有保底变了的那条不再记
                    "动态额度调整" => {
                        let target = e["target"].as_str().unwrap();
                        let upper = target.split(" · 保底 ").next().unwrap().to_string();
                        let (from, to) = upper.trim_start_matches("上限 ").split_once('→').unwrap();
                        if from == to {
                            continue;
                        }
                        e["target"] = Value::String(upper);
                    }
                    // 「V4.5 上限 a（保底 b）· V5 …」→「V4.5 上限 a · V5 …」
                    "动态额度应用" => {
                        let detail = e["detail"].as_str().unwrap().to_string();
                        if let Some((head, rest)) = detail.split_once("（保底 ")
                            && let Some((_, tail)) = rest.split_once("）·")
                        {
                            e["detail"] = Value::String(format!("{head} ·{tail}"));
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            out.push(e);
        }
        Value::Array(out)
    }

    /// 实际的写入序列：JSON 设置按同样的格式重新序列化（与期望一侧同一套处理，只是不改内容）。
    pub fn actual_effects(effects: &Value) -> Value {
        let mut out = effects.clone();
        for e in out.as_array_mut().unwrap() {
            match e["op"].as_str().unwrap() {
                "set" => {
                    let key = e["key"].as_str().unwrap().to_string();
                    if matches!(key.as_str(), "quota_algo_last" | "quota_algo_history") {
                        let v = jsondump::loads(e["value"].as_str().unwrap()).unwrap();
                        e["value"] = Value::String(jsondump::dumps(&v));
                    }
                }
                "bulk" => {
                    for p in e["pairs"].as_array_mut().unwrap() {
                        if p[0] == "quota_algo_history" {
                            let v = jsondump::loads(p[1].as_str().unwrap()).unwrap();
                            p[1] = Value::String(jsondump::dumps(&v));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
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
                let mut input = c.arg(0).clone();
                input.as_object_mut().unwrap().shift_remove("base_blocks");
                let stats = yesterday_stats(&yesterday_inputs(&input));
                assert_eq_py_ordered(
                    &c,
                    &Value::Object(stats.to_map()),
                    &expect::yesterday(c.out()),
                );
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
                // 每小时上限被拦的小时数 3.0 按原因码 `hourly_cap` 数（旧版按文案 LIKE，#61）：只比时间窗口；
                // `base_blocks` 随保底删除
                let hourly = queries(&c, "hourly_hours");
                assert_eq!(hourly.len(), 1, "第 {} 行", c.line);
                assert_eq!(hourly[0][..2], [f(start), f(end)], "第 {} 行", c.line);
                let want = vec![f(start), f(end), json!("paced")];
                assert_eq!(
                    queries(&c, "paced_blocks"),
                    vec![&want],
                    "第 {} 行 paced_blocks",
                    c.line
                );
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
                // 3.0 删掉的键（#61）：旧版只因它们是 inf 而抛的 OverflowError 不再出现
                let removed_inf = expect::REMOVED_CFG.iter().any(|k| {
                    settings
                        .get(*k)
                        .and_then(|s| gate_core::pyc::parse_float(s))
                        .is_some_and(f64::is_infinite)
                });
                match (quota_cfg(&settings), c.err()) {
                    (Ok(cfg), None) => {
                        let mut want = c.out().as_object().cloned().unwrap();
                        for k in expect::REMOVED_CFG {
                            want.shift_remove(k);
                        }
                        assert_eq_py_ordered(
                            &c,
                            &serde_json::to_value(&cfg).unwrap(),
                            &Value::Object(want),
                        )
                    }
                    (Ok(_), Some("OverflowError")) if removed_inf => {}
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
    let mut no_base_error = 0;
    for c in load(M, "quota/run") {
        let rh = c.raw["reset_hour"].as_i64().unwrap();
        let ck = clock(rh);
        let inp: QuotaInputs = serde_json::from_value(expect::inputs(&c.raw["inputs"]))
            .unwrap_or_else(|e| panic!("第 {} 行输入解析失败: {e}", c.line));
        let out = quota::decide(&inp, &ck);

        // 异常类型与异常之前已发生的写入都要一致
        let want_err = c.raw["err"].as_str();
        let effects = expect::actual_effects(&serde_json::to_value(&out.effects).unwrap());
        let want_effects = expect::effects(&c.raw["effects"]);
        // 旧版在第一笔写入之前就因保底设置抛了异常：3.0 照常往下算（可能在后面别处才报错），无从对拍
        if want_err.is_some()
            && out.error.as_deref() != want_err
            && want_effects.as_array().unwrap().is_empty()
            && expect::removed_setting_error(&inp.settings)
        {
            no_base_error += 1;
            continue;
        }
        if want_err == Some("ValueError") && out.error.is_none() {
            // 旧版把保底同步进 Guard 时越界抛的 ValueError（#61）：3.0 不再写保底，照常往下算。
            // 异常之前的写入必须一致，下一步就是写全站 V5 / 新 Key 默认值
            let n = want_effects.as_array().unwrap().len();
            let got = effects.as_array().unwrap();
            assert!(got.len() > n, "第 {} 行", c.line);
            assert_eq_py_ordered(&c, &Value::Array(got[..n].to_vec()), &want_effects);
            assert_eq!(got[n]["pairs"][0][0], "global_daily_v5", "第 {} 行", c.line);
            no_base_error += 1;
            continue;
        }
        assert_eq!(out.error.as_deref(), want_err, "第 {} 行异常类型", c.line);
        assert_eq_py_ordered(&c, &effects, &want_effects);
        match (&out.result, want_err) {
            (Some(got), None) => assert_eq_py_ordered(&c, got, &expect::result(c.out())),
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
            // 3.0 按原因码数（#61）：只比时间窗口
            let hourly = queries(&c, "hourly_hours");
            assert_eq!(hourly.len(), 1);
            assert_eq!(hourly[0][..2], [f(start), f(end)]);
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
    assert!(
        errs > 50,
        "异常路径用例太少: {errs}（另有 {no_base_error} 个保底越界的用例）"
    );
    assert!(adjusted > 200, "每日微调用例太少: {adjusted}");
    assert!(applied > 100, "下发用例太少: {applied}");
}
