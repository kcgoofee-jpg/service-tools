//! 3.0 的额度规则（站长 10-15 批准）的手写测试：规则本身、边界，以及不变量（proptest）。
//!
//! 输入都是现场构造的合成数据。服务层（取数、落库、定时任务）的测试在 gate-server 的 `jobs_quota.rs`。

use gate_core::clock::Clock;
use gate_core::quota::{self, Effect, QuotaInputs};
use gate_core::{jsondump, pyc};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 2030-03-15 08:00（Asia/Shanghai，服务日 7 点切换）。
const NOW: f64 = 1_899_763_200.0;

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

// ---------------------------------------------------------------- 保底删除

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

// ---------------------------------------------------------------- 每人注水删除

#[test]
fn notice_has_no_per_member_pacing() {
    // A = 150 > 126：旧版会写「每小时自动恢复 4 张，当天最多约 126 张」
    let out = run(base_inputs());
    let notice = setting(&out, "algo_notice").unwrap();
    assert!(
        notice.contains("今日额度：V4.5 每人 150 张/天（每日 7 点重置）"),
        "{notice}"
    );
    assert!(
        !notice.contains("自动恢复") && !notice.contains("最多约"),
        "{notice}"
    );
}

#[test]
fn someone_reaching_a_raises_it_by_a_step() {
    // 注水删掉以后用满 A 才可能发生：有人昨天实际用到了 150 → A + 25
    let cfg = quota::QuotaCfg::default();
    let (n, why) = quota::simple(Some(150), &[150, 20, 5], 1000, &cfg);
    assert_eq!(n, 175, "{why}");
    assert!(why.contains("1 人用满 150 张：放宽"), "{why}");
    // 差一张都不算用满
    let (n, why) = quota::simple(Some(150), &[149, 20, 5], 1000, &cfg);
    assert_eq!(n, 150, "{why}");
    // 端到端：复盘按昨天的实际用量把 A 调到 175，新 A 当天就能用满（文案里没有「最多约」）
    let mut v = base_inputs();
    v["daily"]["member_usage"] = json!([150, 20, 5]);
    let out = run(v);
    assert_eq!(out.result.as_ref().unwrap()["ceiling"], 175);
    assert!(
        setting(&out, "algo_notice")
            .unwrap()
            .contains("V4.5 每人 175 张/天（每日 7 点重置）")
    );
}

// ---------------------------------------------------------------- V5 公式

/// 没有恢复速度读数：按保守的 5%/天 → G = ⌊5 × 14.2⌋ = 71。
const G: i64 = 71;

fn v5_of(out: &quota::QuotaOutputs) -> Value {
    out.result.as_ref().unwrap()["v5"].clone()
}

#[test]
fn each_is_pool_over_active_people_without_clamp() {
    for (active, each) in [
        (0, 7),   // 不到 10 人按 10 人：⌊71 ÷ 10⌋
        (10, 7),  //
        (20, 3),  // ⌊71 ÷ 20⌋
        (35, 3),  // ⌊71 ÷ 35⌋ = 2 → 至少 3（10-17 起下限 3，全站池先到先得）
        (75, 3),  // ⌊71 ÷ 75⌋ = 0 → 至少 3
        (500, 3), //
    ] {
        let mut v = base_inputs();
        v["active_v5"] = json!(active);
        let out = run(v);
        let v5 = v5_of(&out);
        assert_eq!(
            (v5["global"].as_i64(), v5["each"].as_i64()),
            (Some(G), Some(each)),
            "active={active}"
        );
        assert_eq!(v5["people"], std::cmp::max(10, active));
        assert!(v5.get("k").is_none(), "不再有电量系数");
        // 新 Key 的默认值和下发用同一个数
        assert_eq!(
            setting(&out, "register_daily_v5"),
            Some(each.to_string().as_str())
        );
        assert_eq!(
            setting(&out, "global_daily_v5"),
            Some(G.to_string().as_str())
        );
        assert!(
            out.effects.contains(&Effect::UpdateKeys {
                daily_images: 150,
                daily_v5: each
            }) || each == 3
        );
    }
}

#[test]
fn v5_pool_adds_up_across_upstream_accounts() {
    // 剩余 % 与恢复速度是单个账号的平均；全站池 = ⌊速度 × 账号数 × 14.2⌋（站长 10-17）
    for (accounts, pool) in [(1, 71), (2, 142), (3, 213)] {
        let mut v = base_inputs();
        v["accounts"] = json!(vec![
            json!({"percent": 90, "recharge_per_day": null});
            accounts
        ]);
        v["active_v5"] = json!(40);
        let out = run(v);
        let v5 = v5_of(&out);
        assert_eq!(v5["global"], pool, "accounts={accounts}");
        assert_eq!(
            v5["each"],
            std::cmp::max(3, pool / 40),
            "accounts={accounts}"
        );
    }
}

#[test]
fn battery_level_no_longer_changes_the_pool() {
    // 旧版：剩余 ≥ 85% 时全站至少 260、每人至少 10，首页写「当前电池充沛…额度放宽」；剩余低时乘 0.3～0.6
    for percent in [95, 50, 5] {
        let mut v = base_inputs();
        v["accounts"] = json!([{"percent": percent, "recharge_per_day": null}]);
        let out = run(v);
        let v5 = v5_of(&out);
        assert_eq!(
            (v5["global"].as_i64(), v5["each"].as_i64()),
            (Some(G), Some(7))
        );
        assert_eq!(v5["percent"], json!(percent as f64), "剩余只用于显示");
        let notice = setting(&out, "algo_notice").unwrap();
        assert!(!notice.contains("额度放宽"), "{notice}");
    }
}

#[test]
fn todays_plan_is_fixed_for_the_whole_day() {
    // 早上 07:00 后第一次计算时定下（这里是 G = 71、每人 7），写进当天方案
    let out = run(base_inputs());
    let plan = setting(&out, quota::V5_DAY_KEY)
        .expect("第一次计算要写当天方案")
        .to_string();
    let plan_v: Value = serde_json::from_str(&plan).unwrap();
    assert_eq!(plan_v["day"], "2030-03-15");
    assert_eq!(plan_v["plan"]["each"], 7);
    // 白天读数变了（电量满了、活跃人数变了）：沿用早上的方案，不放宽也不收紧、不重写
    let mut v = base_inputs();
    v["now"] = json!(NOW + 6.0 * 3600.0);
    v["settings"]["quota_algo_day"] = json!("2030-03-15");
    v["settings"][quota::V5_DAY_KEY] = json!(plan);
    v["accounts"] = json!([{"percent": 99, "recharge_per_day": null}]);
    v["active_v5"] = json!(40);
    let out = run(v);
    let v5 = v5_of(&out);
    assert_eq!(
        (v5["global"].as_i64(), v5["each"].as_i64()),
        (Some(G), Some(7))
    );
    assert!(setting(&out, quota::V5_DAY_KEY).is_none(), "当天不再改");
    // 第二天 07:00 后重新定（昨天的方案作废）
    let mut v = base_inputs();
    v["now"] = json!(NOW + 86400.0);
    v["settings"][quota::V5_DAY_KEY] = json!(plan);
    v["active_v5"] = json!(40);
    let out = run(v);
    assert_eq!(v5_of(&out)["each"], 3, "⌊71 ÷ 40⌋ = 1 → 下限 3");
    assert!(
        setting(&out, quota::V5_DAY_KEY)
            .unwrap()
            .contains("2030-03-16")
    );
}

#[test]
fn oversell_is_judged_by_active_people_not_all_keys() {
    // 200 把算法管理的 Key，但前 3 天只有 20 人用过 V5：每人 3 × 20 = 60 ≤ 71，不算超卖
    let mut v = base_inputs();
    v["members"] = json!(200);
    v["active_v5"] = json!(20);
    let out = run(v);
    let v5 = v5_of(&out);
    assert_eq!(v5["oversell"], Value::Null);
    assert!(!setting(&out, "algo_notice").unwrap().contains("先到先得"));
    // 100 人用过 V5：每人至少 3 张，3 × 100 > 71 → 先到先得
    let mut v = base_inputs();
    v["members"] = json!(200);
    v["active_v5"] = json!(100);
    let out = run(v);
    let v5 = v5_of(&out);
    assert_eq!(
        v5["oversell"],
        json!({"each": 3, "members": 100, "global": G})
    );
    assert!(
        setting(&out, "algo_notice")
            .unwrap()
            .contains("V5 每人 3 张（全站今日共 71 张，先到先得）")
    );
}

#[test]
fn active_people_are_counted_over_the_previous_three_full_service_days() {
    let ck = clock();
    // 2030-03-15 08:00：服务日是 03-15，前 3 个完整服务日是 12～14 日
    assert_eq!(
        quota::active_v5_days(&ck, NOW, quota::V5_ACTIVE_DAYS),
        ["2030-03-12", "2030-03-13", "2030-03-14"]
    );
    // 03-15 06:00 还属于 03-14 这个服务日：前 3 个完整服务日是 11～13 日（旧版按自然日会数进 15 日）
    assert_eq!(
        quota::active_v5_days(&ck, NOW - 2.0 * 3600.0, 3),
        ["2030-03-11", "2030-03-12", "2030-03-13"]
    );
}

#[test]
fn removed_v5_settings_are_not_read() {
    // quota_v5_min / quota_v5_max（3～30 夹取）不再读
    let mut v = base_inputs();
    v["settings"]["quota_v5_min"] = json!("20");
    v["settings"]["quota_v5_max"] = json!("2");
    v["active_v5"] = json!(75);
    let out = run(v);
    assert_eq!(v5_of(&out)["each"], 3);
    assert!(!quota::SETTING_KEYS_READ.contains(&"quota_v5_min"));
    assert!(!quota::SETTING_KEYS_READ.contains(&"quota_v5_max"));
}

// ---------------------------------------------------------------- 节约模式残留

#[test]
fn yesterday_used_is_the_real_image_count() {
    // 旧版会按账号层面「节约模式折算值 / 实际张数」把 used 打折；3.0 就是成员实际出图之和
    let y: quota::YesterdayInputs = serde_json::from_value(json!({
        "counters": [
            {"images": 40, "daily_images": 150, "legacy_free_images": 40, "quota_auto": 1},
            {"images": 25, "daily_images": 150, "legacy_free_images": 25, "quota_auto": 1}
        ],
        "hourly_block_hours": [], "paced_blocks": 0
    }))
    .unwrap();
    assert_eq!(quota::yesterday_stats(&y).used, 65);
    // 输入里已经没有折算用的字段
    assert!(
        serde_json::from_value::<quota::YesterdayInputs>(json!({
            "counters": [], "hourly_block_hours": [], "paced_blocks": 0,
            "upstream_images": 10, "upstream_units": 5.0
        }))
        .is_err()
    );
}

// ---------------------------------------------------------------- 每人上限 A 的规则

#[test]
fn simple_falls_back_to_the_target_average_when_a_is_zero() {
    let cfg = quota::QuotaCfg::default();
    for prev in [None, Some(0)] {
        let (n, why) = quota::simple(prev, &[10, 5], 1000, &cfg);
        assert_eq!(n, 150, "{prev:?}");
        assert!(why.contains("没人用满：不变"), "{why}");
        // 有人用满目标均值：从目标均值起 +25
        assert_eq!(quota::simple(prev, &[150], 1000, &cfg).0, 175);
    }
    let cfg80 = quota::QuotaCfg {
        quota_target_avg: 80,
        ..Default::default()
    };
    assert_eq!(quota::simple(Some(0), &[1], 1000, &cfg80).0, 80);
    // 端到端：库里的上限被写成 0，复盘时退回目标均值
    let mut v = base_inputs();
    v["settings"]["quota_ceiling"] = json!("0");
    let out = run(v);
    assert_eq!(out.result.as_ref().unwrap()["ceiling"], 150);
}

#[test]
fn quota_cfg_falls_back_on_bad_values_but_inf_is_an_error() {
    let one = |k: &str, v: &str| HashMap::from([(k.to_string(), v.to_string())]);
    for bad in ["abc", "", "nan", "1_0_"] {
        assert_eq!(
            quota::quota_cfg(&one("quota_step", bad))
                .unwrap()
                .quota_step,
            25,
            "{bad:?}"
        );
    }
    assert_eq!(
        quota::quota_cfg(&one("quota_step", "30.9"))
            .unwrap()
            .quota_step,
        30
    );
    for inf in ["inf", "-inf", "Infinity"] {
        let e = quota::quota_cfg(&one("quota_ceiling_max", inf)).unwrap_err();
        assert_eq!(e.py_type(), "OverflowError", "{inf}");
    }
    // 整轮：设置里有 inf 就报错，之前什么也没写
    let mut v = base_inputs();
    v["settings"]["quota_step"] = json!("inf");
    let out = run(v);
    assert_eq!(out.error.as_deref(), Some("OverflowError"));
    assert!(out.effects.is_empty(), "{:?}", out.effects);
}

#[test]
fn history_keeps_the_last_29_plus_today() {
    let old: Vec<Value> = (0..40)
        .map(|i| json!({"day": format!("d{i:02}")}))
        .collect();
    let mut v = base_inputs();
    v["settings"][quota::HISTORY_KEY] = json!(jsondump::dumps(&json!(old)));
    let out = run(v);
    let hist: Value = serde_json::from_str(setting(&out, quota::HISTORY_KEY).unwrap()).unwrap();
    let hist = hist.as_array().unwrap();
    assert_eq!(hist.len(), 30);
    assert_eq!(hist[0]["day"], "d11", "最旧的 11 条被挤掉");
    assert_eq!(hist[28]["day"], "d39");
    assert_eq!(hist[29]["day"], "2030-03-14", "最后一条是今天复盘的昨天");
}

#[test]
fn service_day_boundaries() {
    let ck = clock();
    let (start, end) = quota::day_window(&ck, "2030-03-14").unwrap();
    assert_eq!(end - start, 86400.0);
    assert_eq!(ck.format(start, "%Y-%m-%d %H:%M"), "2030-03-14 07:00");
    assert_eq!(ck.day(start), "2030-03-14");
    assert_eq!(ck.day(start - 1.0), "2030-03-13");
    assert!(quota::day_window(&ck, "不是日期").is_err());
    // 07:00 整点切到新的服务日：前 3 个完整服务日跟着往后挪一天
    let s15 = ck.service_day_start("2030-03-15").unwrap();
    assert_eq!(
        quota::active_v5_days(&ck, s15, 3),
        ["2030-03-12", "2030-03-13", "2030-03-14"]
    );
    assert_eq!(
        quota::active_v5_days(&ck, s15 - 1.0, 3),
        ["2030-03-11", "2030-03-12", "2030-03-13"]
    );
    // 复盘只在服务日第一次跑时做
    assert!(quota::daily_adjust_due(Some("2030-03-14"), "2030-03-15"));
    assert!(quota::daily_adjust_due(None, "2030-03-15"));
    assert!(!quota::daily_adjust_due(Some("2030-03-15"), "2030-03-15"));
}

// ---------------------------------------------------------------- water-fill

/// Σ min(用量, b)
fn filled(usage: &[i64], b: i64) -> i64 {
    usage.iter().map(|u| (*u).max(0).min(b)).sum()
}

#[test]
fn water_fill_examples() {
    // 用得少的人用多少算多少，剩下的平分给用得多的人
    assert_eq!(quota::water_fill(&[10, 10], 15.0), 7);
    assert_eq!(quota::water_fill(&[2, 50, 50], 42.0), 20);
    // 需求不到预算：人人都满足，返回最大用量
    assert_eq!(quota::water_fill(&[3, 8], 100.0), 8);
    assert_eq!(quota::water_fill(&[], 100.0), 0);
    assert_eq!(quota::water_fill(&[0, 0], 100.0), 0);
    // 预算很小也至少 1 张
    assert_eq!(quota::water_fill(&[50, 50, 50], 1.0), 1);
}

proptest! {
    /// 需求超过预算时，b 是水位线取整：Σmin(用量, b) ≤ 预算 ≤ Σmin(用量, b+1)，再低一张就不够分。
    /// （水位线本身满足 Σmin(用量, L) = 预算；取整后 b = ⌊L⌋，所以是夹逼而不是严格的 ≥ 预算。）
    #[test]
    fn water_fill_is_the_floor_of_the_water_level(
        usage in prop::collection::vec(0i64..500, 1..40),
        budget in 1i64..5000,
    ) {
        let total: i64 = usage.iter().sum();
        let b = quota::water_fill(&usage, budget as f64);
        if total >= budget {
            prop_assert!(b >= 1);
            prop_assert!(filled(&usage, b + 1) >= budget, "b={}", b);
            if b >= 2 {
                prop_assert!(filled(&usage, b) <= budget, "b={}", b);
                prop_assert!(filled(&usage, b - 1) < budget, "b={} 不是最小的", b);
            }
        } else {
            prop_assert_eq!(b, usage.iter().copied().max().unwrap_or(0));
        }
    }
}

// ---------------------------------------------------------------- 不变量

/// 把一轮的写入落到下一轮的输入上（模拟服务层：设置表 + 算法管理的 Key）。
fn apply(inp: &mut QuotaInputs, effects: &[Effect]) {
    for e in effects {
        match e {
            Effect::Set { key, value } => {
                inp.settings.insert(key.clone(), value.clone());
            }
            Effect::Bulk { pairs } => {
                for (k, v) in pairs {
                    inp.settings.insert(k.clone(), v.clone());
                }
            }
            Effect::UpdateKeys {
                daily_images,
                daily_v5,
            } => {
                for k in &mut inp.managed_keys {
                    k.daily_images = *daily_images;
                    k.daily_v5 = *daily_v5;
                    k.image_model_scope = "all".into();
                }
            }
            _ => {}
        }
    }
}

fn wrote(out: &quota::QuotaOutputs, key: &str) -> bool {
    out.effects.iter().any(|e| match e {
        Effect::Set { key: k, .. } => k == key,
        Effect::Bulk { pairs } => pairs.iter().any(|(k, _)| k == key),
        _ => false,
    })
}

/// 写进 `site_settings` 的值：凡是 JSON 的，`dumps(loads(v)) == v`（2.x 读得懂、格式不漂移），浮点都是有限值。
fn assert_db_format(out: &quota::QuotaOutputs) -> Result<(), TestCaseError> {
    fn finite(v: &Value) -> bool {
        match v {
            Value::Number(n) => n.as_f64().is_some_and(f64::is_finite),
            Value::Array(a) => a.iter().all(finite),
            Value::Object(m) => m.values().all(finite),
            _ => true,
        }
    }
    let mut values: Vec<&str> = Vec::new();
    for e in &out.effects {
        match e {
            Effect::Set { value, .. } => values.push(value),
            Effect::Bulk { pairs } => values.extend(pairs.iter().map(|(_, v)| v.as_str())),
            _ => {}
        }
    }
    for v in values {
        if let Some(parsed) = jsondump::loads(v) {
            prop_assert_eq!(jsondump::dumps(&parsed), v);
            prop_assert!(finite(&parsed), "{}", v);
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Case {
    lo: i64,
    hi: i64,
    step: i64,
    prev: i64,
    usage: Vec<i64>,
    full_coverage: bool,
    cap: i64,
    pool: usize,
    global_v5: Option<i64>,
    percent: Option<i64>,
    recharge: Option<f64>,
    active: i64,
    managed: Vec<(i64, i64, bool)>,
    due: bool,
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        (1i64..=200, 0i64..=400, 1i64..=60, 0.0f64..=1.0),
        prop::collection::vec(0i64..=400, 0..25),
        any::<bool>(),
        (100i64..=3000, 1usize..=4, prop::option::of(0i64..=2000)),
        (
            prop::option::of(0i64..=100),
            prop::option::of(0.5f64..=30.0),
        ),
        0i64..=300,
        prop::collection::vec((0i64..=400, 0i64..=50, any::<bool>()), 0..10),
        any::<bool>(),
    )
        .prop_map(
            |(
                (lo, span, step, pos),
                usage,
                full_coverage,
                (cap, pool, global_v5),
                (percent, recharge),
                active,
                managed,
                due,
            )| {
                let hi = lo + span;
                let prev = lo + ((hi - lo) as f64 * pos).round() as i64;
                Case {
                    lo,
                    hi,
                    step,
                    prev,
                    usage,
                    full_coverage,
                    cap,
                    pool,
                    global_v5,
                    percent,
                    recharge,
                    active,
                    managed,
                    due,
                }
            },
        )
}

fn case_inputs(c: &Case) -> QuotaInputs {
    let mut settings = json!({
        "quota_base_min_fair": c.lo.to_string(),
        "quota_ceiling_max": c.hi.to_string(),
        "quota_step": c.step.to_string(),
        "quota_ceiling": c.prev.to_string(),
        "quota_algo_day": if c.due { "2030-03-14" } else { "2030-03-15" },
    });
    if let Some(g) = c.global_v5 {
        settings["global_daily_v5"] = json!(g.to_string());
    }
    let accounts = match c.percent {
        Some(p) => json!([{"percent": p, "recharge_per_day": c.recharge}]),
        None => json!([]),
    };
    // 不完整的覆盖：昨天 22:00 才开始有日志（9 小时）
    let first_ts = if c.full_coverage {
        1_800_000_000.0
    } else {
        NOW - 10.0 * 3600.0
    };
    let daily = if c.due {
        json!({
            "yesterday": {"counters": [], "hourly_block_hours": [], "paced_blocks": 0},
            "coverage_first_ts": first_ts,
            "member_usage": c.usage,
            "v5_rows": [],
        })
    } else {
        Value::Null
    };
    let managed: Vec<Value> = c
        .managed
        .iter()
        .map(|(d, v5, all)| {
            json!({
                "daily_images": d, "daily_v5": v5,
                "image_model_scope": if *all { "all" } else { "v45" },
            })
        })
        .collect();
    let v = json!({
        "now": NOW,
        "settings": settings,
        "guard": {"account_daily_cap": c.cap},
        "pool_len": c.pool,
        "global_daily_v5_default": 150,
        "members": c.managed.len(),
        "daily": daily,
        "accounts": accounts,
        "quiet": {"snaps": [], "counts": []},
        "active_v5": c.active,
        "managed_keys": managed,
    });
    serde_json::from_value(v).expect("合成输入合法")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A ∈ [min_fair, ceiling_max]；每人 V5 ≥ 1；G = ⌊采用的恢复速度 × 14.2⌋；写库格式不漂移。
    #[test]
    fn a_stays_in_range_and_v5_follows_the_formula(c in case_strategy()) {
        let inp = case_inputs(&c);
        let out = quota::decide(&inp, &clock());
        prop_assert_eq!(&out.error, &None);
        let r = out.result.as_ref().unwrap();
        let a = r["ceiling"].as_i64().unwrap();
        prop_assert!(c.lo <= a && a <= c.hi, "A={} 不在 [{}, {}]", a, c.lo, c.hi);
        let (_, api) = quota::allowance_summary(&inp.accounts);
        let rate = quota::choose_rate(api, None).0;
        let g = (rate * quota::V5_IMAGES_PER_PERCENT).floor() as i64;
        prop_assert_eq!(r["v5"]["global"].as_i64(), Some(g));
        let each = r["v5"]["each"].as_i64().unwrap();
        prop_assert!(each >= quota::V5_MIN_EACH);
        prop_assert_eq!(each, std::cmp::max(quota::V5_MIN_EACH, g / std::cmp::max(10, c.active)));
        prop_assert_eq!(r["review"].is_object(), c.due);
        assert_db_format(&out)?;
    }

    /// 同一个服务日里：方案不变、只复盘一次——不管白天的余量、活跃人数怎么变。
    #[test]
    fn todays_plan_and_review_happen_once(
        c in case_strategy(),
        later in 60.0f64..(22.0 * 3600.0),
        percent2 in 0i64..=100,
        active2 in 0i64..=300,
    ) {
        let mut inp = case_inputs(&c);
        let first = quota::decide(&inp, &clock());
        let r1 = first.result.clone().unwrap();
        apply(&mut inp, &first.effects);
        inp.now = NOW + later;
        inp.accounts = serde_json::from_value(
            json!([{"percent": percent2, "recharge_per_day": 12.0}]),
        )
        .unwrap();
        inp.active_v5 = active2;
        let second = quota::decide(&inp, &clock());
        prop_assert_eq!(&second.error, &None);
        let r2 = second.result.as_ref().unwrap();
        prop_assert_eq!(&r2["ceiling"], &r1["ceiling"]);
        prop_assert_eq!(&r2["v5"]["global"], &r1["v5"]["global"]);
        prop_assert_eq!(&r2["v5"]["each"], &r1["v5"]["each"]);
        prop_assert!(r2["review"].is_null(), "同一天复盘了第二次");
        prop_assert!(!wrote(&second, quota::HISTORY_KEY));
        prop_assert!(!wrote(&second, quota::V5_DAY_KEY));
        assert_db_format(&second)?;
    }

    /// 幂等：同一时刻对同一份输入（落库之后）再跑一次，不再下发 Key、不再复盘。
    #[test]
    fn running_twice_changes_nothing_the_second_time(c in case_strategy()) {
        let mut inp = case_inputs(&c);
        let first = quota::decide(&inp, &clock());
        apply(&mut inp, &first.effects);
        let second = quota::decide(&inp, &clock());
        prop_assert_eq!(&second.error, &None);
        prop_assert!(
            !second.effects.iter().any(|e| matches!(e, Effect::UpdateKeys { .. })),
            "{:?}",
            second.effects
        );
        let r2 = second.result.as_ref().unwrap();
        prop_assert!(r2["review"].is_null());
        prop_assert_eq!(r2["changed_keys"].as_i64(), Some(0));
    }

    /// 开关关着：什么都不写，结果只有 `{"enabled": false}`。
    #[test]
    fn switched_off_writes_nothing(c in case_strategy()) {
        let mut inp = case_inputs(&c);
        inp.settings.insert("quota_auto_enabled".into(), "0".into());
        let out = quota::decide(&inp, &clock());
        prop_assert!(out.effects.is_empty(), "{:?}", out.effects);
        prop_assert_eq!(out.result, Some(json!({"enabled": false})));
    }
}

#[test]
fn float_settings_use_the_python_repr() {
    // 写库的浮点（V5 方案里的剩余等）是 Python `repr` 的形状：63 写成 63.0
    let mut v = base_inputs();
    v["accounts"] = json!([{"percent": 63, "recharge_per_day": 11.0}]);
    let out = run(v);
    let plan = setting(&out, quota::V5_DAY_KEY).unwrap();
    let parsed = jsondump::loads(plan).unwrap();
    assert_eq!(jsondump::dumps(&parsed), plan);
    assert!(plan.contains("\"percent\": 63.0"), "{plan}");
    assert_eq!(pyc::float_repr(5.0), "5.0");
}
