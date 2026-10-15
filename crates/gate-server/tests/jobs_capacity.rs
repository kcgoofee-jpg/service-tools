//! 两个容量相关的定时任务：「上游 V5 快照」和「每小时上限自动调整」。
//! 真实 SQLite + 假上游 + 冻结时钟；公开仓库，全部是合成数据。

mod common;
use common::*;
use gate_core::guard::{AdaptPlan, GuardCore, HOURLY_MAX, HOURLY_MIN};
use gate_core::pyc;
use gate_testkit::Reply;
use serde_json::{Value, json};

/// 2026-10-14 12:00:00 +08:00
const NOW: f64 = 1_791_950_400.0;
const DAY: f64 = 86400.0;

async fn snapshots(e: &Env) -> Vec<Value> {
    query(
        &e.state.db,
        "SELECT ts, v5_percent, v5_rate, anlas, fresh FROM upstream_snapshots ORDER BY ts",
    )
    .await
}

// ------------------------------------------------------------------ 上游 V5 快照

#[tokio::test]
async fn snapshot_at_most_once_an_hour() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    // 假上游：剩余 80%，每 3600 秒恢复 1%（= 24%/天）
    let s = run_job(&e.state, "upstream_snapshot").await.unwrap();
    assert_eq!(s, "已记录：剩余 80%，恢复 24.0%/天");
    assert_eq!(
        snapshots(&e).await,
        [json!({"ts": NOW, "v5_percent": 80.0, "v5_rate": 24.0, "anlas": 0.0, "fresh": 1})]
    );
    for later in [60.0, 1800.0, 3599.0] {
        gate_server::time::freeze(Some(NOW + later));
        assert_eq!(
            run_job(&e.state, "upstream_snapshot").await.unwrap(),
            "距上次不到 1 小时"
        );
    }
    assert_eq!(snapshots(&e).await.len(), 1);
    gate_server::time::freeze(Some(NOW + 3600.0));
    run_job(&e.state, "upstream_snapshot").await.unwrap();
    let rows = snapshots(&e).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["ts"], json!(NOW + 3600.0));
}

#[tokio::test]
async fn a_snapshot_without_a_reading_is_still_recorded() {
    let _clock = clock(Some(NOW)).await;
    // 订阅读失败：这一行没有读数，fresh=0（不能拿来测恢复速度）
    let e = setup(&[]).await;
    e.mock
        .push("/user/subscription", Reply::status(503, "synthetic"));
    let s = run_job(&e.state, "upstream_snapshot").await.unwrap();
    assert_eq!(s, "已记录：没有读数（上游账号都读不到）");
    assert_eq!(
        snapshots(&e).await,
        [json!({"ts": NOW, "v5_percent": null, "v5_rate": null, "anlas": 0.0, "fresh": 0})]
    );
    // 没有可用的上游账号：同样记一行无读数的快照，不报错
    let e = setup(&[("NAI_TOKENS", "")]).await;
    e.mock.clear();
    run_job(&e.state, "upstream_snapshot").await.unwrap();
    assert_eq!(snapshots(&e).await[0]["v5_percent"], Value::Null);
    assert!(e.mock.requests_to("/user/subscription").is_empty());
}

// ------------------------------------------------------------------ 每小时上限

async fn set(e: &Env, k: &str, v: &str) {
    e.state.db.set_setting(k, v).await.unwrap();
}

fn cap(e: &Env) -> i64 {
    e.state.guard.with(|g| g.values["account_hourly_cap"])
}

fn set_cap(e: &Env, v: i64) {
    e.state.guard.with(|g| {
        g.values.insert("account_hourly_cap".into(), v);
    });
}

/// 平稳的一天：3 天前就有日志（覆盖够 20 小时），过去 24 小时有成员被每小时上限拦过，没有 429，上次调整在 25 小时前。
async fn calm_day_that_hit_the_cap(e: &Env) -> i64 {
    let m = member_key(&e.state, 1, &[]).await;
    e.state
        .db
        .run_script(vec![
            format!(
                "INSERT INTO usage_log (ts, key_id, key_name, kind, status) VALUES ({}, NULL, '', 'text', 'ok')",
                NOW - 3.0 * DAY
            ),
            format!(
                "INSERT INTO usage_log (ts, key_id, key_name, kind, status, reason) VALUES ({}, {m}, '', 'image', 'rejected', 'hourly_cap')",
                NOW - 3600.0
            ),
        ])
        .await
        .unwrap();
    set(
        e,
        "guard_hourly_adapted_at",
        &pyc::float_repr(NOW - 25.0 * 3600.0),
    )
    .await;
    m
}

#[tokio::test]
async fn the_first_run_only_starts_the_clock() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(s, "150（第一次运行，开始计时）");
    assert_eq!(
        setting(&e.state, "guard_hourly_adapted_at").await,
        Some(pyc::float_repr(NOW))
    );
    // 紧接着再跑：24 小时内检查过，不动
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(s, "150（24 小时内调整或检查过，不动）");
    assert_eq!(cap(&e), 150);
}

#[tokio::test]
async fn a_calm_day_that_hit_the_cap_adds_ten() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let m = calm_day_that_hit_the_cap(&e).await;
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(s, "150 → 160");
    assert_eq!(cap(&e), 160);
    assert_eq!(
        setting(&e.state, "guard_account_hourly_cap")
            .await
            .as_deref(),
        Some("160")
    );
    settle().await;
    let rows = admin_actions(&e.state.db, 10).await;
    let log = rows
        .iter()
        .find(|r| r["action"] == "自动调整：每小时上限")
        .expect("要记一条操作日志");
    assert_eq!(
        (&log["actor"], &log["detail"]),
        (&json!("系统"), &json!("一天没有上游限流：150 → 160"))
    );
    // 到顶了：再过一天也不再加
    gate_server::time::freeze(Some(NOW + 25.0 * 3600.0));
    e.state
        .db
        .run_script(vec![format!(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, status, reason) VALUES ({}, {m}, '', 'image', 'rejected', 'hourly_cap')",
            NOW + 24.0 * 3600.0
        )])
        .await
        .unwrap();
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(s, format!("160（已到自动调整的最高值 {HOURLY_MAX}）"));
}

#[tokio::test]
async fn no_signal_or_short_logs_only_move_the_timestamp() {
    let _clock = clock(Some(NOW)).await;
    // 没被每小时上限拦过：没信息，不加
    let e = setup(&[]).await;
    calm_day_that_hit_the_cap(&e).await;
    e.state
        .db
        .run_script(vec![
            "DELETE FROM usage_log WHERE reason='hourly_cap'".into(),
        ])
        .await
        .unwrap();
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(
        s,
        "150（没顶到过上限、数据不足 20 小时或已到 429 后的天花板，不加）"
    );
    assert_eq!(cap(&e), 150);
    assert_eq!(
        setting(&e.state, "guard_hourly_adapted_at").await,
        Some(pyc::float_repr(NOW))
    );
    // 日志只覆盖 10 小时：不加
    let e = setup(&[]).await;
    calm_day_that_hit_the_cap(&e).await;
    e.state
        .db
        .run_script(vec![
            format!(
                "UPDATE usage_log SET ts={} WHERE kind='text'",
                NOW - 10.0 * 3600.0
            ),
            format!(
                "UPDATE usage_log SET ts={} WHERE reason='hourly_cap'",
                NOW - 9.0 * 3600.0
            ),
        ])
        .await
        .unwrap();
    run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(cap(&e), 150);
    assert_eq!(
        setting(&e.state, "guard_hourly_adapted_at").await,
        Some(pyc::float_repr(NOW))
    );
    // 24 小时内挨过 429：不动
    let e = setup(&[]).await;
    calm_day_that_hit_the_cap(&e).await;
    set(
        &e,
        "guard_last_upstream_429",
        &pyc::float_repr(NOW - 3600.0),
    )
    .await;
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert_eq!(s, "150（24 小时内挨过上游 429，不动）");
}

#[tokio::test]
async fn after_a_429_the_ceiling_is_eighty_percent_of_that_cap() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    calm_day_that_hit_the_cap(&e).await;
    // 上次挨 429 时上限是 150：之后加窗最多到 ⌊150 × 0.8⌋ = 120
    set(&e, "guard_429_cap_at", "150").await;
    set_cap(&e, 115);
    assert_eq!(run_job(&e.state, "hourly_cap").await.unwrap(), "115 → 120");
    // 已经高于天花板（站长手动调的）：只升不降
    let e = setup(&[]).await;
    calm_day_that_hit_the_cap(&e).await;
    set(&e, "guard_429_cap_at", "150").await;
    set_cap(&e, 140);
    let s = run_job(&e.state, "hourly_cap").await.unwrap();
    assert!(s.starts_with("140（"), "{s}");
    assert_eq!(cap(&e), 140);
}

#[tokio::test]
async fn an_unreadable_timestamp_fails_the_round() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    set(&e, "guard_hourly_adapted_at", "坏").await;
    let err = run_job(&e.state, "hourly_cap").await.unwrap_err();
    assert_eq!(err.ty, "GuardError");
    assert_eq!(bug_sources(&e.state).await, ["job:hourly_cap"]);
    let last = Value::Object(e.state.jobs.last().await);
    assert_eq!(last["hourly_cap"]["ok"], false);
}

/// 每小时上限的不变量（纯函数，逐格扫）：加窗只升不降、不超过 160（手动调高的除外）、24 小时最多一次；
/// 429 减半、不低于 100（已经低于 100 的不会被抬回去）。
#[test]
fn hourly_cap_invariants() {
    let core = |cap: i64| {
        let mut g = GuardCore::new("Asia/Shanghai", 7);
        g.values.insert("account_hourly_cap".into(), cap);
        g
    };
    for old in (0..=240).step_by(5) {
        for seen in [0.0, 90.0, 120.0, 150.0, 200.0, 240.0] {
            for hit in [false, true] {
                let mut g = core(old);
                let r = g
                    .adapt_daily_finish(NOW, hit, Some(NOW - 3.0 * DAY), seen)
                    .unwrap();
                let new = g.values["account_hourly_cap"];
                assert!(new >= old, "只升不降：{old} → {new}");
                assert!(
                    new <= old.max(HOURLY_MAX),
                    "不超过 {HOURLY_MAX}：{old} → {new}"
                );
                assert!(new - old <= 10, "一次最多 +10：{old} → {new}");
                assert_eq!(r.change.is_some(), new != old);
                if !hit {
                    assert_eq!(new, old, "没顶到过上限不加");
                }
            }
        }
        // 24 小时内调整过 / 挨过 429：不动
        for (step, last_429) in [(NOW - 3600.0, 0.0), (NOW - 25.0 * 3600.0, NOW - 60.0)] {
            match GuardCore::adapt_daily_begin(NOW, last_429, step) {
                AdaptPlan::Done(a) => assert_eq!(a.change, None),
                AdaptPlan::NeedUsage => panic!("24 小时内不该再调"),
            }
        }
        // 429：减半，不低于 100；本来就低于 100 的维持
        let mut g = core(old);
        let r = g.on_upstream_429(NOW, 0.0).unwrap();
        let new = g.values["account_hourly_cap"];
        assert!(new <= old, "429 只降不升：{old} → {new}");
        assert_eq!(new, old.min(HOURLY_MIN.max(old / 2)), "{old}");
        assert_eq!(r.change.is_some(), new != old);
    }
    assert_eq!((HOURLY_MIN, HOURLY_MAX), (100, 160));
}
