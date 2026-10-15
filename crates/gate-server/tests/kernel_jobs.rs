//! 后台常驻任务：闲置回收（线上开关关着时绝不删 Key）、维护循环一轮、启动 / 关停序列。
//! 公开仓库：全部是合成数据；Discord / NovelAI / 官方状态页都不出本机。

mod common;
use common::*;
use gate_db::repo::keys::NewKey;
use gate_server::jobs;
use gate_testkit::MockUpstream;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const DAY: f64 = 86400.0;

async fn key(env: &Env, id: i64, created_days_ago: f64, extra: &[(&str, Value)]) {
    let new: NewKey = serde_json::from_value(json!({
        "name": format!("成员{id:02}"), "token": format!("nai-synthetic-{id:04}"), "daily_images": 100,
        "daily_v5": 10, "monthly_anlas": 0.0, "daily_text_tokens": 1000, "rpm": 60,
    }))
    .unwrap();
    let now = env.state.now();
    env.state.db.create_key(&new, now).await.unwrap();
    let mut sql = vec![format!(
        "UPDATE api_keys SET created_at={} WHERE token='nai-synthetic-{id:04}'",
        now - created_days_ago * DAY
    )];
    for (col, v) in extra {
        sql.push(format!(
            "UPDATE api_keys SET {col}={v} WHERE token='nai-synthetic-{id:04}'"
        ));
    }
    env.state.db.run_script(sql).await.unwrap();
}

/// 库里的主键（`common::setup` 已经建了一把成员 Key，所以和这里的编号差 1）。
async fn db_id(env: &Env, id: i64) -> i64 {
    env.state
        .db
        .get_key_by_token(&format!("nai-synthetic-{id:04}"))
        .await
        .unwrap()
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

async fn key_exists(env: &Env, id: i64) -> bool {
    env.state
        .db
        .get_key_by_token(&format!("nai-synthetic-{id:04}"))
        .await
        .unwrap()
        .is_some()
}

/// 注册服务需要的环境（Discord 指到本机假服务；配了「已领 Key」身份组，这样摘身份组会真的发请求）。
const GUILD: &str = "9000000000000000099";
const ROLE: &str = "9000000000000000098";

fn discord_env<'a>(base: &'a str, days: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("KEY_INACTIVITY_DELETE_DAYS", days),
        ("DISCORD_API_BASE", base),
        ("DISCORD_CLIENT_ID", "100000000"),
        ("DISCORD_CLIENT_SECRET", "synthetic-secret"),
        ("DISCORD_BOT_TOKEN", "synthetic-bot-token"),
        (
            "REGISTRATION_BRIDGE_SECRET",
            "0123456789abcdef0123456789abcdef",
        ),
        ("DISCORD_GUILD_ID", GUILD),
        ("DISCORD_MEMBER_ROLE_ID", ROLE),
        ("SITE_URL", "https://owl.example"),
    ]
}

/// 平时的出图记录：往前 32 天每 20 分钟一张成功出图（每个服务日 72 张，≥ 健康线 50 张；最近一张在 30 秒前）。
/// 闲置回收不算站点出不了图的服务日（docs/v3-deviations.md #55），所以「平时」场景要先有正常出图的历史。
async fn seed_healthy_history(env: &Env) {
    let now = env.state.now();
    env.state
        .db
        .run_script(vec![format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {}) \
             INSERT INTO usage_log (ts, key_id, key_name, kind, status, images) \
             SELECT {now} - 30 - i * 1200, NULL, '', 'image', 'ok', 1 FROM n",
            32 * 72
        )])
        .await
        .unwrap();
}

fn role_removals(discord: &MockUpstream) -> Vec<String> {
    discord
        .requests()
        .into_iter()
        .filter(|r| r.method == "DELETE" && r.path.contains("/roles/"))
        .map(|r| r.path)
        .collect()
}

// ------------------------------------------------------------------ 闲置回收

#[tokio::test]
async fn idle_recycle_switched_off_never_deletes_anything() {
    // 关掉回收（KEY_INACTIVITY_DELETE_DAYS=0；线上 10-13 起是 3，开着）
    let discord = MockUpstream::start().await;
    let e = setup(&discord_env(&discord.url(), "0")).await;
    key(&e, 1, 400.0, &[]).await;
    key(
        &e,
        2,
        90.0,
        &[("last_image_at", json!(e.state.now() - 80.0 * DAY))],
    )
    .await;
    let id1 = db_id(&e, 1).await;
    e.state
        .db
        .run_script(vec![format!(
            "INSERT INTO discord_registrations (discord_id, key_id, created_at) VALUES ('9000000000000000001', {id1}, 1.0)"
        )])
        .await
        .unwrap();
    assert_eq!(jobs::delete_inactive_keys(&e.state).await.unwrap(), 0);
    assert!(key_exists(&e, 1).await && key_exists(&e, 2).await);
    settle().await;
    assert!(
        e.state
            .db
            .list_admin_actions(10, 0)
            .await
            .unwrap()
            .is_empty(),
        "什么都没做就不该有操作日志"
    );
    assert!(discord.requests().is_empty(), "不删 Key 就不碰 Discord");
    assert!(e.state.db.list_bans("").await.unwrap().is_empty());
    assert_eq!(
        e.state
            .db
            .key_row_for_discord("9000000000000000001")
            .await
            .unwrap()
            .map(|k| k["id"].clone()),
        Some(json!(id1)),
        "领取记录也不动"
    );

    // 通过真正的循环也一样：起一遍所有循环，等一会儿再关
    e.state
        .kernel
        .set_official_api(Some("http://127.0.0.1:9/never".into()));
    jobs::startup(&e.state).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    jobs::shutdown(&e.state).await;
    assert!(key_exists(&e, 1).await && key_exists(&e, 2).await);
    assert!(role_removals(&discord).is_empty());
}

#[tokio::test]
async fn idle_recycle_deletes_bans_and_logs_when_switched_on() {
    let discord = MockUpstream::start().await;
    let e = setup(&discord_env(&discord.url(), "3")).await;
    let now = e.state.now();
    key(&e, 1, 10.0, &[]).await; // 从没生过图，创建 10 天 → 闲置
    key(&e, 2, 10.0, &[("last_image_at", json!(now - 4.0 * DAY))]).await; // 4 天没生图 → 闲置
    key(&e, 3, 10.0, &[("last_image_at", json!(now - 3600.0))]).await; // 活跃
    key(&e, 4, 10.0, &[("is_test", json!(1))]).await; // 测试 Key 豁免
    key(&e, 5, 10.0, &[("is_admin", json!(1))]).await; // 站长 Key 豁免
    key(&e, 6, 10.0, &[]).await; // 邀请标签豁免
    key(&e, 7, 1.0, &[]).await; // 刚创建
    // 已过期：过期后请求一律 403、last_image_at 不再更新；不算闲置，照旧等 /register 时释放（docs/v3-deviations.md #56）
    key(&e, 8, 10.0, &[("expires_at", json!(now - 4.0 * DAY))]).await;
    // 还没到期的期限 Key 照常判定
    key(&e, 9, 10.0, &[("expires_at", json!(now + 30.0 * DAY))]).await;
    seed_healthy_history(&e).await;
    let (id1, id2, id6) = (db_id(&e, 1).await, db_id(&e, 2).await, db_id(&e, 6).await);
    let id8 = db_id(&e, 8).await;
    e.state
        .db
        .run_script(vec![
            format!("INSERT INTO key_tags (key_id, tag, note, by, ts) VALUES ({id6}, '邀请', '', 'x', 1.0)"),
            format!(
                "INSERT INTO discord_registrations (discord_id, key_id, created_at, username, display_name, avatar) \
                 VALUES ('9000000000000000001', {id1}, 1.0, 'user_one', '一号', 'av1')"
            ),
            format!("INSERT INTO discord_registrations (discord_id, key_id, created_at) VALUES ('9000000000000000002', {id2}, 1.0)"),
            format!("INSERT INTO discord_registrations (discord_id, key_id, created_at) VALUES ('9000000000000000008', {id8}, 1.0)"),
        ])
        .await
        .unwrap();
    let n = jobs::delete_inactive_keys(&e.state).await.unwrap();
    let mut gone = Vec::new();
    for id in 1..=9 {
        if !key_exists(&e, id).await {
            gone.push(id);
        }
    }
    assert_eq!((n, gone), (3, vec![1, 2, 9]));
    for (id, alive) in [
        (1, false),
        (2, false),
        (3, true),
        (4, true),
        (5, true),
        (6, true),
        (7, true),
        (8, true),
        (9, false),
    ] {
        assert_eq!(key_exists(&e, id).await, alive, "Key {id}");
    }
    // 黑名单带上 Discord 资料；理由写明天数。过期 Key 的领取人不进黑名单、领取记录也不动
    let bans = e.state.db.list_bans("").await.unwrap();
    assert_eq!(bans.len(), 2);
    assert!(
        bans.iter()
            .all(|b| b["discord_id"] != "9000000000000000008"),
        "过期 Key 不能当闲置拉黑"
    );
    assert_eq!(
        e.state
            .db
            .registration_key_id("9000000000000000008")
            .await
            .unwrap(),
        Some(id8)
    );
    let one = bans
        .iter()
        .find(|b| b["discord_id"] == "9000000000000000001")
        .unwrap();
    assert_eq!(one["reason"], "连续 3 天未活动自动回收");
    assert_eq!(one["username"], "user_one");
    assert_eq!(one["display_name"], "一号");
    assert_eq!(one["banned_by"], "系统");
    // 领取记录随 Key 一起删；拉黑时摘一次身份组（`delete_key` 已经带走了领取记录，之后的「释放领取记录」没有可释放的，与旧版一致）
    assert_eq!(
        e.state
            .db
            .registration_key_id("9000000000000000001")
            .await
            .unwrap(),
        None
    );
    let removals = role_removals(&discord);
    for id in ["9000000000000000001", "9000000000000000002"] {
        let want = format!("/guilds/{GUILD}/members/{id}/roles/{ROLE}");
        assert_eq!(
            removals.iter().filter(|p| **p == want).count(),
            1,
            "{removals:?}"
        );
    }
    // 操作日志
    let rows = e.state.db.list_admin_actions(10, 0).await.unwrap();
    let del: Vec<&Value> = rows
        .iter()
        .filter(|r| r["action"] == "闲置回收 Key")
        .collect();
    assert_eq!(del.len(), 3);
    assert!(
        del.iter()
            .any(|r| r["target"] == format!("Key #{id1} 成员01")
                && r["detail"] == "连续 3 天未活动自动回收并拉黑"
                && r["actor"] == "系统")
    );
    // 再跑一遍：没有可回收的了
    assert_eq!(jobs::delete_inactive_keys(&e.state).await.unwrap(), 0);
}

#[tokio::test]
async fn idle_recycle_negative_days_is_off_too() {
    let e = setup(&[("KEY_INACTIVITY_DELETE_DAYS", "-5")]).await;
    key(&e, 1, 400.0, &[]).await;
    assert_eq!(jobs::delete_inactive_keys(&e.state).await.unwrap(), 0);
    assert!(key_exists(&e, 1).await);
}

// ------------------------------------------------------------------ 自动驾驶熔断

/// 熔断要求失败来自 ≥ 2 把不同的非测试 Key：一个成员反复发畸形请求、测试 Key 出错都停不了全站
/// （docs/v3-deviations.md #58）。
#[tokio::test]
async fn breaker_needs_upstream_failures_from_two_member_keys() {
    let e = setup(&[]).await;
    let s = &e.state;
    s.db.set_setting("autopilot_breaker", "enforce")
        .await
        .unwrap();
    key(&e, 1, 10.0, &[]).await;
    key(&e, 2, 10.0, &[]).await;
    key(&e, 3, 10.0, &[("is_test", json!(1))]).await;
    let (m1, m2, t) = (db_id(&e, 1).await, db_id(&e, 2).await, db_id(&e, 3).await);
    let now = s.now();
    let log = |key_id: i64, status: &str, up: i64, n: usize| {
        (0..n)
            .map(|i| {
                format!(
                    "INSERT INTO usage_log (ts, key_id, key_name, kind, status, up_status) \
                     VALUES ({}, {key_id}, '', 'image', '{status}', {up})",
                    now - 60.0 - i as f64
                )
            })
            .collect::<Vec<_>>()
    };
    // 一个成员 8 次上游 502（占比 8/12），另有 4 次成功
    let mut sql = log(m1, "error", 502, 8);
    sql.extend(log(m2, "ok", 200, 4));
    s.db.run_script(sql).await.unwrap();
    let rows = s.db.autopilot_breaker_rows(now - 900.0).await.unwrap();
    assert_eq!(
        (rows.fails, rows.total, rows.fail_keys),
        (Some(8), Some(12), 1)
    );
    let tripped = |s: &gate_server::AppState| s.guard.with(|g| g.breaker_until) > s.now();
    let r = gate_server::kernel::run_autopilot_checked(s).await.unwrap();
    assert_eq!(r["rules"]["breaker"]["value"], json!(false), "{r}");
    assert!(!tripped(s), "一把 Key 停不了全站");

    // 测试 Key 的失败不算
    s.db.run_script(log(t, "error", 500, 6)).await.unwrap();
    let rows = s.db.autopilot_breaker_rows(now - 900.0).await.unwrap();
    assert_eq!((rows.fails, rows.fail_keys), (Some(14), 1));
    gate_server::kernel::run_autopilot_checked(s).await.unwrap();
    assert!(!tripped(s), "测试 Key 的失败不算第二把 Key");

    // 第二个成员也遇到上游失败：这才像上游故障
    s.db.run_script(log(m2, "error", 503, 1)).await.unwrap();
    assert_eq!(
        s.db.autopilot_breaker_rows(now - 900.0)
            .await
            .unwrap()
            .fail_keys,
        2
    );
    let r = gate_server::kernel::run_autopilot_checked(s).await.unwrap();
    assert_eq!(r["rules"]["breaker"]["applied"], json!(true), "{r}");
    assert!(tripped(s));
}

// ------------------------------------------------------------------ 维护循环一轮

#[tokio::test]
async fn maintenance_round_purges_and_flushes() {
    let e = setup(&[("USAGE_LOG_RETENTION_DAYS", "10")]).await;
    let s = &e.state;
    s.kernel
        .set_official_api(Some("http://127.0.0.1:9/never".into()));
    let now = s.now();
    let old_hour = ((now - 40.0 * DAY) / 3600.0).floor() as i64 * 3600;
    s.db.run_script(vec![
        format!("INSERT INTO usage_log (ts, key_id, key_name, kind, status) VALUES ({}, NULL, '', 'text', 'ok')", now - 20.0 * DAY),
        format!("INSERT INTO usage_log (ts, key_id, key_name, kind, status) VALUES ({}, NULL, '', 'text', 'ok')", now - 2.0 * DAY),
        format!("INSERT INTO admin_actions (ts, actor, action, target, detail, ok) VALUES ({}, 'a', '旧', '', '', 1)", now - 200.0 * DAY),
        format!("INSERT INTO admin_actions (ts, actor, action, target, detail, ok) VALUES ({}, 'a', '新', '', '', 1)", now - 2.0 * DAY),
        format!("INSERT INTO status_hourly (hour, code, n) VALUES ({old_hour}, 200, 5)"),
        format!("INSERT INTO error_events (sig, source, first_seen, last_seen, resolved_at, count) VALUES ('old', 's', 1.0, {0}, {0}, 1)", now - 40.0 * DAY),
        format!("INSERT INTO error_events (sig, source, first_seen, last_seen, resolved_at, count) VALUES ('open', 's', 1.0, {0}, NULL, 1)", now - 40.0 * DAY),
    ])
    .await
    .unwrap();
    s.status_stats.record(200, now);
    s.status_stats.record(200, now);
    s.status_stats.record(429, now);

    jobs::maintenance_once(s).await;
    settle().await;

    // 明细日志只留 10 天内；操作日志 180 天；30 天前的状态码统计与已处理的旧 Bug 清掉
    let logs = s.db.dump_tables().await.unwrap();
    assert_eq!(logs["usage_log"].as_array().unwrap().len(), 1);
    let actions: Vec<&str> = logs["admin_actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["新"]);
    let errors: Vec<&str> = logs["error_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["sig"].as_str().unwrap())
        .collect();
    assert!(
        errors.contains(&"open") && !errors.contains(&"old"),
        "{errors:?}"
    );
    // 内存里的状态码统计落库并清空
    let hour = (now / 3600.0).floor() as i64 * 3600;
    let totals = s.db.status_totals_since(hour).await.unwrap();
    assert!(
        totals.contains(&(200, 2)) && totals.contains(&(429, 1)),
        "{totals:?}"
    );
    assert!(s.status_stats.pending().is_empty());
    assert!(
        !logs
            .get("status_hourly")
            .unwrap()
            .to_string()
            .contains(&format!("\"hour\":{old_hour}"))
    );
}

#[tokio::test]
async fn flush_status_stats_puts_counts_back_when_the_write_fails() {
    let e = setup(&[]).await;
    let s = &e.state;
    s.status_stats.record(200, s.now());
    // 让写入失败：把表删掉
    s.db.run_script(vec!["DROP TABLE status_hourly".into()])
        .await
        .unwrap();
    assert!(jobs::flush_status_stats(s).await.is_err());
    let pending = s.status_stats.pending();
    assert_eq!(
        pending.iter().map(|(_, n)| n).sum::<i64>(),
        1,
        "写失败要放回去下次再写：{pending:?}"
    );
}

// ------------------------------------------------------------------ 启动 / 关停

#[tokio::test]
async fn startup_runs_loops_and_shutdown_finishes_promptly() {
    let e = setup(&[]).await;
    let s = &e.state;
    s.kernel
        .set_official_api(Some("http://127.0.0.1:9/never".into()));
    // 上次的运行记录在启动时读回来
    s.db.set_setting(
        "kernel_last",
        &json!({"retired_module": {"enabled": true}}).to_string(),
    )
    .await
    .unwrap();
    jobs::startup(s).await.unwrap();
    assert!(s.kernel.last().contains_key("retired_module"));
    // 第一轮内核 tick 立即执行（不等 10 分钟）
    let start = Instant::now();
    loop {
        let raw =
            s.db.get_setting("kernel_last")
                .await
                .unwrap()
                .unwrap_or_default();
        if raw.contains("\"capacity\"") && raw.contains("\"retired_module\"") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "内核没有在启动后立即跑第一轮：{raw}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 关停前内存里还有没落库的状态码统计
    s.status_stats.record(503, s.now());
    let t = Instant::now();
    jobs::shutdown(s).await;
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "关停太慢：{:?}",
        t.elapsed()
    );
    assert!(s.shutdown.is_cancelled() && s.tasks.is_closed() && s.tasks.is_empty());
    let hour = (s.now() / 3600.0).floor() as i64 * 3600;
    assert!(
        s.db.status_totals_since(hour)
            .await
            .unwrap()
            .contains(&(503, 1)),
        "关停时要把状态码统计落库"
    );
}

#[tokio::test]
async fn startup_wires_the_registration_service_into_the_kernel() {
    let discord = MockUpstream::start().await;
    let e = setup(&discord_env(&discord.url(), "0")).await;
    e.state
        .kernel
        .set_official_api(Some("http://127.0.0.1:9/never".into()));
    assert!(e.state.kernel.registrar().is_none());
    // 领 Key 开着，「领 Key」模块才会跑校验
    e.state.db.set_setting("register_open", "1").await.unwrap();
    jobs::startup(&e.state).await.unwrap();
    assert!(e.state.kernel.registrar().is_some());
    // 内核的第一轮 tick 读到了注册服务：「已领人数 ≤ 名额」有了数字，不再是没有注册服务时的空校验
    let start = Instant::now();
    loop {
        let raw = e
            .state
            .db
            .get_setting("kernel_last")
            .await
            .unwrap()
            .unwrap_or_default();
        if raw.contains("已领人数 ≤ 名额") {
            assert!(raw.contains("0 / 10"), "{raw}");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{raw}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    jobs::shutdown(&e.state).await;
}
