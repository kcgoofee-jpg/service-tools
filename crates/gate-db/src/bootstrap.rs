//! 启动时的建表与轻量迁移：逐条复刻旧版 `Database.connect()`（app/database.py）与
//! `status_page.ensure_schema()`，保证三件事：
//! 1. 全新库建出来的 `sqlite_master` 与 Python 建的逐字一致（tests/schema_parity.rs 对拍）；
//! 2. 对 Python 已经迁移过的线上库是空操作（只有幂等的 UPDATE / 重建触发器）；
//! 3. Rust 写过的库，Python 再启动也能照常打开——切回旧版不需要任何手工处理。
//!
//! 表结构只增不减：已删除功能的死列 / 死表在 3.0 上线稳定后再单独迁移删除。

use rusqlite::{Connection, ErrorCode};

const SCHEMA: &str = include_str!("sql/schema.sql");
const STATUS_SCHEMA: &str = include_str!("sql/status_schema.sql");

/// 老库补列（新库建表已含这些列，执行会报 duplicate column，忽略即可）。顺序与旧版一致。
const ADD_COLUMNS: &[&str] = &[
    "ALTER TABLE api_keys ADD COLUMN daily_anlas REAL NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN daily_v5 INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE counters ADD COLUMN v5 INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN exclude_global_v5 INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN image_model_scope TEXT NOT NULL DEFAULT 'legacy'",
    "ALTER TABLE api_keys ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN features TEXT",
    "ALTER TABLE discord_registrations ADD COLUMN role_granted INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE discord_registrations ADD COLUMN username TEXT",
    "ALTER TABLE discord_registrations ADD COLUMN display_name TEXT",
    "ALTER TABLE discord_registrations ADD COLUMN avatar TEXT",
    "ALTER TABLE upstream_token_counters ADD COLUMN images INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE usage_log ADD COLUMN wait_ms INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE usage_log ADD COLUMN dur_ms INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE usage_log ADD COLUMN client TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE usage_log ADD COLUMN up_status INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN anlas_auto INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE usage_log ADD COLUMN rid TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE api_keys ADD COLUMN quota_auto INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE usage_log ADD COLUMN src TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE usage_log ADD COLUMN ver TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE generation_audit ADD COLUMN extra TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE generation_audit ADD COLUMN image BLOB",
    "ALTER TABLE generation_audit ADD COLUMN image_type TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE generation_audit ADD COLUMN image_file TEXT",
    "ALTER TABLE generation_audit ADD COLUMN image_size INTEGER",
    "ALTER TABLE usage_log ADD COLUMN reason TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE upstream_snapshots ADD COLUMN fresh INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE discord_registrations ADD COLUMN member_checked_at REAL NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN v5_pinned INTEGER",
    "ALTER TABLE upstream_token_counters ADD COLUMN units REAL",
    "ALTER TABLE share_state ADD COLUMN pause_reason TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE discord_bans ADD COLUMN reason TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE discord_bans ADD COLUMN username TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE discord_bans ADD COLUMN display_name TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE discord_bans ADD COLUMN avatar TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE discord_bans ADD COLUMN banned_by TEXT NOT NULL DEFAULT '系统'",
    "ALTER TABLE deleted_key_usage_flags ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE api_keys ADD COLUMN last_image_at REAL",
];

/// 3.0 新增的列（旧版 Python 不知道它们；表结构只增不减，nullable，回滚到 2.x 不受影响）。
/// 只放在这里、不写进 `schema.sql`：全新库与线上库走同一条路（建出旧版的表再补列），
/// `tests/schema_parity.rs` 的「Python 建的表」一侧也是按这份清单补出来的。
pub const RUST_ADDED_COLUMNS: &[&str] = &[
    // Bug 自动处理的原因码（gate_core::errors::AutoReason::code）；站长手动处理 / 未处理为 NULL。
    "ALTER TABLE error_events ADD COLUMN auto_resolved TEXT",
    // Key 的停用原因（gate_core::key_status）；启用中 / 2.x 停用的为 NULL。
    "ALTER TABLE api_keys ADD COLUMN disabled_reason TEXT",
];

/// 3.0 新增的表（旧版 Python 不认识；只增不减，回滚到 2.x 不受影响）。同 `RUST_ADDED_COLUMNS`：
/// 不写进 `schema.sql`，`tests/schema_parity.rs` 的「Python 建的表」一侧也按这份清单补出来。
pub const RUST_ADDED_TABLES: &[&str] = &[
    // 成员用 Discord OAuth 授权后核验到的身份组（领 Key 的门槛只认这张表，不再用机器人 Token 查）。
    "CREATE TABLE IF NOT EXISTS role_verifications (\
     discord_id TEXT NOT NULL, guild_id TEXT NOT NULL, role_id TEXT NOT NULL, verified_at REAL NOT NULL, \
     PRIMARY KEY (discord_id, guild_id, role_id))",
];

/// 触发器每次启动按同一定义重建（旧库里可能还是不写 is_test 的老定义）。
const REBUILD_TRIGGER: &str = "DROP TRIGGER IF EXISTS preserve_deleted_key_usage; \
    CREATE TRIGGER preserve_deleted_key_usage BEFORE DELETE ON api_keys BEGIN \
    INSERT INTO deleted_key_usage_flags(key_id, exclude_global_v5, is_test) \
    VALUES (OLD.id, OLD.exclude_global_v5, OLD.is_test) \
    ON CONFLICT(key_id) DO UPDATE SET is_test=MAX(is_test, OLD.is_test); \
    DELETE FROM daily_quota_offsets WHERE key_id=OLD.id; END;";

/// 已删除功能的表：旧版每次启动都 DROP IF EXISTS，这里同样处理（只删 Python 也会删的）。
const DROPPED_TABLES: &[&str] = &[
    "waitlist",
    "key_sources",
    "share_evidence",
    "req_features",
    "key_idle_reminders",
    "share_confirm",
];

const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_log_ts_upstatus ON usage_log (ts, up_status)",
    "CREATE INDEX IF NOT EXISTS idx_log_key_id ON usage_log (key_id, id DESC)",
    "CREATE INDEX IF NOT EXISTS idx_log_rid ON usage_log (rid)",
    "CREATE INDEX IF NOT EXISTS idx_audit_key_id ON generation_audit (key_id, id DESC)",
    "CREATE INDEX IF NOT EXISTS idx_admin_actions_action ON admin_actions (action)",
    "CREATE INDEX IF NOT EXISTS idx_error_events_resolved_last_seen ON error_events (resolved_at, last_seen DESC)",
    "CREATE INDEX IF NOT EXISTS idx_feedback_discord_id_ts ON feedback (discord_id, ts DESC)",
];

/// 本版本写入 `PRAGMA user_version` 的值。Python 版从不读它，所以写它不影响回滚。
pub const SCHEMA_VERSION: i64 = 1;

/// 连接级设置：与旧版一致（WAL + synchronous=NORMAL + 锁等待 15 秒）。
pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(())
}

fn is_duplicate_column(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, Some(msg))
        if f.code == ErrorCode::Unknown && msg.contains("duplicate column"))
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 建表 + 迁移。`now` 是 Unix 秒（用于「最近 2 天」的原因码回填窗口）；
/// `backfill` 是 (原因码, LIKE 模式) 列表，来自 gate_core::reasons::BACKFILL。
pub fn run(conn: &mut Connection, now: f64, backfill: &[(&str, &str)]) -> rusqlite::Result<()> {
    configure(conn)?;
    conn.execute_batch(SCHEMA)?;
    for ddl in ADD_COLUMNS {
        match conn.execute(ddl, []) {
            Ok(_) => {}
            Err(e) if is_duplicate_column(&e) => {}
            Err(e) => return Err(e),
        }
    }
    conn.execute_batch(REBUILD_TRIGGER)?;
    conn.execute(
        "UPDATE upstream_token_counters SET units=images WHERE units IS NULL",
        [],
    )?;
    if !has_column(conn, "api_keys", "discord_id")? {
        conn.execute("ALTER TABLE api_keys ADD COLUMN discord_id TEXT", [])?;
    }
    // 3.0 的列放在旧版所有列（含上面的 discord_id）之后：全新库和线上库的列顺序一致，
    // 旧版整行插入（`INSERT ... VALUES(...)` 不带列名）的位置也不受影响。
    for ddl in RUST_ADDED_COLUMNS {
        match conn.execute(ddl, []) {
            Ok(_) => {}
            Err(e) if is_duplicate_column(&e) => {}
            Err(e) => return Err(e),
        }
    }
    for ddl in RUST_ADDED_TABLES {
        conn.execute(ddl, [])?;
    }
    conn.execute(
        "UPDATE api_keys SET discord_id=\
         (SELECT r.discord_id FROM discord_registrations r WHERE r.key_id=api_keys.id) \
         WHERE discord_id IS NULL",
        [],
    )?;
    for t in DROPPED_TABLES {
        conn.execute(&format!("DROP TABLE IF EXISTS {t}"), [])?;
    }
    conn.execute(
        "UPDATE api_keys SET last_image_at=\
         (SELECT MAX(u.ts) FROM usage_log u WHERE u.key_id=api_keys.id AND u.status='ok' \
          AND (u.kind LIKE 'image%' OR u.kind IN ('upscale','augment-image','vibe_encode'))) \
         WHERE last_image_at IS NULL",
        [],
    )?;
    conn.execute(
        "UPDATE api_keys SET v5_pinned=NULL WHERE v5_pinned IS NOT NULL",
        [],
    )?;
    {
        let tx = conn.transaction()?;
        for (code, pattern) in backfill {
            tx.execute(
                "UPDATE usage_log SET reason=?1 WHERE reason='' AND status='rejected' AND ts>?2 AND detail LIKE ?3",
                rusqlite::params![code, now - 2.0 * 86400.0, pattern],
            )?;
        }
        tx.commit()?;
    }
    // 两条更老的一次性迁移（unconfirmed_anlas / legacy_free_images）只对 2026-10 之前的库有意义：
    // 当前建表语句已含这两列，线上库也早已迁过。真遇到缺列的古董库就直接报错，不在这里重放历史。
    for (table, col) in [
        ("usage_log", "unconfirmed_anlas"),
        ("counters", "legacy_free_images"),
    ] {
        if !has_column(conn, table, col)? {
            return Err(rusqlite::Error::InvalidColumnName(format!(
                "{table}.{col}：库太旧，请先用 2.x 版启动一次完成迁移"
            )));
        }
    }
    for idx in INDEXES {
        conn.execute(idx, [])?;
    }
    conn.execute_batch(STATUS_SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}
