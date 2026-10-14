//! 自动驾驶对拍：`tools/parity/golden/autopilot.py` 生成的夹具（旧 Python 的真实输出）。

use gate_core::autopilot::{self, *};
use gate_core::clock::Clock;
use gate_golden::{Case, as_f64, assert_eq_py, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn opt_f(v: &Value) -> Option<f64> {
    if v.is_null() { None } else { as_f64(v) }
}

#[test]
fn pure_golden() {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
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
            "key_guard_rule" => match key_guard_rule(
                c.arg(0).as_i64().unwrap(),
                c.arg(1).as_i64().unwrap(),
                c.arg(2).as_i64().unwrap(),
                c.arg(3).as_i64().unwrap(),
            ) {
                Some((action, secs, why)) => json!([action, secs, why]),
                None => Value::Null,
            },
            other => panic!("未知函数 {other}"),
        };
        assert_eq_py_ordered(&c, &got, c.out());
    }
    for k in ["slots_rule", "breaker_rule", "key_guard_rule"] {
        assert!(seen.get(k).copied().unwrap_or(0) > 100, "{k} 用例太少");
    }
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

#[test]
fn run_golden() {
    let ck = Clock::new("Asia/Shanghai", 7);
    let (mut errs, mut slots, mut breaker, mut pauses) = (0, 0, 0, 0);
    for c in load(M, "autopilot/run") {
        let inp: AutopilotInputs = serde_json::from_value(c.raw["inputs"].clone())
            .unwrap_or_else(|e| panic!("第 {} 行输入解析失败: {e}", c.line));
        let out = autopilot::decide(&inp, &ck);
        let want_err = c.raw["err"].as_str();
        assert_eq!(out.error.as_deref(), want_err, "第 {} 行异常类型", c.line);
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
    assert!(breaker > 30, "熔断生效用例太少: {breaker}");
    assert!(pauses > 100, "Key 限流生效用例太少: {pauses}");
    let _ = assert_eq_py;
}
