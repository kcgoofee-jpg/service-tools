//! 状态页事件命令行对拍：夹具由 tools/parity/golden/ops_incident.py 在旧 Python 上生成
//! （原样执行 deploy/ops/incident.py 里跑在容器内的那段程序），这里在 Rust 上重放同样的步骤，
//! 比较每一步的 stdout 与最终 `incidents` / `incident_updates` 两张表的整表行（含字段类型与顺序）。

use gate_golden::{assert_eq_py_ordered, load};
use gate_ops::incident::{Action, execute};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::Path;

const M: &str = env!("CARGO_MANIFEST_DIR");

fn s(v: &Value, k: &str) -> String {
    v[k].as_str()
        .unwrap_or_else(|| panic!("夹具缺少字符串字段 {k}"))
        .to_string()
}

fn action_of(name: &str, p: &Value) -> Action {
    match name {
        "open" => Action::Open {
            title: s(p, "title"),
            body: Some(s(p, "body")),
            impact: s(p, "impact"),
            components: s(p, "components"),
            status: s(p, "status"),
        },
        "update" => Action::Update {
            id: p["id"].as_i64().unwrap(),
            status: s(p, "status"),
            body: Some(s(p, "body")),
        },
        "resolve" => Action::Resolve {
            id: p["id"].as_i64().unwrap(),
            body: Some(s(p, "body")),
        },
        "list" => Action::List {
            limit: p["limit"].as_i64().unwrap(),
        },
        other => panic!("夹具里有未知动作 {other}"),
    }
}

fn dump(path: &Path, sql: &str) -> Value {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt
        .query_map([], gate_db::value::row_json)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    Value::Array(rows)
}

#[tokio::test]
async fn incident_rows_match_python() {
    let cases = load(M, "ops_incident");
    assert!(cases.len() >= 8, "夹具场景太少：{}", cases.len());
    for c in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nai_gate.db");
        // 线上这两张表由网关启动时建好；这里同样先让 gate-db 建库
        drop(gate_db::Db::open(&path, 1.79e9, gate_core::reasons::BACKFILL).unwrap());

        let steps = c.get("steps").unwrap().as_array().unwrap();
        let mut got_steps = Vec::new();
        for step in steps {
            let action = action_of(step["action"].as_str().unwrap(), &step["payload"]);
            let now = step["now"].as_f64().unwrap();
            let out = execute(&path, &action, now)
                .await
                .unwrap_or_else(|e| panic!("夹具第 {} 行 {step}：{e:#}", c.line));
            got_steps.push(json!({"action": step["action"], "stdout": out}));
        }
        let want_steps: Vec<Value> = steps
            .iter()
            .map(|st| json!({"action": st["action"], "stdout": st["stdout"]}))
            .collect();
        assert_eq_py_ordered(&c, &Value::Array(got_steps), &Value::Array(want_steps));

        let incidents = dump(&path, "SELECT * FROM incidents ORDER BY id");
        assert_eq_py_ordered(&c, &incidents, c.get("incidents").unwrap());
        let updates = dump(&path, "SELECT * FROM incident_updates ORDER BY id");
        assert_eq_py_ordered(&c, &updates, c.get("updates").unwrap());
    }
}
