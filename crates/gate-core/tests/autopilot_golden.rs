//! 自动驾驶对拍：旧 Python 生成的夹具（真实输出，生成脚本见标签 py-final）。夹具冻结不改，
//! 3.0 有意删掉的东西在这里按输入过滤 / 在期望一侧改写：
//!
//! - docs/v3-deviations.md #51：2.x 单 Key 守护的「换网段」「多客户端全天在用」两条规则（输入 `events`，恒为空）。
//!   `pure` 里只测这两条规则的 `key_guard_rule` 用例跳过；`run` 里 `events` 在解析前去掉，某把 Key 的 `events`
//!   会触发这两条规则的用例只做部分对拍（见下）。
//! - docs/v3-deviations.md #58：熔断另要求失败来自 ≥ 2 把不同的非测试 Key。冻结夹具的 `breaker` 输入是旧版
//!   不分 Key 的统计，没有 `fail_keys`：逐位对拍时按「失败来自 2 把 Key」补上 `fail_keys = 2`（这时旧版的结论
//!   全部成立）；再用 `fail_keys = 1`（失败全来自同一把 Key）把每条用例跑一遍：一律不熔断（没有 `TripBreaker`、
//!   `rules.breaker.value` 为 false），异常类型与单 Key 限流不变。同一条里还删了 `decide` 中只为复刻旧版报错而
//!   再算一遍的 `breaker_since`：`autopilot_breaker_until` 读不懂时旧版整轮抛 ValueError，3.0 继续往下走；这类用例
//!   只断言旧版报错之前已产生的副作用（去掉名额那部分）原样是 3.0 副作用的前缀。
//! - docs/v3-deviations.md #68：「名额 +5」（`slots`）与每条规则的 `off` / `observe` / `enforce` 模式删除，
//!   熔断与单 Key 限流一律执行。`slots_rule` 用例跳过，`run` 的输入去掉 `registrar` / `global_daily_v5_default` /
//!   `queue_hours`；期望里去掉名额那一条规则、它的写入（`register_max_users`、`autopilot_slots_at`、
//!   「自动驾驶：名额」日志）和每小时快照里的 `slots`。旧版只在名额那一步（读上一轮额度复盘）抛的异常，3.0 不再有。
//!   熔断和单 Key 限流在旧版也是 `enforce` 的用例逐位对拍；不是的（旧版只观察 / 关闭），只比两条规则的判断，
//!   再按「一律执行」核对 3.0 的动作。

use gate_core::autopilot::{self, *};
use gate_core::clock::Clock;
use gate_core::jsondump;
use gate_core::quota::Effect;
use gate_golden::{Case, as_f64, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

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
            // 「名额 +5」已删除（#68）
            "slots_rule" => continue,
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
    assert!(
        seen.get("breaker_rule").copied().unwrap_or(0) > 100,
        "breaker_rule 用例太少"
    );
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
    // `over_base`（保底）随保底一层删除（docs/v3-deviations.md #61），期望一侧去掉；
    // `slots_*` / `queue_busy` 随「名额 +5」删除（#68），不再比
    let want: Vec<&str> = c.raw["skip_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .filter(|r| *r != "over_base")
        .collect();
    assert_eq!(autopilot::KEY_GUARD_SKIP_REASONS.to_vec(), want);
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

/// 旧版 `_mode`：不认识的值回到 `observe`。
fn old_mode(inputs: &Value, rule: &str) -> String {
    match inputs["settings"][format!("autopilot_{rule}")].as_str() {
        Some(v @ ("observe" | "enforce" | "off")) => v.to_string(),
        _ => "observe".to_string(),
    }
}

/// 期望一侧：去掉名额规则（#68）。`v` 是 `autopilot_last` / 返回值。
fn without_slots(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(rules) = v.get_mut("rules").and_then(Value::as_object_mut) {
        rules.shift_remove("slots");
    }
    v
}

/// 期望一侧的写入序列：去掉名额那一条规则的写入、日志与快照字段（#68）。
fn effects_without_slots(effects: &Value) -> Value {
    let mut out = Vec::new();
    for e in effects.as_array().unwrap() {
        let mut e = e.clone();
        match e["op"].as_str().unwrap() {
            "set_registration_max_users" => continue,
            "log" if e["action"] == "自动驾驶：名额" => continue,
            "set" => match e["key"].as_str().unwrap() {
                "autopilot_slots_at" => continue,
                STATE_KEY => {
                    let v = jsondump::loads(e["value"].as_str().unwrap()).unwrap();
                    e["value"] = json!(jsondump::dumps(&without_slots(&v)));
                }
                HISTORY_KEY => {
                    // 最后一条是这一轮新记的快照；之前的是输入里原有的，不动
                    let mut v = jsondump::loads(e["value"].as_str().unwrap()).unwrap();
                    if let Some(last) = v
                        .as_array_mut()
                        .and_then(|a| a.last_mut())
                        .and_then(Value::as_object_mut)
                    {
                        last.shift_remove("slots");
                    }
                    e["value"] = json!(jsondump::dumps(&v));
                }
                _ => {}
            },
            _ => {}
        }
        out.push(e);
    }
    Value::Array(out)
}

/// 规则的判断（与模式无关的部分）：`value`（单 Key 限流去掉 `applied`）与 `why`。
fn verdict(rule: &Value) -> Value {
    let mut value = rule["value"].clone();
    if let Some(list) = value.as_array_mut() {
        for d in list {
            d.as_object_mut().unwrap().shift_remove("applied");
        }
    }
    json!([value, rule["why"]])
}

/// 一律执行（#68）：熔断触发就熔断（已在熔断中除外），触发限流的 Key 都暂停（已暂停的除外）。
fn check_enforced(c: &Case, inp: &AutopilotInputs, out: &autopilot::AutopilotOutputs) {
    let r = out.result.as_ref().unwrap();
    let trips = out
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::TripBreaker { .. }))
        .count();
    let want_trip = r["rules"]["breaker"]["value"] == true
        && inp
            .guard
            .as_ref()
            .is_some_and(|g| g.breaker_until <= inp.now);
    assert_eq!(trips, usize::from(want_trip), "第 {} 行熔断", c.line);
    let paused: Vec<i64> = out
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::PauseKey { key_id, .. } => Some(*key_id),
            _ => None,
        })
        .collect();
    let want: Vec<i64> = r["rules"]["key_guard"]["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["key"].as_i64().unwrap())
        .filter(|k| {
            inp.pauses
                .as_ref()
                .is_some_and(|p| p.paused.get(k).copied().unwrap_or(0.0) <= inp.now)
        })
        .collect();
    assert_eq!(paused, want, "第 {} 行暂停", c.line);
    for rule in ["breaker", "key_guard"] {
        assert_eq!(r["rules"][rule]["mode"], MODE, "第 {} 行", c.line);
    }
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::Log { action, .. } if action == "自动驾驶（观察）")),
        "第 {} 行：不再有观察日志",
        c.line
    );
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
    // 名额规则 3.0 已删除（#68），只剩单 Key 限流要与 2 把 Key 时一致
    assert_eq_py_ordered(
        c,
        &got["rules"]["key_guard"],
        &expected["rules"]["key_guard"],
    );
    expected["rules"]["breaker"]["value"] == json!(true)
}

#[test]
fn run_golden() {
    let ck = Clock::new("Asia/Shanghai", 7);
    let (mut errs, mut breaker, mut pauses, mut partial, mut no_slots_err) = (0, 0, 0, 0, 0);
    let mut observed = 0;
    let (mut since_dev, mut single_key_suppressed) = (0, 0);
    for c in load(M, "autopilot/run") {
        // `events` 输入已随被删规则一起去掉；没触发被删规则的用例里它本来就不起作用，去掉后结果不变
        let removed = run_case_hits_removed_rules(&c.raw["inputs"]);
        let mut raw_inputs = c.raw["inputs"].clone();
        for k in [
            "events",
            "registrar",
            "global_daily_v5_default",
            "queue_hours",
        ] {
            raw_inputs.as_object_mut().unwrap().remove(k);
        }
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
            // 旧版在熔断这一步因为 `autopilot_breaker_until` 读不懂而整轮报错；3.0 继续（#58）。
            // 旧版报错前的副作用（去掉名额那部分，#68）必须原样是 3.0 副作用的前缀
            since_dev += 1;
            let effects = serde_json::to_value(&out.effects).unwrap();
            let want_effects = effects_without_slots(&c.raw["effects"]);
            let want_effects = want_effects.as_array().unwrap();
            let got = effects.as_array().unwrap();
            assert!(
                got.len() >= want_effects.len(),
                "第 {} 行：旧版报错前的副作用必须都在",
                c.line
            );
            assert_eq_py_ordered(
                &c,
                &Value::Array(got[..want_effects.len()].to_vec()),
                &Value::Array(want_effects.clone()),
            );
            continue;
        }
        // 旧版在名额那一步（读上一轮额度复盘 / 额度状态）就抛了异常、没走到熔断：3.0 没有这一步（#68），
        // 照常往下（可能在后面别处才报错）
        if want_err.is_some() && out.error.as_deref() != want_err {
            assert!(
                queries(&c, "breaker").is_empty(),
                "第 {} 行：旧版的异常应发生在熔断之前",
                c.line
            );
            no_slots_err += 1;
            continue;
        }
        assert_eq!(out.error.as_deref(), want_err, "第 {} 行异常类型", c.line);
        errs += usize::from(want_err.is_some());
        if !removed && assert_single_key_never_trips(&c, &raw_inputs, &out) {
            single_key_suppressed += 1;
        }
        let enforced = old_mode(&c.raw["inputs"], "breaker") == "enforce"
            && old_mode(&c.raw["inputs"], "key_guard") == "enforce";
        if removed || !enforced {
            // 旧版只观察 / 关闭了某条规则，或期望里带着被删规则的结论：只比规则的判断，动作按「一律执行」核对
            partial += 1;
            if !enforced {
                observed += 1;
            }
            match (&out.result, want_err) {
                (Some(got), None) => {
                    let want = &c.out()["rules"];
                    assert_eq_py_ordered(
                        &c,
                        &verdict(&got["rules"]["breaker"]),
                        &verdict(&want["breaker"]),
                    );
                    if !removed {
                        assert_eq_py_ordered(
                            &c,
                            &verdict(&got["rules"]["key_guard"]),
                            &verdict(&want["key_guard"]),
                        );
                    }
                    check_enforced(&c, &inp, &out);
                }
                (None, Some(_)) => {}
                other => panic!("第 {} 行：结果与异常不匹配 {other:?}", c.line),
            }
        } else {
            let effects = serde_json::to_value(&out.effects).unwrap();
            assert_eq_py_ordered(&c, &effects, &effects_without_slots(&c.raw["effects"]));
            match (&out.result, want_err) {
                (Some(got), None) => {
                    assert_eq_py_ordered(&c, got, &without_slots(c.out()));
                    check_enforced(&c, &inp, &out);
                }
                (None, Some(_)) => {}
                other => panic!("第 {} 行：结果与异常不匹配 {other:?}", c.line),
            }
        }
        // 生效的动作（逐位对拍的用例与按「一律执行」核对的用例都算）
        if let Some(r) = &out.result {
            breaker += usize::from(r["rules"]["breaker"].get("applied").is_some());
            pauses += r["rules"]["key_guard"]["value"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| d.get("applied").is_some())
                .count();
        }

        // ---- 服务层该传的查询参数（文档里的清单）----
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
    assert!(
        errs > 20,
        "异常路径用例太少: {errs}（名额那一步的异常 {no_slots_err} 个）"
    );
    assert!(breaker > 20, "熔断生效用例太少: {breaker}");
    assert!(pauses > 40, "Key 限流生效用例太少: {pauses}");
    assert!(
        partial > 100 && observed > 50,
        "部分对拍的用例：{partial}（其中旧版只观察 / 关闭的 {observed}）"
    );
    assert!(
        single_key_suppressed > 20,
        "旧版会熔断、失败只来自 1 把 Key 时不熔断的用例太少: {single_key_suppressed}"
    );
    assert!(
        since_dev > 0,
        "应有 `autopilot_breaker_until` 读不懂、3.0 不再整轮报错的用例"
    );
}
