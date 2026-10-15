//! 自动驾驶对拍：旧 Python 生成的夹具（真实输出，生成脚本见标签 py-final）。
//!
//! 2.x 单 Key 守护的「换网段」「多客户端全天在用」两条规则（输入 `events`，恒为空）在 3.0 已删除
//! （docs/v3-deviations.md #51）。夹具是冻结的 Python 输出，不改，在这里按输入过滤：
//! - `pure`：`key_guard_rule` 里只测这两条规则的用例整条跳过，其余照常对拍；
//! - `run`：`events` 在解析前去掉；某把 Key 的 `events` 会触发这两条规则的用例，期望输出里的 key_guard 判断 /
//!   状态快照 / 日志带着被删规则的结论，只比较与它无关的部分（异常类型、名额、熔断、查询参数），其余用例逐位对拍。
//!
//! 熔断另要求失败来自 ≥ 2 把不同的非测试 Key（docs/v3-deviations.md #58）。冻结夹具的 `breaker` 输入是旧版
//! 不分 Key 的统计，没有 `fail_keys`：
//! - 逐位对拍时按「失败来自 2 把 Key」补上 `fail_keys = 2`——这时旧版的结论全部成立；
//! - 再用 `fail_keys = 1`（失败全来自同一把 Key）把每条用例跑一遍：一律不熔断（没有 `TripBreaker`、
//!   `rules.breaker.value` 为 false），异常类型、名额、单 Key 限流与期望一致。
//!
//! 同一条里还删了 `decide` 中只为复刻旧版报错而再算一遍的 `breaker_since`：`autopilot_breaker_until` 读不懂时
//! 旧版整轮抛 ValueError，3.0 继续往下走。这类用例（期望 ValueError、`breaker_since` 本身报错、3.0 不再报同样的错）
//! 只断言旧版报错之前已产生的副作用原样是 3.0 副作用的前缀。

use gate_core::autopilot::{self, *};
use gate_core::clock::Clock;
use gate_core::quota::Effect;
use gate_golden::{Case, as_f64, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn opt_f(v: &Value) -> Option<f64> {
    if v.is_null() { None } else { as_f64(v) }
}

/// 旧版 `key_guard_rule(alternate, clients, allday, rejects)` 的前三个参数：会触发被删规则
/// （`alternate >= 2`，或 `clients` 与 `allday` 同时出现）。
fn removed_key_guard_branch(alternate: i64, clients: i64, allday: i64) -> bool {
    alternate >= 2 || (clients != 0 && allday != 0)
}

/// `run` 用例里，任何一把 Key 24 小时内的 `events` 会触发被删规则（期望输出里带着被删规则的判断）。
fn run_case_hits_removed_rules(inputs: &Value) -> bool {
    let now = as_f64(&inputs["now"]).unwrap();
    let events = inputs["events"].as_array().unwrap();
    inputs["keys"].as_array().unwrap().iter().any(|k| {
        let kid = k[0].as_i64().unwrap();
        let count = |kind: &str| {
            events
                .iter()
                .filter(|e| {
                    e[1].as_i64() == Some(kid)
                        && as_f64(&e[0]).unwrap() >= now - 86400.0
                        && e[2] == kind
                })
                .count() as i64
        };
        removed_key_guard_branch(count("alternate"), count("clients"), count("allday"))
    })
}

#[test]
fn pure_golden() {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    let mut skipped = 0;
    for c in load(M, "autopilot/pure") {
        let name = c.raw["fn"].as_str().unwrap().to_string();
        *seen.entry(name.clone()).or_default() += 1;
        let got: Value = match name.as_str() {
            "slots_rule" => {
                let a = c.args();
                let (n, why) = slots_rule(
                    a[0].as_i64().unwrap(),
                    a[1].as_i64().unwrap(),
                    as_f64(&a[2]).unwrap(),
                    a[3].as_i64().unwrap(),
                    a.get(4).map_or(0, |v| v.as_i64().unwrap()),
                    a.get(5).and_then(opt_f),
                );
                json!([n, why])
            }
            "breaker_rule" => {
                let (trip, why) =
                    breaker_rule(c.arg(0).as_i64().unwrap(), c.arg(1).as_i64().unwrap());
                json!([trip, why])
            }
            "key_guard_rule" => {
                let (alternate, clients, allday) = (
                    c.arg(0).as_i64().unwrap(),
                    c.arg(1).as_i64().unwrap(),
                    c.arg(2).as_i64().unwrap(),
                );
                if removed_key_guard_branch(alternate, clients, allday) {
                    skipped += 1;
                    continue;
                }
                match key_guard_rule(c.arg(3).as_i64().unwrap()) {
                    Some(why) => json!(["pause", KEY_GUARD_PAUSE_SECS, why]),
                    None => Value::Null,
                }
            }
            other => panic!("未知函数 {other}"),
        };
        assert_eq_py_ordered(&c, &got, c.out());
    }
    for k in ["slots_rule", "breaker_rule"] {
        assert!(seen.get(k).copied().unwrap_or(0) > 100, "{k} 用例太少");
    }
    // 被删规则的用例（alternate >= 2，或 clients 与 allday 同时出现）已跳过，剩下的覆盖「被拒次数」两侧
    assert!(skipped > 100, "被删规则的用例应该被跳过: {skipped}");
    assert!(
        seen.get("key_guard_rule").copied().unwrap_or(0) > 50,
        "key_guard_rule 用例太少"
    );
}

#[test]
fn consts_golden() {
    let cases = load(M, "autopilot/consts");
    let c = &cases[0];
    let want: Vec<&str> = c.raw["skip_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(autopilot::KEY_GUARD_SKIP_REASONS.to_vec(), want);
    assert_eq!(c.raw["slots_max"], SLOTS_MAX);
    assert_eq!(c.raw["slots_step"], SLOTS_STEP);
    assert_eq!(as_f64(&c.raw["slots_cooldown"]).unwrap(), SLOTS_COOLDOWN);
    assert_eq!(c.raw["queue_busy"], QUEUE_BUSY_REJECTS);
    assert_eq!(as_f64(&c.raw["slots_v5_min"]).unwrap(), SLOTS_V5_MIN);
    assert_eq!(c.raw["state_key"], STATE_KEY);
    assert_eq!(c.raw["history_key"], HISTORY_KEY);
}

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

/// 把夹具输入里的 `breaker` 补上 `fail_keys`（docs/v3-deviations.md #58）。
fn with_fail_keys(raw: &Value, n: i64) -> Value {
    let mut v = raw.clone();
    v["breaker"]
        .as_object_mut()
        .expect("breaker 是对象")
        .insert("fail_keys".into(), json!(n));
    v
}

/// 失败全来自同一把 Key 时：不熔断，其余与期望一致。返回期望里本来会熔断（`breaker.value` 为真）的话 true。
fn assert_single_key_never_trips(
    c: &Case,
    raw_inputs: &Value,
    want: &autopilot::AutopilotOutputs,
) -> bool {
    let inp: AutopilotInputs = serde_json::from_value(with_fail_keys(raw_inputs, 1)).unwrap();
    let ck = Clock::new("Asia/Shanghai", 7);
    let out = autopilot::decide(&inp, &ck);
    assert_eq!(
        out.error, want.error,
        "第 {} 行（1 把 Key）异常类型",
        c.line
    );
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::TripBreaker { .. })),
        "第 {} 行：失败只来自 1 把 Key 不该熔断",
        c.line
    );
    let Some(got) = &out.result else {
        return false;
    };
    assert_eq!(
        got["rules"]["breaker"]["value"],
        json!(false),
        "第 {} 行",
        c.line
    );
    assert!(
        got["rules"]["breaker"].get("applied").is_none(),
        "第 {} 行",
        c.line
    );
    let expected = want.result.as_ref().unwrap();
    for rule in ["slots", "key_guard"] {
        assert_eq_py_ordered(c, &got["rules"][rule], &expected["rules"][rule]);
    }
    expected["rules"]["breaker"]["value"] == json!(true)
}

#[test]
fn run_golden() {
    let ck = Clock::new("Asia/Shanghai", 7);
    let (mut errs, mut slots, mut breaker, mut pauses, mut partial) = (0, 0, 0, 0, 0);
    let (mut since_dev, mut single_key_suppressed) = (0, 0);
    for c in load(M, "autopilot/run") {
        // `events` 输入已随被删规则一起去掉；没触发被删规则的用例里它本来就不起作用，去掉后结果不变
        let removed = run_case_hits_removed_rules(&c.raw["inputs"]);
        let mut raw_inputs = c.raw["inputs"].clone();
        raw_inputs.as_object_mut().unwrap().remove("events");
        // 旧版的结论按「失败来自 2 把 Key」成立（#58）
        let raw_inputs = with_fail_keys(&raw_inputs, BREAKER_MIN_KEYS);
        let inp: AutopilotInputs = serde_json::from_value(raw_inputs.clone())
            .unwrap_or_else(|e| panic!("第 {} 行输入解析失败: {e}", c.line));
        let out = autopilot::decide(&inp, &ck);
        let want_err = c.raw["err"].as_str();
        let since_err = breaker_since(
            inp.now,
            inp.settings
                .get("autopilot_breaker_until")
                .map(String::as_str),
        )
        .is_err();
        if since_err && want_err == Some("ValueError") && out.error.as_deref() != want_err {
            // 旧版在熔断这一步因为 `autopilot_breaker_until` 读不懂而整轮报错；3.0 继续（#58）
            since_dev += 1;
            let effects = serde_json::to_value(&out.effects).unwrap();
            let want_effects = c.raw["effects"].as_array().unwrap();
            let got = effects.as_array().unwrap();
            assert!(
                got.len() >= want_effects.len(),
                "第 {} 行：旧版报错前的副作用必须都在",
                c.line
            );
            assert_eq_py_ordered(
                &c,
                &Value::Array(got[..want_effects.len()].to_vec()),
                &c.raw["effects"],
            );
            continue;
        }
        assert_eq!(out.error.as_deref(), want_err, "第 {} 行异常类型", c.line);
        if !removed && assert_single_key_never_trips(&c, &raw_inputs, &out) {
            single_key_suppressed += 1;
        }
        if removed {
            // 期望里的 key_guard 判断、状态快照、操作日志带着被删规则的结论：只比较与它无关的部分
            partial += 1;
            match (&out.result, want_err) {
                (Some(got), None) => {
                    for rule in ["slots", "breaker"] {
                        assert_eq_py_ordered(&c, &got["rules"][rule], &c.out()["rules"][rule]);
                    }
                }
                (None, Some(_)) => {}
                other => panic!("第 {} 行：结果与异常不匹配 {other:?}", c.line),
            }
        } else {
            let effects = serde_json::to_value(&out.effects).unwrap();
            assert_eq_py_ordered(&c, &effects, &c.raw["effects"]);
            match (&out.result, want_err) {
                (Some(got), None) => assert_eq_py_ordered(&c, got, c.out()),
                (None, Some(_)) => errs += 1,
                other => panic!("第 {} 行：结果与异常不匹配 {other:?}", c.line),
            }
            if let Some(r) = &out.result {
                slots += usize::from(r["rules"]["slots"].get("applied").is_some());
                breaker += usize::from(r["rules"]["breaker"].get("applied").is_some());
                pauses += r["rules"]["key_guard"]["value"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|d| d.get("applied").is_some())
                    .count();
            }
        }

        // ---- 服务层该传的查询参数（文档里的清单）----
        let hist_raw = inp.settings.get("quota_algo_history").map(String::as_str);
        let q = queries(&c, "queue_hours");
        if !q.is_empty() {
            let (start, end) = queue_window(hist_raw, &ck)
                .unwrap()
                .expect("旧版查了就必须有窗口");
            assert_eq!(
                q,
                vec![&vec![f(start), f(end), json!(5)]],
                "第 {} 行 queue_hours",
                c.line
            );
        }
        for q in queries(&c, "breaker") {
            let since = breaker_since(
                inp.now,
                inp.settings
                    .get("autopilot_breaker_until")
                    .map(String::as_str),
            )
            .unwrap();
            assert_eq!(*q, vec![f(since)], "第 {} 行 breaker", c.line);
        }
        for q in queries(&c, "rejects") {
            assert_eq!(*q, vec![f(inp.now - 3600.0)]);
        }
    }
    assert!(errs > 50, "异常路径用例太少: {errs}");
    assert!(slots > 50, "名额生效用例太少: {slots}");
    assert!(breaker > 20, "熔断生效用例太少: {breaker}");
    assert!(pauses > 40, "Key 限流生效用例太少: {pauses}");
    assert!(
        partial > 100,
        "触发被删规则的用例应该只做部分对拍: {partial}"
    );
    assert!(
        single_key_suppressed > 20,
        "旧版会熔断、失败只来自 1 把 Key 时不熔断的用例太少: {single_key_suppressed}"
    );
    assert!(
        since_dev > 0,
        "应有 `autopilot_breaker_until` 读不懂、3.0 不再整轮报错的用例"
    );
    let _ = assert_eq_py;
}
