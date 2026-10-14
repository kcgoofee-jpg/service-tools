//! 账号保护（guard）需要的查询：启动时回填最近 3 小时的出图时间、每日加窗前看「是否顶到过上限」。

use crate::{Db, Result, value};

impl Db {
    /// `seed_hour`：最近 3 小时成功出图的 (ts, images)，按时间排序（旧版 guard.seed_hour）。
    pub async fn recent_image_stamps(&self, since: f64) -> Result<Vec<(f64, i64)>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT ts, images FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%' AND images>0 ORDER BY ts",
            )?;
            let rows = stmt.query_map([since], |r| Ok((value::f64_or(r, 0, 0.0)?, value::i64_or(r, 1, 0)?)))?;
            rows.collect()
        })
        .await
    }

    /// `adapt_daily`：过去 24 小时非测试、非管理员 Key 的日志里有没有该原因码（`hourly_cap`）。
    pub async fn reason_hit_since(&self, since: f64, reason: &str) -> Result<bool> {
        let reason = reason.to_string();
        self.read(move |c| {
            c.query_row(
                "SELECT 1 FROM usage_log u LEFT JOIN api_keys k ON k.id=u.key_id WHERE u.ts>? \
                 AND COALESCE(k.is_test,0)=0 AND COALESCE(k.is_admin,0)=0 AND u.reason=? LIMIT 1",
                rusqlite::params![since, reason],
                |_| Ok(()),
            )
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(other),
            })
        })
        .await
    }

    /// `SELECT MIN(ts) FROM usage_log`
    pub async fn first_log_ts(&self) -> Result<Option<f64>> {
        self.read(|c| {
            c.query_row("SELECT MIN(ts) FROM usage_log", [], |r| {
                value::f64_opt(r, 0)
            })
        })
        .await
    }
}
