//! 公告与逐条私信：全部打到本机假 Discord（`common::discord_mock`），不会真的发任何消息。
//! 重点：默认只演练、账本防重复发送（含「文案变了拒绝」「账本损坏拒绝」）、断点续发、原位编辑、429 听 Discord 的。

mod common;

use common::*;
use gate_ops::announce;
use gate_ops::discord::Discord;
use gate_ops::dm_broadcast::{self, Opts, Store};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

const IDS: [&str; 3] = [
    "900000000000000001",
    "900000000000000002",
    "900000000000000003",
];
const TOKEN: &str = "synthetic-bot-token";

fn recipients() -> Vec<String> {
    IDS.iter().map(|s| s.to_string()).collect()
}

fn client(base: &str) -> Discord {
    Discord::new(base, TOKEN, Duration::from_secs(5)).unwrap()
}

fn opts(slug: &str, text: &str, send: bool) -> Opts {
    Opts {
        slug: slug.into(),
        text: text.into(),
        edit_text: None,
        rate: 0.0,
        send,
    }
}

async fn run_dm(d: Option<&Discord>, record: &Path, o: &Opts) -> (i32, String) {
    let mut store = Store::load(record).unwrap();
    let mut out = Vec::new();
    let code = dm_broadcast::run(d, &mut store, &recipients(), o, &mut out)
        .await
        .unwrap();
    (code, String::from_utf8(out).unwrap())
}

fn record_file(dir: &Path) -> std::path::PathBuf {
    dm_broadcast::record_path(dir)
}

fn read_record(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(record_file(dir)).unwrap()).unwrap()
}

#[test]
fn record_lives_under_data_dir_ops() {
    assert_eq!(
        dm_broadcast::record_path(Path::new("/srv/data")),
        Path::new("/srv/data/ops/dm_broadcast_sent.json")
    );
}

#[tokio::test]
async fn dry_run_is_the_default_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let d = client(&mock.base);
    // 即便手里有可用的客户端，send=false 也不能发
    let (code, out) = run_dm(
        Some(&d),
        &record_file(dir.path()),
        &opts("notice", "今晚重置 Key", false),
    )
    .await;
    assert_eq!(code, 0);
    assert!(
        out.contains("在册 3 人，已发 0，本次待发 3，限速 0.0s/人"),
        "{out}"
    );
    assert!(out.contains("演练"), "{out}");
    assert!(log.lock().unwrap().is_empty());
    assert!(!record_file(dir.path()).exists(), "演练不写账本");
    // 没有客户端（没配 token）也能演练
    let (code, _) = run_dm(
        None,
        &record_file(dir.path()),
        &opts("notice", "今晚重置 Key", false),
    )
    .await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn send_records_every_message_and_rerun_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record_file(dir.path());
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let d = client(&mock.base);

    let (code, out) = run_dm(
        Some(&d),
        &rec,
        &opts("reset", "今晚重置 Key，请到首页领新的。", true),
    )
    .await;
    assert_eq!(code, 0);
    assert!(out.contains("在册 3 人，已发 0，本次待发 3"), "{out}");
    assert!(out.contains("完成：成功 3，失败 0"), "{out}");
    {
        let reqs = log.lock().unwrap();
        assert_eq!(reqs.len(), 6);
        for (i, id) in IDS.iter().enumerate() {
            let (open, post) = (&reqs[i * 2], &reqs[i * 2 + 1]);
            assert_eq!(
                (open.path.as_str(), &open.body),
                ("/users/@me/channels", &json!({"recipient_id": id}))
            );
            assert_eq!(post.path, format!("/channels/dm-{id}/messages"));
            assert_eq!(
                post.body,
                json!({"content": "今晚重置 Key，请到首页领新的。", "allowed_mentions": {"parse": []}})
            );
            assert_eq!(post.auth, format!("Bot {TOKEN}"));
        }
    }
    let r = read_record(dir.path());
    let c = &r["campaigns"]["reset"];
    assert_eq!(c["text_hash"].as_str().unwrap().len(), 12);
    assert!(c["updated_at"].as_f64().unwrap() > 1.7e9);
    assert_eq!(c["sent"].as_object().unwrap().len(), 3);
    assert_eq!(c["sent"][IDS[0]], json!("dm-900000000000000001:msg-1"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&rec).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(
        !rec.with_extension("json.tmp").exists(),
        "原子写入不留临时文件"
    );

    // 再跑一遍 = 0 发送（2026-10-13 同一晚发了 3 遍的教训）
    let (code, out) = run_dm(
        Some(&d),
        &rec,
        &opts("reset", "今晚重置 Key，请到首页领新的。", true),
    )
    .await;
    assert_eq!(code, 0);
    assert!(out.contains("已发 3，本次待发 0"), "{out}");
    assert_eq!(log.lock().unwrap().len(), 6, "不能重发");
}

#[tokio::test]
async fn failures_are_not_recorded_and_are_retried_on_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record_file(dir.path());
    let (mock, log) = discord_mock(DiscordScript {
        dm_forbidden: vec![IDS[1].to_string()],
        ..Default::default()
    })
    .await;
    let (code, out) = run_dm(Some(&client(&mock.base)), &rec, &opts("n", "hello", true)).await;
    assert_eq!(code, 0);
    assert!(out.contains("完成：成功 2，失败 1"), "{out}");
    assert_eq!(
        read_record(dir.path())["campaigns"]["n"]["sent"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        log.lock().unwrap().len(),
        3 + 2,
        "失败的那个只开了私信频道，没有发消息，也没有重试"
    );

    // 对方现在能收了：只补发失败的那一个
    let (mock2, log2) = discord_mock(DiscordScript::default()).await;
    let (_, out) = run_dm(Some(&client(&mock2.base)), &rec, &opts("n", "hello", true)).await;
    assert!(
        out.contains("已发 2，本次待发 1") && out.contains("完成：成功 1，失败 0"),
        "{out}"
    );
    let reqs = log2.lock().unwrap();
    assert_eq!(reqs[0].body, json!({"recipient_id": IDS[1]}));
    assert_eq!(
        read_record(dir.path())["campaigns"]["n"]["sent"]
            .as_object()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn changed_text_is_refused_but_a_new_slug_is_independent() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record_file(dir.path());
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let d = client(&mock.base);
    run_dm(Some(&d), &rec, &opts("a", "first text", true)).await;
    let before = log.lock().unwrap().len();

    let (code, out) = run_dm(Some(&d), &rec, &opts("a", "second text", true)).await;
    assert_eq!(code, 2);
    assert!(out.starts_with("⚠ 文案与账本「a」不同（旧 "), "{out}");
    assert_eq!(log.lock().unwrap().len(), before);

    let (code, out) = run_dm(Some(&d), &rec, &opts("b", "second text", true)).await;
    assert_eq!(code, 0);
    assert!(out.contains("已发 0，本次待发 3"), "{out}");
    let r = read_record(dir.path());
    assert_eq!(r["campaigns"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn missing_text_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out) = run_dm(None, &record_file(dir.path()), &opts("a", "", false)).await;
    assert_eq!(
        (code, out.as_str()),
        (2, "没有私信内容：用 --text / --file\n")
    );
}

#[tokio::test]
async fn edit_patches_delivered_messages_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record_file(dir.path());
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let d = client(&mock.base);

    // 账本里还没有记录：没东西可编辑
    let o = Opts {
        edit_text: Some("v2".into()),
        ..opts("reset", "", true)
    };
    let (code, out) = run_dm(Some(&d), &rec, &o).await;
    assert_eq!(
        (code, out.as_str()),
        (2, "账本里没有已发送记录，没东西可编辑\n")
    );

    run_dm(Some(&d), &rec, &opts("reset", "v1", true)).await;
    let sent_before = read_record(dir.path());
    log.lock().unwrap().clear();

    // 编辑同样默认只演练
    let (code, out) = run_dm(
        Some(&d),
        &rec,
        &Opts {
            edit_text: Some("v2".into()),
            ..opts("reset", "", false)
        },
    )
    .await;
    assert_eq!(code, 0);
    assert!(
        out.contains("原位编辑 3 条已送达私信") && out.contains("演练"),
        "{out}"
    );
    assert!(log.lock().unwrap().is_empty());

    let (code, out) = run_dm(Some(&d), &rec, &o).await;
    assert_eq!(code, 0);
    assert!(out.contains("完成：成功 3，失败 0"), "{out}");
    let reqs = log.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    assert!(reqs.iter().all(|r| r.method == "PATCH"
        && r.body == json!({"content": "v2", "allowed_mentions": {"parse": []}})));
    let mut paths: Vec<_> = reqs.iter().map(|r| r.path.clone()).collect();
    paths.sort();
    assert_eq!(paths[0], "/channels/dm-900000000000000001/messages/msg-1");
    assert_eq!(read_record(dir.path()), sent_before, "编辑不改账本");
}

#[tokio::test]
async fn rate_limit_is_obeyed_once_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, log) = discord_mock(DiscordScript {
        rate_limit_first_message: true,
        ..Default::default()
    })
    .await;
    let (code, out) = run_dm(
        Some(&client(&mock.base)),
        &record_file(dir.path()),
        &opts("n", "hello", true),
    )
    .await;
    assert_eq!(code, 0);
    assert!(out.contains("Discord 限速，睡 0s"), "{out}");
    assert!(out.contains("完成：成功 3，失败 0"), "{out}");
    // 第一个收件人：开频道 + 发（429）+ 重发 = 3 次；其余两人各 2 次
    assert_eq!(log.lock().unwrap().len(), 3 + 2 + 2);
}

#[tokio::test]
async fn corrupt_record_refuses_instead_of_resending_to_everyone() {
    let dir = tempfile::tempdir().unwrap();
    let rec = record_file(dir.path());
    std::fs::create_dir_all(rec.parent().unwrap()).unwrap();
    std::fs::write(&rec, "{ this is not json").unwrap();
    let err = match Store::load(&rec) {
        Ok(_) => panic!("损坏的账本必须报错"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("损坏"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&rec).unwrap(),
        "{ this is not json",
        "不能覆盖损坏的账本"
    );
}

#[test]
fn recipients_are_enabled_registered_members_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let c = rw(&db);
    add_key(&c, 1, "nai-synthetic-key-1", false, true);
    add_key(&c, 2, "nai-synthetic-key-2", false, false); // 已停用：不发
    add_key(&c, 3, "nai-synthetic-key-3", false, true);
    add_key(&c, 4, "nai-synthetic-key-4", false, true); // 没有注册记录：不发
    for (did, kid) in [(IDS[2], 3), (IDS[1], 2), (IDS[0], 1)] {
        c.execute(
            "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,1.0)",
            rusqlite::params![did, kid],
        )
        .unwrap();
    }
    assert_eq!(
        dm_broadcast::load_recipients(&db).unwrap(),
        vec![IDS[0].to_string(), IDS[2].to_string()]
    );
}

#[test]
fn text_flag_wins_over_file_and_files_are_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("notice.txt");
    std::fs::write(&f, "\n  from file  \n").unwrap();
    assert_eq!(
        dm_broadcast::pick_text("  inline ", Some(&f)).unwrap(),
        "inline"
    );
    assert_eq!(dm_broadcast::pick_text("", Some(&f)).unwrap(), "from file");
    assert_eq!(dm_broadcast::pick_text("", None).unwrap(), "");
    assert!(dm_broadcast::pick_text("", Some(&dir.path().join("nope.txt"))).is_err());
}

// ------------------------------------------------------------ 公告

#[tokio::test]
async fn announce_posts_with_no_mentions_allowed() {
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let (code, line) = announce::run(&announce::Args {
        api_base: &mock.base,
        token: TOKEN,
        channel: "900000000000000020",
        text: "\n  @everyone 今晚 23:00 例行维护  \n",
        dry_run: false,
    })
    .await
    .unwrap();
    assert_eq!((code, line.as_str()), (0, "posted 200 msg-1"));
    let reqs = log.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/channels/900000000000000020/messages");
    assert_eq!(reqs[0].auth, format!("Bot {TOKEN}"));
    assert!(reqs[0].user_agent.starts_with("DiscordBot ("));
    // 与网关共用的 UA 写法（gate_core::discord_ua）；站点地址只从配置读，代码里不写死域名
    assert_eq!(reqs[0].user_agent, gate_ops::discord::user_agent_from_env());
    assert_eq!(
        reqs[0].body,
        json!({"content": "@everyone 今晚 23:00 例行维护", "allowed_mentions": {"parse": []}})
    );
}

#[tokio::test]
async fn announce_truncates_to_1900_chars_and_reports_failures() {
    let (mock, log) = discord_mock(DiscordScript {
        message_status: 500,
        ..Default::default()
    })
    .await;
    let (code, line) = announce::run(&announce::Args {
        api_base: &mock.base,
        token: TOKEN,
        channel: "900000000000000020",
        text: &"猫".repeat(2500),
        dry_run: false,
    })
    .await
    .unwrap();
    assert_eq!(code, 1, "HTTP 非 2xx 要让脚本看得见");
    assert_eq!(line, "posted 500 {\"message\":\"scripted failure\"}");
    assert_eq!(
        log.lock().unwrap()[0].body["content"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1900
    );
}

#[tokio::test]
async fn announce_dry_run_and_input_checks_send_nothing() {
    let (mock, log) = discord_mock(DiscordScript::default()).await;
    let base = mock.base.clone();
    let args = |text, channel, token, dry_run| announce::Args {
        api_base: &base,
        token,
        channel,
        text,
        dry_run,
    };
    let (code, line) = announce::run(&args("hello", "900000000000000020", "", true))
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(
        line.starts_with("dry-run：将发到频道 900000000000000020 共 5 字")
            && line.ends_with("hello"),
        "{line}"
    );
    assert!(
        announce::run(&args("  \n ", "900000000000000020", TOKEN, false))
            .await
            .is_err()
    );
    assert!(
        announce::run(&args("hello", "", TOKEN, false))
            .await
            .is_err()
    );
    assert!(
        announce::run(&args("hello", "900000000000000020", "", false))
            .await
            .is_err()
    );
    assert!(log.lock().unwrap().is_empty());
}
