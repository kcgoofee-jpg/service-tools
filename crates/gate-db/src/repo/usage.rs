//! 用量日志：成功记账、拒绝 / 失败日志、各类统计查询。

use super::{
    INSERT_LOG, INSERT_REJECT, LOG_COLUMNS, NOT_TEST, PyMap, SqlValue, UPSERT_COUNTERS, marks,
};
use crate::{Db, Result, value};
use gate_core::pyc;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

/// 一次成功请求的记账内容（`record_success`）。字段缺省值与旧版关键字参数默认值一致。
#[derive(Debug, Clone, Default)]
pub struct Success {
    pub key_id: i64,
    pub key_name: String,
    pub kind: String,
    pub model: String,
    pub day: String,
    pub images: i64,
    pub anlas: f64,
    pub tokens: i64,
    pub v5: i64,
    pub legacy_free_images: i64,
    pub detail: String,
    pub unconfirmed_anlas: f64,
    pub wait_ms: i64,
    pub dur_ms: i64,
    pub up_status: i64,
    pub rid: String,
    /// 处理这条请求的上游账号；没发到上游为 None。
    pub token_id: Option<String>,
}

/// 一条拒绝 / 失败日志（`add_log`）。
#[derive(Debug, Clone, Default)]
pub struct NewLog {
    pub key_id: Option<i64>,
    pub key_name: String,
    pub kind: String,
    pub model: String,
    pub status: String,
    pub images: i64,
    pub anlas: f64,
    pub tokens: i64,
    pub detail: String,
    pub unconfirmed_anlas: f64,
    pub wait_ms: i64,
    pub dur_ms: i64,
    pub up_status: i64,
    pub rid: String,
    pub reason: String,
    /// 选中过的上游账号（选号之后才失败的请求有值）；选号之前就被拒为 None。
    pub token_id: Option<String>,
}

/// 日志筛选条件（旧版 `_log_filter` 的参数）。
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// 0 与 None 一样表示不筛（旧版 `if key_id:`）。
    pub key_id: Option<i64>,
    /// None = 不筛；Some(空) = 一条都不匹配（旧版生成 `0`）。
    pub kinds: Option<Vec<String>>,
    pub hide_test: bool,
    /// 空串 = 不筛。
    pub rid: String,
}

/// `member_milestones` 里每把 Key 的一项。`rejected_why` 只有被拒过的 Key 才有（序列化时省略）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Milestone {
    pub first_image_at: Option<f64>,
    pub last_image_at: Option<f64>,
    pub rejected_24h: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_why: Option<Vec<(String, i64)>>,
}

/// `image_perf_rows`：(ts, model, status, images, wait_ms, dur_ms, up_status, detail)
pub type ImagePerfRow = (f64, String, String, i64, i64, i64, i64, String);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageStability {
    pub ok: i64,
    pub error: i64,
}

/// `user_image_timing` 的结果（秒，保留 1 位小数）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageTiming {
    pub count: i64,
    pub avg_dur_s: Option<f64>,
    pub avg_wait_s: Option<f64>,
    pub site_avg_dur_s: f64,
}

/// 真正出图（`image` / `image_stream`）成功才刷新 last_image_at：闲置回收只认它。
/// 查额度、文本聊天、标签补全不算；放大、导演工具、Vibe 编码也不算
/// （旧版前缀匹配 `image*` 并把这三种算进去，只用它们「保活」的 Key 不会被回收）。
fn is_image_kind(kind: &str) -> bool {
    matches!(kind, "image" | "image_stream")
}

/// 旧版 `_log_filter`：返回 (` WHERE ...` 或空串, 绑定参数)。
fn log_filter(f: &LogFilter) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if !f.rid.is_empty() {
        clauses.push("rid=?".into());
        args.push(SqlValue::Text(f.rid.clone()));
    }
    if f.hide_test {
        clauses.push(NOT_TEST.into());
    }
    if let Some(k) = f.key_id.filter(|k| *k != 0) {
        clauses.push("key_id=?".into());
        args.push(SqlValue::Integer(k));
    }
    if let Some(kinds) = &f.kinds {
        if kinds.is_empty() {
            clauses.push("0".into());
        } else {
            clauses.push(format!("kind IN ({})", marks(kinds.len())));
            args.extend(kinds.iter().cloned().map(SqlValue::Text));
        }
    }
    let where_ = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_, args)
}

impl Db {
    /// 成功日志、额度与使用时间一起提交；写入失败时整笔回退（一个事务）。
    /// 旧版在被别的写入短暂锁住时会重试几次（睡眠 0.5 / 1 / 2 秒）；这里写线程独占写连接，
    /// 锁冲突由 `Db::write` 的 BUSY 重试兜住，可见结果相同。
    pub async fn record_success(&self, s: &Success, now: f64) -> Result<()> {
        let s = s.clone();
        self.write(move |tx| {
            tx.execute(
                INSERT_LOG,
                params![
                    now,
                    s.key_id,
                    s.key_name,
                    s.kind,
                    s.model,
                    "ok",
                    s.images,
                    s.anlas,
                    s.tokens,
                    pyc::take_chars(&s.detail, 500),
                    s.unconfirmed_anlas,
                    s.wait_ms.max(0),
                    s.dur_ms.max(0),
                    s.up_status,
                    pyc::take_chars(&s.rid, 16),
                    s.token_id,
                ],
            )?;
            tx.execute(
                UPSERT_COUNTERS,
                params![
                    s.key_id,
                    s.day,
                    s.images,
                    s.anlas,
                    s.tokens,
                    1_i64,
                    s.v5,
                    s.legacy_free_images
                ],
            )?;
            tx.execute(
                "UPDATE api_keys SET last_used_at=?, \
                 last_image_at=CASE WHEN ? THEN ? ELSE last_image_at END WHERE id=?",
                params![now, i64::from(is_image_kind(&s.kind)), now, s.key_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn add_log(&self, l: &NewLog, now: f64) -> Result<()> {
        let l = l.clone();
        self.write(move |tx| {
            tx.execute(
                INSERT_REJECT,
                params![
                    now,
                    l.key_id,
                    pyc::take_chars(&l.key_name, 80),
                    l.kind,
                    pyc::take_chars(&l.model, 80),
                    l.status,
                    l.images,
                    l.anlas,
                    l.tokens,
                    pyc::take_chars(&l.detail, 500),
                    l.unconfirmed_anlas,
                    l.wait_ms.max(0),
                    l.dur_ms.max(0),
                    l.up_status,
                    pyc::take_chars(&l.rid, 16),
                    pyc::take_chars(&l.reason, 24),
                    l.token_id,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// 日志列表（新→旧）；`limit` 夹在 1..=500，`offset` 不小于 0。
    pub async fn list_logs(
        &self,
        limit: i64,
        offset: i64,
        filter: &LogFilter,
    ) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);
        let (where_, mut args) = log_filter(filter);
        args.push(SqlValue::Integer(limit));
        args.push(SqlValue::Integer(offset));
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {LOG_COLUMNS} FROM usage_log{where_} ORDER BY id DESC LIMIT ? OFFSET ?"
            ))?;
            stmt.query_map(rusqlite::params_from_iter(args.iter()), value::row_json)?
                .collect()
        })
        .await
    }

    pub async fn count_logs(&self, filter: &LogFilter) -> Result<i64> {
        let (where_, args) = log_filter(filter);
        self.read(move |c| {
            c.query_row(
                &format!("SELECT COUNT(*) AS c FROM usage_log{where_}"),
                rusqlite::params_from_iter(args.iter()),
                |r| value::i64_or(r, "c", 0),
            )
        })
        .await
    }

    /// 每个时间段 `[from, to)` 里全站成功出图的条数（与 `ranges` 一一对应）。
    /// 闲置回收用它判断「站点那天有没有在正常出图」。
    pub async fn ok_image_counts(&self, ranges: Vec<(f64, f64)>) -> Result<Vec<i64>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT COUNT(*) FROM usage_log \
                 WHERE ts>=? AND ts<? AND status='ok' AND kind IN ('image','image_stream')",
            )?;
            ranges
                .iter()
                .map(|(from, to)| stmt.query_row(params![from, to], |r| value::i64_or(r, 0, 0)))
                .collect()
        })
        .await
    }

    /// 只清理明细日志；每日计数器和账本不受影响。返回删除条数。
    pub async fn purge_usage_log(&self, older_than: f64) -> Result<i64> {
        self.write(move |tx| {
            Ok(tx.execute("DELETE FROM usage_log WHERE ts<?", [older_than])? as i64)
        })
        .await
    }

    /// 每把 Key：首次 / 最近一次成功出图时间，以及 `since` 之后被拒次数和前 3 个被拒原因（用于成员筛选）。
    /// 基于用量日志，超过日志保留期的早期记录不计入。`reason_label(code, detail)` 是 `app/reasons.py::reason_label`。
    pub async fn member_milestones(
        &self,
        since: f64,
        reason_label: &(dyn Fn(&str, &str) -> String + Sync),
    ) -> Result<PyMap<i64, Milestone>> {
        // 先在读连接里取回原始分组行，再在这里按旧版逻辑合并（reason_label 不必跨线程）
        type RawWhy = (i64, String, String, i64);
        type RawImg = (i64, Option<f64>, Option<f64>);
        let (img, rej, why): (Vec<RawImg>, Vec<(i64, i64)>, Vec<RawWhy>) = self
            .read(move |c| {
                let img = c
                    .prepare(
                        "SELECT key_id, MIN(ts), MAX(ts) FROM usage_log \
                         WHERE status='ok' AND kind IN ('image','image_stream') AND key_id IS NOT NULL GROUP BY key_id",
                    )?
                    .query_map([], |r| {
                        Ok((
                            value::i64_or(r, 0, 0)?,
                            value::f64_opt(r, 1)?,
                            value::f64_opt(r, 2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let rej = c
                    .prepare(
                        "SELECT key_id, COUNT(*) FROM usage_log WHERE status='rejected' AND ts>=? AND key_id IS NOT NULL \
                         GROUP BY key_id",
                    )?
                    .query_map([since], |r| Ok((value::i64_or(r, 0, 0)?, value::i64_or(r, 1, 0)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let why = c
                    .prepare(
                        "SELECT key_id, reason, MIN(detail), COUNT(*) FROM usage_log \
                         WHERE status='rejected' AND ts>=? AND key_id IS NOT NULL \
                         GROUP BY key_id, CASE WHEN reason!='' THEN reason ELSE substr(detail, 1, 24) END",
                    )?
                    .query_map([since], |r| {
                        Ok((
                            value::i64_or(r, 0, 0)?,
                            value::text_or(r, 1, "")?,
                            value::text_or(r, 2, "")?,
                            value::i64_or(r, 3, 0)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((img, rej, why))
            })
            .await?;

        let mut out: PyMap<i64, Milestone> = Vec::new();
        fn slot(out: &mut PyMap<i64, Milestone>, key: i64) -> &mut Milestone {
            let idx = match out.iter().position(|(k, _)| *k == key) {
                Some(i) => i,
                None => {
                    out.push((
                        key,
                        Milestone {
                            first_image_at: None,
                            last_image_at: None,
                            rejected_24h: 0,
                            rejected_why: None,
                        },
                    ));
                    out.len() - 1
                }
            };
            &mut out[idx].1
        }
        for (key_id, first, last) in img {
            // out[key] = {...}：覆盖写（GROUP BY 之后键唯一）
            let m = slot(&mut out, key_id);
            m.first_image_at = first;
            m.last_image_at = last;
            m.rejected_24h = 0;
        }
        for (key_id, n) in rej {
            slot(&mut out, key_id).rejected_24h = n;
        }
        // 被拒原因：同一句话（只差数字）合并，按次数降序取前 3（次数相同保持首次出现的顺序）
        let mut by_key: PyMap<i64, Vec<(String, i64)>> = Vec::new();
        for (key_id, code, detail, n) in why {
            let label = reason_label(&code, &detail);
            let idx = match by_key.iter().position(|(k, _)| *k == key_id) {
                Some(i) => i,
                None => {
                    by_key.push((key_id, Vec::new()));
                    by_key.len() - 1
                }
            };
            let items = &mut by_key[idx].1;
            match items.iter_mut().find(|(l, _)| *l == label) {
                Some(e) => e.1 += n,
                None => items.push((label, n)),
            }
        }
        for (key_id, mut items) in by_key {
            items.sort_by_key(|(_, n)| std::cmp::Reverse(*n)); // 稳定排序，同 Python sorted(key=-n)
            items.truncate(3);
            slot(&mut out, key_id).rejected_why = Some(items);
        }
        Ok(out)
    }

    /// 出图请求的 (ts, model, status, images, wait_ms, dur_ms, up_status, detail)，供上游表现分析（不含测试 Key）。
    pub async fn image_perf_rows(&self, since: f64) -> Result<Vec<ImagePerfRow>> {
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT ts, model, status, images, wait_ms, dur_ms, up_status, detail FROM usage_log \
                 WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error') \
                   AND {NOT_TEST} \
                 ORDER BY ts"
            ))?;
            stmt.query_map([since], |r| {
                Ok((
                    value::f64_or(r, 0, 0.0)?,
                    value::text_or(r, 1, "None")?,
                    value::text_or(r, 2, "None")?,
                    value::i64_or(r, 3, 0)?,
                    value::i64_or(r, 4, 0)?,
                    value::i64_or(r, 5, 0)?,
                    value::i64_or(r, 6, 0)?,
                    value::text_or(r, 7, "None")?,
                ))
            })?
            .collect()
        })
        .await
    }

    /// `since` 之后出图请求的成功 / 上游失败次数（不含参数错误等被拒请求），用于首页稳定性图标。
    pub async fn image_stability(&self, since: f64) -> Result<ImageStability> {
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT status, COUNT(*) FROM usage_log \
                 WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error') \
                   AND {NOT_TEST} \
                 GROUP BY status"
            ))?;
            let rows = stmt.query_map([since], |r| {
                Ok((value::text_or(r, 0, "")?, value::i64_or(r, 1, 0)?))
            })?;
            let mut out = ImageStability { ok: 0, error: 0 };
            for row in rows {
                let (status, n) = row?;
                match status.as_str() {
                    "ok" => out.ok = n,
                    "error" => out.error = n,
                    _ => {}
                }
            }
            Ok(out)
        })
        .await
    }

    /// 按 kind / status 汇总 `since` 之后的用量日志（日志保留期内有效；不含测试 Key）。
    pub async fn usage_by_kind(&self, since: f64) -> Result<Vec<Value>> {
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT kind, status, COUNT(*) AS n, COALESCE(SUM(images),0) AS images, \
                        COALESCE(SUM(tokens),0) AS tokens, COALESCE(SUM(anlas),0) AS anlas, \
                        COUNT(DISTINCT key_id) AS users \
                 FROM usage_log WHERE ts>=? AND {NOT_TEST} GROUP BY kind, status"
            ))?;
            stmt.query_map([since], value::row_json)?.collect()
        })
        .await
    }

    /// 每把 Key 成功生成的张数（含流式完成的图）。
    pub async fn generated_image_totals(&self, key_id: Option<i64>) -> Result<PyMap<i64, i64>> {
        self.read(move |c| {
            let mut sql = "SELECT key_id, COALESCE(SUM(images), 0) AS images \
                           FROM usage_log \
                           WHERE key_id IS NOT NULL AND kind IN ('image', 'image_stream') \
                             AND status='ok'"
                .to_string();
            if key_id.is_some() {
                sql.push_str(" AND key_id=?");
            }
            sql.push_str(" GROUP BY key_id");
            let mut stmt = c.prepare(&sql)?;
            let map = |r: &rusqlite::Row<'_>| {
                Ok((
                    value::i64_or(r, "key_id", 0)?,
                    value::i64_or(r, "images", 0)?,
                ))
            };
            match key_id {
                Some(k) => stmt.query_map([k], map)?.collect(),
                None => stmt.query_map([], map)?.collect(),
            }
        })
        .await
    }

    /// 指定 Key 的平均出图耗时（秒）及全站近 24 小时参考耗时。
    /// 该 Key 近 24 小时成功出图不足 3 次时改用它的历史全部记录（有的话）。
    pub async fn user_image_timing(&self, key_id: i64, now: f64) -> Result<ImageTiming> {
        let day_ago = now - 86400.0;
        self.read(move |c| {
            type Agg = (i64, Option<f64>, Option<f64>);
            let agg = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Agg> {
                c.query_row(sql, args, |r| {
                    Ok((
                        value::i64_or(r, 0, 0)?,
                        value::f64_opt(r, 1)?,
                        value::f64_opt(r, 2)?,
                    ))
                })
            };
            let (mut cnt, mut avg_dur, mut avg_wait) = agg(
                "SELECT COUNT(*), AVG(dur_ms), AVG(wait_ms) \
                 FROM usage_log \
                 WHERE key_id=? AND kind IN ('image', 'image_stream') AND status='ok' AND dur_ms>0 AND ts>=?",
                &[&key_id, &day_ago],
            )?;
            if cnt < 3 {
                let all = agg(
                    "SELECT COUNT(*), AVG(dur_ms), AVG(wait_ms) \
                     FROM usage_log \
                     WHERE key_id=? AND kind IN ('image', 'image_stream') AND status='ok' AND dur_ms>0",
                    &[&key_id],
                )?;
                if all.0 > 0 {
                    (cnt, avg_dur, avg_wait) = all;
                }
            }
            let site: Option<f64> = c.query_row(
                "SELECT AVG(dur_ms) FROM usage_log \
                 WHERE kind IN ('image', 'image_stream') AND status='ok' AND dur_ms>0 AND ts>=?",
                [day_ago],
                |r| value::f64_opt(r, 0),
            )?;
            // Python `if x`：None 与 0.0 都是假
            let truthy = |v: Option<f64>| v.filter(|x| *x != 0.0);
            let site_avg = truthy(site).unwrap_or(9000.0);
            Ok(ImageTiming {
                count: cnt,
                avg_dur_s: truthy(avg_dur).map(|x| pyc::round(x / 1000.0, 1)),
                avg_wait_s: truthy(avg_wait).map(|x| pyc::round(x / 1000.0, 1)),
                site_avg_dur_s: pyc::round(site_avg / 1000.0, 1),
            })
        })
        .await
    }
}
