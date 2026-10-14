//! 状态页事件命令行：与旧版逐行一致的部分见 incident_golden.rs；这里测 Rust 版新增的校验与默认值，
//! 以及词表常量与旧版对拍。

mod common;

use common::*;
use gate_golden::load;
use gate_ops::incident::{self, Action, COMPONENTS, IMPACTS, STANDARD_BODY, STATUSES, execute};
use rusqlite::Connection;
use std::path::Path;

const T: f64 = 1_790_000_000.5;
const M: &str = env!("CARGO_MANIFEST_DIR");

fn strs(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn constants_match_python() {
    let c = &load(M, "ops_incident_constants")[0];
    let body = c.get("standard_body").unwrap().as_object().unwrap();
    assert_eq!(body.len(), STANDARD_BODY.len());
    for (status, sentence) in STANDARD_BODY {
        assert_eq!(body[status].as_str().unwrap(), sentence, "{status}");
    }
    assert_eq!(strs(c.get("components").unwrap()), COMPONENTS);
    assert_eq!(strs(c.get("impacts").unwrap()), IMPACTS);
    assert_eq!(strs(c.get("statuses").unwrap()), STATUSES);
}

fn open(title: &str, body: Option<&str>, status: &str, components: &str) -> Action {
    Action::Open {
        title: title.into(),
        body: body.map(String::from),
        impact: "minor".into(),
        components: components.into(),
        status: status.into(),
    }
}

fn rows(path: &Path, sql: &str) -> Vec<(i64, String, String)> {
    let c = Connection::open(path).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn blank_body_uses_the_standard_sentence_for_the_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    execute(
        &db,
        &open("出图接口请求超时", None, "investigating", "api"),
        T,
    )
    .await
    .unwrap();
    execute(
        &db,
        &Action::Update {
            id: 1,
            status: "monitoring".into(),
            body: Some("   \n ".into()),
        },
        T + 1.0,
    )
    .await
    .unwrap();
    execute(
        &db,
        &Action::Update {
            id: 1,
            status: "update".into(),
            body: None,
        },
        T + 2.0,
    )
    .await
    .unwrap();
    execute(&db, &Action::Resolve { id: 1, body: None }, T + 3.0)
        .await
        .unwrap();
    let got = rows(
        &db,
        "SELECT incident_id, status, body FROM incident_updates ORDER BY id",
    );
    let want: Vec<_> = [
        ("investigating", "我们正在调查此问题。"),
        ("monitoring", "修复已上线，我们正在观察结果。"),
        ("update", "我们正在继续处理此问题。"),
        ("resolved", "此事件已解决。"),
    ]
    .iter()
    .map(|(s, b)| (1, s.to_string(), b.to_string()))
    .collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn title_and_body_are_trimmed_and_capped_like_the_admin_api() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let long_title = format!("  {}  ", "标".repeat(250));
    let long_body = "文".repeat(2500);
    execute(
        &db,
        &open(
            &long_title,
            Some(&format!("  {long_body}  ")),
            "identified",
            "api",
        ),
        T,
    )
    .await
    .unwrap();
    let c = Connection::open(&db).unwrap();
    let title: String = c
        .query_row("SELECT title FROM incidents WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let body: String = c
        .query_row("SELECT body FROM incident_updates WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!((title.chars().count(), body.chars().count()), (200, 2000));
    assert!(title.starts_with('标') && body.starts_with('文'));
}

#[tokio::test]
async fn components_are_validated_defaulted_and_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    for (input, stored) in [
        ("api", r#"["api"]"#),
        (" upstream , api ", r#"["upstream", "api"]"#),
        ("", r#"["api"]"#),
        (",,", r#"["api"]"#),
    ] {
        let out = execute(&db, &open("t", None, "identified", input), T)
            .await
            .unwrap();
        assert!(out.starts_with("created "));
        let id: i64 = out.trim().trim_start_matches("created ").parse().unwrap();
        let comps: String = Connection::open(&db)
            .unwrap()
            .query_row("SELECT components FROM incidents WHERE id=?", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(comps, stored, "{input:?}");
    }
    let err = execute(&db, &open("t", None, "identified", "api,nope"), T)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("未知组件 \"nope\""), "{err:#}");
    assert_eq!(
        rows(&db, "SELECT id, title, impact FROM incidents").len(),
        4,
        "校验失败不写任何行"
    );
}

#[tokio::test]
async fn invalid_inputs_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let bad_impact = Action::Open {
        title: "t".into(),
        body: None,
        impact: "huge".into(),
        components: "api".into(),
        status: "identified".into(),
    };
    assert!(execute(&db, &bad_impact, T).await.is_err());
    assert!(
        execute(&db, &open("   ", None, "identified", "api"), T)
            .await
            .is_err(),
        "标题不能为空"
    );
    assert!(
        execute(&db, &open("t", None, "bogus", "api"), T)
            .await
            .is_err()
    );
    assert!(
        execute(
            &db,
            &Action::Update {
                id: 1,
                status: "bogus".into(),
                body: None
            },
            T
        )
        .await
        .is_err()
    );
    assert!(rows(&db, "SELECT id, title, impact FROM incidents").is_empty());
}

/// 旧版会往不存在的事件上塞一条孤儿更新（update 页面永远不会显示它）；现在直接报错、什么都不写。
#[tokio::test]
async fn updating_a_missing_incident_is_an_error_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    execute(&db, &open("t", Some("b"), "identified", "api"), T)
        .await
        .unwrap();
    for action in [
        Action::Update {
            id: 99,
            status: "monitoring".into(),
            body: Some("x".into()),
        },
        Action::Resolve {
            id: 99,
            body: Some("x".into()),
        },
    ] {
        let err = execute(&db, &action, T + 1.0).await.unwrap_err();
        assert_eq!(err.to_string(), "事件 #99 不存在");
    }
    assert_eq!(
        rows(
            &db,
            "SELECT incident_id, status, body FROM incident_updates"
        )
        .len(),
        1
    );
}

#[tokio::test]
async fn list_survives_a_database_without_the_tables() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("bare.db");
    Connection::open(&bare)
        .unwrap()
        .execute_batch("CREATE TABLE other(x)")
        .unwrap();
    assert_eq!(
        execute(&bare, &Action::List { limit: 5 }, T).await.unwrap(),
        "",
        "没有 incidents 表 = 没有事件"
    );

    let db = new_db(dir.path());
    execute(&db, &open("甲", Some("b"), "identified", "api"), T)
        .await
        .unwrap();
    execute(&db, &open("乙", Some("b"), "resolved", "api"), T + 1.0)
        .await
        .unwrap();
    let out = execute(&db, &Action::List { limit: 5 }, T + 2.0)
        .await
        .unwrap();
    assert_eq!(out, "#2 [已解决] minor 乙\n#1 [进行中] minor 甲\n");
    assert_eq!(incident::standard_body("resolved"), Some("此事件已解决。"));
    assert_eq!(incident::standard_body("nope"), None);
}
