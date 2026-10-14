//! 状态页事件命令行（移植自 deploy/ops/incident.py）。
//!
//! 旧版把一段小程序用 stdin 交给 nai-gate 容器里的 python 执行（借用 app.status_page）；
//! 现在直接对数据库写 `incidents` / `incident_updates` 两张表，落库的行与旧版逐字段相同
//! （`tools/parity/golden/ops_incident.py` 在旧 Python 上生成夹具，`tests/incident_golden.rs` 对拍）。
//!
//! 写法照 status_page.py 的约定：标题 = 功能 + 现象，正文只写成员能感知的影响，不写内部细节。
//! 不确定写什么时正文留空，自动用 [`STANDARD_BODY`] 的 Statuspage 标准句式。
//!
//! 写库的只有 [`create_incident`] / [`add_update`] 两个函数（走 gate-db 的写线程）；`list` 只读。

use anyhow::{Context, Result, bail};
use gate_core::{pyc, pyjson};
use gate_db::Db;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use std::path::Path;

/// Statuspage 标准句式（中文本地化），与 app/status_page.py 的 `STANDARD_BODY` 一致。
pub const STANDARD_BODY: [(&str, &str); 5] = [
    ("investigating", "我们正在调查此问题。"),
    ("identified", "问题已定位，正在实施修复。"),
    ("update", "我们正在继续处理此问题。"),
    ("monitoring", "修复已上线，我们正在观察结果。"),
    ("resolved", "此事件已解决。"),
];

/// 事件可以标的组件（状态页只展示前两个）。
pub const COMPONENTS: [&str; 4] = ["upstream", "api", "web", "bot"];
pub const IMPACTS: [&str; 4] = ["none", "minor", "major", "critical"];
pub const STATUSES: [&str; 5] = [
    "investigating",
    "identified",
    "update",
    "monitoring",
    "resolved",
];

pub fn standard_body(status: &str) -> Option<&'static str> {
    STANDARD_BODY
        .iter()
        .find(|(s, _)| *s == status)
        .map(|(_, b)| *b)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Open {
        title: String,
        body: Option<String>,
        impact: String,
        components: String,
        status: String,
    },
    Update {
        id: i64,
        status: String,
        body: Option<String>,
    },
    Resolve {
        id: i64,
        body: Option<String>,
    },
    List {
        limit: i64,
    },
}

/// `INSERT INTO incidents ...` + 第一条更新，同一个事务。返回新事件 id。
///
/// 与旧版 `create_incident` 一致：`resolved_at` 只有 status 本身就是 resolved 时才等于 `at`。
pub async fn create_incident(
    db: &Db,
    title: &str,
    impact: &str,
    components: &[String],
    status: &str,
    body: &str,
    at: f64,
) -> gate_db::Result<i64> {
    let (title, impact, status, body) = (
        title.to_string(),
        impact.to_string(),
        status.to_string(),
        body.to_string(),
    );
    // json.dumps(components)：默认分隔符 ", "、ensure_ascii=True
    let components = pyjson::default_str(&Value::from(components.to_vec()));
    db.write(move |tx| {
        let resolved_at: Option<f64> = (status == "resolved").then_some(at);
        tx.execute(
            "INSERT INTO incidents(title, impact, components, started_at, resolved_at) VALUES (?,?,?,?,?)",
            params![title, impact, components, at, resolved_at],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO incident_updates(incident_id, status, body, at) VALUES (?,?,?,?)",
            params![id, status, body, at],
        )?;
        Ok(id)
    })
    .await
}

/// 追加一条更新，并按状态改写 `resolved_at`（resolved → `at`；其它状态 → NULL，即重新打开）。
///
/// 与旧版的唯一差异：事件不存在时什么都不写、返回 `false`（旧版会插入一条挂不到任何事件上的孤儿更新）。
pub async fn add_update(
    db: &Db,
    incident_id: i64,
    status: &str,
    body: &str,
    at: f64,
) -> gate_db::Result<bool> {
    let (status, body) = (status.to_string(), body.to_string());
    db.write(move |tx| {
        let exists = tx
            .query_row("SELECT 1 FROM incidents WHERE id=?", [incident_id], |_| {
                Ok(())
            })
            .is_ok();
        if !exists {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO incident_updates(incident_id, status, body, at) VALUES (?,?,?,?)",
            params![incident_id, status, body, at],
        )?;
        let resolved_at: Option<f64> = (status == "resolved").then_some(at);
        tx.execute(
            "UPDATE incidents SET resolved_at=? WHERE id=?",
            params![resolved_at, incident_id],
        )?;
        Ok(true)
    })
    .await
}

/// `list`：最近的事件，每行 `#id [已解决|进行中] impact title`。只读；表还不存在按没有事件。
pub fn list_incidents(db_path: &Path, limit: i64) -> Result<String> {
    let conn = Connection::open_with_flags(
        crate::util::sqlite_uri(db_path, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打不开数据库 {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let mut stmt = match conn
        .prepare("SELECT id, title, impact, resolved_at FROM incidents ORDER BY id DESC LIMIT ?")
    {
        Ok(s) => s,
        Err(e) if e.to_string().contains("no such table") => return Ok(String::new()),
        Err(e) => return Err(e.into()),
    };
    let rows = stmt.query_map([limit], |r| {
        let resolved: rusqlite::types::Value = r.get(3)?;
        let done = match resolved {
            rusqlite::types::Value::Integer(i) => i != 0,
            rusqlite::types::Value::Real(f) => f != 0.0,
            _ => false,
        };
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            done,
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        let (id, title, impact, done) = row?;
        out.push_str(&format!(
            "#{id} [{}] {impact} {title}\n",
            if done { "已解决" } else { "进行中" }
        ));
    }
    Ok(out)
}

/// 正文：去首尾空白，留空用该状态的标准句式，最长 2000 字（与后台接口一致）。
fn body_or_standard(body: Option<&str>, status: &str) -> Result<String> {
    let text = body
        .map(pyc::strip)
        .filter(|b| !b.is_empty())
        .or_else(|| standard_body(status));
    let text = text.with_context(|| format!("未知状态 {status}"))?;
    Ok(pyc::take_chars(text, 2000).to_string())
}

/// `--components api,upstream`：逗号分隔，必须是已知组件；留空 = `["api"]`。
pub fn parse_components(raw: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for c in raw.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        if !COMPONENTS.contains(&c) {
            bail!("未知组件 {c:?}（可选：{}）", COMPONENTS.join(" / "));
        }
        out.push(c.to_string());
    }
    if out.is_empty() {
        out.push("api".into());
    }
    Ok(out)
}

fn open_db(db_path: &Path, now: f64) -> Result<Db> {
    Db::open(db_path, now, gate_core::reasons::BACKFILL)
        .with_context(|| format!("打开数据库 {} 失败", db_path.display()))
}

/// 执行一个动作，返回要打印的文本。`now` 是 Unix 秒（测试里固定）。
pub async fn execute(db_path: &Path, action: &Action, now: f64) -> Result<String> {
    match action {
        Action::List { limit } => list_incidents(db_path, *limit),
        Action::Open {
            title,
            body,
            impact,
            components,
            status,
        } => {
            let title = pyc::take_chars(pyc::strip(title), 200);
            if title.is_empty() {
                bail!("需要 --title");
            }
            if !IMPACTS.contains(&impact.as_str()) {
                bail!("impact 必须是 {}", IMPACTS.join(" / "));
            }
            if !STATUSES.contains(&status.as_str()) {
                bail!("status 必须是 {}", STATUSES.join(" / "));
            }
            let comps = parse_components(components)?;
            let text = body_or_standard(body.as_deref(), status)?;
            let db = open_db(db_path, now)?;
            let id = create_incident(&db, title, impact, &comps, status, &text, now).await?;
            Ok(format!("created {id}\n"))
        }
        Action::Update { id, status, body } => {
            if !STATUSES.contains(&status.as_str()) {
                bail!("status 必须是 {}", STATUSES.join(" / "));
            }
            let text = body_or_standard(body.as_deref(), status)?;
            let db = open_db(db_path, now)?;
            if !add_update(&db, *id, status, &text, now).await? {
                bail!("事件 #{id} 不存在");
            }
            Ok(format!("updated {id}\n"))
        }
        Action::Resolve { id, body } => {
            let text = body_or_standard(body.as_deref(), "resolved")?;
            let db = open_db(db_path, now)?;
            // status=resolved 时 add_update 会顺手写 resolved_at，不用另外调函数
            if !add_update(&db, *id, "resolved", &text, now).await? {
                bail!("事件 #{id} 不存在");
            }
            Ok(format!("resolved {id}\n"))
        }
    }
}
