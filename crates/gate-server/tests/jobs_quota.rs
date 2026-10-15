//! 定时任务「动态额度」：真实 SQLite + 假上游 + 冻结时钟，从取数、`quota::decide` 到落库走一遍。
//!
//! 规则本身（A 的调整、water-fill、V5 公式）的边界在 gate-core 的 `quota_rules.rs`；这里守的是服务层：
//! 复盘时机、取数口径、下发到 Key、开关、出错时的行为、和后台重算的串行化，以及连续多天跑下来的不变量。
//! 公开仓库：全部是合成数据；上游是本地假服务。

mod common;
use common::*;
use gate_core::jsondump;
use gate_testkit::Reply;
use serde_json::{Value, json};

/// 2026-10-14 07:00:00 +08:00（服务日 2026-10-14 的起点）。
const D7: f64 = 1_791_932_400.0;
/// 2026-10-14 12:00:00 +08:00
const NOON: f64 = D7 + 5.0 * 3600.0;
const TODAY: &str = "2026-10-14";
const YDAY: &str = "2026-10-13";

/// 动态额度会写的设置键。
const QUOTA_KEYS: [&str; 10] = [
    "quota_ceiling",
    "quota_algo_day",
    "quota_algo_history",
    "quota_v5_day_plan",
    "global_daily_v5",
    "register_daily_images",
    "register_daily_v5",
    "register_image_scope",
    "algo_notice",
    "quota_algo_last",
];

async fn set(e: &Env, k: &str, v: &str) {
    e.state.db.set_setting(k, v).await.unwrap();
}

async fn sql(e: &Env, stmts: Vec<String>) {
    e.state.db.run_script(stmts).await.unwrap();
}

/// 3 天前就开始有日志：复盘时「昨天的数据覆盖 ≥ 20 小时」。
async fn full_coverage(e: &Env) {
    sql(
        e,
        vec![format!(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, status) VALUES ({}, NULL, '', 'text', 'ok')",
            D7 - 3.0 * 86400.0
        )],
    )
    .await;
}

/// 某把 Key 某一天用掉的 V4.5 免费图（复盘按它算「用满」）。
async fn used(e: &Env, key_id: i64, day: &str, n: i64) {
    sql(
        e,
        vec![format!(
            "INSERT INTO counters (key_id, day, images, legacy_free_images) VALUES ({key_id}, '{day}', {n}, {n}) \
             ON CONFLICT(key_id, day) DO UPDATE SET images = images + {n}, legacy_free_images = legacy_free_images + {n}"
        )],
    )
    .await;
}

/// 这把 Key 那一天用过 V5（算进「前 3 天用过 V5 的人数」）。
async fn used_v5(e: &Env, key_id: i64, day: &str) {
    sql(
        e,
        vec![format!(
            "INSERT INTO counters (key_id, day, v5) VALUES ({key_id}, '{day}', 1) \
             ON CONFLICT(key_id, day) DO UPDATE SET v5 = v5 + 1"
        )],
    )
    .await;
}

async fn last(e: &Env) -> Value {
    setting_json(&e.state, "quota_algo_last").await
}

async fn history(e: &Env) -> Vec<Value> {
    setting_json(&e.state, "quota_algo_history")
        .await
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn job_last(state: &gate_server::AppState) -> Value {
    Value::Object(state.jobs.last().await)["quota"].clone()
}

/// 算法管理、没停用的成员 Key 里，额度和算法结果不一致的把数（原「分配」模块的自检，现在是测试里的恒等式）。
async fn drift(e: &Env) -> i64 {
    let r = last(e).await;
    let (a, each) = (
        r["ceiling"].as_i64().unwrap(),
        r["v5"]["each"].as_i64().unwrap(),
    );
    query(
        &e.state.db,
        &format!(
            "SELECT COUNT(*) AS n FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 AND enabled=1 \
             AND (daily_images<>{a} OR daily_v5<>{each} OR image_model_scope<>'all')"
        ),
    )
    .await[0]["n"]
        .as_i64()
        .unwrap()
}

/// 写进库的动态额度设置：凡是 JSON 的，`dumps(loads(v)) == v`（回滚到 2.x 也读得懂）。
async fn assert_db_format(e: &Env) {
    for k in QUOTA_KEYS {
        let Some(v) = setting(&e.state, k).await else {
            continue;
        };
        if let Some(parsed) = jsondump::loads(&v) {
            assert_eq!(jsondump::dumps(&parsed), v, "{k}");
        }
    }
}

async fn quota(e: &Env) -> String {
    run_job(&e.state, "quota")
        .await
        .expect("动态额度这一轮应该成功")
}

// ------------------------------------------------------------------ 复盘时机

#[tokio::test]
async fn review_happens_once_at_the_first_run_after_seven() {
    let _clock = clock(Some(D7 - 60.0)).await; // 06:59，还是 10-13 这个服务日
    let e = setup(&[]).await;
    full_coverage(&e).await;
    set(&e, "quota_algo_day", YDAY).await;

    let s = quota(&e).await;
    assert!(!s.contains("今日复盘"), "{s}");
    assert_eq!(
        setting(&e.state, "quota_algo_day").await.as_deref(),
        Some(YDAY)
    );
    assert!(history(&e).await.is_empty());

    gate_server::time::freeze(Some(D7 + 60.0)); // 07:01：新服务日的第一轮
    let s = quota(&e).await;
    assert!(s.contains("今日复盘：150→150"), "{s}");
    assert_eq!(
        setting(&e.state, "quota_algo_day").await.as_deref(),
        Some(TODAY)
    );
    let h = history(&e).await;
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["day"], YDAY);

    gate_server::time::freeze(Some(D7 + 660.0)); // 07:11
    let s = quota(&e).await;
    assert!(!s.contains("今日复盘"), "{s}");
    assert_eq!(history(&e).await.len(), 1);
    // 摘要和接口里的运行记录一致
    assert_eq!(job_last(&e.state).await["summary"], s);
    assert_eq!(job_last(&e.state).await["ok"], true);
    assert_eq!(job_last(&e.state).await["at"], json!(D7 + 660.0));
}

#[tokio::test]
async fn short_log_coverage_keeps_a() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    let m = member_key(&e.state, 1, &[]).await;
    used(&e, m, YDAY, 150).await; // 用满了，本该 +25
    // 昨天 22:00 才开始有日志：只覆盖 9 小时
    sql(
        &e,
        vec![format!(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, status) VALUES ({}, NULL, '', 'text', 'ok')",
            D7 - 9.0 * 3600.0
        )],
    )
    .await;
    quota(&e).await;
    let review = &last(&e).await["review"];
    assert_eq!(review["to"], json!([150]));
    assert_eq!(review["coverage_hours"], json!(9.0));
    assert_eq!(
        review["reasons"],
        json!(["昨天只有 9.0 小时数据（不足 20 小时），每人额度不变"])
    );
}

// ------------------------------------------------------------------ 每人上限 A

/// 复盘一次，返回 (复盘结果, 操作日志里的「动态额度调整」)。
async fn review_with(extra: &[(&str, &str)], usage: &[i64]) -> (Value, Vec<Value>) {
    let e = setup(&[]).await;
    full_coverage(&e).await;
    for (k, v) in extra {
        set(&e, k, v).await;
    }
    for (i, n) in usage.iter().enumerate() {
        let id = member_key(&e.state, i as i64 + 1, &[]).await;
        used(&e, id, YDAY, *n).await;
    }
    quota(&e).await;
    settle().await;
    let logs = admin_actions(&e.state.db, 50)
        .await
        .into_iter()
        .filter(|r| r["action"] == "动态额度调整")
        .collect();
    (last(&e).await["review"].clone(), logs)
}

#[tokio::test]
async fn a_grows_only_when_someone_used_it_all() {
    let _clock = clock(Some(NOON)).await;
    // 没人用满：不变，不记「调整」
    let (r, logs) = review_with(&[], &[40, 20]).await;
    assert_eq!(r["to"], json!([150]));
    assert!(
        r["reasons"][0].as_str().unwrap().contains("没人用满：不变"),
        "{r}"
    );
    assert!(logs.is_empty());
    // 有人用满：+25，记一条调整
    let (r, logs) = review_with(&[], &[150, 20]).await;
    assert_eq!(r["to"], json!([175]));
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["target"], "上限 150→175");
    // 不超过上限的上限
    let (r, _) = review_with(&[("quota_ceiling_max", "160")], &[150]).await;
    assert_eq!(r["to"], json!([160]));
}

#[tokio::test]
async fn high_demand_is_water_filled_but_not_below_min_fair() {
    let _clock = clock(Some(NOON)).await;
    // 可分 = 1000 − 150 = 850；10 人各用 100 = 1000 ≥ 95% → 平分：⌊850 ÷ 10⌋ = 85
    let (r, _) = review_with(&[], &[100; 10]).await;
    assert_eq!(r["to"], json!([85]));
    let why = r["reasons"][0].as_str().unwrap();
    assert!(why.contains("按需求平分"), "{why}");
    assert!(
        why.contains("（可分 = 账号日上限 1000 − 全站 V5 150）"),
        "{why}"
    );
    // 不低于最低公平值
    let (r, _) = review_with(&[("quota_base_min_fair", "100")], &[100; 10]).await;
    assert_eq!(r["to"], json!([100]));
}

#[tokio::test]
async fn budget_is_accounts_times_daily_cap_minus_global_v5() {
    let _clock = clock(Some(NOON)).await;
    for (tokens, cap, global, want_cap) in [
        ("pst-synthetic-a", 1000, None, 1000),
        ("pst-synthetic-a,pst-synthetic-b", 1000, None, 2000),
        ("pst-synthetic-a", 800, None, 800),
        ("pst-synthetic-a,pst-synthetic-b", 800, Some(400), 1600),
    ] {
        let e = setup(&[("NAI_TOKENS", tokens)]).await;
        full_coverage(&e).await;
        e.state.guard.with(|g| {
            g.values.insert("account_daily_cap".into(), cap);
        });
        if let Some(g) = global {
            set(&e, "global_daily_v5", &g.to_string()).await;
        }
        quota(&e).await;
        let r = last(&e).await;
        let v5 = global.unwrap_or(150);
        assert_eq!(r["cap"], want_cap, "{tokens} {cap}");
        assert_eq!(r["review"]["budget"], want_cap - v5);
        assert!(
            r["review"]["reasons"][0]
                .as_str()
                .unwrap()
                .contains(&format!("账号日上限 {want_cap} − 全站 V5 {v5}")),
            "{r}"
        );
    }
}

#[tokio::test]
async fn only_algorithm_managed_member_keys_are_updated() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    let managed = member_key(&e.state, 1, &[("daily_images", json!(7))]).await;
    let disabled = member_key(&e.state, 2, &[("enabled", json!(0))]).await;
    let manual = member_key(
        &e.state,
        3,
        &[("quota_auto", json!(-1)), ("daily_images", json!(33))],
    )
    .await;
    let test = member_key(
        &e.state,
        4,
        &[("is_test", json!(1)), ("daily_images", json!(44))],
    )
    .await;
    let admin = member_key(
        &e.state,
        5,
        &[("is_admin", json!(1)), ("daily_images", json!(55))],
    )
    .await;
    let s = quota(&e).await;
    assert!(s.contains("同步"), "{s}");
    let row = |id: i64| {
        let db = e.state.db.clone();
        async move { db.get_key(id).await.unwrap().unwrap() }
    };
    for id in [managed, disabled] {
        let k = row(id).await;
        assert_eq!(
            (&k["daily_images"], &k["daily_v5"], &k["image_model_scope"]),
            (&json!(150), &json!(7), &json!("all")),
            "Key {id}"
        );
    }
    for (id, want) in [(manual, 33), (test, 44), (admin, 55)] {
        assert_eq!(row(id).await["daily_images"], want, "Key {id} 不归算法管");
    }
    assert_eq!(drift(&e).await, 0, "算法管理的 Key 都已同步");
    // 再跑一轮：没有要同步的了
    let s = quota(&e).await;
    assert!(!s.contains("同步"), "{s}");
}

#[tokio::test]
async fn switched_off_reads_and_writes_nothing() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    set(&e, "quota_auto_enabled", "0").await;
    e.mock.clear();
    let before = query(
        &e.state.db,
        "SELECT * FROM site_settings WHERE key<>'job_last' ORDER BY key",
    )
    .await;
    assert_eq!(quota(&e).await, "动态额度已关闭，未计算");
    let after = query(
        &e.state.db,
        "SELECT * FROM site_settings WHERE key<>'job_last' ORDER BY key",
    )
    .await;
    assert_eq!(before, after, "开关关着不写任何设置");
    assert!(
        e.mock.requests_to("/user/subscription").is_empty(),
        "开关关着不向上游发订阅查询"
    );
}

// ------------------------------------------------------------------ V5

#[tokio::test]
async fn todays_v5_plan_is_fixed_and_only_the_display_follows_the_live_percent() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    quota(&e).await;
    let plan = setting(&e.state, "quota_v5_day_plan").await.unwrap();
    assert_eq!(last(&e).await["v5"]["percent"], json!(80.0));

    // 白天余量变了：同一个进程里读的是缓存，方案不动
    e.mock.set_v5_percent(30);
    quota(&e).await;
    assert_eq!(setting(&e.state, "quota_v5_day_plan").await.unwrap(), plan);

    // 重启后缓存是空的：重新读到 30%，显示跟着变，方案还是早上那份
    let s2 = e.restart().await;
    run_job(&s2, "quota").await.unwrap();
    assert_eq!(setting(&s2, "quota_v5_day_plan").await.unwrap(), plan);
    let r = setting_json(&s2, "quota_algo_last").await;
    assert_eq!(r["v5"]["percent"], json!(30.0));
    let p: Value = serde_json::from_str(&plan).unwrap();
    assert_eq!(r["v5"]["each"], p["plan"]["each"]);
    assert_eq!(r["v5"]["global"], p["plan"]["global"]);
}

#[tokio::test]
async fn each_is_the_pool_over_active_people_and_oversell_counts_people() {
    let _clock = clock(Some(NOON)).await;
    // 假上游的恢复速度 24%/天超出合理范围，按保守的 5%/天：全站池 ⌊5 × 14.2⌋ = 71
    for (people, each, oversell) in [(0, 7, false), (20, 3, false), (100, 1, true)] {
        let e = setup(&[]).await;
        full_coverage(&e).await;
        for i in 0..people {
            let id = member_key(&e.state, i + 1, &[]).await;
            used_v5(&e, id, ["2026-10-11", "2026-10-12", YDAY][i as usize % 3]).await;
        }
        // 今天用过 V5 的不算（只看前 3 个完整服务日）
        let today_only = member_key(&e.state, 900, &[]).await;
        used_v5(&e, today_only, TODAY).await;
        quota(&e).await;
        let v5 = last(&e).await["v5"].clone();
        assert_eq!(v5["global"], 71, "{people}");
        assert_eq!(v5["each"], each, "{people}");
        assert_eq!(v5["people"], std::cmp::max(10, people));
        let notice = setting(&e.state, "algo_notice").await.unwrap();
        if oversell {
            assert_eq!(
                v5["oversell"],
                json!({"each": each, "members": people, "global": 71})
            );
            assert!(notice.contains("先到先得"), "{notice}");
        } else {
            assert_eq!(v5["oversell"], Value::Null);
            assert!(!notice.contains("先到先得"), "{notice}");
        }
    }
}

#[tokio::test]
async fn quiet_hours_measure_the_recovery_rate() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    // 6 条「主动读」的快照，每小时 +0.5%，中间一张 V5 都没出：5 个安静小时，12%/天
    let snaps: Vec<String> = (0..6)
        .map(|i| {
            format!(
                "INSERT INTO upstream_snapshots (ts, v5_percent, v5_rate, anlas, fresh) VALUES ({}, {}, NULL, 0, 1)",
                NOON - (6 - i) as f64 * 3600.0,
                50.0 + 0.5 * i as f64
            )
        })
        .collect();
    sql(&e, snaps).await;
    quota(&e).await;
    let r = last(&e).await;
    assert_eq!(
        r["v5"]["rate_check"]["quiet"],
        json!({"rate": 12.0, "hours": 5.0, "gain": 2.5})
    );
    assert_eq!(r["v5"]["rate_check"]["used"], json!(12.0));
    assert_eq!(r["v5"]["global"], 170, "⌊12 × 14.2⌋");

    // 有一段出了 V5：那一小时不算，还剩 4 个安静小时
    let v5_log = |ts: f64| {
        format!(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, model, status, images) \
             VALUES ({ts}, NULL, '', 'image', 'nai-diffusion-5', 'ok', 1)"
        )
    };
    sql(&e, vec![v5_log(NOON - 4.5 * 3600.0)]).await;
    quota(&e).await;
    assert_eq!(
        last(&e).await["v5"]["rate_check"]["quiet"]["hours"],
        json!(4.0)
    );
    // 再出一张：不到 4 个安静小时，不给结果；当天方案照旧
    sql(&e, vec![v5_log(NOON - 2.5 * 3600.0)]).await;
    quota(&e).await;
    let r = last(&e).await;
    assert!(r["v5"]["rate_check"].get("quiet").is_none(), "{r}");
    assert_eq!(r["v5"]["global"], 170);
}

#[tokio::test]
async fn an_empty_cache_is_read_once_and_a_failed_read_is_not_an_error() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    e.mock.clear();
    quota(&e).await;
    assert_eq!(e.mock.requests_to("/user/subscription").len(), 1);
    quota(&e).await;
    assert_eq!(
        e.mock.requests_to("/user/subscription").len(),
        1,
        "缓存里有读数就不再查"
    );

    let e = setup(&[]).await;
    e.mock.clear();
    e.mock
        .push("/user/subscription", Reply::status(500, "synthetic"));
    let s = quota(&e).await;
    assert!(s.starts_with("每人 V4.5 150 张"), "{s}");
    assert_eq!(e.mock.requests_to("/user/subscription").len(), 1);
    assert_eq!(last(&e).await["v5"]["percent"], Value::Null, "没有读数");
}

#[tokio::test]
async fn landing_text_and_new_key_defaults_follow_the_plan() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    let s = quota(&e).await;
    assert!(
        s.starts_with("每人 V4.5 150 张 · V5 7 张（全站 V5 71）"),
        "{s}"
    );
    for (k, v) in [
        ("register_daily_images", "150"),
        ("register_daily_v5", "7"),
        ("register_image_scope", "all"),
        ("global_daily_v5", "71"),
        ("quota_ceiling", "150"),
    ] {
        assert_eq!(setting(&e.state, k).await.as_deref(), Some(v), "{k}");
    }
    let notice = setting(&e.state, "algo_notice").await.unwrap();
    assert!(
        notice.contains("今日额度：V4.5 每人 150 张/天（每日 7 点重置） · V5 每人 7 张"),
        "{notice}"
    );
    assert_db_format(&e).await;
}

// ------------------------------------------------------------------ 出错

#[tokio::test]
async fn corrupt_settings_fail_the_round_and_file_a_bug() {
    let _clock = clock(Some(NOON)).await;
    // 上限是 inf：一开始就出错，什么都没写
    let e = setup(&[]).await;
    set(&e, "quota_ceiling", "inf").await;
    let err = run_job(&e.state, "quota").await.unwrap_err();
    assert_eq!(err.ty, "OverflowError");
    let j = job_last(&e.state).await;
    assert_eq!(j["ok"], false);
    assert!(
        j["summary"]
            .as_str()
            .unwrap()
            .starts_with("OverflowError: "),
        "{j}"
    );
    assert_eq!(setting(&e.state, "quota_algo_last").await, None);
    assert_eq!(bug_sources(&e.state).await, ["job:quota"]);

    // 复盘历史是坏 JSON：同样整轮失败
    let e = setup(&[]).await;
    full_coverage(&e).await;
    set(&e, "quota_algo_history", "{坏").await;
    assert_eq!(
        run_job(&e.state, "quota").await.unwrap_err().ty,
        "JSONDecodeError"
    );
    assert_eq!(setting(&e.state, "quota_algo_day").await, None);

    // 当天方案读不懂（不是对象）：出错之前的复盘已经写进库，保留
    let e = setup(&[]).await;
    full_coverage(&e).await;
    set(&e, "quota_v5_day_plan", "[1, 2]").await;
    assert_eq!(
        run_job(&e.state, "quota").await.unwrap_err().ty,
        "AttributeError"
    );
    assert_eq!(
        setting(&e.state, "quota_algo_day").await.as_deref(),
        Some(TODAY)
    );
    assert_eq!(history(&e).await.len(), 1);
    assert_eq!(setting(&e.state, "quota_algo_last").await, None);
    assert_eq!(bug_sources(&e.state).await, ["job:quota"]);
}

#[tokio::test]
async fn admin_recompute_and_the_loop_do_not_both_review() {
    let _clock = clock(Some(NOON)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    let m = member_key(&e.state, 1, &[]).await;
    used(&e, m, YDAY, 150).await;
    let admin = Admin::login(&e.state).await;
    let (looped, (status, _)) = tokio::join!(
        run_job(&e.state, "quota"),
        admin.json("PUT", "/quota-algo", Some(json!({"quota_step": 25}))),
    );
    looped.unwrap();
    assert_eq!(status, 200);
    assert_eq!(history(&e).await.len(), 1, "只复盘一次");
    settle().await;
    let adjusted = admin_actions(&e.state.db, 50)
        .await
        .into_iter()
        .filter(|r| r["action"] == "动态额度调整")
        .count();
    assert_eq!(adjusted, 1);
    assert_eq!(last(&e).await["ceiling"], 175);
}

// ------------------------------------------------------------------ 连续多天

/// 3 个服务日、每天 4 轮，中间成员照常出图：每一步都检查不变量。
#[tokio::test]
async fn three_days_of_rounds_keep_the_invariants() {
    let _clock = clock(Some(D7 + 3600.0)).await;
    let e = setup(&[]).await;
    full_coverage(&e).await;
    let mut members = Vec::new();
    for i in 0..12 {
        members.push(member_key(&e.state, i + 1, &[]).await);
    }
    member_key(&e.state, 99, &[("is_test", json!(1))]).await;
    let mut seed: u64 = 0x5eed;
    let mut rand = move |n: i64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) % n as u64) as i64
    };
    for day in 0..3 {
        let mut plan: Option<String> = None;
        for hours in [1.0, 6.0, 12.0, 20.0] {
            let t = D7 + day as f64 * 86400.0 + hours * 3600.0;
            gate_server::time::freeze(Some(t));
            let today = e.state.clock.day(t);
            quota(&e).await;
            let r = last(&e).await;
            let a = r["ceiling"].as_i64().unwrap();
            assert!((10..=300).contains(&a), "A = {a}");
            assert!(r["v5"]["each"].as_i64().unwrap() >= 1);
            assert_eq!(drift(&e).await, 0, "{today} {hours}");
            let p = setting(&e.state, "quota_v5_day_plan").await.unwrap();
            assert!(p.contains(&today), "{p}");
            match &plan {
                None => plan = Some(p),
                Some(first) => assert_eq!(&p, first, "{today} 的方案变了"),
            }
            assert_eq!(history(&e).await.len(), day + 1, "{today} 每天只复盘一次");
            assert_eq!(
                setting(&e.state, "quota_algo_day").await.as_deref(),
                Some(today.as_str())
            );
            assert_db_format(&e).await;
            // 成员接着出图（不超过当前的每人上限）
            for &m in &members {
                let n = rand(a / 4 + 1);
                used(&e, m, &today, n).await;
            }
        }
    }
}
