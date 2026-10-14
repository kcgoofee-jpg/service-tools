//! 上游 Token 的每日计数与逐 Token 设置（V5 日上限 / 启停 / 出图并发）。

use super::PyMap;
use crate::{Db, Result, value};
use rusqlite::params;
use serde::Serialize;

/// `get_upstream_counter` 的结果。`units` 是按算力折算的张数（节约模式 14 步的图算 0.5 张）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpstreamCounter {
    pub images: i64,
    pub v5: i64,
    pub units: f64,
}

impl Db {
    pub async fn get_upstream_counter(&self, token_id: &str, day: &str) -> Result<UpstreamCounter> {
        let (token_id, day) = (token_id.to_string(), day.to_string());
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT images, v5, units FROM upstream_token_counters WHERE token_id=? AND day=?",
            )?;
            let mut rows = stmt.query(params![token_id, day])?;
            let Some(row) = rows.next()? else {
                return Ok(UpstreamCounter {
                    images: 0,
                    v5: 0,
                    units: 0.0,
                });
            };
            let images = value::i64_or(row, "images", 0)?;
            // 老数据 units 为 NULL：按 1 张 = 1 份
            let units = value::f64_opt(row, "units")?.unwrap_or(images as f64);
            Ok(UpstreamCounter {
                images,
                v5: value::i64_or(row, "v5", 0)?,
                units,
            })
        })
        .await
    }

    /// 把旧的按位置编号的计数（`token-<序号>-<后缀>`）合并到稳定的哈希标识上。
    /// 与旧版一致：合并时只累加 images / v5，不带 units（新行 units 为 NULL，读取时按 images 回填）。
    pub async fn migrate_upstream_token_ids(&self, token_ids: &[String]) -> Result<()> {
        // 旧版 `set(token_ids)`：去重（Python set 的遍历顺序不确定，这里保持首次出现顺序）
        let mut seen: Vec<String> = Vec::new();
        for t in token_ids {
            if !seen.contains(t) {
                seen.push(t.clone());
            }
        }
        self.write(move |tx| {
            for token_id in &seen {
                let suffix = token_id.strip_prefix("token-").unwrap_or(token_id);
                let pattern = format!("token-%-{suffix}");
                let rows: Vec<(String, String, i64, i64)> = {
                    let mut stmt = tx.prepare(
                        "SELECT token_id, day, images, v5 FROM upstream_token_counters WHERE token_id LIKE ?",
                    )?;
                    stmt.query_map([&pattern], |r| {
                        Ok((
                            value::text_or(r, 0, "")?,
                            value::text_or(r, 1, "")?,
                            value::i64_or(r, 2, 0)?,
                            value::i64_or(r, 3, 0)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (old_id, day, images, v5) in rows {
                    tx.execute(
                        "INSERT INTO upstream_token_counters(token_id, day, images, v5) \
                         VALUES(?,?,?,?) ON CONFLICT(token_id,day) DO UPDATE SET \
                         images=images+excluded.images, v5=v5+excluded.v5",
                        params![token_id, day, images, v5],
                    )?;
                    tx.execute(
                        "DELETE FROM upstream_token_counters WHERE token_id=? AND day=?",
                        params![old_id, day],
                    )?;
                }
            }
            Ok(())
        })
        .await
    }

    /// 令牌被替换（同一账号重置了 Token）：把设置和计数从旧标识迁移到新标识。
    pub async fn move_upstream_token(&self, old_id: &str, new_id: &str) -> Result<()> {
        if old_id == new_id {
            return Ok(());
        }
        let (old_id, new_id) = (old_id.to_string(), new_id.to_string());
        self.write(move |tx| {
            for table in [
                "upstream_token_settings",
                "upstream_token_enabled",
                "upstream_token_image_concurrency",
                "upstream_token_counters",
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE token_id=?"), [&new_id])?;
                tx.execute(
                    &format!("UPDATE {table} SET token_id=? WHERE token_id=?"),
                    params![new_id, old_id],
                )?;
            }
            Ok(())
        })
        .await
    }

    pub async fn get_upstream_token_limits(&self) -> Result<PyMap<String, i64>> {
        self.read(|c| {
            let mut stmt =
                c.prepare("SELECT token_id, v5_daily_limit FROM upstream_token_settings")?;
            stmt.query_map([], |r| {
                Ok((value::text_or(r, 0, "")?, value::i64_or(r, 1, 0)?))
            })?
            .collect()
        })
        .await
    }

    pub async fn set_upstream_token_limit(&self, token_id: &str, limit: i64) -> Result<()> {
        let token_id = token_id.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_token_settings(token_id, v5_daily_limit) VALUES(?,?) \
                 ON CONFLICT(token_id) DO UPDATE SET v5_daily_limit=excluded.v5_daily_limit",
                params![token_id, limit],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_upstream_token_enabled(&self) -> Result<PyMap<String, bool>> {
        self.read(|c| {
            let mut stmt = c.prepare("SELECT token_id, enabled FROM upstream_token_enabled")?;
            stmt.query_map([], |r| Ok((value::text_or(r, 0, "")?, value::flag(r, 1)?)))?
                .collect()
        })
        .await
    }

    pub async fn set_upstream_token_enabled(&self, token_id: &str, enabled: bool) -> Result<()> {
        let token_id = token_id.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_token_enabled(token_id, enabled) VALUES(?,?) \
                 ON CONFLICT(token_id) DO UPDATE SET enabled=excluded.enabled",
                params![token_id, i64::from(enabled)],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_upstream_token_image_concurrency(&self) -> Result<PyMap<String, i64>> {
        self.read(|c| {
            let mut stmt =
                c.prepare("SELECT token_id, concurrency FROM upstream_token_image_concurrency")?;
            stmt.query_map([], |r| {
                Ok((value::text_or(r, 0, "")?, value::i64_or(r, 1, 0)?))
            })?
            .collect()
        })
        .await
    }

    pub async fn set_upstream_token_image_concurrency(
        &self,
        token_id: &str,
        limit: i64,
    ) -> Result<()> {
        let token_id = token_id.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_token_image_concurrency(token_id, concurrency) VALUES(?,?) \
                 ON CONFLICT(token_id) DO UPDATE SET concurrency=excluded.concurrency",
                params![token_id, limit],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_upstream_v5_counter(&self, token_id: &str, day: &str) -> Result<i64> {
        Ok(self.get_upstream_counter(token_id, day).await?.v5)
    }

    /// 上游 Token 当日出图张数累加；`weight` 是算力折算（节约模式 0.5），负数 / NaN 按 0 算。
    /// `images < 1` 什么都不做。
    pub async fn bump_upstream_image_counter(
        &self,
        token_id: &str,
        day: &str,
        images: i64,
        weight: f64,
    ) -> Result<()> {
        if images < 1 {
            return Ok(());
        }
        // Python: max(0.0, float(weight))——NaN 和 -0.0 都得 0.0
        let w = if weight > 0.0 { weight } else { 0.0 };
        let units = images as f64 * w;
        let (token_id, day) = (token_id.to_string(), day.to_string());
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_token_counters(token_id, day, images, units) VALUES (?,?,?,?) \
                 ON CONFLICT(token_id, day) DO UPDATE SET images=images+excluded.images, \
                 units=COALESCE(units, images)+excluded.units",
                params![token_id, day, images, units],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn bump_upstream_v5_counter(&self, token_id: &str, day: &str) -> Result<()> {
        let (token_id, day) = (token_id.to_string(), day.to_string());
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_token_counters(token_id, day, v5) VALUES (?,?,1) \
                 ON CONFLICT(token_id, day) DO UPDATE SET v5=v5+1",
                params![token_id, day],
            )?;
            Ok(())
        })
        .await
    }
}
