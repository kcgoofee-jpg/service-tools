//! 后台操作记录。

use crate::{Db, Result};
use rusqlite::params;

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

    pub async fn purge_admin_actions(&self, older_than: f64) -> Result<i64> {
        self.write(move |tx| {
            let n = tx.execute("DELETE FROM admin_actions WHERE ts<?", [older_than])?;
            Ok(n as i64)
        })
        .await
    }
}
