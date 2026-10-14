//! 每日计数器、额度基线、全站汇总。

use super::{PyMap, UPSERT_COUNTERS, marks};
use crate::{Db, DbError, Result, value};
use gate_core::{clock::Clock, pyc};
use rusqlite::params;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

/// `bump_counters` 的增量（缺省值与旧版一致：requests 默认 1，其余 0）。
#[derive(Debug, Clone, Copy)]
pub struct CounterDelta {
    pub images: i64,
    pub anlas: f64,
    pub text_tokens: i64,
    pub requests: i64,
    pub v5: i64,
    pub legacy_free_images: i64,
}

impl Default for CounterDelta {
    fn default() -> Self {
        Self {
            images: 0,
            anlas: 0.0,
            text_tokens: 0,
            requests: 1,
            v5: 0,
            legacy_free_images: 0,
        }
    }
}

/// `get_counter` 的结果：已扣除「重置额度」基线后的当日用量。
///
/// 序列化与旧版返回的 dict 逐键一致：
/// - 有记录：key_id, day, images, legacy_free_images, anlas, text_tokens, requests, v5；
///   其中 anlas 是 `max(0, anlas - 基线)`——旧版 `max(0, x)` 在 x ≤ 0 时返回**整数** 0（不是 0.0），照抄；
/// - 没记录：images, legacy_free_images, anlas(0.0), text_tokens, requests, v5（没有 key_id / day）。
#[derive(Debug, Clone, PartialEq)]
pub struct Counter {
    /// 数据库里是否有这一行（决定序列化的键集合）。
    pub exists: bool,
    pub key_id: i64,
    pub day: String,
    pub images: i64,
    pub legacy_free_images: i64,
    pub anlas: f64,
    pub text_tokens: i64,
    pub requests: i64,
    pub v5: i64,
}

impl Counter {
    fn empty() -> Self {
        Self {
            exists: false,
            key_id: 0,
            day: String::new(),
            images: 0,
            legacy_free_images: 0,
            anlas: 0.0,
            text_tokens: 0,
            requests: 0,
            v5: 0,
        }
    }
}

impl Serialize for Counter {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(if self.exists { 8 } else { 6 }))?;
        if self.exists {
            m.serialize_entry("key_id", &self.key_id)?;
            m.serialize_entry("day", &self.day)?;
        }
        m.serialize_entry("images", &self.images)?;
        m.serialize_entry("legacy_free_images", &self.legacy_free_images)?;
        let positive = self.anlas > 0.0;
        if self.exists && !positive {
            // Python: max(0, x) 在 x <= 0（含 nan）时返回整数 0
            m.serialize_entry("anlas", &0_i64)?;
        } else {
            m.serialize_entry("anlas", &self.anlas)?;
        }
        m.serialize_entry("text_tokens", &self.text_tokens)?;
        m.serialize_entry("requests", &self.requests)?;
        m.serialize_entry("v5", &self.v5)?;
        m.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewToday {
    pub images: i64,
    /// 成员出图：不含测试 Key 和已删除的 Key；上面的 images 是账号真实消耗。
    pub member_images: i64,
    pub anlas: f64,
    pub text_tokens: i64,
    pub requests: i64,
    pub v5: i64,
    pub unconfirmed_requests: i64,
    pub unconfirmed_anlas: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewWeekDay {
    pub day: String,
    pub images: i64,
    pub anlas: f64,
    pub text_tokens: i64,
    pub requests: i64,
    pub v5: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewMonth {
    pub anlas: f64,
    pub unconfirmed_requests: i64,
    pub unconfirmed_anlas: f64,
}

/// 后台总览（键顺序与旧版 `overview()` 返回的 dict 一致）。
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub today: OverviewToday,
    pub active_members: i64,
    pub members_total: i64,
    pub keys_total: i64,
    pub keys_active: i64,
    pub week: Vec<OverviewWeekDay>,
    pub month: OverviewMonth,
}

impl Db {
    pub async fn bump_counters(&self, key_id: i64, day: &str, d: &CounterDelta) -> Result<()> {
        let day = day.to_string();
        let d = *d;
        self.write(move |tx| {
            tx.execute(
                UPSERT_COUNTERS,
                params![
                    key_id,
                    day,
                    d.images,
                    d.anlas,
                    d.text_tokens,
                    d.requests,
                    d.v5,
                    d.legacy_free_images
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_counter(&self, key_id: i64, day: &str) -> Result<Counter> {
        let day = day.to_string();
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT c.*, COALESCE(o.anlas, 0) AS _quota_offset_anlas, \
                        COALESCE(o.v5, 0) AS _quota_offset_v5 \
                 FROM counters AS c \
                 LEFT JOIN daily_quota_offsets AS o ON o.key_id=c.key_id AND o.day=c.day \
                 WHERE c.key_id=? AND c.day=?",
            )?;
            let mut rows = stmt.query(params![key_id, day])?;
            let Some(row) = rows.next()? else {
                return Ok(Counter::empty());
            };
            let anlas =
                value::f64_or(row, "anlas", 0.0)? - value::f64_or(row, "_quota_offset_anlas", 0.0)?;
            let v5 = value::i64_or(row, "v5", 0)? - value::i64_or(row, "_quota_offset_v5", 0)?;
            Ok(Counter {
                exists: true,
                key_id: value::i64_or(row, "key_id", 0)?,
                day: value::text_or(row, "day", "")?,
                images: value::i64_or(row, "images", 0)?,
                legacy_free_images: value::i64_or(row, "legacy_free_images", 0)?,
                anlas,
                text_tokens: value::i64_or(row, "text_tokens", 0)?,
                requests: value::i64_or(row, "requests", 0)?,
                v5: v5.max(0),
            })
        })
        .await
    }

    /// 重置单个 Key 当日的 V5 与 Anlas 可用额度基线：保留原始记账用量，以重置时的累计量更新基线。
    /// 已删除的 Key 被触发器 `prevent_deleted_key_quota_reset` 静默忽略。
    pub async fn reset_daily_image_quota(&self, key_id: i64, day: &str) -> Result<()> {
        let day = day.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO daily_quota_offsets (key_id, day, anlas, v5) \
                 SELECT key_id, day, anlas, v5 FROM counters WHERE key_id=? AND day=? \
                 ON CONFLICT(key_id, day) DO UPDATE SET anlas=excluded.anlas, v5=excluded.v5",
                params![key_id, day],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn month_anlas(&self, key_id: i64, month: &str) -> Result<f64> {
        let month = month.to_string();
        self.read(move |c| {
            c.query_row(
                "SELECT COALESCE(SUM(anlas),0) AS a FROM counters WHERE key_id=? AND substr(day,1,7)=?",
                params![key_id, month],
                |r| value::f64_or(r, "a", 0.0),
            )
        })
        .await
    }

    /// 全站当日 V5 消耗，不包含明确配置为独立额度的 Key。
    pub async fn day_v5_total(&self, day: &str) -> Result<i64> {
        let day = day.to_string();
        self.read(move |c| {
            c.query_row(
                "SELECT COALESCE(SUM(c.v5),0) AS c \
                 FROM counters AS c \
                 LEFT JOIN api_keys AS k ON k.id = c.key_id \
                 LEFT JOIN deleted_key_usage_flags AS d ON d.key_id = c.key_id \
                 WHERE c.day=? AND COALESCE(k.exclude_global_v5, d.exclude_global_v5, 1)=0",
                [day],
                |r| value::i64_or(r, "c", 0),
            )
        })
        .await
    }

    /// 全站当日消耗的 Anlas 总数。
    pub async fn day_anlas_total(&self, day: &str) -> Result<f64> {
        let day = day.to_string();
        self.read(move |c| {
            c.query_row(
                "SELECT COALESCE(SUM(anlas), 0) AS a FROM counters WHERE day=?",
                [day],
                |r| value::f64_or(r, "a", 0.0),
            )
        })
        .await
    }

    /// 全站所有 Key 本月的 Anlas 消耗（全站预算总闸）。
    pub async fn month_anlas_all(&self, month: &str) -> Result<f64> {
        let month = month.to_string();
        self.read(move |c| {
            c.query_row(
                "SELECT COALESCE(SUM(anlas),0) AS a FROM counters WHERE substr(day,1,7)=?",
                [month],
                |r| value::f64_or(r, "a", 0.0),
            )
        })
        .await
    }

    /// 每个 Key 近一段时间的用量汇总（来自每日计数器）；值里带 key_id（旧版 `dict(row)`）。
    pub async fn member_usage(&self, since_day: &str) -> Result<PyMap<i64, Value>> {
        let since_day = since_day.to_string();
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT key_id, COALESCE(SUM(images),0) AS images, COALESCE(SUM(v5),0) AS v5, \
                        COALESCE(SUM(anlas),0) AS anlas, COALESCE(SUM(text_tokens),0) AS text_tokens, \
                        COALESCE(SUM(requests),0) AS requests \
                 FROM counters WHERE day>=? GROUP BY key_id",
            )?;
            let rows = stmt.query_map([since_day], |r| {
                Ok((value::i64_or(r, "key_id", 0)?, value::row_json(r)?))
            })?;
            // dict 同键后写覆盖前写、位置保持首次出现（GROUP BY 后键唯一，这里只是照抄）
            let mut out: PyMap<i64, Value> = Vec::new();
            for item in rows {
                let (k, v) = item?;
                match out.iter_mut().find(|(ek, _)| *ek == k) {
                    Some(slot) => slot.1 = v,
                    None => out.push((k, v)),
                }
            }
            Ok(out)
        })
        .await
    }

    /// 后台总览。`today` 是服务日字符串；`week_days` 由调用方给（`state.week_days`）。
    /// 异常统计的时间窗按 `today` 当天的**本地自然日 0 点**起算（不按 7 点重置，与旧版一致）。
    pub async fn overview(
        &self,
        today: &str,
        week_days: &[String],
        now: f64,
        clock: &Clock,
    ) -> Result<Overview> {
        let today = today.to_string();
        let week_days = week_days.to_vec();
        // 旧版：datetime.fromisoformat(today).replace(tzinfo=tz)；month 窗从当月 1 日 0 点起，两窗同止于次日 0 点
        let date: jiff::civil::Date = today
            .parse()
            .map_err(|e| DbError::Invalid(format!("today 不是 YYYY-MM-DD：{e}")))?;
        let local = |d: jiff::civil::Date| {
            clock
                .local_midnight(&d.to_string())
                .ok_or_else(|| DbError::Invalid(format!("无法换算本地 0 点：{d}")))
        };
        let day_start = local(date)?;
        let month_start = local(date.first_of_month())?;
        let end = local(
            date.tomorrow()
                .map_err(|e| DbError::Invalid(e.to_string()))?,
        )?;

        self.read(move |c| {
            let one_i = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<i64> {
                c.query_row(sql, args, |r| value::i64_or(r, 0, 0))
            };
            let one_f = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<f64> {
                c.query_row(sql, args, |r| value::f64_or(r, 0, 0.0))
            };
            let today_images = one_i("SELECT COALESCE(SUM(images),0) FROM counters WHERE day=?", &[&today])?;
            let today_anlas = one_f("SELECT COALESCE(SUM(anlas),0) FROM counters WHERE day=?", &[&today])?;
            let today_tokens = one_i("SELECT COALESCE(SUM(text_tokens),0) FROM counters WHERE day=?", &[&today])?;
            let today_requests = one_i("SELECT COALESCE(SUM(requests),0) FROM counters WHERE day=?", &[&today])?;
            let today_v5 = one_i(
                "SELECT COALESCE(SUM(c.v5),0) \
                 FROM counters AS c \
                 LEFT JOIN api_keys AS k ON k.id = c.key_id \
                 LEFT JOIN deleted_key_usage_flags AS d ON d.key_id = c.key_id \
                 WHERE c.day=? AND COALESCE(k.exclude_global_v5, d.exclude_global_v5, 1)=0",
                &[&today],
            )?;
            let keys_total = one_i("SELECT COUNT(*) FROM api_keys", &[])?;
            let keys_active = one_i(
                "SELECT COUNT(*) FROM api_keys WHERE enabled=1 AND (expires_at IS NULL OR expires_at>?)",
                &[&now],
            )?;
            let active_members = one_i(
                "SELECT COUNT(DISTINCT c.key_id) \
                 FROM counters AS c \
                 JOIN api_keys AS k ON k.id = c.key_id \
                 WHERE c.day=? AND k.is_admin=0 AND k.is_test=0 AND c.requests>0",
                &[&today],
            )?;
            let members_total = one_i("SELECT COUNT(*) FROM api_keys WHERE is_admin=0 AND is_test=0", &[])?;

            let mut anomaly = Vec::new();
            for start in [day_start, month_start] {
                let (n, sum) = c.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(unconfirmed_anlas),0) FROM usage_log \
                     WHERE unconfirmed_anlas>0 AND ts>=? AND ts<?",
                    params![start, end],
                    |r| Ok((value::i64_or(r, 0, 0)?, value::f64_or(r, 1, 0.0)?)),
                )?;
                anomaly.push((n, pyc::round(sum, 2)));
            }

            let sql = format!(
                "SELECT c.day, \
                        SUM(c.images) AS images, SUM(c.anlas) AS anlas, \
                        SUM(c.text_tokens) AS text_tokens, SUM(c.requests) AS requests, \
                        SUM(CASE WHEN COALESCE(k.exclude_global_v5, d.exclude_global_v5, 1)=0 THEN c.v5 ELSE 0 END) AS v5 \
                 FROM counters AS c \
                 LEFT JOIN api_keys AS k ON k.id = c.key_id \
                 LEFT JOIN deleted_key_usage_flags AS d ON d.key_id = c.key_id \
                 WHERE c.day IN ({}) GROUP BY c.day",
                marks(week_days.len())
            );
            let mut stmt = c.prepare(&sql)?;
            let mut by_day: std::collections::HashMap<String, OverviewWeekDay> = Default::default();
            let rows = stmt.query_map(rusqlite::params_from_iter(week_days.iter()), |r| {
                let day = value::text_or(r, "day", "")?;
                Ok(OverviewWeekDay {
                    day: day.clone(),
                    images: value::i64_or(r, "images", 0)?,
                    anlas: value::f64_or(r, "anlas", 0.0)?,
                    text_tokens: value::i64_or(r, "text_tokens", 0)?,
                    requests: value::i64_or(r, "requests", 0)?,
                    v5: value::i64_or(r, "v5", 0)?,
                })
            })?;
            for r in rows {
                let r = r?;
                by_day.insert(r.day.clone(), r);
            }
            let week = week_days
                .iter()
                .map(|d| {
                    by_day.get(d).cloned().unwrap_or(OverviewWeekDay {
                        day: d.clone(),
                        images: 0,
                        anlas: 0.0,
                        text_tokens: 0,
                        requests: 0,
                        v5: 0,
                    })
                })
                .collect();

            let member_images = one_i(
                "SELECT COALESCE(SUM(c.images),0) FROM counters c JOIN api_keys k ON k.id=c.key_id \
                 WHERE c.day=? AND k.is_test=0",
                &[&today],
            )?;
            let month_prefix: String = pyc::take_chars(&today, 7).to_string();
            let month_anlas = one_f(
                "SELECT COALESCE(SUM(anlas),0) FROM counters WHERE substr(day,1,7)=?",
                &[&month_prefix],
            )?;
            Ok(Overview {
                today: OverviewToday {
                    images: today_images,
                    member_images,
                    anlas: pyc::round(today_anlas, 2),
                    text_tokens: today_tokens,
                    requests: today_requests,
                    v5: today_v5,
                    unconfirmed_requests: anomaly[0].0,
                    unconfirmed_anlas: anomaly[0].1,
                },
                active_members,
                members_total,
                keys_total,
                keys_active,
                week,
                month: OverviewMonth {
                    anlas: pyc::round(month_anlas, 2),
                    unconfirmed_requests: anomaly[1].0,
                    unconfirmed_anlas: anomaly[1].1,
                },
            })
        })
        .await
    }
}
