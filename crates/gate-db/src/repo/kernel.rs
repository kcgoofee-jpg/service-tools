//! 自适应内核（动态额度 / 自动驾驶 / 模块校验 / 风险检查 / 维护循环）要用的查询与写入。
//!
//! 旧版散在 `quota_algo.py` / `autopilot.py` / `modules.py` / `risk_check.py` / `background_tasks.py` /
//! `state.py` 里的 `db._db.execute*`，这里按「同一个任务读到的一批数据」收成一个方法一次读完。
//! SQL 文本与旧版逐字相同（包括没有 `ORDER BY` 的地方：行顺序由 SQLite 决定，两边引擎一致）。
//!
//! 评估周期内额度 / 处罚结果不许变：这里**不做任何「顺手优化」**，怪癖照抄。

use super::NOT_TEST;
use crate::{Db, Result, value};
use gate_core::autopilot::BreakerRows;
use gate_core::modules::SnapRow;
use gate_core::quota::{
    CounterRow, DailyInputs, ManagedKey, PyNum, QuietInputs, QuietSnap, V5Row, YesterdayInputs,
    quiet_count_pairs, quiet_snaps_since,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, types::ValueRef};
use serde_json::{Map, Value};

/// 领取记录里闲置回收要用到的资料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationProfile {
    pub discord_id: String,
    pub username: String,
    pub display_name: String,
    pub avatar: String,
}

/// 用量集中度：`(key_id, key_name, 张数)` 前几名 + 全站总张数。
pub type Concentration = (Vec<(Option<i64>, String, i64)>, i64);

fn count_i64(
    c: &rusqlite::Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> rusqlite::Result<i64> {
    c.query_row(sql, args, |r| value::i64_or(r, 0, 0))
}

fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(x) => serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            let mut m = Map::new();
            m.insert("$b".into(), Value::String(hex_blob(b)));
            Value::Object(m)
        }
    }
}

fn hex_blob(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Db {
    // ------------------------------------------------------------------ 动态额度（quota_algo.run）

    /// `_members`：算法管理的成员 Key 数（`SELECT id FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0 AND quota_auto=1` 的行数）。
    pub async fn quota_member_count(&self) -> Result<usize> {
        self.read(|c| {
            count_i64(
                c,
                "SELECT COUNT(*) FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0 AND quota_auto=1",
                [],
            )
            .map(|n| n as usize)
        })
        .await
    }

    /// 「每日微调」要读的全部数据（`_yesterday` / `day_coverage` / `member_usage` / `_v5_used_equiv`）。
    /// `yday` = 前一个服务日，`[start, end)` = 它的查询窗口（见 `gate_core::quota::day_window`）。
    pub async fn quota_daily_inputs(
        &self,
        yday: &str,
        start: f64,
        end: f64,
    ) -> Result<DailyInputs> {
        let yday = yday.to_string();
        self.read(move |c| {
            let counters: Vec<CounterRow> = {
                let mut stmt = c.prepare(
                    "SELECT c.key_id, c.images, k.daily_images, c.legacy_free_images, k.quota_auto FROM counters c \
                     JOIN api_keys k ON k.id=c.key_id WHERE c.day=? AND k.is_test=0 AND k.is_admin=0",
                )?;
                stmt.query_map([&yday], |r| {
                    Ok(CounterRow {
                        images: value::i64_or(r, 1, 0)?,
                        daily_images: value::i64_or(r, 2, 0)?,
                        legacy_free_images: value::i64_or(r, 3, 0)?,
                        quota_auto: value::i64_or(r, 4, 0)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let (upstream_images, upstream_units) = c.query_row(
                "SELECT COALESCE(SUM(images),0), COALESCE(SUM(COALESCE(units, images)),0) FROM upstream_token_counters WHERE day=?",
                [&yday],
                |r| Ok((value::i64_or(r, 0, 0)?, value::f64_or(r, 1, 0.0)?)),
            )?;
            // 只数成员（不含测试号、站长号）被拦的记录，按原因码数（不依赖中文文案）。
            // 旧版每小时上限按 `detail LIKE '%出图量已达上限%'` 数，还数了「保底」文案；保底已删除，
            // 改成按原因码（docs/v3-deviations.md #61）
            let member_rejects = "FROM usage_log u JOIN api_keys k ON k.id=u.key_id WHERE u.ts>=? AND u.ts<? \
                                  AND u.status='rejected' AND k.is_test=0 AND k.is_admin=0 AND u.reason=?";
            let hourly_block_hours: Vec<i64> = {
                let mut stmt = c.prepare(&format!(
                    "SELECT DISTINCT CAST(u.ts/3600 AS INT) {member_rejects}"
                ))?;
                stmt.query_map(
                    params![start, end, gate_core::reasons::HOURLY_CAP],
                    |r| value::i64_or(r, 0, 0),
                )?
                .collect::<rusqlite::Result<_>>()?
            };
            let paced_blocks = count_i64(
                c,
                &format!("SELECT COUNT(*) {member_rejects}"),
                params![start, end, gate_core::reasons::PACED],
            )?;
            let coverage_first_ts = c.query_row("SELECT MIN(ts) FROM usage_log", [], |r| {
                value::f64_opt(r, 0)
            })?;
            let member_usage: Vec<i64> = {
                let mut stmt = c.prepare(
                    "SELECT c.legacy_free_images FROM counters c JOIN api_keys k ON k.id=c.key_id \
                     WHERE c.day=? AND k.is_test=0 AND k.is_admin=0 AND k.quota_auto=1 AND c.legacy_free_images>0",
                )?;
                stmt.query_map([&yday], |r| value::i64_or(r, 0, 0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let v5_rows: Vec<V5Row> = {
                let mut stmt = c.prepare(
                    "SELECT detail, images FROM usage_log WHERE ts>=? AND ts<? AND status='ok' AND model LIKE '%diffusion-5%'",
                )?;
                stmt.query_map(params![start, start + 86400.0], |r| {
                    Ok(V5Row {
                        detail: value::text_or(r, 0, "")?,
                        images: value::i64_or(r, 1, 0)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            Ok(DailyInputs {
                yesterday: YesterdayInputs {
                    counters,
                    upstream_images,
                    upstream_units,
                    hourly_block_hours,
                    paced_blocks,
                },
                coverage_first_ts,
                member_usage,
                v5_rows,
            })
        })
        .await
    }

    /// `quiet_recovery` 的输入：72 小时内「主动读」的快照，以及相邻两条（间隔 ≤ 3 小时）之间的 V5 出图条数。
    pub async fn quota_quiet_inputs(&self, now: f64) -> Result<QuietInputs> {
        self.read(move |c| {
            let snaps: Vec<QuietSnap> = {
                let mut stmt = c.prepare(
                    "SELECT ts, v5_percent FROM upstream_snapshots WHERE ts>? AND fresh=1 AND v5_percent IS NOT NULL ORDER BY ts",
                )?;
                stmt.query_map([quiet_snaps_since(now)], |r| {
                    Ok(QuietSnap {
                        ts: value::f64_or(r, 0, 0.0)?,
                        percent: value::f64_or(r, 1, 0.0)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let mut counts = vec![0_i64; snaps.len().saturating_sub(1)];
            for i in quiet_count_pairs(&snaps) {
                counts[i] = count_i64(
                    c,
                    "SELECT COUNT(*) FROM usage_log WHERE ts>? AND ts<=? AND status='ok' AND model LIKE '%diffusion-5%'",
                    params![snaps[i].ts, snaps[i + 1].ts],
                )?;
            }
            Ok(QuietInputs { snaps, counts })
        })
        .await
    }

    /// `_active_v5`：这些服务日里用过 V5 的成员数（不含测试 / 站长 Key）；日期见 `quota::active_v5_days`。
    pub async fn quota_active_v5(&self, days: &[String]) -> Result<i64> {
        let days = days.to_vec();
        self.read(move |c| {
            let marks = vec!["?"; days.len()].join(",");
            count_i64(
                c,
                &format!(
                    "SELECT COUNT(DISTINCT c.key_id) FROM counters c JOIN api_keys k ON k.id=c.key_id \
                     WHERE c.v5>0 AND k.is_test=0 AND k.is_admin=0 AND c.day IN ({marks})"
                ),
                params_from_iter(days.iter()),
            )
        })
        .await
    }

    /// 算法管理的成员 Key 当前的额度（`run` 用它数「要同步几把」）。
    pub async fn quota_managed_keys(&self) -> Result<Vec<ManagedKey>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT daily_images, daily_v5, image_model_scope FROM api_keys \
                 WHERE quota_auto=1 AND is_admin=0 AND is_test=0",
            )?;
            stmt.query_map([], |r| {
                Ok(ManagedKey {
                    daily_images: value::i64_or(r, 0, 0)?,
                    daily_v5: value::i64_or(r, 1, 0)?,
                    image_model_scope: value::text_or(r, 2, "")?,
                })
            })?
            .collect()
        })
        .await
    }

    /// 把算法结果下发给所有算法管理的成员 Key。
    pub async fn quota_apply_to_keys(&self, daily_images: i64, daily_v5: i64) -> Result<()> {
        self.write(move |tx| {
            tx.execute(
                "UPDATE api_keys SET daily_images=?, daily_v5=?, image_model_scope='all' \
                 WHERE quota_auto=1 AND is_admin=0 AND is_test=0",
                params![daily_images, daily_v5],
            )?;
            Ok(())
        })
        .await
    }

    // ------------------------------------------------------------------ 自动驾驶（autopilot.run）

    /// 上一轮复盘那天里「排队满被拒」≥ `min_rejects` 次的小时数。
    pub async fn autopilot_queue_hours(
        &self,
        start: f64,
        end: f64,
        min_rejects: i64,
    ) -> Result<i64> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT CAST(ts/3600 AS INT) h FROM usage_log WHERE ts>=? AND ts<? \
                 AND reason='queue_full' GROUP BY h HAVING COUNT(*)>=?",
            )?;
            let n = stmt
                .query_map(params![start, end, min_rejects], |_| Ok(()))?
                .count();
            Ok(n as i64)
        })
        .await
    }

    /// 熔断统计：`since` 之后上游真正出错的次数与出图请求总数（没有行时 SUM 为 NULL）。
    pub async fn autopilot_breaker_rows(&self, since: f64) -> Result<BreakerRows> {
        self.read(move |c| {
            c.query_row(
                "SELECT SUM(CASE WHEN status='error' AND (up_status>=500 OR up_status BETWEEN 200 AND 299) \
                 THEN 1 ELSE 0 END), COUNT(*) FROM usage_log \
                 WHERE ts>=? AND kind LIKE 'image%' AND status IN ('ok','error')",
                [since],
                |r| {
                    Ok(BreakerRows {
                        fails: value::i64_opt(r, 0)?,
                        total: value::i64_opt(r, 1)?,
                    })
                },
            )
        })
        .await
    }

    /// 每把 Key 近 1 小时的被拒次数（排除「Key 暂停」本身和全站层面的拒绝）。
    pub async fn autopilot_rejects(&self, since: f64, skip: &[&str]) -> Result<Vec<(i64, i64)>> {
        let skip = skip.join("','");
        self.read(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' \
                 AND key_id IS NOT NULL AND reason NOT IN ('{skip}') GROUP BY key_id"
            ))?;
            stmt.query_map([since], |r| {
                Ok((value::i64_or(r, 0, 0)?, value::i64_or(r, 1, 0)?))
            })?
            .collect()
        })
        .await
    }

    /// 参与单 Key 限流判断的成员 Key（id, name）。
    pub async fn autopilot_keys(&self) -> Result<Vec<(i64, String)>> {
        self.read(|c| {
            // 旧版没有 ORDER BY；全表扫描就是 rowid 顺序，这里写明，免得换了查询计划后顺序漂移
            let mut stmt = c.prepare(
                "SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0 ORDER BY id",
            )?;
            stmt.query_map([], |r| Ok((value::i64_or(r, 0, 0)?, value::text_or(r, 1, "None")?)))?
                .collect()
        })
        .await
    }

    // ------------------------------------------------------------------ 模块（modules.py）

    /// `SELECT MAX(ts) FROM upstream_snapshots`
    pub async fn latest_snapshot_ts(&self) -> Result<Option<f64>> {
        self.read(|c| {
            c.query_row("SELECT MAX(ts) FROM upstream_snapshots", [], |r| {
                value::f64_opt(r, 0)
            })
        })
        .await
    }

    /// 每小时记一次上游真实状态。
    pub async fn insert_upstream_snapshot(
        &self,
        ts: f64,
        v5_percent: Option<f64>,
        v5_rate: Option<f64>,
        anlas: i64,
        fresh: i64,
    ) -> Result<()> {
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO upstream_snapshots(ts, v5_percent, v5_rate, anlas, fresh) VALUES (?,?,?,?,?)",
                params![ts, v5_percent, v5_rate, anlas, fresh],
            )?;
            Ok(())
        })
        .await
    }

    /// 容量校验：`since` 之后（**不含**边界）成功出图的张数。
    pub async fn images_ok_after(&self, since: f64) -> Result<i64> {
        self.read(move |c| {
            count_i64(
                c,
                "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'",
                [since],
            )
        })
        .await
    }

    /// 观测校验：`start` 起（**含**边界）成功出图的张数。
    pub async fn images_ok_from(&self, start: f64) -> Result<i64> {
        self.read(move |c| {
            count_i64(
                c,
                "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>=? AND status='ok' AND kind LIKE 'image%'",
                [start],
            )
        })
        .await
    }

    /// 容量校验：24 小时内带 V5 剩余的快照（按时间升序）。
    pub async fn snapshot_rows_since(&self, since: f64) -> Result<Vec<SnapRow>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT ts, v5_percent, v5_rate FROM upstream_snapshots WHERE ts>? AND v5_percent IS NOT NULL ORDER BY ts",
            )?;
            stmt.query_map([since], |r| {
                Ok(SnapRow {
                    ts: value::f64_or(r, 0, 0.0)?,
                    percent: value::f64_or(r, 1, 0.0)?,
                    rate: value::f64_opt(r, 2)?,
                })
            })?
            .collect()
        })
        .await
    }

    /// 容量校验：`(t0, t1]` 里日志数到的 V5 张数。
    pub async fn v5_images_between(&self, t0: f64, t1: f64) -> Result<i64> {
        self.read(move |c| {
            count_i64(
                c,
                "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND ts<=? AND status='ok' AND model LIKE '%diffusion-5%'",
                params![t0, t1],
            )
        })
        .await
    }

    /// 分配校验：算法管理的启用 Key 数，以及其中额度和算法结果不一致的数量。
    /// `a` 是 `quota_algo_last.ceiling`（可能为 NULL：SQL 比较恒不成立 → 0），`each` 是 `v5.each`。
    pub async fn managed_keys_drift(&self, a: &Value, each: PyNum) -> Result<(i64, i64)> {
        let a = super::json_to_sql(a)?;
        let each = match each {
            PyNum::Int(i) => SqlValue::Integer(i),
            PyNum::Float(f) => SqlValue::Real(f),
        };
        self.read(move |c| {
            let keys = count_i64(
                c,
                "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 AND enabled=1",
                [],
            )?;
            let drift = count_i64(
                c,
                "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 \
                 AND enabled=1 AND (daily_images<>? OR daily_v5<>?)",
                params![a, each],
            )?;
            Ok((keys, drift))
        })
        .await
    }

    /// 观测校验：`start` 起生成记录里成功的条数。
    pub async fn audited_ok_from(&self, start: f64) -> Result<i64> {
        self.read(move |c| {
            count_i64(
                c,
                "SELECT COUNT(*) FROM generation_audit WHERE ts>=? AND status='ok'",
                [start],
            )
        })
        .await
    }

    // ------------------------------------------------------------------ 风险检查（risk_check.py）

    /// 用量集中度：`since` 之后各 Key 的成功出图张数前 6 名 + 全站总数（都不含测试 Key）。
    pub async fn usage_concentration(&self, since: f64) -> Result<Concentration> {
        self.read(move |c| {
            let rows: Vec<(Option<i64>, String, i64)> = {
                let mut stmt = c.prepare(&format!(
                    "SELECT key_id, key_name, COUNT(*) AS c FROM usage_log \
                     WHERE ts>=? AND kind IN ('image','image_stream') AND status='ok' AND {NOT_TEST} \
                     GROUP BY key_id ORDER BY c DESC LIMIT 6"
                ))?;
                stmt.query_map([since], |r| {
                    Ok((
                        value::i64_opt(r, 0)?,
                        value::text_or(r, 1, "")?,
                        value::i64_or(r, 2, 0)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let total = count_i64(
                c,
                &format!(
                    "SELECT COUNT(*) FROM usage_log \
                     WHERE ts>=? AND kind IN ('image','image_stream') AND status='ok' AND {NOT_TEST}"
                ),
                [since],
            )?;
            Ok((rows, total))
        })
        .await
    }

    // ------------------------------------------------------------------ 维护循环 / 闲置回收

    /// 删除 `before` 之前的状态码统计（旧版 `DELETE FROM status_hourly WHERE hour<?` 绑的是浮点时间）。
    pub async fn purge_status_hourly_older_than(&self, before: f64) -> Result<()> {
        self.write(move |tx| {
            tx.execute("DELETE FROM status_hourly WHERE hour<?", [before])?;
            Ok(())
        })
        .await
    }

    /// 该 Key 的领取记录（闲置回收拉黑名单时带上用户资料）。
    pub async fn registration_profiles_for_key(
        &self,
        key_id: i64,
    ) -> Result<Vec<RegistrationProfile>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT discord_id, username, display_name, avatar FROM discord_registrations WHERE key_id=?",
            )?;
            stmt.query_map([key_id], |r| {
                Ok(RegistrationProfile {
                    discord_id: value::text_or(r, 0, "None")?,
                    username: value::text_opt(r, 1)?.unwrap_or_default(),
                    display_name: value::text_opt(r, 2)?.unwrap_or_default(),
                    avatar: value::text_opt(r, 3)?.unwrap_or_default(),
                })
            })?
            .collect()
        })
        .await
    }

    /// 关停时把 WAL 并回主库（`PRAGMA wal_checkpoint(TRUNCATE)`）。返回 `(被占用?, WAL 页数, 已并回页数)`。
    pub async fn wal_checkpoint(&self) -> Result<(i64, i64, i64)> {
        self.write_raw(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
        })
        .await
    }

    // ------------------------------------------------------------------ 对拍测试支撑

    /// 所有非空表的整表内容（按 rowid），`{表名: [ {列: 值} ]}`。BLOB 写成 `{"$b": 十六进制}`。
    pub async fn dump_tables(&self) -> Result<Value> {
        self.read(|c| {
            let names: Vec<String> = c
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut out = Map::new();
            for name in names {
                let mut stmt = c.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))?;
                let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
                let rows: Vec<Value> = stmt
                    .query_map([], |r| {
                        let mut m = Map::new();
                        for (i, col) in cols.iter().enumerate() {
                            m.insert(col.clone(), sql_to_json(r.get_ref(i)?));
                        }
                        Ok(Value::Object(m))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                if !rows.is_empty() {
                    out.insert(name, Value::Array(rows));
                }
            }
            Ok(Value::Object(out))
        })
        .await
    }

    /// 在一个事务里依次执行一批 SQL（对拍夹具的 `seed_sql`）。
    pub async fn run_script(&self, statements: Vec<String>) -> Result<()> {
        self.write(move |tx| {
            for s in &statements {
                tx.execute_batch(s)?;
            }
            Ok(())
        })
        .await
    }
}
