//! 巡检各项检查的单元测试：全部在合成数据库上跑（gate-db 建库 + 手工灌数据），不碰 docker / 网络。
//! 期望的文案都是照 deploy/ops/patrol.py 手工算出来的。

mod common;

use common::*;
use gate_ops::patrol::*;
use jiff::tz::TimeZone;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::Path;

/// 2026-09-21 22:13:20 +08:00
const NOW: f64 = 1_790_000_000.0;

fn shanghai() -> TimeZone {
    TimeZone::get("Asia/Shanghai").unwrap()
}

fn cfg(dir: &Path) -> Config {
    Config {
        db: dir.join("nai_gate.db"),
        env_file: dir.join(".env"),
        health_url: "http://127.0.0.1:1/healthz".into(),
        queue_url: "http://127.0.0.1:1/queue-status".into(),
        state_dir: dir.join("state"),
        cooldown: 1800,
        stale_seconds: 2700,
        api_base: "http://127.0.0.1:1".into(),
        containers: vec!["nai-gate".into(), "nai-gate-discord".into(), "caddy".into()],
    }
}

fn ro(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn rows(rep: &Report) -> Vec<(Sev, &'static str, String)> {
    rep.rows
        .iter()
        .map(|r| (r.sev, r.area, r.text.clone()))
        .collect()
}

fn text_of(rep: &Report, needle: &str) -> Row {
    rep.rows
        .iter()
        .find(|r| r.text.contains(needle))
        .unwrap_or_else(|| panic!("报告里没有包含 {needle:?} 的行：{:#?}", rows(rep)))
        .clone()
}

fn real_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn healthy_infra() -> Infra {
    Infra {
        containers: Some(vec![
            ("nai-gate".to_string(), true),
            ("nai-gate-discord".to_string(), true),
            ("caddy".to_string(), true),
        ]),
        health_code: 200,
        queue: json!({"global": {"waiting": 0, "active": 1}, "image_cooldown_remaining": 0}),
        resources: Ok((900, 24)),
        db_bytes: Some(2 * 1048576),
    }
}

// ------------------------------------------------------------ Report / 系统探测

/// 非 ok 行的稳定标识（按出现顺序）。
fn pkeys(rep: &Report) -> Vec<String> {
    rep.rows
        .iter()
        .filter(|r| r.sev != Sev::Ok)
        .map(|r| r.key.clone())
        .collect()
}

#[test]
fn report_keys_use_stable_key_or_text() {
    let mut r = Report::default();
    r.add(Sev::Ok, "系统", "fine");
    r.add(Sev::Warn, "系统", "text with 12 numbers");
    r.add_key(Sev::Red, "数据", "bug 3 times", "bug-count");
    r.known("数据", "旧问题");
    // 没给标识时用文本，数字抹成 #（数字变了还是同一个问题）
    assert_eq!(pkeys(&r), vec!["text with # numbers", "bug-count"]);
    assert_eq!(r.red_keys(), vec!["bug-count"]);
    assert_eq!(r.worst(), Sev::Red);
    assert_eq!(r.problems().len(), 2);
    let last = r.rows.last().unwrap();
    assert_eq!(
        (last.sev, last.text.as_str()),
        (Sev::Ok, "（已知，不告警）旧问题")
    );
}

#[test]
fn queue_status_texts() {
    let (sev, area, text) = status_of_queue(
        &json!({"global": {"waiting": 3, "active": 2}, "image_cooldown_remaining": 0}),
    );
    assert_eq!(
        (sev, area, text.as_str()),
        (Sev::Ok, "系统", "队列 active 2 / waiting 3")
    );
    let (sev, _, text) = status_of_queue(&json!({"global": {"waiting": 21, "active": 0}}));
    assert_eq!(
        (sev, text.as_str()),
        (Sev::Warn, "队列 active 0 / waiting 21")
    );
    let (sev, _, text) = status_of_queue(
        &json!({"global": {"waiting": "5.7", "active": 1}, "image_cooldown_remaining": 30}),
    );
    assert_eq!(
        (sev, text.as_str()),
        (Sev::Warn, "队列 active 1 / waiting 5 · 上游冷却 30s")
    );
    let (sev, _, text) = status_of_queue(&json!({}));
    assert_eq!((sev, text.as_str()), (Sev::Warn, "队列状态读取失败"));
}

#[test]
fn infra_rows_and_thresholds() {
    let infra = Infra {
        containers: Some(vec![
            ("nai-gate".to_string(), true),
            ("nai-gate-discord".to_string(), false),
            ("caddy".to_string(), true),
        ]),
        health_code: 0,
        queue: json!({}),
        resources: Ok((150, 85)),
        db_bytes: Some(3 * 1048576),
    };
    let mut rep = Report::default();
    check_infra(&mut rep, &infra);
    assert_eq!(
        rows(&rep),
        vec![
            (Sev::Ok, "系统", "nai-gate 容器 运行中".to_string()),
            (
                Sev::Red,
                "系统",
                "nai-gate-discord 容器 没有运行".to_string()
            ),
            (Sev::Ok, "系统", "caddy 容器 运行中".to_string()),
            (Sev::Red, "系统", "/healthz → 无响应".to_string()),
            (Sev::Warn, "系统", "队列状态读取失败".to_string()),
            (Sev::Red, "系统", "可用内存 150MB · 磁盘 85%".to_string()),
            (Sev::Warn, "系统", "磁盘已用 85%".to_string()),
            (Sev::Ok, "系统", "数据库 3.0MB".to_string()),
        ]
    );
    assert_eq!(rep.data["mem_available_mb"], json!(150));
    assert_eq!(rep.data["disk_used_pct"], json!(85));
    assert!(
        rep.data["db_mb"].is_f64(),
        "db_mb 在旧版里是 float（round(x, 1)）"
    );
}

#[test]
fn infra_failures_are_reported_not_fatal() {
    let infra = Infra {
        containers: None,
        health_code: 503,
        queue: json!({}),
        resources: Err("FileNotFoundError"),
        db_bytes: None,
    };
    let mut rep = Report::default();
    check_infra(&mut rep, &infra);
    let texts: Vec<_> = rep.rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "读不到容器状态（docker 不可用？）",
            "/healthz → 503",
            "队列状态读取失败",
            "资源读取失败：FileNotFoundError"
        ]
    );
    assert_eq!(rep.rows[0].sev, Sev::Warn);
    assert_eq!(rep.rows[1].sev, Sev::Red);
    assert!(!rep.data.contains_key("db_mb"));
}

#[test]
fn memory_and_disk_thresholds() {
    for (avail, disk, mem_sev, disk_sev) in [
        (349, 80, Sev::Warn, Sev::Ok),
        (350, 81, Sev::Ok, Sev::Warn),
        (199, 91, Sev::Red, Sev::Red),
        (200, 90, Sev::Warn, Sev::Warn),
    ] {
        let mut infra = healthy_infra();
        infra.resources = Ok((avail, disk));
        let mut rep = Report::default();
        check_infra(&mut rep, &infra);
        let mem = text_of(&rep, "可用内存");
        let dsk = text_of(&rep, "磁盘已用");
        assert_eq!(
            (mem.sev, dsk.sev),
            (mem_sev, disk_sev),
            "avail={avail} disk={disk}"
        );
    }
}

#[test]
fn parse_free_and_df_output() {
    let free = "              total        used        free      shared  buff/cache   available\n\
                Mem:           1963         812         151          12        1000         900\n\
                Swap:             0           0           0\n";
    let df = "Filesystem     1K-blocks    Used Available Use% Mounted on\n/dev/vda1       41151808 9000000  30000000  24% /\n";
    assert_eq!(parse_resources(free, df), Ok((900, 24)));
    assert_eq!(parse_resources("", df), Err("IndexError"));
    assert_eq!(
        parse_resources(free, "Filesystem only header\n"),
        Err("IndexError")
    );
    assert_eq!(
        parse_resources(free, "h\n/dev/x 1 2 3 abc% /\n"),
        Err("ValueError")
    );
}

#[test]
fn docker_time_parsing() {
    assert_eq!(parse_docker_time("2026-10-13T08:00:00Z"), 1791878400.0);
    // 纳秒截到微秒
    assert_eq!(
        parse_docker_time("2026-10-13T08:00:00.123456789Z"),
        1791878400.123456
    );
    assert_eq!(parse_docker_time("2026-10-13T08:00:00.5Z"), 1791878400.5);
    // 带偏移（接受 +08:00 与 +0800）
    assert_eq!(
        parse_docker_time("2026-10-13T16:00:00.5+08:00"),
        1791878400.5
    );
    assert_eq!(
        parse_docker_time(" 2026-10-13T16:00:00+0800 \n"),
        1791878400.0
    );
    // 无法解析一律 0.0
    for bad in [
        "",
        "   ",
        "not a time",
        "2026-10-13 08:00:00Z",
        "2026-13-45T25:61:61Z",
        "2026-10-13T08:00:00",
    ] {
        assert_eq!(parse_docker_time(bad), 0.0, "{bad:?}");
    }
}

#[test]
fn restart_history_accumulates_dedups_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    assert_eq!(restart_history(&state, 1000.0), vec![1000.0]);
    assert_eq!(
        std::fs::read_to_string(state.join("restarts.json")).unwrap(),
        "[1000.0]"
    );
    // 与上一次相差不到 1 秒视为同一次
    assert_eq!(restart_history(&state, 1000.5), vec![1000.0]);
    assert_eq!(restart_history(&state, 2000.25), vec![1000.0, 2000.25]);
    assert_eq!(
        std::fs::read_to_string(state.join("restarts.json")).unwrap(),
        "[1000.0, 2000.25]"
    );
    // 拿不到启动时间（0.0）：只读不加
    assert_eq!(restart_history(&state, 0.0), vec![1000.0, 2000.25]);
    // 只留最近 20 次
    let mut last = vec![];
    for i in 1..=25 {
        last = restart_history(&state, 10_000.0 + f64::from(i) * 100.0);
    }
    assert_eq!(last.len(), 20);
    assert_eq!(last[0], 10_600.0);
    assert_eq!(*last.last().unwrap(), 12_500.0);
}

#[test]
fn restart_history_survives_corrupt_or_unwritable_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("restarts.json"), "{ not json").unwrap();
    assert_eq!(restart_history(&state, 5.0), vec![5.0]);
    // 状态目录其实是个文件：写不进去也不报错，照常返回
    let file = dir.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(restart_history(&file, 7.0), vec![7.0]);
}

// ------------------------------------------------------------ 数据库检查

#[test]
fn upstream_capacity_and_v5() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    let (today, yesterday): (String, String) = c
        .query_row(
            "SELECT date('now','+8 hours'), date('now','+8 hours','-1 day')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    for (tok, day, n) in [
        ("acct-a", &today, 450),
        ("acct-b", &today, 450),
        ("acct-b", &yesterday, 100),
    ] {
        c.execute(
            "INSERT INTO upstream_token_counters(token_id, day, images) VALUES (?,?,?)",
            rusqlite::params![tok, day, n],
        )
        .unwrap();
    }
    set_setting(&c, "guard_account_daily_cap", "500");
    set_setting(
        &c,
        "quota_algo_last",
        r#"{"v5": {"percent": 1.5, "rate": 12.5}}"#,
    );

    let mut rep = Report::default();
    check_upstream(&mut rep, &ro(&path)).unwrap();
    // 额度是 PVP（先到先得）：不拿「每人额度 × 人数」去比日容量
    assert_eq!(
        rows(&rep),
        vec![
            (
                Sev::Warn,
                "上游",
                "今日已出图 900 / 1000 张（90%）· 2 个账号 × 500 张".to_string()
            ),
            (
                Sev::Warn,
                "上游",
                "V5 剩余 1.5%（恢复 12.5%/天）".to_string()
            ),
        ]
    );
    assert_eq!(rep.data["today_images"], json!(900));
    assert_eq!(rep.data["site_daily"], json!(1000));
    assert_eq!(rep.data["capacity_used_pct"], json!(90.0));
    assert!(rep.data["capacity_used_pct"].is_f64());
    assert_eq!(rep.data["v5_percent"], json!(1.5));
    // 键的顺序决定 --json 的输出顺序
    let keys: Vec<_> = rep.data.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "today_images",
            "site_daily",
            "capacity_used_pct",
            "v5_percent"
        ]
    );
}

#[test]
fn upstream_defaults_and_quirks() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let mut rep = Report::default();
    check_upstream(&mut rep, &ro(&path)).unwrap();
    // 没有任何计数行：按 1 个账号 × 默认 1000 张；没有 V5 记录
    assert_eq!(
        rows(&rep),
        vec![(
            Sev::Ok,
            "上游",
            "今日已出图 0 / 1000 张（0%）· 1 个账号 × 1000 张".to_string()
        )]
    );
    assert!(rep.data["capacity_used_pct"].is_f64());

    // 日上限被设成 0：旧版 share 是整数 0，round(0, 1) 仍是整数，JSON 里是 0 而不是 0.0
    set_setting(&rw(&path), "guard_account_daily_cap", "0");
    let mut rep = Report::default();
    check_upstream(&mut rep, &ro(&path)).unwrap();
    assert_eq!(rep.data["capacity_used_pct"], json!(0));
    assert!(rep.data["capacity_used_pct"].is_i64());
}

#[test]
fn v5_percent_edge_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    for (raw, expect) in [
        (
            r#"{"v5": {"percent": 2, "rate": null}}"#,
            Some((Sev::Warn, "V5 剩余 2%（恢复 None%/天）")),
        ),
        (
            r#"{"v5": {"percent": 2.5, "rate": 10}}"#,
            Some((Sev::Ok, "V5 剩余 2.5%（恢复 10%/天）")),
        ),
        (
            r#"{"v5": {"percent": "1.0", "rate": 3.0}}"#,
            Some((Sev::Warn, "V5 剩余 1.0%（恢复 3.0%/天）")),
        ),
        (
            r#"{"v5": {"percent": "x"}}"#,
            Some((Sev::Ok, "V5 剩余 x%（恢复 None%/天）")),
        ),
        (r#"{"v5": {"percent": null}}"#, None),
        (r#"{"v5": {}}"#, None),
        (r#"{"v5": null}"#, None),
        ("not json", None),
        ("[1, 2]", None),
    ] {
        set_setting(&c, "quota_algo_last", raw);
        let mut rep = Report::default();
        check_upstream(&mut rep, &ro(&path)).unwrap();
        let v5 = rep
            .rows
            .iter()
            .find(|r| r.text.starts_with("V5 剩余"))
            .map(|r| (r.sev, r.text.as_str()));
        assert_eq!(v5, expect, "{raw}");
    }
}

/// 灌一组覆盖所有分支的成员请求记录（全是合成数据）。
fn seed_members(c: &Connection) {
    add_key(c, 1, "nai-synthetic-key-1", false, true);
    add_key(c, 2, "nai-synthetic-key-2", true, true); // 测试 Key：不计入活跃成员
    add_key(c, 3, "nai-synthetic-key-3", false, true);
    for (dt, wait) in [(100.0, 1000), (90.0, 3000), (60.0, 5000)] {
        add_log(
            c,
            &Log1 {
                wait_ms: wait,
                ..Log1::image_ok(NOW - dt, 1)
            },
        );
    }
    add_log(
        c,
        &Log1 {
            wait_ms: 130_000,
            ..Log1::image_ok(NOW - 500.0, 2)
        },
    );
    for dt in [7200.0, 7000.0] {
        add_log(c, &Log1::image_ok(NOW - dt, 3));
    }
    for (i, reason) in [
        "paced",
        "paced",
        "paced",
        "paced",
        "paced",
        "daily_cap",
        "queue_full",
        "queue_full",
    ]
    .into_iter()
    .enumerate()
    {
        add_log(
            c,
            &Log1 {
                status: "rejected",
                images: 0,
                reason,
                up_status: 0,
                ..Log1::image_ok(NOW - 20_000.0 - i as f64, 1)
            },
        );
    }
    add_log(
        c,
        &Log1 {
            status: "error",
            images: 0,
            up_status: 502,
            ..Log1::image_ok(NOW - 20_100.0, 1)
        },
    );
}

#[test]
fn members_full_picture() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    seed_members(&rw(&path));
    let mut rep = Report::default();
    check_members(&mut rep, &ro(&path), NOW).unwrap();
    assert_eq!(
        rows(&rep),
        vec![
            (
                Sev::Warn,
                "成员",
                "近 1 小时成功 4 次 / 4 张 · 平均等待 35s · 最长 130s".to_string()
            ),
            // 被拒不算故障；只有 1 次出错，样本太小不报
            (
                Sev::Ok,
                "成员",
                "24 小时：成功 6 · 被拒 8（限流 / 额度，按设计）· 出错 1（出错率 14.3%）"
                    .to_string()
            ),
            (
                Sev::Warn,
                "成员",
                "24 小时拒绝原因：paced 5、queue_full 2、daily_cap 1".to_string()
            ),
            (
                Sev::Ok,
                "成员",
                "近 1 小时间隔疑似被绕过（同 Key <12 秒连发）：1 次".to_string()
            ),
        ]
    );
    assert_eq!(rep.data["hour_ok"], json!(4));
    assert_eq!(rep.data["hour_worst_wait_ms"], json!(130_000));
    assert_eq!(
        rep.data["day_status"],
        json!({"error": 1, "ok": 6, "rejected": 8})
    );
    assert_eq!(
        rep.data["day_reasons"],
        json!([["paced", 5], ["queue_full", 2], ["daily_cap", 1]])
    );
    assert_eq!(
        rep.data["active_members_24h"],
        json!(2),
        "测试 Key 不算活跃成员"
    );
    assert!(
        rep.data.get("clients_24h").is_none(),
        "客户端分布已删除（docs/v3-deviations.md #50）"
    );
}

#[test]
fn members_empty_db_and_interval_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let mut rep = Report::default();
    check_members(&mut rep, &ro(&path), NOW).unwrap();
    assert_eq!(
        rows(&rep),
        vec![
            (
                Sev::Ok,
                "成员",
                "近 1 小时成功 0 次 / 0 张 · 平均等待 0s · 最长 0s".to_string()
            ),
            (
                Sev::Ok,
                "成员",
                "24 小时：成功 0 · 被拒 0（限流 / 额度，按设计）· 出错 0（出错率 0.0%）"
                    .to_string()
            ),
            (
                Sev::Ok,
                "成员",
                "近 1 小时间隔疑似被绕过（同 Key <12 秒连发）：0 次".to_string()
            ),
        ]
    );
    // 同一把 Key 每 5 秒一张，连续 5 张 = 4 次 <12 秒的间隔 > 3 次 → 注意
    let c = rw(&path);
    add_key(&c, 9, "nai-synthetic-key-9", false, true);
    for i in 0..5 {
        add_log(&c, &Log1::image_ok(NOW - 300.0 + f64::from(i) * 5.0, 9));
    }
    let mut rep = Report::default();
    check_members(&mut rep, &ro(&path), NOW).unwrap();
    let r = text_of(&rep, "间隔疑似被绕过");
    assert_eq!(
        (r.sev, r.text.as_str()),
        (
            Sev::Warn,
            "近 1 小时间隔疑似被绕过（同 Key <12 秒连发）：4 次"
        )
    );
}

#[test]
fn registration_status_without_waitlist() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    for (k, v) in [
        ("register_open", "1"),
        ("register_daily_images", "20"),
        ("register_daily_v5", "3"),
        ("register_max_users", "50"),
        ("issue_hourly_cap", "2"),
    ] {
        set_setting(&c, k, v);
    }
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    add_key(&c, 2, "nai-synthetic-key-2", false, true);
    add_key(&c, 3, "nai-synthetic-key-3", false, true);
    c.execute("UPDATE api_keys SET expires_at=? WHERE id=2", [NOW - 10.0])
        .unwrap(); // 已过期：不算「已领」
    c.execute(
        "UPDATE api_keys SET expires_at=? WHERE id=3",
        [NOW + 1000.0],
    )
    .unwrap();
    for (did, kid, at) in [
        ("900000000000000001", 1, NOW - 100.0),
        ("900000000000000002", 2, NOW - 5000.0),
        ("900000000000000003", 3, NOW - 90_000.0),
    ] {
        c.execute(
            "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
            rusqlite::params![did, kid, at],
        )
        .unwrap();
    }
    let env = dir.path().join(".env");
    std::fs::write(
        &env,
        "REGISTRATION_BRIDGE_SECRET=synthetic\nREGISTER_DAILY_IMAGES=30\nREGISTER_MAX_USERS=50\n",
    )
    .unwrap();

    let mut rep = Report::default();
    check_registration(&mut rep, &ro(&path), NOW, &env).unwrap();
    // 后台每人 20 张、环境变量写的是 30：额度是动态的，两边对不上是常态，不报
    assert_eq!(
        rows(&rep),
        vec![
            (
                Sev::Ok,
                "注册",
                "开放 · 已领 2 人 / 名额 50 · 每人 20 张（V5 3）· 每小时最多发 2 个".to_string()
            ),
            (
                Sev::Ok,
                "注册",
                "新注册：24 小时 2 人 · 1 小时 1 人".to_string()
            ),
        ]
    );
    // 候补功能已删除：文案和 data 里都不能再出现
    assert!(rep.rows.iter().all(|r| !r.text.contains("候补")));
    assert!(!rep.data.contains_key("waitlist"));
    let keys: Vec<_> = rep.data.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "register_open",
            "registered",
            "register_daily_images",
            "register_daily_v5",
            "register_max_users",
            "new_24h",
            "new_1h",
            "issue_hourly_cap"
        ]
    );
    assert_eq!(rep.data["register_open"], json!(true));
}

#[test]
fn registration_defaults_missing_secret_and_hourly_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let no_env = dir.path().join("missing.env");
    let mut rep = Report::default();
    check_registration(&mut rep, &ro(&path), NOW, &no_env).unwrap();
    assert_eq!(
        rows(&rep),
        vec![
            (
                Sev::Ok,
                "注册",
                "关闭 · 已领 0 人 / 名额 不限 · 每人 30 张（V5 0）· 每小时最多发 12 个".to_string()
            ),
            (
                Sev::Ok,
                "注册",
                "新注册：24 小时 0 人 · 1 小时 0 人".to_string()
            ),
            (
                Sev::Warn,
                "注册",
                "网关没有 REGISTRATION_BRIDGE_SECRET：/register 会直接失败".to_string()
            ),
        ]
    );
    // 开着注册却没有桥接密钥 = 危险；本小时领取达到上限 = 注意
    let c = rw(&path);
    set_setting(&c, "register_open", "on");
    set_setting(&c, "issue_hourly_cap", "1");
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    c.execute("INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES ('900000000000000001', 1, ?)", [NOW - 10.0]).unwrap();
    let mut rep = Report::default();
    check_registration(&mut rep, &ro(&path), NOW, &no_env).unwrap();
    assert_eq!(
        text_of(&rep, "本小时领取已达上限").text,
        "本小时领取已达上限（1/1），成员会看到限流提示"
    );
    assert_eq!(text_of(&rep, "REGISTRATION_BRIDGE_SECRET").sev, Sev::Red);

    // 每小时领取上限 0 = 不限（后台显示「不限」）：本小时已领 1 人也不能报「已达上限（1/0）」
    set_setting(&c, "issue_hourly_cap", "0");
    let mut rep = Report::default();
    check_registration(&mut rep, &ro(&path), NOW, &no_env).unwrap();
    assert!(
        rep.rows.iter().all(|r| !r.text.contains("已达上限")),
        "{:?}",
        rows(&rep)
    );
    assert!(
        text_of(&rep, "每小时最多发")
            .text
            .ends_with("每小时最多发 不限")
    );
    assert_eq!(rep.data["issue_hourly_cap"], json!(0));
}

fn seed_error(
    c: &Connection,
    level: &str,
    title: &str,
    count: i64,
    last_seen: f64,
    resolved: bool,
) {
    c.execute(
        "INSERT INTO error_events(sig, source, level, title, detail, count, first_seen, last_seen, resolved_at) VALUES (?,?,?,?,?,?,?,?,?)",
        rusqlite::params![format!("sig-{title}"), "request", level, title, "", count, last_seen - 10.0, last_seen, resolved.then_some(NOW)],
    )
    .unwrap();
}

#[test]
fn data_counts_and_bug_tracking() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    let today: String = c
        .query_row("SELECT date('now','+8 hours')", [], |r| r.get(0))
        .unwrap();
    c.execute(
        "INSERT INTO counters(key_id, day, images) VALUES (1, ?, 100)",
        [&today],
    )
    .unwrap();
    c.execute(
        "INSERT INTO upstream_token_counters(token_id, day, images) VALUES ('acct-a', ?, 110)",
        [&today],
    )
    .unwrap();
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    for i in 0..3 {
        add_log(&c, &Log1::image_ok(real_now() - f64::from(i), 1)); // 「日志」口径按真实的「今天」算
    }
    let restart = NOW - 1000.0;
    seed_error(
        &c,
        "error",
        "live: GET /public/live failed",
        5,
        restart + 60.0,
        false,
    );
    seed_error(&c, "warn", "旧问题", 1, NOW - 3000.0, false);
    seed_error(&c, "warn", "新问题", 3, NOW - 60.0, false);
    seed_error(&c, "info", "信息", 1, NOW - 30.0, false);
    seed_error(&c, "error", "已处理的问题", 9, NOW - 5.0, true);

    let config = cfg(dir.path());
    let ctx = Ctx {
        now: NOW,
        cfg: &config,
        tz: shanghai(),
        restarts: vec![restart],
    };
    let mut rep = Report::default();
    check_data(&mut rep, &ro(&path), &ctx).unwrap();

    assert_eq!(
        rows(&rep),
        vec![
            (Sev::Warn, "数据", "今日计数：成员 100 · 上游 110 · 日志 3 张".to_string()),
            (Sev::Warn, "数据", "未解决 Bug：error 1 · warn 2".to_string()),
            (Sev::Ok, "数据", "[info] 信息（1 次）".to_string()),
            (Sev::Warn, "数据", "[warn] 新问题（3 次）".to_string()),
            (
                Sev::Ok,
                "数据",
                "（已知，不告警）live: GET /public/live failed（5 次）— 最近一次在网关重启（21:56）后 3 分钟内，部署窗口的正常失败".to_string()
            ),
            (Sev::Ok, "数据", "（已知，不告警）[warn] 旧问题（1 次，最近 50 分钟没再出现）".to_string()),
        ]
    );
    // 告警去重用的标识：固定名字 / 标题，不含会变的数字
    assert_eq!(pkeys(&rep), ["counts", "bug-count", "bug:新问题"]);
    assert_eq!(
        rep.data["bugs_open"],
        json!({"error": 1, "info": 1, "warn": 2})
    );
    assert_eq!(rep.data["today_counters"], json!(100));
    assert_eq!(rep.data["today_logged"], json!(3));
}

#[test]
fn data_live_error_outside_restart_window_is_a_real_warning() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    seed_error(
        &c,
        "error",
        "live: GET /public/live failed",
        7,
        NOW - 100.0,
        false,
    );
    let config = cfg(dir.path());
    // 重启发生在 10 分钟前，最近一次失败在重启 3 分钟之后 → 不再算部署窗口
    let ctx = Ctx {
        now: NOW,
        cfg: &config,
        tz: shanghai(),
        restarts: vec![NOW - 600.0],
    };
    let mut rep = Report::default();
    check_data(&mut rep, &ro(&path), &ctx).unwrap();
    let r = text_of(&rep, "live: GET");
    assert_eq!(
        (r.sev, r.text.as_str()),
        (Sev::Warn, "[error] live: GET /public/live failed（7 次）")
    );
    assert!(pkeys(&rep).contains(&"bug:live: GET /public/live failed".to_string()));
    // 同样的记录，重启恰好在 100 秒之前 → 部署窗口
    let ctx = Ctx {
        now: NOW,
        cfg: &config,
        tz: shanghai(),
        restarts: vec![NOW - 150.0],
    };
    let mut rep = Report::default();
    check_data(&mut rep, &ro(&path), &ctx).unwrap();
    assert_eq!(text_of(&rep, "live: GET").sev, Sev::Ok);
}

#[test]
fn channels_text_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    for i in 0..10 {
        let ok = i < 6;
        let l = Log1 {
            kind: "chat",
            status: if ok { "ok" } else { "error" },
            images: 0,
            detail: if ok {
                ""
            } else {
                "上游返回错误：这是一段很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长的说明文字"
            },
            ..Log1::image_ok(NOW - 600.0 + f64::from(i), 1)
        };
        add_log(&c, &l);
    }
    c.execute(
        "UPDATE usage_log SET rid='rid-synthetic-1' WHERE status='error'",
        [],
    )
    .unwrap();
    let mut rep = Report::default();
    check_channels(&mut rep, &ro(&path), NOW).unwrap();
    let r = text_of(&rep, "文本/聊天 30 分钟");
    assert_eq!(
        (r.sev, r.text.as_str()),
        (Sev::Warn, "文本/聊天 30 分钟：6/10 成功（60%）")
    );
    let when: String = c
        .query_row(
            "SELECT datetime(?, 'unixepoch', 'localtime')",
            [NOW - 600.0 + 9.0],
            |r| r.get(0),
        )
        .unwrap();
    let detail: String = "上游返回错误：这是一段很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长的说明文字".chars().take(50).collect();
    assert_eq!(
        text_of(&rep, "最近一条文本失败").text,
        format!("最近一条文本失败：{when} {detail}（rid rid-synthetic-1）")
    );
    assert_eq!(pkeys(&rep), ["text-channel", "text-last-error"]);
    assert_eq!(
        (
            rep.data["text_30m"].clone(),
            rep.data["text_ok_30m"].clone()
        ),
        (json!(10), json!(6))
    );
}

#[test]
fn channels_text_rate_bands() {
    for (ok, total, sev) in [
        (9, 10, Sev::Ok),
        (5, 10, Sev::Warn),
        // 文本是次要功能：再低也只是「注意」，不私信
        (4, 10, Sev::Warn),
        (0, 3, Sev::Warn),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = new_db(dir.path());
        let c = rw(&path);
        for i in 0..total {
            let l = Log1 {
                kind: "text",
                status: if i < ok { "ok" } else { "error" },
                images: 0,
                ..Log1::image_ok(NOW - 100.0 - f64::from(i), 1)
            };
            add_log(&c, &l);
        }
        let mut rep = Report::default();
        check_channels(&mut rep, &ro(&path), NOW).unwrap();
        assert_eq!(text_of(&rep, "文本/聊天").sev, sev, "{ok}/{total}");
    }
}

#[test]
fn channels_image_500_inpaint_429_and_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    // 上游 500 × 2（<5 → 注意）
    for i in 0..2 {
        add_log(
            &c,
            &Log1 {
                status: "error",
                images: 0,
                up_status: 500,
                ..Log1::image_ok(NOW - 300.0 - f64::from(i), 1)
            },
        );
    }
    // 局部重绘：2 成功 1 失败
    for (i, st) in ["ok", "ok", "error"].into_iter().enumerate() {
        add_log(
            &c,
            &Log1 {
                status: st,
                model: "nai-diffusion-4-5-inpainting",
                ..Log1::image_ok(NOW - 900.0 - i as f64, 1)
            },
        );
    }
    // 429：设置里的最近时间 + 日志里的次数
    set_setting(&c, "guard_last_upstream_429", &format!("{}", NOW - 600.0));
    for i in 0..2 {
        add_log(
            &c,
            &Log1 {
                status: "error",
                images: 0,
                up_status: 429,
                ..Log1::image_ok(NOW - 1200.0 - f64::from(i), 1)
            },
        );
    }
    set_setting(&c, "guard_account_hourly_cap", "10");
    let mut rep = Report::default();
    check_channels(&mut rep, &ro(&path), NOW).unwrap();
    assert_eq!(
        rows(&rep),
        vec![
            (Sev::Warn, "通道", "近 30 分钟出图上游 500 × 2（上游没给原因：按 rid 查 [upstream-5xx] 日志里的请求形态）".to_string()),
            (Sev::Warn, "通道", "局部重绘 1 小时：2/3 成功".to_string()),
            (Sev::Red, "通道", "上游 10 分钟前返回 429（限流）：上限已自动减半并进入冷却，请确认没有额外突发打到上游".to_string()),
            (Sev::Red, "通道", "近 60 分钟上游 429 × 2（账号级限流，必须立刻降速）".to_string()),
            // 成功的出图 = 2 张（局部重绘那两条）；2 < 10 × 0.9 → 正常
            (Sev::Ok, "通道", "近 60 分钟 2 张 / 每小时上限 10 张（余量 8）".to_string()),
        ]
    );
    assert_eq!(
        pkeys(&rep),
        ["image-500", "inpaint", "upstream-429", "upstream-429-count"]
    );
    assert_eq!(rep.data["inpaint_1h"], json!(3));
    assert_eq!(rep.data["hourly_used"], json!(2));
}

#[test]
fn channels_500_burst_is_only_a_warning_and_old_429_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    for i in 0..5 {
        add_log(
            &c,
            &Log1 {
                status: "error",
                images: 0,
                up_status: 500,
                ..Log1::image_ok(NOW - 60.0 - f64::from(i), 1)
            },
        );
    }
    set_setting(&c, "guard_last_upstream_429", &format!("{}", NOW - 3600.0)); // 刚好满 1 小时：不再报
    set_setting(&c, "guard_account_hourly_cap", "0"); // 0 = 没设上限，不出余量行
    let mut rep = Report::default();
    check_channels(&mut rep, &ro(&path), NOW).unwrap();
    // 一位成员反复提交上游处理不了的请求就能刷出一串 500：只算「注意」，不私信
    assert_eq!(text_of(&rep, "出图上游 500 × 5").sev, Sev::Warn);
    assert!(rep.rows.iter().all(|r| !r.text.contains("429")));
    assert!(rep.rows.iter().all(|r| !r.text.contains("每小时上限")));
}

// ------------------------------------------------------------ 整体流程与退出码

/// 死表回归：3.0 的迁移会 DROP waitlist / share_evidence，旧版巡检在这时直接崩溃。
#[test]
fn full_run_on_database_without_dropped_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = new_db(dir.path());
    let c = rw(&path);
    for t in ["waitlist", "share_evidence"] {
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name=?",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{t} 应该已被迁移删除");
    }
    let config = cfg(dir.path());
    let ctx = Ctx {
        now: NOW,
        cfg: &config,
        tz: shanghai(),
        restarts: vec![],
    };
    let mut rep = Report::default();
    run_checks(&mut rep, &ro(&path), &healthy_infra(), &ctx).unwrap();
    assert!(
        rep.rows
            .iter()
            .all(|r| !r.text.contains("候补") && !r.text.contains("防分享"))
    );
    assert!(!rep.data.contains_key("waitlist"));
    // 检查的执行顺序决定报告里的顺序
    let areas: Vec<_> = rep.rows.iter().map(|r| r.area).collect();
    let first = |a: &str| areas.iter().position(|x| *x == a).unwrap();
    assert!(
        first("系统") < first("数据")
            && first("数据") < first("注册")
            && first("注册") < first("成员")
    );
}

fn healthy_db(dir: &Path) -> std::path::PathBuf {
    let path = new_db(dir);
    let c = rw(&path);
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    for i in 0..20 {
        add_log(
            &c,
            &Log1::image_ok(real_now() - 120.0 - f64::from(i) * 20.0, 1),
        );
    }
    std::fs::write(dir.join(".env"), "REGISTRATION_BRIDGE_SECRET=synthetic\n").unwrap();
    path
}

#[tokio::test]
async fn exit_codes_follow_worst_severity() {
    let dir = tempfile::tempdir().unwrap();
    let path = healthy_db(dir.path());
    let config = cfg(dir.path());
    let args = Args::default();

    let code = run_with(
        &config,
        &args,
        &ro(&path),
        &healthy_infra(),
        vec![],
        real_now(),
        shanghai(),
    )
    .await
    .unwrap();
    assert_eq!(code, 0, "全部正常");

    let mut warn = healthy_infra();
    warn.resources = Ok((300, 24)); // 可用内存 <350MB → 注意
    assert_eq!(
        run_with(
            &config,
            &args,
            &ro(&path),
            &warn,
            vec![],
            real_now(),
            shanghai()
        )
        .await
        .unwrap(),
        1
    );

    let mut red = healthy_infra();
    red.containers = Some(vec![
        ("nai-gate".to_string(), false),
        ("nai-gate-discord".to_string(), true),
        ("caddy".to_string(), true),
    ]);
    assert_eq!(
        run_with(
            &config,
            &args,
            &ro(&path),
            &red,
            vec![],
            real_now(),
            shanghai()
        )
        .await
        .unwrap(),
        2
    );

    for flags in [
        Args {
            json: true,
            ..Args::default()
        },
        Args {
            oneline: true,
            ..Args::default()
        },
    ] {
        assert_eq!(
            run_with(
                &config,
                &flags,
                &ro(&path),
                &healthy_infra(),
                vec![],
                real_now(),
                shanghai()
            )
            .await
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn unreadable_database_is_exit_2_and_internal_error_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    config.db = dir.path().join("does-not-exist.db");
    assert_eq!(
        run(&config, &Args::default()).await.unwrap(),
        2,
        "打不开数据库 = 危险（与旧版一致）"
    );

    // 库能打开但缺表：这是巡检自己遇到的内部错误 → Err（main 转成退出码 3，而不是 1）
    let empty = dir.path().join("empty.db");
    Connection::open(&empty)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(x)")
        .unwrap();
    let r = run_with(
        &config,
        &Args::default(),
        &ro(&empty),
        &healthy_infra(),
        vec![],
        NOW,
        shanghai(),
    )
    .await;
    let err = r.expect_err("缺表必须是内部错误");
    assert!(format!("{err:#}").contains("no such table"), "{err:#}");
}

// ------------------------------------------------------------ 输出格式

fn sample_report() -> Report {
    let mut r = Report::default();
    r.add(Sev::Ok, "系统", "容器正常");
    r.add(Sev::Warn, "系统", "A 注意");
    r.add(Sev::Warn, "成员", "B 注意");
    r.add(Sev::Red, "通道", "C 危险");
    r.add(Sev::Warn, "数据", "D 注意");
    r.known("数据", "旧问题");
    r.data.insert("x".into(), json!(1.0));
    r.data.insert("tiny".into(), json!(1e-5));
    r.data.insert("flag".into(), json!(true));
    r.data
        .insert("nested".into(), json!({"a": [], "b": {}, "c": [["k", 1]]}));
    r
}

#[test]
fn oneline_puts_red_first_and_keeps_three() {
    let line = render_oneline(&sample_report(), NOW, &shanghai());
    assert_eq!(
        line,
        "[patrol RED] 09-21 22:13 通道: C 危险；系统: A 注意；成员: B 注意"
    );
    assert_eq!(
        render_oneline(&Report::default(), NOW, &shanghai()),
        "[patrol OK] 09-21 22:13 全部正常"
    );
}

#[test]
fn human_report_groups_by_area() {
    let text = render_human(&sample_report(), NOW, &shanghai());
    assert_eq!(
        text,
        "=== 巡检 2026-09-21 22:13:20 → RED ===\n\
         \x20 [ok  ] 容器正常\n\
         \x20 [warn] A 注意\n\
         \x20 [RED ] C 危险\n\
         \x20 [warn] B 注意\n\
         \x20 [warn] D 注意\n\
         \x20 [ok  ] （已知，不告警）旧问题\n\
         === 结论：RED（4 项需要关注）===\n"
    );
}

#[test]
fn json_output_matches_python_dumps() {
    let json_text = render_json(&sample_report());
    let expected = r#"{
  "worst": "red",
  "data": {
    "x": 1.0,
    "tiny": 1e-05,
    "flag": true,
    "nested": {
      "a": [],
      "b": {},
      "c": [
        [
          "k",
          1
        ]
      ]
    }
  },
  "rows": [
    {
      "severity": "ok",
      "area": "系统",
      "text": "容器正常"
    },
    {
      "severity": "warn",
      "area": "系统",
      "text": "A 注意"
    },
    {
      "severity": "warn",
      "area": "成员",
      "text": "B 注意"
    },
    {
      "severity": "red",
      "area": "通道",
      "text": "C 危险"
    },
    {
      "severity": "warn",
      "area": "数据",
      "text": "D 注意"
    },
    {
      "severity": "ok",
      "area": "数据",
      "text": "（已知，不告警）旧问题"
    }
  ]
}"#;
    assert_eq!(json_text, expected);
    let _: Value = serde_json::from_str(&json_text).unwrap();
}

#[test]
fn alert_body_lists_only_red_and_counts_warnings() {
    let mut r = Report::default();
    for i in 0..3 {
        r.add(Sev::Warn, "系统", format!("w{i}"));
    }
    for i in 0..14 {
        r.add(Sev::Red, "通道", format!("boom{i}"));
    }
    let body = alert_body(&r, NOW, &shanghai());
    let lines: Vec<_> = body.lines().collect();
    assert_eq!(lines[0], "🔎 **巡检** 09-21 22:13");
    assert_eq!(lines[1], "❗ 通道：boom0");
    assert_eq!(lines.len(), 1 + 12 + 1, "最多 12 条危险 + 一行「注意」计数");
    assert_eq!(lines[13], "（另有 3 项「注意」，不单独提醒，后台总览可看）");
}

// ------------------------------------------------------------ 告警（假 Discord）

fn alert_env(dir: &Path, extra: &str) -> Config {
    let config = cfg(dir);
    std::fs::write(
        &config.env_file,
        format!("DISCORD_BOT_TOKEN=synthetic-bot-token\n{extra}"),
    )
    .unwrap();
    config
}

fn report_with(text: &str, key: &str) -> Report {
    let mut r = Report::default();
    r.add_key(Sev::Red, "数据", text, key);
    r
}

fn warn_only() -> Report {
    let mut r = Report::default();
    r.add(
        Sev::Warn,
        "注册",
        "每人 104 张 × 161 人 = 16744 张 vs 日容量 1700 张",
    );
    r
}

fn content(post: &Req) -> String {
    post.body["content"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn warnings_alone_never_message_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let mut config = alert_env(dir.path(), "ALERT_CHANNEL_ID=900000000000000010\n");
    config.api_base = mock.base.clone();
    for i in 0..5 {
        maybe_alert(
            &config,
            &warn_only(),
            NOW + f64::from(i) * 300.0,
            &shanghai(),
        )
        .await
        .unwrap();
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "「注意」只进日志和后台，不私信"
    );
    assert!(!config.state_dir.join("last_alert.json").exists());
}

#[tokio::test]
async fn red_is_deduplicated_by_check_key_and_recovery_is_announced_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let mut config = alert_env(dir.path(), "ALERT_CHANNEL_ID=900000000000000010\n");
    config.api_base = mock.base.clone();
    let tz = shanghai();

    maybe_alert(&config, &report_with("/healthz → 502", "healthz"), NOW, &tz)
        .await
        .unwrap();
    {
        let posts = log.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].path, "/channels/900000000000000010/messages");
        assert_eq!(posts[0].auth, "Bot synthetic-bot-token");
        assert!(
            posts[0].user_agent.starts_with("DiscordBot ("),
            "UA 必须是 Cloudflare 放行的那组：{}",
            posts[0].user_agent
        );
        assert_eq!(
            posts[0].body,
            json!({"content": "🔎 **巡检** 09-21 22:13\n❗ 数据：/healthz → 502", "allowed_mentions": {"parse": []}})
        );
    }
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(config.state_dir.join("last_alert.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (state["red"].clone(), state["at"].clone()),
        (json!(true), json!(NOW))
    );
    assert_eq!(state["digest"].as_str().unwrap().len(), 16);

    // 冷却内、同一检查项（数字变了没关系），再加几条「注意」→ 不再发
    let mut same = report_with("/healthz → 503", "healthz");
    same.add(Sev::Warn, "注册", "成功率 59%");
    maybe_alert(&config, &same, NOW + 10.0, &tz).await.unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    // 冷却内、出现了新的危险项 → 立刻发
    let mut more = report_with("/healthz → 503", "healthz");
    more.add_key(Sev::Red, "系统", "磁盘已用 95%", "disk");
    maybe_alert(&config, &more, NOW + 20.0, &tz).await.unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
    // 同一批问题，冷却过后 → 再提醒一次；差 1 秒 → 不发
    let cd = config.cooldown as f64;
    maybe_alert(&config, &more, NOW + 20.0 + cd + 1.0, &tz)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 3);
    maybe_alert(&config, &more, NOW + 20.0 + 2.0 * cd, &tz)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 3);
    // 危险消失（只剩「注意」）→ 一条「已恢复」，之后不再响
    maybe_alert(&config, &warn_only(), NOW + 600.0 + 2.0 * cd, &tz)
        .await
        .unwrap();
    maybe_alert(&config, &warn_only(), NOW + 900.0 + 2.0 * cd, &tz)
        .await
        .unwrap();
    let posts = log.lock().unwrap();
    assert_eq!(posts.len(), 4);
    let recovered = content(&posts[3]);
    assert!(recovered.starts_with("✅ **巡检**"), "{recovered}");
    assert!(
        recovered.contains("/healthz → 503；磁盘已用 95%"),
        "{recovered}"
    );
}

#[tokio::test]
async fn alert_goes_to_dm_when_only_user_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let mut config = alert_env(dir.path(), "ALERT_USER_ID=900000000000000011\n");
    config.api_base = mock.base.clone();
    maybe_alert(&config, &report_with("x", "k"), NOW, &shanghai())
        .await
        .unwrap();
    let posts = log.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        (posts[0].path.as_str(), &posts[0].body),
        (
            "/users/@me/channels",
            &json!({"recipient_id": "900000000000000011"})
        )
    );
    assert_eq!(posts[1].path, "/channels/dm-900000000000000011/messages");
}

#[tokio::test]
async fn failed_alert_leaves_no_state_so_it_retries() {
    let dir = tempfile::tempdir().unwrap();
    for script in [
        DiscordScript {
            message_status: 500,
            ..Default::default()
        },
        DiscordScript {
            cloudflare_1010: true,
            ..Default::default()
        },
    ] {
        let (mock, log) = discord_mock(script).await;
        let mut config = alert_env(dir.path(), "ALERT_CHANNEL_ID=900000000000000010\n");
        config.api_base = mock.base.clone();
        let rep = report_with("x", "k");
        maybe_alert(&config, &rep, NOW, &shanghai()).await.unwrap();
        maybe_alert(&config, &rep, NOW + 1.0, &shanghai())
            .await
            .unwrap();
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "没发成功就不能记冷却，下一轮要重试"
        );
        assert!(!config.state_dir.join("last_alert.json").exists());
    }
}

#[tokio::test]
async fn unconfigured_alert_channel_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let mut config = cfg(dir.path()); // 没有 .env
    config.api_base = mock.base.clone();
    maybe_alert(&config, &report_with("x", "k"), NOW, &shanghai())
        .await
        .unwrap();
    std::fs::write(&config.env_file, "DISCORD_BOT_TOKEN=synthetic-bot-token\n").unwrap(); // 有 token 但没有收件人
    maybe_alert(&config, &report_with("x", "k"), NOW, &shanghai())
        .await
        .unwrap();
    assert!(log.lock().unwrap().is_empty());
}
