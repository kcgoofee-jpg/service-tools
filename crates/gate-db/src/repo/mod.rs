//! 旧版 `app/database.py` 里 `class Database` 的逐方法移植。
//!
//! 约定（与 docs/PORTING.md 一致）：
//! - 方法名、参数顺序、SQL 语义与旧版一致；旧版里的 `time.time()` 一律改成调用方传入的 `now: f64`，
//!   按日 / 按月的计算用 [`gate_core::clock::Clock`]。
//! - 旧版一个方法里「多条语句 + 一次 commit」= 这里一个 `db.write` 闭包 = 一个事务；
//!   只读方法走只读连接池。`record_success` 的重试循环、`_record_lock` 在单写线程下不改变可见结果，不移植。
//! - 返回「原样的行」（`SELECT *` 之类）用 `serde_json::Value`（[`crate::value::row_json`]，保持列顺序与
//!   INTEGER / REAL 区分，序列化出来与旧版 `dict(row)` 一致）；旧代码自己拼的 dict 用同名字段的结构体。
//! - 旧版返回 `dict[int, ...]` / `dict[str, ...]` 的，这里返回 `Vec<(K, V)>`，顺序就是旧版 dict 的插入顺序。
//!
//! 旧版没有放进 `class Database` 的表（error_events / status_hourly / feedback / discord_registrations /
//! pending_role_removals / upstream_snapshots / key_tags 的读写）是各业务模块直接用 `db._db` 写的 SQL，
//! 不在本次移植范围内。

pub mod admin_actions;
pub mod audit;
pub mod counters;
pub mod coupons;
pub mod discord;
pub mod errors;
pub mod feedback;
pub mod guard;
pub mod keys;
pub mod misc;
pub mod reconciliation;
pub mod settings;
pub mod share_state;
pub mod upstream;
pub mod usage;

use rusqlite::types::Value as SqlValue;

/// 站长手动发的邀请 Key：只由站长手动删除，闲置回收跳过（10-11 站长）。
pub const INVITE_TAG: &str = "邀请";

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_TEST: &str = "test";
pub const ROLE_INVITE: &str = "invite";
pub const ROLE_REGULAR: &str = "regular";
/// 身份组 → 中文标签（顺序与旧版 `ROLE_LABELS` 一致）。
pub const ROLE_LABELS: [(&str, &str); 4] = [
    (ROLE_ADMIN, "站长"),
    (ROLE_TEST, "测试"),
    (ROLE_INVITE, "邀请"),
    (ROLE_REGULAR, "普通"),
];

/// 统计成员用量 / 上游表现时排除测试 Key 的日志。删除 Key 时触发器会把 is_test 记进墓碑表：
/// 否则「删掉测试 Key」会让它历史的日志 / 计数重新变成「成员用量」。
pub const NOT_TEST: &str = "COALESCE(key_id, 0) NOT IN (SELECT id FROM api_keys WHERE is_test=1) \
AND COALESCE(key_id, 0) NOT IN (SELECT key_id FROM deleted_key_usage_flags WHERE is_test=1)";
/// 原图在 BLOB（老记录）或文件里。
pub const HAS_IMAGE: &str = "(image IS NOT NULL OR image_file IS NOT NULL)";
pub const IMAGE_SIZE: &str = "COALESCE(image_size, LENGTH(image), 0)";

/// 成功记账写 usage_log（不带 reason 列，与旧版 `_INSERT_LOG` 一致）。
pub(crate) const INSERT_LOG: &str = "INSERT INTO usage_log (ts, key_id, key_name, kind, model, status, \
images, anlas, tokens, detail, unconfirmed_anlas, wait_ms, dur_ms, client, up_status, rid, src, ver) \
VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";
/// 拒绝 / 失败日志（带 reason 列，旧版 `_INSERT_REJECT`）。
pub(crate) const INSERT_REJECT: &str = "INSERT INTO usage_log (ts, key_id, key_name, kind, model, status, \
images, anlas, tokens, detail, unconfirmed_anlas, wait_ms, dur_ms, client, up_status, rid, src, ver, reason) \
VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";
/// 计数器累加（旧版 `_UPSERT_COUNTERS`，列顺序：key_id, day, images, anlas, text_tokens, requests, v5, legacy_free_images）。
pub(crate) const UPSERT_COUNTERS: &str = "INSERT INTO counters \
(key_id, day, images, anlas, text_tokens, requests, v5, legacy_free_images) \
VALUES (?,?,?,?,?,?,?,?) \
ON CONFLICT(key_id, day) DO UPDATE SET \
images = images + excluded.images, \
anlas = anlas + excluded.anlas, \
text_tokens = text_tokens + excluded.text_tokens, \
requests = requests + excluded.requests, \
v5 = v5 + excluded.v5, \
legacy_free_images = legacy_free_images + excluded.legacy_free_images";

/// 旧版返回 `dict[K, V]` 的方法：保持 dict 的插入顺序。
pub type PyMap<K, V> = Vec<(K, V)>;

/// `?,?,?` 占位符串（旧版 `",".join("?" * n)`）。
pub(crate) fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// JSON 值 → SQLite 绑定值（Python sqlite3 绑定参数的规则：bool 当整数；list / dict 不支持）。
pub(crate) fn json_to_sql(v: &serde_json::Value) -> crate::Result<SqlValue> {
    use serde_json::Value;
    Ok(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) if n.is_f64() => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => SqlValue::Integer(
            n.as_i64()
                .ok_or_else(|| crate::DbError::Invalid(format!("整数超出 SQLite 范围：{n}")))?,
        ),
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(crate::DbError::Invalid(
                "不支持的绑定参数类型（list / dict）".into(),
            ));
        }
    })
}
