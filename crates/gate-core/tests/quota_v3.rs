//! 3.0 的额度规则（站长 10-15 批准，docs/v3-deviations.md #61 起）的直接测试。
//!
//! 对拍夹具只能证明「和旧版一样」；这里证明「和新规则一样」：输入都是现场构造的合成数据。

use gate_core::clock::Clock;
use gate_core::quota::{self, Effect, QuotaInputs};
use serde_json::{Value, json};

/// 2030-03-15 08:00（Asia/Shanghai，服务日 7 点切换）。
const NOW: f64 = 1_899_781_200.0;

fn clock() -> Clock {
    Clock::new("Asia/Shanghai", 7)
}

/// 一个今天还没做过每日微调的普通输入：昨天 3 个成员、每人上限 150。
fn base_inputs() -> Value {
    json!({
        "now": NOW,
        "settings": {"quota_ceiling": "150", "quota_algo_day": "2030-03-14"},
        "guard": {"account_daily_cap": 1000},
        "pool_len": 1,
        "global_daily_v5_default": 150,
        "members": 3,
        "daily": {
            "yesterday": {
                "counters": [
                    {"images": 40, "daily_images": 150, "legacy_free_images": 40, "quota_auto": 1},
                    {"images": 20, "daily_images": 150, "legacy_free_images": 20, "quota_auto": 1},
                    {"images": 5, "daily_images": 150, "legacy_free_images": 5, "quota_auto": 1}
                ],
                "upstream_images": 65,
                "upstream_units": 65.0,
                "hourly_block_hours": [],
                "paced_blocks": 0
            },
            "coverage_first_ts": 1_800_000_000.0,
            "member_usage": [40, 20, 5],
            "v5_rows": []
        },
        "accounts": [],
        "quiet": {"snaps": [], "counts": []},
        "active_v5": 0,
        "managed_keys": [{"daily_images": 150, "daily_v5": 3, "image_model_scope": "all"}]
    })
}

fn run(v: Value) -> quota::QuotaOutputs {
    let inp: QuotaInputs = serde_json::from_value(v).expect("合成输入合法");
    quota::decide(&inp, &clock())
}

fn setting<'a>(out: &'a quota::QuotaOutputs, key: &str) -> Option<&'a str> {
    out.effects.iter().rev().find_map(|e| match e {
        Effect::Set { key: k, value } if k == key => Some(value.as_str()),
        Effect::Bulk { pairs } => pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str()),
        _ => None,
    })
}

#[test]
fn the_clock_is_where_we_think() {
    assert_eq!(clock().day(NOW), "2030-03-15");
}

// ---------------------------------------------------------------- #61 保底删除

#[test]
fn no_base_anywhere() {
    let out = run(base_inputs());
    assert_eq!(out.error, None);
    let result = out.result.as_ref().unwrap();
    // 结果里没有保底
    assert!(result.get("base").is_none(), "{result}");
    let review = &result["review"];
    assert_eq!(review["from"], json!([150]));
    assert_eq!(review["to"], json!([150]));
    assert!(review.get("base_blocks").is_none());
    // 不写保底相关的设置
    for e in &out.effects {
        match e {
            Effect::Set { key, .. } => assert_ne!(key, "quota_base_now"),
            Effect::Bulk { pairs } => {
                assert!(
                    pairs.iter().all(|(k, _)| k != "quota_base_now"),
                    "{pairs:?}"
                )
            }
            Effect::Log { target, detail, .. } => {
                assert!(
                    !target.contains("保底") && !detail.contains("保底"),
                    "{e:?}"
                )
            }
            _ => {}
        }
    }
    // 首页文案也不提保底
    assert!(!setting(&out, "algo_notice").unwrap().contains("保底"));
}

#[test]
fn only_a_changes_are_logged_as_adjustments() {
    // 旧版的 quota_base_now 与 A 不同也会记一条「调整」；3.0 不读它
    let mut v = base_inputs();
    v["settings"]["quota_base_now"] = json!("80");
    let out = run(v);
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::Log { action, .. } if action == "动态额度调整")),
        "{:?}",
        out.effects
    );
    // A 真的变了才记，目标只写上限
    let mut v = base_inputs();
    v["daily"]["member_usage"] = json!([150, 20, 5]);
    let out = run(v);
    let target = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::Log { action, target, .. } if action == "动态额度调整" => {
                Some(target.clone())
            }
            _ => None,
        })
        .expect("A 变了要记一条调整");
    assert_eq!(target, "上限 150→175");
}

#[test]
fn quota_cfg_has_no_base_settings() {
    let cfg = serde_json::to_value(quota::QuotaCfg::default()).unwrap();
    for k in ["quota_base", "quota_base_min", "quota_base_step"] {
        assert!(cfg.get(k).is_none(), "{k}");
    }
    assert!(!quota::SETTING_KEYS_READ.contains(&"quota_base_now"));
}
