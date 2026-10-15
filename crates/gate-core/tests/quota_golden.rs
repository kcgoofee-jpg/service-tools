//! 额度算法对拍：`tools/parity/golden/quota.py` 生成的夹具（旧 Python 的真实输出）。
//!
//! 夹具冻结不改。3.0 有意改掉的行为在比较前从期望一侧改写（见 [`expect`]）：
//! - docs/v3-deviations.md #61：「保底」一层删除——输入里的 `guard.base_daily_images`、`base_blocks` 去掉；
//!   `_cfg` 不再有 `quota_base` / `quota_base_min` / `quota_base_step`；写入里没有 `quota_base_now`、
//!   `guard_base_daily_images`；复盘的 `from` / `to` 只记 `[A]`、没有 `base_blocks`；结果里没有 `base`；
//!   操作日志不再写「保底」；每小时上限被拦的小时数按原因码数（查询参数只比时间窗口）。
//! - docs/v3-deviations.md #62：每人注水删除——`free_paced_cap` 不再对拍；首页文案去掉
//!   「每小时自动恢复 4 张，当天最多约 N 张，」。
//! - docs/v3-deviations.md #63：V5 改成「全站池 G = ⌊恢复速度 × 14.2⌋，每人 = max(1, ⌊G ÷ max(10, N)⌋)，
//!   N = 前 3 个完整服务日用过 V5 的人数，当天不变」。旧公式的结果无从对拍：`v5_factor` 跳过；`v5_plan` 只比
//!   恢复速度与剩余，全站池 / 人数 / 每人按新公式核对；`_cfg` 没有 `quota_v5_min` / `quota_v5_max`；
//!   首页文案去掉「当前电池充沛…额度放宽」；`_active_v5_days` 按「今天之前的 3 个服务日」核对。
//!   `run` 里凡是取决于 V5 结果的写入（当天方案、全站 V5 / 新 Key 的 V5 默认值、下发、首页文案的 V5 段、
//!   结果里的 `v5` / `changed_keys`、「动态额度应用」日志）两边都遮住后逐位对拍，再单独按新规则核对 Rust 的输出。

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

fn clock(rh: i64) -> Clock {
    Clock::new("Asia/Shanghai", rh)
}

/// 期望一侧的改写（3.0 的有意差异，见文件头）。
mod expect {
    use gate_core::jsondump;
    use serde_json::{Map, Value};

    /// 首页文案去掉每人注水那一句（#62）：「（每小时自动恢复 4 张，当天最多约 N 张，每日 7 点重置）」
    /// →「（每日 7 点重置）」。
    pub fn notice(text: &str) -> String {
        let Some(i) = text.find("（每小时自动恢复 ") else {
            return text.to_string();
        };
        let rest = &text[i..];
        let j = rest.find("当天最多约 ").expect("注水文案");
        let k = rest[j..].find(" 张，").expect("注水文案") + j + " 张，".len();
        format!("{}（{}", &text[..i], &rest[k..])
    }

    /// `_cfg` 里 3.0 删掉的键（#61 保底、#63 V5 夹取）。
    pub const REMOVED_CFG: [&str; 5] = [
        "quota_base",
        "quota_base_min",
        "quota_base_step",
        "quota_v5_min",
        "quota_v5_max",
    ];

    /// 首页文案去掉 V5 洪峰放宽那一句（#63）。
    pub fn notice_v5(text: &str) -> String {
        let Some(i) = text.find("（当前电池充沛 ") else {
            return text.to_string();
        };
        let j = text[i..].find("额度放宽）").expect("洪峰文案") + i + "额度放宽）".len();
        format!("{}{}", &text[..i], &text[j..])
    }

    const MASK: &str = "<V5：按新公式另行核对>";

    /// 首页文案里的 V5 段（「 · V5 每人 …」到「 · Anlas」或结尾）遮住。
    fn mask_notice(text: &str) -> String {
        let Some(i) = text.find(" · V5 每人 ") else {
            return text.to_string();
        };
        let rest = &text[i..];
        let tail = rest.find(" · Anlas").map_or("", |j| &rest[j..]);
        format!("{} · {MASK}{tail}", &text[..i])
    }

    /// 结果 / `quota_algo_last` 里取决于 V5 结果的字段遮住（#63）。
    pub fn mask_result(v: &Value) -> Value {
        let mut v = v.clone();
        if let Some(m) = v.as_object_mut()
            && m.contains_key("v5")
        {
            m.insert("v5".into(), Value::String(MASK.into()));
            m.insert("changed_keys".into(), Value::String(MASK.into()));
        }
        v
    }

    /// 写入序列里取决于 V5 结果的部分遮住（两边都做，#63）：当天方案、下发（是否发生取决于每人 V5）、
    /// 「动态额度应用」日志去掉；全站 V5 / 新 Key 的 V5 默认值、首页文案的 V5 段、结果里的 V5 换成占位。
    pub fn mask_v5(effects: &Value) -> Value {
        let mut out = Vec::new();
        for e in effects.as_array().unwrap() {
            let mut e = e.clone();
            match e["op"].as_str().unwrap() {
                "update_keys" => continue,
                "log" if e["action"] == "动态额度应用" => continue,
                "set" => match e["key"].as_str().unwrap() {
                    "quota_v5_day_plan" => continue,
                    "algo_notice" => {
                        let t = mask_notice(e["value"].as_str().unwrap());
                        e["value"] = Value::String(t);
                    }
                    "quota_algo_last" => {
                        let v = jsondump::loads(e["value"].as_str().unwrap()).unwrap();
                        e["value"] = Value::String(jsondump::dumps(&mask_result(&v)));
                    }
                    _ => {}
                },
                "bulk" => {
                    for p in e["pairs"].as_array_mut().unwrap() {
                        if p[0] == "global_daily_v5" || p[0] == "register_daily_v5" {
                            p[1] = Value::String(MASK.into());
                        }
                    }
                }
                _ => {}
            }
            out.push(e);
        }
        Value::Array(out)
    }

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
        // 私密参数只剩 min_coverage_hours（#63：V5 的系数表 / 人数下限 / 洪峰 / 收紧都随旧公式删除）
        if let Some(p) = v.get_mut("params").and_then(Value::as_object_mut) {
            p.retain(|k, _| k == "min_coverage_hours");
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
                    let value = if key == "algo_notice" {
                        let i = if drop_review_note {
                            raw.find("今日额度：").expect("首页文案")
                        } else {
                            0
                        };
                        notice_v5(&notice(&raw[i..]))
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
            // 随每人注水删除（#62）
            "free_paced_cap" => continue,
            // 电量系数随旧 V5 公式删除（#63）
            "v5_factor" => continue,
            "v5_plan" => {
                // 只比与公式无关的恢复速度和剩余；全站池 / 人数 / 每人按新公式核对（#63）
                let (rate, active) = (opt_f(c.arg(1)), c.arg(2).as_i64().unwrap());
                let plan = v5_plan(opt_f(c.arg(0)), rate, active);
                let m = plan.to_map();
                assert_eq!(
                    m.keys().collect::<Vec<_>>(),
                    ["rate", "percent", "global", "people", "each"],
                    "第 {} 行",
                    c.line
                );
                assert_eq_py(&c, &m["rate"], &c.out()["rate"]);
                assert_eq_py(&c, &m["percent"], &c.out()["percent"]);
                let used = rate.filter(|r| *r > 0.0).unwrap_or(quota::V5_FALLBACK_RATE);
                let g = (used * quota::V5_IMAGES_PER_PERCENT).floor() as i64;
                let people = active.max(10);
                assert_eq!(
                    (plan.global, plan.people, plan.each),
                    (g, people, (g / people).max(1)),
                    "第 {} 行",
                    c.line
                );
                continue;
            }
            "quota_notice" => {
                let kw = |k: &str| c.kwarg(k).unwrap();
                let want = json!(expect::notice_v5(&expect::notice(
                    c.out().as_str().unwrap()
                )));
                let got = json!(
                    quota_notice(
                        c.arg(0).as_i64().unwrap(),
                        c.arg(1),
                        c.arg(2).as_i64().unwrap(),
                        kw("today").as_str().unwrap(),
                        Some(kw("review")).filter(|v| !v.is_null()),
                        kw("reset_hour").as_i64().unwrap(),
                        kw("anlas").as_i64().unwrap(),
                    )
                    .unwrap()
                );
                assert_eq_py_ordered(&c, &got, &want);
                continue;
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
                // 3.0：今天之前的 n 个完整服务日，从旧到新（旧版是含今天的 n 个自然日，#63）
                let ck = clock(7);
                let now = as_f64(c.arg(0)).unwrap();
                let n = c.arg(1).as_u64().unwrap() as usize;
                let want: Vec<String> = (1..=n)
                    .rev()
                    .map(|i| ck.day(now - 86400.0 * i as f64))
                    .collect();
                assert_eq!(active_v5_days(&ck, now, n), want, "第 {} 行", c.line);
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
    let (mut fresh_plans, mut stored_plans, mut no_zero_div) = (0, 0, 0);
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
        // 取决于 V5 结果的写入两边都遮住（#63），V5 另按新规则核对
        let (effects, want_effects) = (expect::mask_v5(&effects), expect::mask_v5(&want_effects));
        if want_err.is_none() || out.error.is_none() {
            match v5_rule(&c, &inp, &out, &ck) {
                Some(true) => stored_plans += 1,
                Some(false) => fresh_plans += 1,
                None => {}
            }
        }
        if want_err == Some("ZeroDivisionError") && out.error.is_none() {
            // 旧版私密参数把人数下限设成 0、又没人用过 V5 时 `G // 0` 抛异常；3.0 人数下限固定为 10（#63）。
            // 异常之前的写入必须一致
            let n = want_effects.as_array().unwrap().len();
            let got = effects.as_array().unwrap();
            assert_eq_py_ordered(&c, &Value::Array(got[..n].to_vec()), &want_effects);
            no_zero_div += 1;
            continue;
        }
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
            (Some(got), None) => assert_eq_py_ordered(
                &c,
                &expect::mask_result(got),
                &expect::mask_result(&expect::result(c.out())),
            ),
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
        // `active_v5` 的日期参数 3.0 换成了前 3 个完整服务日（#63），夹具里记的是旧版的自然日：不再比
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
            // 参数 (A, 每人 V5)：每人 V5 按新公式（#63），只比 A
            let q = queries(&c, "count_managed");
            assert_eq!(q.len(), 1, "第 {} 行", c.line);
            assert_eq!(q[0][0], r["ceiling"], "第 {} 行", c.line);
        }
        let _: Option<&Map<String, Value>> = None;
    }
    assert!(
        errs > 50,
        "异常路径用例太少: {errs}（另有 {no_base_error} 个保底越界的用例）"
    );
    assert!(adjusted > 200, "每日微调用例太少: {adjusted}");
    assert!(applied > 100, "下发用例太少: {applied}");
    assert!(
        fresh_plans > 100 && stored_plans > 50,
        "V5 新规则核对的用例太少：当天新定 {fresh_plans}，沿用 {stored_plans}（旧版人数下限为 0 的 {no_zero_div}）"
    );
}

/// 按 3.0 的 V5 规则（docs/v3-deviations.md #63）核对 Rust 的输出；返回 `Some(是否沿用了当天已定的方案)`，
/// 没有结果（出错 / 算法关着）返回 None。
fn v5_rule(c: &Case, inp: &QuotaInputs, out: &quota::QuotaOutputs, ck: &Clock) -> Option<bool> {
    let r = out.result.as_ref()?;
    if r["enabled"] != true {
        return None;
    }
    let today = ck.day(inp.now);
    let v5 = &r["v5"];
    let num = |v: &Value| as_f64(v).unwrap_or_else(|| panic!("第 {} 行：不是数字 {v}", c.line));
    let (each, global) = (num(&v5["each"]) as i64, num(&v5["global"]) as i64);
    let stored = inp
        .settings
        .get(quota::V5_DAY_KEY)
        .and_then(|s| gate_core::jsondump::loads(s))
        .filter(|v| v["day"] == today.as_str() && gate_core::pyval::truthy(&v["plan"]))
        .map(|v| v["plan"].clone());
    let plan_writes: Vec<&Effect> = out
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::Set { key, .. } if key == quota::V5_DAY_KEY))
        .collect();
    let reused = match &stored {
        // 当天已定的方案原样沿用，不再改（旧版白天会放宽 / 收紧）
        Some(p) => {
            assert_eq!(
                (num(&p["each"]) as i64, num(&p["global"]) as i64),
                (each, global),
                "第 {} 行：当天方案被改了",
                c.line
            );
            assert!(plan_writes.is_empty(), "第 {} 行：沿用方案不该重写", c.line);
            true
        }
        // 新定：每人 = max(1, ⌊G ÷ max(10, N)⌋)，没有夹取、没有电量系数
        None => {
            let people = inp.active_v5.max(10);
            assert_eq!(v5["people"], people, "第 {} 行", c.line);
            assert_eq!(each, (global / people).max(1), "第 {} 行", c.line);
            assert!(global >= 1 && v5.get("k").is_none(), "第 {} 行", c.line);
            assert_eq!(plan_writes.len(), 1, "第 {} 行：当天方案要写一次", c.line);
            false
        }
    };
    // 超卖按活跃人数（前 3 个完整服务日用过 V5 的人）判断
    let over = global != 0 && each * inp.active_v5.max(1) > global;
    assert_eq!(!v5["oversell"].is_null(), over, "第 {} 行 oversell", c.line);
    if over {
        assert_eq!(v5["oversell"]["members"], inp.active_v5, "第 {} 行", c.line);
    }
    // 下发、默认值、首页文案都用同一组数
    let a = r["ceiling"].as_i64().unwrap();
    let changed = inp
        .managed_keys
        .iter()
        .filter(|k| k.daily_images != a || k.daily_v5 != each || k.image_model_scope != "all")
        .count() as i64;
    assert_eq!(r["changed_keys"], changed, "第 {} 行", c.line);
    let updates: Vec<&Effect> = out
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::UpdateKeys { .. }))
        .collect();
    if changed > 0 {
        assert_eq!(
            updates,
            [&Effect::UpdateKeys {
                daily_images: a,
                daily_v5: each
            }],
            "第 {} 行",
            c.line
        );
    } else {
        assert!(updates.is_empty(), "第 {} 行", c.line);
    }
    let bulk = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::Bulk { pairs } if pairs[0].0 == "global_daily_v5" => Some(pairs.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("第 {} 行：没写全站 V5", c.line));
    assert_eq!(bulk[0].1, global.to_string(), "第 {} 行", c.line);
    assert_eq!(bulk[2], ("register_daily_v5".into(), each.to_string()));
    let notice = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::Set { key, value } if key == quota::NOTICE_KEY => Some(value.clone()),
            _ => None,
        })
        .unwrap();
    let mut v5_text = format!(" · V5 每人 {each} 张");
    if over {
        v5_text.push_str(&format!("（全站今日共 {global} 张，先到先得）"));
    }
    assert!(notice.contains(&v5_text), "第 {} 行：{notice}", c.line);
    assert!(!notice.contains("额度放宽"), "第 {} 行：{notice}", c.line);
    Some(reused)
}
