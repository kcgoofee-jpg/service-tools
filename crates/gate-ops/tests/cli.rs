//! 二进制 `nai-ops` 的命令行层：参数解析、退出码、默认只演练。
//! 所有子进程都清掉 Discord 相关环境变量，并把 `DISCORD_API_BASE` 指向本机一个没人监听的端口——
//! 就算代码出错也不可能真的发出消息。

mod common;

use common::*;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn nai(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nai-ops"));
    c.current_dir(dir)
        .env_remove("DISCORD_BOT_TOKEN")
        .env_remove("ANNOUNCE_CHANNEL_ID")
        .env_remove("NAI_GATE_DB")
        .env_remove("SMOKE_KEY")
        .env_remove("SMOKE_BASE")
        .env("DISCORD_API_BASE", "http://127.0.0.1:1")
        .env("DATA_DIR", dir.join("data"))
        // 巡检：健康检查地址、状态目录都指向沙盒
        .env("NAI_GATE_HEALTH", "http://127.0.0.1:1/healthz")
        .env("NAI_GATE_QUEUE", "http://127.0.0.1:1/queue-status")
        .env("PATROL_STATE_DIR", dir.join("state"))
        .env("NAI_GATE_ENV", dir.join("missing.env"));
    c
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

#[test]
fn help_lists_every_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let o = nai(dir.path()).arg("--help").output().unwrap();
    assert_eq!(code(&o), 0);
    let text = out(&o);
    for sub in [
        "patrol",
        "snapshot",
        "ro-sanitize",
        "prompt-stats",
        "incident",
        "announce",
        "dm-broadcast",
        "smoke",
    ] {
        assert!(text.contains(sub), "--help 里没有 {sub}：{text}");
    }
}

#[test]
fn patrol_exit_codes_distinguish_danger_from_its_own_crash() {
    let dir = tempfile::tempdir().unwrap();
    // 打不开数据库 = 危险（旧版也是 2），输出写在 stdout
    let o = nai(dir.path())
        .args(["patrol", "--oneline", "--db"])
        .arg(dir.path().join("none.db"))
        .output()
        .unwrap();
    assert_eq!(code(&o), 2);
    assert!(out(&o).starts_with("[patrol] 打不开数据库 "), "{}", out(&o));

    // 库能打开但缺表 = 巡检自己出错：退出码 3（旧版是 1，systemd 分不出和「有警告」的区别）
    let bad = dir.path().join("bad.db");
    rusqlite::Connection::open(&bad)
        .unwrap()
        .execute_batch("CREATE TABLE other(x)")
        .unwrap();
    let o = nai(dir.path())
        .args(["patrol", "--oneline", "--db"])
        .arg(&bad)
        .output()
        .unwrap();
    assert_eq!(code(&o), 3, "stdout={} stderr={}", out(&o), err(&o));
    assert!(
        err(&o).contains("[patrol] 内部错误") && err(&o).contains("no such table"),
        "{}",
        err(&o)
    );
}

#[test]
fn patrol_runs_on_a_database_without_the_dropped_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let o = nai(dir.path())
        .args(["patrol", "--json", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    // 空库 + 没有桥接密钥 + 连不上网关：一定是「注意」或「危险」，但绝不能是崩溃（3）
    assert!(
        matches!(code(&o), 1 | 2),
        "exit={} stderr={}",
        code(&o),
        err(&o)
    );
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert!(v["data"].get("waitlist").is_none());
    assert!(
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| !r["text"].as_str().unwrap().contains("候补"))
    );
}

#[test]
fn incident_open_list_update_resolve_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let run = |args: &[&str]| {
        nai(dir.path())
            .arg("incident")
            .arg("--db")
            .arg(&db)
            .args(args)
            .output()
            .unwrap()
    };

    let o = run(&["open", "--title", "出图接口请求超时"]);
    assert_eq!(
        (code(&o), out(&o).as_str()),
        (0, "created 1\n"),
        "{}",
        err(&o)
    );
    let o = run(&["update", "1", "--status", "monitoring"]);
    assert_eq!((code(&o), out(&o).as_str()), (0, "updated 1\n"));
    assert_eq!(out(&run(&["list"])), "#1 [进行中] minor 出图接口请求超时\n");
    let o = run(&["resolve", "1", "--body", "维护完成"]);
    assert_eq!((code(&o), out(&o).as_str()), (0, "resolved 1\n"));
    assert_eq!(
        out(&run(&["list", "--limit", "1"])),
        "#1 [已解决] minor 出图接口请求超时\n"
    );

    let c = rusqlite::Connection::open(&db).unwrap();
    let bodies: Vec<String> = c
        .prepare("SELECT status || ':' || body FROM incident_updates ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        bodies,
        [
            "identified:问题已定位，正在实施修复。",
            "monitoring:修复已上线，我们正在观察结果。",
            "resolved:维护完成"
        ]
    );

    // 参数错误由 clap 拦下（退出码 2），不写库
    assert_eq!(code(&run(&["open", "--title", "x", "--impact", "huge"])), 2);
    assert_eq!(code(&run(&["open"])), 2, "缺 --title");
    assert_eq!(code(&run(&["update", "99"])), 1, "事件不存在");
}

#[test]
fn dm_broadcast_is_dry_run_unless_send_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let c = rw(&db);
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    c.execute("INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES ('900000000000000001', 1, 1.0)", []).unwrap();
    let base = |extra: &[&str]| {
        nai(dir.path())
            .args(["dm-broadcast", "--db"])
            .arg(&db)
            .args(["--text", "hello"])
            .args(extra)
            .output()
            .unwrap()
    };

    let o = base(&[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        out(&o).contains("在册 1 人，已发 0，本次待发 1") && out(&o).contains("演练"),
        "{}",
        out(&o)
    );
    assert!(
        !dir.path().join("data").join("ops").exists(),
        "演练不创建账本"
    );
    // 没有 token 时 --send 直接拒绝（并且在发出任何请求之前）
    let o = base(&["--send"]);
    assert_eq!(code(&o), 2);
    assert!(out(&o).contains("没有 DISCORD_BOT_TOKEN"));
    // --send 与 --dry-run 互斥
    assert_eq!(code(&base(&["--send", "--dry-run"])), 2);
    // 旧命令行里的 --dry-run 仍然可用
    assert_eq!(code(&base(&["--dry-run"])), 0);
}

#[test]
fn announce_reads_stdin_and_dry_run_does_not_need_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = nai(dir.path())
        .args(["announce", "--dry-run"])
        .env("ANNOUNCE_CHANNEL_ID", "900000000000000020")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("  今晚维护  \n".as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        out(&o).starts_with("dry-run：将发到频道 900000000000000020 共 4 字")
            && out(&o).contains("今晚维护"),
        "{}",
        out(&o)
    );
    // 没有频道配置 → 报错退出
    let o = nai(dir.path())
        .args(["announce", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
}

#[test]
fn snapshot_then_sanitize_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    add_key(&rw(&db), 1, "nai-synthetic-key-PLAINTEXT", false, true);
    let gz = dir.path().join("snap.db.gz");
    let o = nai(dir.path())
        .arg("snapshot")
        .arg(&gz)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).contains(", integrity ok, keys=1)"), "{}", out(&o));

    let mut raw = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(std::fs::File::open(&gz).unwrap()),
        &mut raw,
    )
    .unwrap();
    let plain = dir.path().join("plain.db");
    std::fs::write(&plain, raw).unwrap();
    let o = nai(dir.path())
        .arg("ro-sanitize")
        .arg(&plain)
        .output()
        .unwrap();
    assert_eq!(
        (code(&o), out(&o).as_str()),
        (0, "sanitized 1 keys\n"),
        "{}",
        err(&o)
    );
    assert!(
        !std::fs::read(&plain)
            .unwrap()
            .windows(10)
            .any(|w| w == b"PLAINTEXT")
    );

    // 源库不存在：失败，不留下输出
    let o = nai(dir.path())
        .arg("snapshot")
        .arg(dir.path().join("x.db.gz"))
        .arg("--db")
        .arg(dir.path().join("none.db"))
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(!dir.path().join("x.db.gz").exists());
}

#[test]
fn prompt_stats_and_smoke_failures_exit_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let o = nai(dir.path())
        .args(["prompt-stats", "--db"])
        .arg(&db)
        .args(["--out"])
        .arg(dir.path().join("r.html"))
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("最近 30 天没有审计记录"), "{}", err(&o));

    let o = nai(dir.path())
        .args(["smoke", "http://127.0.0.1:1", "--timeout", "2"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(
        out(&o).contains("FAIL") && out(&o).contains("=== 汇总 ==="),
        "{}",
        out(&o)
    );
}
