//! 扩展巡检、notify、report、traffic：全部在合成数据库 / 合成日志上跑（gate-db 建库，Discord 指向本机假服务）。

mod common;

use common::*;
use gate_ops::checks::{ExtraCtx, run_extra_checks};
use gate_ops::notify::{self, NotifyArgs, NotifyState};
use gate_ops::patrol::{Config, Report, Sev};
use gate_ops::{report, traffic};
use jiff::tz::TimeZone;
use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;

/// 2026-09-21 22:13:20 +08:00
const NOW: f64 = 1_790_000_000.0;

/// 合成的自签证书（CN=owl.example，2026-10-16 签发，10 年有效），notAfter = 2036-10-13 15:53:25 UTC。
const CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBYDCCAQagAwIBAgIUNEOZlznuFPR9UhUHAIzzzlhPYdYwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLb3dsLmV4YW1wbGUwHhcNMjYxMDE2MTU1MzI1WhcNMzYxMDEz
MTU1MzI1WjAWMRQwEgYDVQQDDAtvd2wuZXhhbXBsZTBZMBMGByqGSM49AgEGCCqG
SM49AwEHA0IABLqj2tn916OT0EbuRVEB2OWF3H1aGPIp7xtkKQEen/fogAv1IIud
XD8UZaWnKM4hlWZXKQFbb25G76huqTVUhtKjMjAwMB0GA1UdDgQWBBSsqPeFhg6U
wh04LKgWIGZJVZyG/TAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUC
IQDv5eltUxHf0vYg5GmMnDBQFTwoFB5jdsAXk14xsNReRwIgMYceR7U62VI73WP8
syp5fwaFfo4ct/G1t2E50jBYk3w=
-----END CERTIFICATE-----
";
const CERT_NOT_AFTER: i64 = 2_107_526_005;

fn shanghai() -> TimeZone {
    TimeZone::get("Asia/Shanghai").unwrap()
}

fn ro(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn log(
    c: &Connection,
    ts: f64,
    key: i64,
    kind: &str,
    status: &str,
    reason: &str,
    wait_ms: i64,
    up: i64,
    anlas: f64,
    client: &str,
) {
    c.execute(
        "INSERT INTO usage_log(ts, key_id, key_name, kind, status, reason, wait_ms, up_status, anlas, client) \
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![ts, key, "synthetic", kind, status, reason, wait_ms, up, anlas, client],
    )
    .unwrap();
}

fn ctx(dir: &Path) -> ExtraCtx {
    ExtraCtx {
        audit_dir: dir.join("audit_images"),
        caddy_data: None,
        site_host: Some("owl.example".into()),
        gate_started_at: None,
        gate_container: "owl-gate".into(),
    }
}

fn find<'a>(rep: &'a Report, area: &str) -> Vec<&'a gate_ops::patrol::Row> {
    rep.rows.iter().filter(|r| r.area == area).collect()
}

// ------------------------------------------------------------ 扩展巡检

#[test]
fn extras_on_empty_db_are_quiet_and_tolerate_missing_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &ctx(dir.path()), NOW).unwrap();
    assert_eq!(rep.worst(), Sev::Ok);
    let skew = &find(&rep, "账号")[0].text;
    // 另一条分支会给 usage_log 加 token_id；没加的时候必须是 n/a，不能报错
    let has_col = skew.contains("样本不足");
    assert!(skew.contains("n/a") || has_col, "{skew}");
    assert!(find(&rep, "Anlas")[0].text.contains("n/a"));
    assert!(find(&rep, "证书")[0].text.contains("n/a"));
    assert!(find(&rep, "原图")[0].text.contains("n/a"));
}

#[test]
fn queue_health_and_upstream_auth() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let w = rw(&db);
    add_key(&w, 1, "tok-1", false, true);
    for i in 0..10 {
        log(
            &w,
            NOW - 60.0 - i as f64,
            1,
            "image",
            "ok",
            "",
            50_000,
            200,
            0.0,
            "",
        );
    }
    for i in 0..6 {
        log(
            &w,
            NOW - 30.0 - i as f64,
            1,
            "image",
            "rejected",
            "queue_full",
            0,
            0,
            0.0,
            "",
        );
    }
    // 窗口外的不算
    log(
        &w,
        NOW - 4000.0,
        1,
        "image",
        "rejected",
        "queue_timeout",
        0,
        0,
        0.0,
        "",
    );
    for i in 0..2 {
        log(
            &w,
            NOW - 100.0 - i as f64,
            1,
            "image",
            "error",
            "",
            0,
            403,
            0.0,
            "",
        );
    }
    drop(w);
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &ctx(dir.path()), NOW).unwrap();
    let q = &find(&rep, "队列")[0];
    assert_eq!(q.sev, Sev::Warn, "{}", q.text);
    assert!(
        q.text.contains("queue_full 6") && q.text.contains("queue_timeout 0"),
        "{}",
        q.text
    );
    assert!(q.text.contains("p90 50.0s"), "{}", q.text);
    let up = find(&rep, "通道");
    assert_eq!(up[0].sev, Sev::Warn);
    assert!(up[0].text.contains("403 × 2"));
}

#[test]
fn account_skew_uses_token_id_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let w = rw(&db);
    add_key(&w, 1, "tok-1", false, true);
    w.execute("ALTER TABLE usage_log ADD COLUMN token_id TEXT", [])
        .unwrap();
    for i in 0..110 {
        w.execute(
            "INSERT INTO usage_log(ts, key_id, kind, status, token_id) VALUES (?,?,?,?,?)",
            params![NOW - 100.0 - i as f64, 1, "image", "ok", "acct-aaaaaaaa"],
        )
        .unwrap();
    }
    for i in 0..10 {
        w.execute(
            "INSERT INTO usage_log(ts, key_id, kind, status, token_id) VALUES (?,?,?,?,?)",
            params![NOW - 100.0 - i as f64, 1, "image", "ok", "acct-bbbbbbbb"],
        )
        .unwrap();
    }
    drop(w);
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &ctx(dir.path()), NOW).unwrap();
    let r = &find(&rep, "账号")[0];
    assert_eq!(r.sev, Sev::Warn, "{}", r.text);
    assert!(
        r.text.contains("acct-a") && r.text.contains("92%"),
        "{}",
        r.text
    );
}

#[test]
fn anlas_estimate_from_latest_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let w = rw(&db);
    add_key(&w, 1, "tok-1", false, true);
    // 对账之前的一条（id=1）和之后的两条
    log(
        &w,
        NOW - 5.0 * 86400.0,
        1,
        "image",
        "ok",
        "",
        0,
        200,
        40.0,
        "",
    );
    let expires = NOW + 2.0 * 86400.0;
    let snap = serde_json::json!({
        "checked_at": NOW - 4.0 * 86400.0,
        "balance": 1000,
        "ledger": {"last_log_id": 1},
        "accounts": {"acct-a": {"fixed": 900, "purchased": 100, "expires_at": expires}},
    });
    w.execute(
        "INSERT INTO anlas_reconciliations(snapshot) VALUES (?)",
        [snap.to_string()],
    )
    .unwrap();
    log(
        &w,
        NOW - 3.0 * 86400.0,
        1,
        "image",
        "ok",
        "",
        0,
        200,
        100.0,
        "",
    );
    log(&w, NOW - 3600.0, 1, "image", "ok", "", 0, 200, 50.0, "");
    drop(w);
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &ctx(dir.path()), NOW).unwrap();
    let rows = find(&rep, "Anlas");
    assert!(
        rows[0]
            .text
            .contains("对账余额 1000，之后记账消耗 150，约剩 850"),
        "{}",
        rows[0].text
    );
    assert!(
        rows.iter()
            .any(|r| r.text.contains("对账已") && r.sev == Sev::Warn)
    );
    assert!(
        rows.iter()
            .any(|r| r.text.contains("到期") && r.sev == Sev::Warn)
    );
}

#[test]
fn originals_dir_and_tls() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let w = rw(&db);
    set_setting(&w, "audit_image_cap_mb", "1");
    drop(w);
    let audit = dir.path().join("audit_images/20260921");
    std::fs::create_dir_all(&audit).unwrap();
    std::fs::write(audit.join("a.png"), vec![0u8; 1_700_000]).unwrap();
    // Caddy 数据目录
    let certs = dir.path().join("caddy/certificates/acme-v02/owl.example");
    std::fs::create_dir_all(&certs).unwrap();
    std::fs::write(certs.join("owl.example.crt"), CERT).unwrap();
    assert_eq!(
        gate_ops::checks::cert_not_after_pem(CERT),
        Some(CERT_NOT_AFTER)
    );
    let mut c = ctx(dir.path());
    c.caddy_data = Some(dir.path().join("caddy"));
    // 证书剩 3 天
    let now = CERT_NOT_AFTER as f64 - 3.0 * 86400.0;
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &c, now).unwrap();
    let o = &find(&rep, "原图")[0];
    assert_eq!(o.sev, Sev::Red, "{}", o.text); // 1.7MB / 1MB 上限 = 162%
    let t = &find(&rep, "证书")[0];
    assert_eq!(t.sev, Sev::Red, "{}", t.text);
    assert!(
        t.text.contains("owl.example") && t.text.contains("3.0 天"),
        "{}",
        t.text
    );
}

#[test]
fn role_pending_counts_only_enabled_real_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let w = rw(&db);
    for i in 1..=3 {
        add_key(&w, i, &format!("tok-{i}"), false, i != 3);
        w.execute(
            "INSERT INTO discord_registrations(discord_id, key_id, created_at, role_granted) VALUES (?,?,?,0)",
            params![format!("90000000000000000{i}"), i, NOW - 100.0],
        )
        .unwrap();
    }
    drop(w);
    let conn = ro(&db);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &ctx(dir.path()), NOW).unwrap();
    let r = &find(&rep, "成员")[0];
    assert!(r.text.contains("2 人"), "{}", r.text);
    assert_eq!(r.sev, Sev::Ok);
}

#[test]
fn restart_note_only_when_recent() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let conn = ro(&db);
    let mut c = ctx(dir.path());
    c.gate_started_at = Some(NOW - 600.0);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &c, NOW).unwrap();
    assert!(find(&rep, "系统")[0].text.contains("10 分钟前启动"));
    c.gate_started_at = Some(NOW - 7200.0);
    let mut rep = Report::default();
    run_extra_checks(&mut rep, &conn, &c, NOW).unwrap();
    assert!(find(&rep, "系统").is_empty());
}

// ------------------------------------------------------------ notify

fn cfg(dir: &Path, api_base: &str) -> Config {
    let env = dir.join(".env");
    std::fs::write(
        &env,
        "DISCORD_BOT_TOKEN=synthetic-bot-token\nALERT_USER_ID=900000000000000009\n",
    )
    .unwrap();
    Config {
        db: dir.join("nai_gate.db"),
        env_file: env,
        health_url: "http://127.0.0.1:1/healthz".into(),
        state_dir: dir.join("state"),
        cooldown: 1800,
        api_base: api_base.into(),
        containers: vec![],
    }
}

fn rep_of(sev: Sev) -> Report {
    let mut r = Report::default();
    r.add_key(sev, "队列", "排队：合成的一行", "queue-health");
    r
}

#[tokio::test]
async fn notify_sends_on_change_once_and_recovers_once() {
    // 环境变量里不留真凭据
    unsafe_free_env();
    let dir = tempfile::tempdir().unwrap();
    let (mock, reqs) = discord_mock(DiscordScript::default()).await;
    let cfg = cfg(dir.path(), &mock.base);
    let args = NotifyArgs {
        dry_run: false,
        state_path: dir.path().join("state/notify.json"),
        cooldown_min: 60,
        red_repeat_min: 360,
    };
    let tz = shanghai();
    let sent = || {
        reqs.lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.ends_with("/messages"))
            .count()
    };

    // 正常：不发
    notify::run_with(&cfg, &args, &rep_of(Sev::Ok), NOW, &tz)
        .await
        .unwrap();
    assert_eq!(sent(), 0);
    // 变成注意：发一条
    let code = notify::run_with(&cfg, &args, &rep_of(Sev::Warn), NOW + 60.0, &tz)
        .await
        .unwrap();
    assert_eq!(code, 1);
    assert_eq!(sent(), 1);
    // 一直注意：不再发
    notify::run_with(&cfg, &args, &rep_of(Sev::Warn), NOW + 120.0, &tz)
        .await
        .unwrap();
    assert_eq!(sent(), 1);
    // 升级到危险：发
    notify::run_with(&cfg, &args, &rep_of(Sev::Red), NOW + 180.0, &tz)
        .await
        .unwrap();
    assert_eq!(sent(), 2);
    // 恢复：只发一次
    notify::run_with(&cfg, &args, &rep_of(Sev::Ok), NOW + 240.0, &tz)
        .await
        .unwrap();
    assert_eq!(sent(), 3);
    notify::run_with(&cfg, &args, &rep_of(Sev::Ok), NOW + 300.0, &tz)
        .await
        .unwrap();
    assert_eq!(sent(), 3);

    let log = reqs.lock().unwrap().clone();
    let last = log
        .iter()
        .rev()
        .find(|r| r.path.ends_with("/messages"))
        .unwrap();
    let text = last.body["content"].as_str().unwrap();
    assert!(text.contains("已恢复"), "{text}");
    assert_eq!(
        last.body["allowed_mentions"]["parse"],
        serde_json::json!([])
    );
    assert_eq!(last.auth, "Bot synthetic-bot-token");
    // 私信只发给 ALERT_USER_ID
    let dm = log
        .iter()
        .find(|r| r.path == "/users/@me/channels")
        .unwrap();
    assert_eq!(dm.body["recipient_id"], "900000000000000009");
    // 状态文件内容
    let st = notify::load_state(&args.state_path);
    assert_eq!(st.level, "ok");
    assert_eq!(st.notified_level, "ok");
    assert!(st.last_sent.contains_key("red"));
}

#[tokio::test]
async fn notify_dry_run_sends_nothing_and_keeps_state() {
    unsafe_free_env();
    let dir = tempfile::tempdir().unwrap();
    let (mock, reqs) = discord_mock(DiscordScript::default()).await;
    let cfg = cfg(dir.path(), &mock.base);
    let args = NotifyArgs {
        dry_run: true,
        state_path: dir.path().join("notify.json"),
        cooldown_min: 60,
        red_repeat_min: 360,
    };
    notify::run_with(&cfg, &args, &rep_of(Sev::Red), NOW, &shanghai())
        .await
        .unwrap();
    assert!(reqs.lock().unwrap().is_empty());
    assert!(!args.state_path.exists());
}

#[tokio::test]
async fn notify_failed_send_is_retried_next_round() {
    unsafe_free_env();
    let dir = tempfile::tempdir().unwrap();
    let (mock, _reqs) = discord_mock(DiscordScript {
        message_status: 500,
        ..Default::default()
    })
    .await;
    let cfg = cfg(dir.path(), &mock.base);
    let args = NotifyArgs {
        dry_run: false,
        state_path: dir.path().join("notify.json"),
        cooldown_min: 60,
        red_repeat_min: 360,
    };
    let code = notify::run_with(&cfg, &args, &rep_of(Sev::Red), NOW, &shanghai())
        .await
        .unwrap();
    assert_eq!(code, 3);
    let st: NotifyState = notify::load_state(&args.state_path);
    assert_eq!(st.level, "red");
    assert_eq!(st.notified_level, "ok", "没发成功不能记成已通知");
}

/// 不让开发机上的真实凭据影响测试（读凭据时进程环境变量优先于 .env）。
fn unsafe_free_env() {
    // 测试进程里没人设置这两个变量；这里只是断言一下，免得以后有人在 CI 里设了
    assert!(
        std::env::var("DISCORD_BOT_TOKEN").is_err(),
        "测试环境不该有 DISCORD_BOT_TOKEN"
    );
    assert!(std::env::var("ALERT_USER_ID").is_err());
}

// ------------------------------------------------------------ report（快照）

fn snowflake(unix_secs: f64) -> String {
    let ms = (unix_secs * 1000.0) as u64 - 1_420_070_400_000;
    (ms << 22).to_string()
}

const UA_A: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) SyntheticBrowser/1.0";
const UA_IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) SyntheticMobile/1.0";
const UA_ANDROID: &str = "Mozilla/5.0 (Linux; Android 14) SyntheticMobile/1.0";

fn build_report_db(dir: &Path) -> std::path::PathBuf {
    let db = new_db(dir);
    let w = rw(&db);
    for (id, name, test) in [
        (1, "alpha", false),
        (2, "beta", false),
        (3, "gamma", false),
        (4, "synthetic-test", true),
        (5, "n5", false),
        (6, "n6", false),
        (7, "n7", false),
        (8, "n8", false),
        (9, "n9", false),
    ] {
        w.execute(
            "INSERT INTO api_keys(id, name, token, enabled, is_test, created_at) VALUES (?,?,?,1,?,?)",
            params![id, name, format!("tok-{id}"), test, 1.7e9],
        )
        .unwrap();
    }
    // 1 号：重试轰炸（22 个 key_rate + 3 个 key_busy，每 2 秒一次）
    for i in 0..25 {
        let reason = if i < 22 { "key_rate" } else { "key_busy" };
        log(
            &w,
            NOW - 3600.0 + 2.0 * i as f64,
            1,
            "image",
            "rejected",
            reason,
            0,
            0,
            0.0,
            UA_A,
        );
    }
    // 2 号：每分钟一张，连续 4 小时，共用设备 UA_A
    for i in 0..240 {
        log(
            &w,
            NOW - 86400.0 + 60.0 * i as f64,
            2,
            "image",
            "ok",
            "",
            0,
            200,
            0.0,
            UA_A,
        );
    }
    // 3 号：三天都碰到个人额度上限；同一天 iOS + Android
    for d in 0..3 {
        log(
            &w,
            NOW - 86400.0 * (d as f64 + 1.0),
            3,
            "image",
            "rejected",
            "member_quota",
            0,
            0,
            0.0,
            UA_IOS,
        );
    }
    log(
        &w,
        NOW - 86400.0 * 1.0 + 30.0,
        3,
        "image",
        "ok",
        "",
        0,
        200,
        0.0,
        UA_ANDROID,
    );
    // 测试 Key 不算
    log(
        &w,
        NOW - 100.0,
        4,
        "image",
        "rejected",
        "key_rate",
        0,
        0,
        0.0,
        UA_A,
    );
    // 审计：1、2 号相同的长提示词；3 号命中词表
    let long = "synthetic long prompt ".repeat(6);
    for (k, p) in [
        (1, long.as_str()),
        (2, long.as_str()),
        (3, "a forbiddenword here"),
        (3, "harmless"),
    ] {
        w.execute(
            "INSERT INTO generation_audit(ts, key_id, key_name, prompt) VALUES (?,?,?,?)",
            params![NOW - 500.0, k, "synthetic", p],
        )
        .unwrap();
    }
    // 领取：1 号账号年龄 2 天；5~9 号 5 分钟内连领；其余老号
    let reg = |w: &Connection, key: i64, ts: f64, age_days: f64| {
        w.execute(
            "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
            params![snowflake(ts - age_days * 86400.0), key, ts],
        )
        .unwrap();
    };
    reg(&w, 1, NOW - 5.0 * 86400.0, 2.0);
    reg(&w, 2, NOW - 4.0 * 86400.0, 900.0);
    for (i, k) in (5..=9).enumerate() {
        reg(&w, k, NOW - 3.0 * 86400.0 + 60.0 * i as f64, 400.0);
    }
    // 现有暂停
    w.execute(
        "INSERT INTO share_state(key_id, strikes, paused_until) VALUES (3, 2, ?)",
        [NOW + 3600.0],
    )
    .unwrap();
    db
}

/// 对照 tests/snapshots/ops/<name>；`UPDATE_SNAPSHOT=1` 本地重写（CI 里禁止）。
fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/ops")
        .join(name);
    if std::env::var("UPDATE_SNAPSHOT").is_ok_and(|v| v == "1") {
        assert!(std::env::var("CI").is_err(), "CI 里禁止更新快照");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "缺快照 {}；本地用 UPDATE_SNAPSHOT=1 生成并审阅",
            path.display()
        )
    });
    assert_eq!(
        actual, want,
        "快照 {name} 不一致（有意改动请 UPDATE_SNAPSHOT=1 后审阅 git diff）"
    );
}

#[test]
fn weekly_report_snapshot_without_terms() {
    let dir = tempfile::tempdir().unwrap();
    let db = build_report_db(dir.path());
    let text = report::build(
        &ro(&db),
        &report::ReportArgs {
            days: 7,
            terms: Some(std::path::PathBuf::from("/nonexistent/risk-terms.txt")),
            until: NOW,
        },
        &shanghai(),
    )
    .unwrap();
    golden("report_weekly.txt", &text);
    // 只有 Key 编号 / 显示名 / 计数：没有提示词、UA 原文
    assert!(!text.contains("synthetic long prompt"));
    assert!(!text.contains("SyntheticBrowser"));
    assert!(!text.contains("forbiddenword"));
}

#[test]
fn weekly_report_with_terms_file_prints_counts_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = build_report_db(dir.path());
    let terms = dir.path().join("terms.txt");
    std::fs::write(&terms, "## 合成词表\n# demo-category\nforbidden\\w+\n(broken\n# empty-category\nnever-matches-anything\n").unwrap();
    let text = report::build(
        &ro(&db),
        &report::ReportArgs {
            days: 7,
            terms: Some(terms),
            until: NOW,
        },
        &shanghai(),
    )
    .unwrap();
    golden("report_weekly_terms.txt", &text);
    assert!(!text.contains("forbiddenword"));
}

#[test]
fn shipped_example_terms_file_is_empty_of_rules() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/risk-terms.example.txt");
    let text = std::fs::read_to_string(&p).expect("仓库应带 deploy/risk-terms.example.txt");
    let (rules, bad) = report::parse_terms(&text);
    assert!(rules.is_empty(), "示例词表里不许有任何规则");
    assert!(bad.is_empty());
    for l in text.lines() {
        assert!(
            l.trim().is_empty() || l.starts_with("##"),
            "示例词表只许有 ## 注释：{l}"
        );
    }
}

// ------------------------------------------------------------ traffic

#[test]
fn traffic_summary_strips_ip_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        // 带 IP / 头 / 查询参数的原始 Caddy 行：这些都不该出现在输出里
        r#"{"level":"info","ts":1789999000.5,"msg":"handled request","request":{"remote_ip":"203.0.113.9","client_ip":"203.0.113.9","method":"POST","uri":"/ai/generate-image?secret=abc","headers":{"Authorization":["Bearer synthetic"]}},"duration":2.0,"size":10,"status":200}"#,
        r#"{"level":"info","ts":1789999010.0,"request":{"method":"POST","uri":"/ai/generate-image-stream"},"duration":4.0,"status":200}"#,
        r#"{"level":"info","ts":1789999020.0,"request":{"method":"GET","uri":"/healthz"},"duration":0.001,"status":200}"#,
        r#"{"level":"info","ts":1789999030.0,"request":{"method":"GET","uri":"/admin/api/members?page=2"},"duration":0.05,"status":401}"#,
        r#"{"level":"info","ts":1789999040.0,"request":{"method":"GET","uri":"/v1/me"},"duration":0.01,"status":502}"#,
        r#"{"level":"info","ts":1789000000.0,"request":{"method":"GET","uri":"/old"},"duration":0.01,"status":200}"#,
        "not json at all",
    ];
    let p = dir.path().join("access.log");
    std::fs::write(&p, lines.join("\n")).unwrap();
    // 再来一个 .gz 滚动文件
    {
        use std::io::Write;
        let f = std::fs::File::create(dir.path().join("access-old.log.gz")).unwrap();
        let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        gz.write_all(lines[2].as_bytes()).unwrap();
        gz.finish().unwrap();
    }
    let text = traffic::run_tz(&[dir.path().to_path_buf()], 3600.0, NOW, &shanghai()).unwrap();
    assert!(!text.contains("203.0.113"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    assert!(!text.contains("Authorization"), "{text}");
    assert!(text.contains("image-gen") && text.contains("admin-api") && text.contains("gate-api"));
    // 6 个窗口内请求（5 个来自 access.log + 1 个来自 .gz）；/old 在窗口外；1 行坏数据被跳过
    assert!(text.contains("共 6 个请求"), "{text}");
    assert!(text.contains("跳过 1 行"), "{text}");
    let _ = p;
    golden("traffic.txt", &text);
}
