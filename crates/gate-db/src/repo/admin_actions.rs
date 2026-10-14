//! 后台操作记录。

use crate::{Db, Result, value};
use rusqlite::params;
use serde_json::Value;

impl Db {
    pub async fn add_admin_action(
        &self,
        actor: &str,
        action: &str,
        target: &str,
        detail: &str,
        ok: bool,
        now: f64,
    ) -> Result<()> {
        let (actor, action, target, detail) = (
            actor.to_string(),
            action.to_string(),
            target.to_string(),
            detail.to_string(),
        );
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO admin_actions (ts, actor, action, target, detail, ok) VALUES (?,?,?,?,?,?)",
                params![now, actor, action, target, detail, i64::from(ok)],
            )?;
            Ok(())
        })
        .await
    }

    /// 最新的在前；`limit` 夹在 1..=200，`offset` 不小于 0。
    pub async fn list_admin_actions(&self, limit: i64, offset: i64) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, 200);
        let offset = offset.max(0);
        self.read(move |c| {
            let mut stmt =
                c.prepare("SELECT * FROM admin_actions ORDER BY id DESC LIMIT ? OFFSET ?")?;
            stmt.query_map(params![limit, offset], value::row_json)?
                .collect()
        })
        .await
    }

    pub async fn count_admin_actions(&self) -> Result<i64> {
        self.read(|c| {
            c.query_row("SELECT COUNT(*) FROM admin_actions", [], |r| {
                value::i64_or(r, 0, 0)
            })
        })
        .await
    }

    pub async fn purge_admin_actions(&self, older_than: f64) -> Result<i64> {
        self.write(move |tx| {
            let n = tx.execute("DELETE FROM admin_actions WHERE ts<?", [older_than])?;
            Ok(n as i64)
        })
        .await
    }
}
