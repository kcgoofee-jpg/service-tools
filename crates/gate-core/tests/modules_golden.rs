//! 模块登记对拍：`tools/parity/golden/modules.py` 生成的夹具（旧 Python 的真实输出）。
//!
//! 夹具是冻结的，不重新生成。3.0 删掉了「就绪度」模块和所有「每人额度 × 人数 vs 容量」的承诺类校验
//! （docs/v3-deviations.md #44）：夹具里这些部分在比较前从期望里去掉；只测被删函数的用例（就绪度校验、
//! 名额文案、该模块的元数据）已经从夹具里删掉。

use gate_core::clock::Clock;
use gate_core::modules::{self, *};
use gate_core::quota::{self, AccountAllowance};
use gate_golden::{Case, as_f64, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

fn de<T: serde::de::DeserializeOwned>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap()
}

fn checks_json(cs: &[Check]) -> Value {
    serde_json::to_value(cs).unwrap()
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

/// 夹具的登记顺序里「3.0 不再登记」的模块个数（见文件头）。
const REMOVED_MODULES: usize = 1;
/// 夹具元数据里的 `report_bugs`：3.0 的模块校验不记 Bug，字段已删除。
const REMOVED_META_FIELDS: [&str; 1] = ["report_bugs"];

/// 3.0 额度改动对模块元数据的影响（期望一侧改写）。
fn v3_meta(name: &str, want: &mut Value) {
    if name == "allocation" {
        // V5 每人上限改成公式、没有 3～30 的范围（docs/v3-deviations.md #63）
        let p = &mut want["params"][3];
        assert_eq!(p[0], "V5 每人范围");
        *p = json!([
            "V5 每人",
            "每天 7 点按恢复速度定全站池，再按前 3 天用过 V5 的人数分，当天不变"
        ]);
    }
}

#[test]
fn meta_golden() {
    let cases = load(M, "modules/meta");
    let mut names = Vec::new();
    for c in cases.iter().filter(|c| c.raw["fn"] == "meta") {
        let name = c.raw["name"].as_str().unwrap();
        let m = MODULES
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("缺少模块 {name}"));
        let params: Vec<Value> = m.params.iter().map(|(n, note)| json!([n, note])).collect();
        let got = json!({
            "fn": "meta", "name": m.name, "title": m.title, "question": m.question,
            "hard_note": m.hard_note, "default_enabled": m.default_enabled,
            "periodic": m.periodic,
            "has_checks": m.has_checks, "custom_enabled": m.custom_enabled, "params": params,
        });
        let mut want = c.raw.clone();
        for field in REMOVED_META_FIELDS {
            want.as_object_mut().unwrap().shift_remove(field);
        }
        v3_meta(name, &mut want);
        assert_eq_py_ordered(c, &got, &want);
        names.push(name.to_string());
    }
    let order = cases.iter().find(|c| c.raw["fn"] == "order").unwrap();
    let all: Vec<&str> = order.raw["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let want: Vec<&str> = all
        .iter()
        .copied()
        .filter(|n| MODULES.iter().any(|m| m.name == *n))
        .collect();
    assert_eq!(all.len() - want.len(), REMOVED_MODULES);
    assert_eq!(MODULES.iter().map(|m| m.name).collect::<Vec<_>>(), want);
    assert_eq!(names, want);
}

#[test]
fn capacity_checks_golden() {
    let mut counted = 0;
    for c in load(M, "modules/checks_capacity") {
        let inp: CapacityInputs = de(&c.raw["inputs"]);
        let got = checks_json(&capacity_checks(&inp));
        assert_eq_py_ordered(&c, &got, &c.raw["out"]);
        // 服务层该传的窗口参数
        let q = queries(&c, "counted");
        match capacity_v5_count_window(&inp.snaps) {
            Some((t0, t1)) if inp.tokens > 0 => {
                assert_eq!(q, vec![&vec![f(t0), f(t1)]], "第 {} 行", c.line);
                counted += 1;
            }
            _ => assert!(q.is_empty(), "第 {} 行：旧版多查了 counted", c.line),
        }
    }
    assert!(counted > 100);
}

#[test]
fn capacity_ticks_golden() {
    for c in load(M, "modules/ticks_capacity") {
        let i = &c.raw["inputs"];
        let now = as_f64(&i["now"]).unwrap();
        let last = if i["last"].is_null() {
            None
        } else {
            as_f64(&i["last"])
        };
        let changed = i["changed"]
            .as_array()
            .map(|a| (a[0].as_i64().unwrap(), a[1].as_i64().unwrap()));
        let hourly = i["hourly"].as_i64().unwrap();
        let summary = capacity_tick_summary(hourly, changed);
        assert_eq_py_ordered(&c, &json!(summary), &c.raw["out"]);
        let mut effects = Vec::new();
        if snapshot_due(last, now) {
            let accounts: Vec<AccountAllowance> = de(&i["accounts"]);
            let (pct, rate) = quota::allowance_summary(&accounts);
            let fresh = i["fresh_possible"].as_bool().unwrap();
            let row = snapshot_row(now, pct, rate, fresh);
            effects.push(json!({"op": "insert_snapshot",
                "row": [f(row.0), row.1.map_or(Value::Null, f), row.2.map_or(Value::Null, f), row.3, row.4]}));
        }
        if let Some(ch) = changed {
            effects.push(
                json!({"op": "log", "actor": "系统", "action": "自动调整：每小时上限",
                "target": "", "detail": capacity_adjust_log_detail(ch)}),
            );
        }
        assert_eq_py_ordered(&c, &Value::Array(effects), &c.raw["effects"]);
    }
}

/// 夹具里分配模块 3.0 已删除的校验：「V5：每人额度 × 人数 ≤ 全站上限」（承诺 vs 容量，#44）、
/// 「V4.5：保底 ≤ 每日上限」（保底一层删除，docs/v3-deviations.md #61）。
const REMOVED_ALLOCATION_CHECKS: [&str; 2] =
    ["V5：每人额度 × 人数 ≤ 全站上限", "V4.5：保底 ≤ 每日上限"];

#[test]
fn allocation_checks_golden() {
    let (mut errs, mut empties, mut removed) = (0, 0, 0);
    for c in load(M, "modules/checks_allocation") {
        // 夹具输入里的 `ack_oversell`、`base` 是已删除校验的参数
        let mut raw_inputs = c.raw["inputs"].clone();
        raw_inputs.as_object_mut().unwrap().remove("ack_oversell");
        raw_inputs.as_object_mut().unwrap().remove("base");
        let inp: AllocationInputs = de(&raw_inputs);
        let want_err = c.raw["err"].as_str();
        match (allocation_checks(&inp), want_err) {
            (Ok(cs), None) => {
                let want: Vec<Value> = c.raw["out"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|x| {
                        !REMOVED_ALLOCATION_CHECKS
                            .iter()
                            .any(|name| x["name"] == *name)
                    })
                    .cloned()
                    .collect();
                assert_eq_py_ordered(&c, &checks_json(&cs), &Value::Array(want));
                if cs.is_empty() {
                    empties += 1;
                }
            }
            (Err(e), Some(t)) => {
                assert_eq!(e.py_type(), t, "第 {} 行", c.line);
                errs += 1;
            }
            // 旧版只在保底校验里比较「保底 ≤ 每人上限」时抛 TypeError（每人上限不是数字）；
            // 这条校验删了，3.0 只剩同步校验
            (Ok(cs), Some("TypeError")) if allocation_query(inp.last_raw.as_deref()).is_ok() => {
                assert_eq!(cs.len(), 1, "第 {} 行", c.line);
                removed += 1;
            }
            (got, want) => panic!("第 {} 行：{got:?} 与 {want:?} 不符", c.line),
        }
        // drift 查询的参数 (a, each)
        if let Ok(Some(q)) = allocation_query(inp.last_raw.as_deref()) {
            let ps = queries(&c, "drift");
            if !ps.is_empty() {
                let each = match q.each {
                    quota::PyNum::Int(i) => json!(i),
                    quota::PyNum::Float(x) => f(x),
                };
                assert_eq!(ps, vec![&vec![q.a.clone(), each]], "第 {} 行", c.line);
            }
        }
    }
    assert!(errs > 5 && empties > 100, "{errs} {empties} {removed}");
}

#[test]
fn allocation_and_autopilot_ticks_golden() {
    for c in load(M, "modules/ticks_allocation") {
        let r = &c.raw["inputs"]["r"];
        let got = allocation_tick_summary(if r.is_null() { None } else { Some(r) });
        assert_eq_py_ordered(&c, &json!(got), &c.raw["out"]);
    }
    for c in load(M, "modules/ticks_autopilot") {
        let got = autopilot_tick_summary(&c.raw["inputs"]["r"]);
        assert_eq_py_ordered(&c, &json!(got), &c.raw["out"]);
    }
}

#[test]
fn registration_golden() {
    for c in load(M, "modules/checks_registration") {
        // 夹具里的注册服务数字还带着已删除校验用的字段（每人张数 / 环境变量），这里只取领 Key 校验用的两项
        let r = &c.raw["inputs"]["registrar"];
        let reg = (!r.is_null()).then(|| RegistrarInfo {
            max_users: r["max_users"].as_i64().unwrap(),
            active: r["active"].as_i64().unwrap(),
        });
        assert_eq_py_ordered(
            &c,
            &checks_json(&registration_checks(reg.as_ref())),
            &c.raw["out"],
        );
    }
}

#[test]
fn observation_golden() {
    let ck = Clock::new("Asia/Shanghai", 7);
    for c in load(M, "modules/checks_observation") {
        let i = &c.raw["inputs"];
        let day = i["day"].as_str().unwrap();
        let start = observation_start(&ck, day).unwrap();
        assert_eq!(
            queries(&c, "logged"),
            vec![&vec![f(start)]],
            "第 {} 行",
            c.line
        );
        let got = observation_checks(
            i["logged"].as_i64().unwrap(),
            i["audited"].as_i64().unwrap(),
            i["quota_state"].as_str(),
        );
        match (got, c.raw["err"].as_str()) {
            (Ok(cs), None) => assert_eq_py_ordered(&c, &checks_json(&cs), &c.raw["out"]),
            (Err(e), Some(t)) => assert_eq!(e.py_type(), t, "第 {} 行", c.line),
            (got, want) => panic!("第 {} 行：{got:?} 与 {want:?} 不符", c.line),
        }
    }
}

#[test]
fn risk_golden() {
    for c in load(M, "modules/checks_risk") {
        let items: Vec<Value> = de(&c.raw["inputs"]["items"]);
        let cs = risk_checks(&items).unwrap();
        assert_eq_py_ordered(&c, &checks_json(&cs), &c.raw["out"]);
    }
}

#[test]
fn params_golden() {
    for c in load(M, "modules/params") {
        let name = c.raw["fn"].as_str().unwrap();
        let out = &c.raw["out"];
        match name {
            "capacity_params" => {
                let i = &c.raw["inputs"];
                let burst = i["daily_burst"].as_i64();
                let cap = i["account_daily_cap"].as_i64().unwrap();
                assert_eq!(
                    json!(capacity_burst_text(burst, cap)),
                    out["按时段放行"],
                    "第 {} 行",
                    c.line
                );
            }
            "allocation_params" => {
                assert_eq!(json!(allocation_rule_text()), out["规则"]);
                assert_eq!(json!(allocation_range_text()), out["范围"]);
                // 旧版是「V5 每人范围」3～30 张；3.0 换成公式说明（#63）
                assert_eq!(out["V5 每人范围"], "3～30 张");
                assert_eq!(
                    allocation_v5_rule_text(),
                    "全站池 ÷ max(10, 前 3 天用过 V5 的人数)，至少 1 张"
                );
            }
            "autopilot_params" => {
                assert_eq!(json!(AUTOPILOT_BREAKER_TEXT), out["全站熔断"]);
                assert_eq!(json!(AUTOPILOT_KEY_GUARD_TEXT), out["单个 Key 限流"]);
                assert_eq!(json!(autopilot_slots_text()), out["名额"]);
            }
            "registration_params" => {
                let days = c.raw["inputs"]["days"].as_i64().unwrap();
                assert_eq!(json!(idle_recycle_text(days)), out["闲置回收"]);
            }
            other => panic!("未知 {other}"),
        }
    }
    let _ = modules::MODULES.len();
}
