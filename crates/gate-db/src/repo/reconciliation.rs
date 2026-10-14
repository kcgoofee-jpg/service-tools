//! Anlas 对账快照。

use crate::{Db, DbError, Result, value};
use gate_core::jsondump;
use rusqlite::params;
use serde_json::Value;

impl Db {
    /// 累计对账总数：含已删除的 Key，不随每日额度重置。
    /// 返回 dict 的原样行：SUM 在空表上是整数 0、有数据时是浮点——保持 SQLite 给的类型。
    pub async fn reconciliation_totals(&self) -> Result<Value> {
        self.read(|c| {
            c.query_row(
                "SELECT (SELECT COALESCE(SUM(anlas), 0) FROM counters) AS anlas, \
                        COALESCE(SUM(unconfirmed_anlas), 0) AS unconfirmed_anlas, \
                        COALESCE(SUM(unconfirmed_anlas > 0), 0) AS unconfirmed_requests, \
                        COALESCE(MAX(id), 0) AS last_log_id FROM usage_log",
                [],
                value::row_json,
            )
        })
        .await
    }

    /// 存一份对账快照。旧版 `json.dumps(snapshot, ensure_ascii=False, allow_nan=False)`：
    /// 默认分隔符（", " / ": "）、保留 UTF-8；`Value` 里不可能有 NaN / Infinity。
    pub async fn save_reconciliation(&self, snapshot: &Value) -> Result<()> {
        let text = jsondump::dumps(snapshot);
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO anlas_reconciliations(snapshot) VALUES (?)",
                params![text],
            )?;
            Ok(())
        })
        .await
    }

    /// 最近的对账快照（新→旧）；每条是快照内容再追加 / 覆盖 `id`（`{**snapshot, "id": id}`）。
    pub async fn reconciliation_history(&self, limit: i64) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, 20);
        let rows: Vec<(i64, String)> = self
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, snapshot FROM anlas_reconciliations ORDER BY id DESC LIMIT ?",
                )?;
                stmt.query_map([limit], |r| {
                    Ok((
                        value::i64_or(r, "id", 0)?,
                        value::text_or(r, "snapshot", "")?,
                    ))
                })?
                .collect()
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, snapshot) in rows {
            let parsed: Value = serde_json::from_str(&snapshot)
                .map_err(|e| DbError::Invalid(format!("对账快照不是合法 JSON：{e}")))?;
            let Value::Object(mut map) = parsed else {
                return Err(DbError::Invalid("对账快照不是 JSON 对象".into()));
            };
            map.insert("id".into(), Value::from(id));
            out.push(Value::Object(map));
        }
        Ok(out)
    }
}
