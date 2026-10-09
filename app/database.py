"""SQLite 持久化：虚拟 key、每日计数器、用量日志。"""

from __future__ import annotations

import asyncio
import json
import math
import time
from typing import Any, Optional
from uuid import uuid4

import aiosqlite

SCHEMA = """
CREATE TABLE IF NOT EXISTS api_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL DEFAULT '',
    token TEXT NOT NULL UNIQUE,
    enabled INTEGER NOT NULL DEFAULT 1,
    daily_images INTEGER NOT NULL DEFAULT 100,
    daily_anlas REAL NOT NULL DEFAULT 0,
    daily_v5 INTEGER NOT NULL DEFAULT 0,
    monthly_anlas REAL NOT NULL DEFAULT 500,
    daily_text_tokens INTEGER NOT NULL DEFAULT 150000,
    rpm INTEGER NOT NULL DEFAULT 10,
    allow_anlas INTEGER NOT NULL DEFAULT 0,
    allow_img2img INTEGER NOT NULL DEFAULT 0,
    exclude_global_v5 INTEGER NOT NULL DEFAULT 0,
    image_model_scope TEXT NOT NULL DEFAULT 'legacy',
    is_admin INTEGER NOT NULL DEFAULT 0,
    is_test INTEGER NOT NULL DEFAULT 0,   -- 测试 Key：不计入成员统计、上游表现，不会被闲置回收
    anlas_auto INTEGER NOT NULL DEFAULT 0,
    quota_auto INTEGER NOT NULL DEFAULT 1,   -- 1 额度由动态额度算法管理（quota_algo.py）；-1 站长手动 -- 1 自动分配管理（只用于 V5 续杯）；0 交给算法；-1 站长手动（关闭或手动额度）
    expires_at REAL,
    created_at REAL NOT NULL,
    last_used_at REAL
);
CREATE TABLE IF NOT EXISTS site_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS anlas_reconciliations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS discord_registrations (
    discord_id TEXT PRIMARY KEY,
    key_id INTEGER NOT NULL UNIQUE,
    created_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS discord_bans (
    discord_id TEXT PRIMARY KEY,
    created_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS pending_role_removals (
    discord_id TEXT PRIMARY KEY,
    created_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS counters (
    key_id INTEGER NOT NULL,
    day TEXT NOT NULL,            -- YYYY-MM-DD (按配置时区)
    images INTEGER NOT NULL DEFAULT 0,
    legacy_free_images INTEGER NOT NULL DEFAULT 0,
    anlas REAL NOT NULL DEFAULT 0,
    text_tokens INTEGER NOT NULL DEFAULT 0,
    requests INTEGER NOT NULL DEFAULT 0,
    v5 INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, day)
);
CREATE TABLE IF NOT EXISTS generation_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    key_id INTEGER,
    key_name TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'image',
    model TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    prompt TEXT NOT NULL DEFAULT '',
    negative TEXT NOT NULL DEFAULT '',
    thumb BLOB
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON generation_audit(ts);
CREATE INDEX IF NOT EXISTS idx_audit_key ON generation_audit(key_id, ts);
CREATE TABLE IF NOT EXISTS usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    key_id INTEGER,
    key_name TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,           -- image / text / voice / tags / chat
    model TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,         -- ok / error / rejected
    images INTEGER NOT NULL DEFAULT 0,
    anlas REAL NOT NULL DEFAULT 0,
    tokens INTEGER NOT NULL DEFAULT 0,
    unconfirmed_anlas REAL NOT NULL DEFAULT 0,
    detail TEXT NOT NULL DEFAULT '',
    wait_ms INTEGER NOT NULL DEFAULT 0,   -- 从收到请求到发往上游（排队 + 冷却）
    dur_ms INTEGER NOT NULL DEFAULT 0,    -- 上游处理耗时；没发到上游为 0
    client TEXT NOT NULL DEFAULT '',      -- User-Agent 摘要，客户端自报，仅供参考
    up_status INTEGER NOT NULL DEFAULT 0, -- 上游最后返回的 HTTP 状态码；没收到响应为 0
    rid TEXT NOT NULL DEFAULT '',         -- 请求编号（响应头 X-Request-Id），成员报错时据此定位
    src TEXT NOT NULL DEFAULT ''          -- 来源网络打码标签（如 120.235.*.*），防分享溯源用，不存完整 IP
);
CREATE INDEX IF NOT EXISTS idx_log_ts ON usage_log (ts DESC);
CREATE INDEX IF NOT EXISTS idx_log_key ON usage_log (key_id, ts DESC);
CREATE TABLE IF NOT EXISTS upstream_token_counters (
    token_id TEXT NOT NULL,
    day TEXT NOT NULL,            -- YYYY-MM-DD (按配置时区)
    images INTEGER NOT NULL DEFAULT 0,
    v5 INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token_id, day)
);
CREATE TABLE IF NOT EXISTS share_state (       -- 防分享风险分（share_guard.py）
    key_id INTEGER PRIMARY KEY,
    score REAL NOT NULL DEFAULT 0,
    score_ts REAL NOT NULL DEFAULT 0,
    strikes INTEGER NOT NULL DEFAULT 0,
    paused_until REAL NOT NULL DEFAULT 0,
    warned_ts REAL NOT NULL DEFAULT 0,
    paused_ts REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS req_features (      -- 出图请求的特征（只存哈希），给防分享回测用；30 天后删除
    ts REAL NOT NULL,
    key_id INTEGER NOT NULL,
    src TEXT NOT NULL DEFAULT '',          -- 来源网络打码标签
    fp TEXT NOT NULL DEFAULT '',           -- 客户端指纹（User-Agent）的哈希
    os TEXT NOT NULL DEFAULT '',
    sig TEXT NOT NULL DEFAULT '',          -- 参数签名哈希（采样器 / 步数 / CFG / 负面词 …）
    toks TEXT NOT NULL DEFAULT '',         -- 提示词词条哈希（空格分隔，最多 80 个），不可还原
    busy INTEGER NOT NULL DEFAULT 0        -- 当时这把 Key 是否还有图在生成
);
CREATE INDEX IF NOT EXISTS idx_req_features ON req_features (key_id, ts);
CREATE TABLE IF NOT EXISTS share_evidence (    -- 防分享证据与处罚记录，后台溯源用
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    key_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    points REAL NOT NULL DEFAULT 0,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_share_evidence_key ON share_evidence (key_id, ts DESC);
CREATE TABLE IF NOT EXISTS error_events (
    sig TEXT PRIMARY KEY,          -- 错误特征：异常类型 + 出错位置（或来源 + 去掉数字的消息）
    source TEXT NOT NULL,          -- request / upstream / maintenance:xxx / web:landing / disconnect ...
    level TEXT NOT NULL DEFAULT 'error',
    title TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',   -- 最近一次的堆栈 / 上下文（只给站长看）
    count INTEGER NOT NULL DEFAULT 0,
    first_seen REAL NOT NULL,
    last_seen REAL NOT NULL,
    last_rid TEXT NOT NULL DEFAULT '',
    last_key INTEGER,
    last_path TEXT NOT NULL DEFAULT '',
    resolved_at REAL               -- 站长标记已处理；之后再出现视为「复发」
);
CREATE TABLE IF NOT EXISTS waitlist (
    discord_id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    joined_at REAL NOT NULL,
    invited_at REAL                -- 有名额时发出邀请的时间；24 小时内未领取则让给下一位
);
CREATE TABLE IF NOT EXISTS key_idle_reminders (
    key_id INTEGER PRIMARY KEY,
    activity REAL NOT NULL,       -- 提醒时 Key 的最后活动时间；之后再有活动会重新计时并允许再次提醒
    sent_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS admin_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    actor TEXT NOT NULL,          -- 后台(打码 IP) / Discord:<id> / 系统
    action TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    ok INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_admin_actions_ts ON admin_actions (ts DESC);
CREATE TABLE IF NOT EXISTS key_sources (
    key_id INTEGER NOT NULL,
    net_hash TEXT NOT NULL,       -- 加盐哈希后的来源网段（IPv4 /24、IPv6 /48），不存完整 IP
    label TEXT NOT NULL DEFAULT '',
    first_seen REAL NOT NULL,
    last_seen REAL NOT NULL,
    hits INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, net_hash)
);
CREATE INDEX IF NOT EXISTS idx_key_sources_seen ON key_sources (last_seen);
CREATE TABLE IF NOT EXISTS upstream_token_settings (
    token_id TEXT PRIMARY KEY,
    v5_daily_limit INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS upstream_token_enabled (
    token_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS upstream_token_image_concurrency (
    token_id TEXT PRIMARY KEY,
    concurrency INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS daily_quota_offsets (
    key_id INTEGER NOT NULL,
    day TEXT NOT NULL,
    anlas REAL NOT NULL DEFAULT 0,
    v5 INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, day)
);
CREATE TABLE IF NOT EXISTS deleted_key_usage_flags (
    key_id INTEGER PRIMARY KEY,
    exclude_global_v5 INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS preserve_deleted_key_usage
BEFORE DELETE ON api_keys
BEGIN
    INSERT INTO deleted_key_usage_flags(key_id, exclude_global_v5)
        VALUES (OLD.id, OLD.exclude_global_v5)
        ON CONFLICT(key_id) DO NOTHING;
    DELETE FROM daily_quota_offsets WHERE key_id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS prevent_deleted_key_quota_reset
BEFORE INSERT ON daily_quota_offsets
WHEN EXISTS (SELECT 1 FROM deleted_key_usage_flags WHERE key_id=NEW.key_id)
BEGIN
    SELECT RAISE(IGNORE);
END;
"""

# 统计成员用量 / 上游表现时排除测试 Key 的日志。删除测试 Key 前先删它的日志，否则这些日志会重新算进成员统计。
NOT_TEST = "COALESCE(key_id, 0) NOT IN (SELECT id FROM api_keys WHERE is_test=1)"

_INSERT_LOG = """INSERT INTO usage_log (ts, key_id, key_name, kind, model, status,
                                      images, anlas, tokens, detail, unconfirmed_anlas,
                                      wait_ms, dur_ms, client, up_status, rid, src)
                VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"""
_UPSERT_COUNTERS = """INSERT INTO counters
                     (key_id, day, images, anlas, text_tokens, requests, v5, legacy_free_images)
                     VALUES (?,?,?,?,?,?,?,?)
                     ON CONFLICT(key_id, day) DO UPDATE SET
                       images = images + excluded.images,
                       anlas = anlas + excluded.anlas,
                       text_tokens = text_tokens + excluded.text_tokens,
                       requests = requests + excluded.requests,
                       v5 = v5 + excluded.v5,
                       legacy_free_images = legacy_free_images + excluded.legacy_free_images"""


class Database:
    def __init__(self, path: str, tz: str = "Asia/Shanghai"):
        self.path = path
        self.tz = tz
        self._db: Optional[aiosqlite.Connection] = None
        self._record_lock = asyncio.Lock()
        # 独立事务也需访问同一个内存库；主连接关闭前保留该库。
        self._connection_path = (f"file:gate-{uuid4().hex}?mode=memory&cache=shared"
                                 if path == ":memory:" else path)

    def _open_connection(self):
        return aiosqlite.connect(self._connection_path, uri=self.path == ":memory:")

    async def connect(self) -> None:
        self._db = await self._open_connection()
        self._db.row_factory = aiosqlite.Row
        await self._db.execute("PRAGMA journal_mode=WAL")
        await self._db.executescript(SCHEMA)
        # 轻量迁移：老库补列（新库建表已含该列，会抛 duplicate column，忽略即可）
        for ddl in (
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
        ):
            try:
                await self._db.execute(ddl)
                await self._db.commit()
            except aiosqlite.OperationalError:
                pass  # 列已存在
        log_columns = await (await self._db.execute("PRAGMA table_info(usage_log)")).fetchall()
        if "unconfirmed_anlas" not in {row["name"] for row in log_columns}:
            # 旧日志缺少报价，待核对金额初始为 0。
            await self._db.execute(
                "ALTER TABLE usage_log ADD COLUMN unconfirmed_anlas REAL NOT NULL DEFAULT 0"
            )
            await self._db.commit()
        columns = await (await self._db.execute("PRAGMA table_info(counters)")).fetchall()
        if "legacy_free_images" not in {row["name"] for row in columns}:
            # One-time migration: preserve today's usage rather than granting a fresh
            # 100 images when the service is upgraded in the middle of a day.
            from datetime import datetime
            from zoneinfo import ZoneInfo

            await self._db.execute(
                "ALTER TABLE counters ADD COLUMN legacy_free_images INTEGER NOT NULL DEFAULT 0"
            )
            await self._db.execute(
                "UPDATE api_keys SET daily_images=100 WHERE daily_images=0 AND is_admin=0"
            )
            cur = await self._db.execute(
                """SELECT key_id, ts, images FROM usage_log
                   WHERE kind IN ('image', 'image_stream') AND status='ok'
                     AND anlas=0 AND images>0
                     AND model NOT LIKE 'nai-diffusion-5%'
                     AND model NOT LIKE 'nai-v5%'"""
            )
            counts: dict[tuple[int, str], int] = {}
            timezone = ZoneInfo(self.tz)
            for row in await cur.fetchall():
                day = datetime.fromtimestamp(row["ts"], timezone).strftime("%Y-%m-%d")
                identity = (row["key_id"], day)
                counts[identity] = counts.get(identity, 0) + int(row["images"])
            for (key_id, day), images in counts.items():
                await self._db.execute(
                    "UPDATE counters SET legacy_free_images=? WHERE key_id=? AND day=?",
                    (images, key_id, day),
                )
            await self._db.commit()

    async def close(self) -> None:
        if self._db:
            await self._db.close()

    async def reconciliation_totals(self) -> dict:
        # Lifetime totals include deleted Keys and survive daily quota resets.
        row = await (await self._db.execute("""
            SELECT (SELECT COALESCE(SUM(anlas), 0) FROM counters) AS anlas,
                   COALESCE(SUM(unconfirmed_anlas), 0) AS unconfirmed_anlas,
                   COALESCE(SUM(unconfirmed_anlas > 0), 0) AS unconfirmed_requests,
                   COALESCE(MAX(id), 0) AS last_log_id FROM usage_log
        """)).fetchone()
        return dict(row)

    async def save_reconciliation(self, snapshot: dict) -> None:
        # Isolate snapshot commits and rollbacks from concurrent ledger writes.
        async with self._open_connection() as db:
            await db.execute("INSERT INTO anlas_reconciliations(snapshot) VALUES (?)",
                             (json.dumps(snapshot, ensure_ascii=False, allow_nan=False),))
            await db.commit()

    async def reconciliation_history(self, limit: int = 20) -> list[dict]:
        rows = await (await self._db.execute(
            "SELECT id, snapshot FROM anlas_reconciliations ORDER BY id DESC LIMIT ?",
            (min(20, max(1, limit)),))).fetchall()
        return [{**json.loads(row["snapshot"]), "id": row["id"]} for row in rows]

    # ---------- upstream token counters ----------
    async def get_upstream_counter(self, token_id: str, day: str) -> dict[str, int]:
        cur = await self._db.execute(
            "SELECT images, v5 FROM upstream_token_counters WHERE token_id=? AND day=?",
            (token_id, day),
        )
        row = await cur.fetchone()
        if not row:
            return {"images": 0, "v5": 0}
        return {"images": int(row["images"]), "v5": int(row["v5"])}

    async def migrate_upstream_token_ids(self, token_ids: list[str]) -> None:
        """Merge old position-based counters into stable hashed token identities."""
        for token_id in set(token_ids):
            suffix = token_id.removeprefix("token-")
            rows = await (await self._db.execute(
                "SELECT token_id, day, images, v5 FROM upstream_token_counters WHERE token_id LIKE ?",
                (f"token-%-{suffix}",),
            )).fetchall()
            for row in rows:
                await self._db.execute(
                    """INSERT INTO upstream_token_counters(token_id, day, images, v5)
                       VALUES(?,?,?,?) ON CONFLICT(token_id,day) DO UPDATE SET
                       images=images+excluded.images, v5=v5+excluded.v5""",
                    (token_id, row["day"], row["images"], row["v5"]),
                )
                await self._db.execute(
                    "DELETE FROM upstream_token_counters WHERE token_id=? AND day=?",
                    (row["token_id"], row["day"]),
                )
        await self._db.commit()

    async def move_upstream_token(self, old_id: str, new_id: str) -> None:
        """令牌被替换（同一账号重置了 Token）：把设置和计数从旧标识迁移到新标识。"""
        if old_id == new_id:
            return
        for table in ("upstream_token_settings", "upstream_token_enabled", "upstream_token_image_concurrency",
                      "upstream_token_counters"):
            await self._db.execute(f"DELETE FROM {table} WHERE token_id=?", (new_id,))
            await self._db.execute(f"UPDATE {table} SET token_id=? WHERE token_id=?", (new_id, old_id))
        await self._db.commit()

    async def get_upstream_token_limits(self) -> dict[str, int]:
        rows = await (await self._db.execute(
            "SELECT token_id, v5_daily_limit FROM upstream_token_settings"
        )).fetchall()
        return {row["token_id"]: int(row["v5_daily_limit"]) for row in rows}

    async def set_upstream_token_limit(self, token_id: str, limit: int) -> None:
        await self._db.execute(
            """INSERT INTO upstream_token_settings(token_id, v5_daily_limit) VALUES(?,?)
               ON CONFLICT(token_id) DO UPDATE SET v5_daily_limit=excluded.v5_daily_limit""",
            (token_id, limit),
        )
        await self._db.commit()

    async def get_upstream_token_enabled(self) -> dict[str, bool]:
        rows = await (await self._db.execute(
            "SELECT token_id, enabled FROM upstream_token_enabled"
        )).fetchall()
        return {row["token_id"]: bool(row["enabled"]) for row in rows}

    async def set_upstream_token_enabled(self, token_id: str, enabled: bool) -> None:
        await self._db.execute(
            """INSERT INTO upstream_token_enabled(token_id, enabled) VALUES(?,?)
               ON CONFLICT(token_id) DO UPDATE SET enabled=excluded.enabled""",
            (token_id, int(enabled)),
        )
        await self._db.commit()

    async def get_upstream_token_image_concurrency(self) -> dict[str, int]:
        rows = await (await self._db.execute(
            "SELECT token_id, concurrency FROM upstream_token_image_concurrency"
        )).fetchall()
        return {row["token_id"]: int(row["concurrency"]) for row in rows}

    async def set_upstream_token_image_concurrency(self, token_id: str, limit: int) -> None:
        await self._db.execute(
            """INSERT INTO upstream_token_image_concurrency(token_id, concurrency) VALUES(?,?)
               ON CONFLICT(token_id) DO UPDATE SET concurrency=excluded.concurrency""",
            (token_id, limit),
        )
        await self._db.commit()

    async def get_upstream_v5_counter(self, token_id: str, day: str) -> int:
        return (await self.get_upstream_counter(token_id, day))["v5"]

    async def bump_upstream_image_counter(self, token_id: str, day: str,
                                           images: int) -> None:
        if images < 1:
            return
        await self._db.execute(
            """INSERT INTO upstream_token_counters(token_id, day, images) VALUES (?,?,?)
               ON CONFLICT(token_id, day) DO UPDATE SET images=images+excluded.images""",
            (token_id, day, images),
        )
        await self._db.commit()

    async def bump_upstream_v5_counter(self, token_id: str, day: str) -> None:
        await self._db.execute(
            """INSERT INTO upstream_token_counters(token_id, day, v5) VALUES (?,?,1)
               ON CONFLICT(token_id, day) DO UPDATE SET v5=v5+1""",
            (token_id, day),
        )
        await self._db.commit()

    # ---------- keys ----------
    async def create_key(self, fields: dict[str, Any]) -> aiosqlite.Row:
        now = time.time()
        cur = await self._db.execute(
            """INSERT INTO api_keys
               (name, token, enabled, daily_images, daily_anlas, daily_v5, monthly_anlas,
               daily_text_tokens, rpm, allow_anlas, allow_img2img, exclude_global_v5, image_model_scope, is_admin, expires_at, created_at, features,
               is_test)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
            (
                fields["name"],
                fields["token"],
                1 if fields.get("enabled", True) else 0,
                int(fields["daily_images"]),
                float(fields.get("daily_anlas", 0)),
                int(fields.get("daily_v5", 0)),
                float(fields["monthly_anlas"]),
                int(fields["daily_text_tokens"]),
                int(fields["rpm"]),
                1 if fields.get("allow_anlas") else 0,
                1 if fields.get("allow_img2img") else 0,
                1 if fields.get("exclude_global_v5") else 0,
                fields.get("image_model_scope", "legacy"),
                1 if fields.get("is_admin") else 0,
                fields.get("expires_at"),
                now,
                fields.get("features"),
                1 if fields.get("is_test") else 0,
            ),
        )
        await self._db.commit()
        return await self.get_key(cur.lastrowid)

    async def get_key(self, key_id: int) -> Optional[aiosqlite.Row]:
        cur = await self._db.execute("SELECT * FROM api_keys WHERE id=?", (key_id,))
        return await cur.fetchone()

    async def get_key_by_token(self, token: str) -> Optional[aiosqlite.Row]:
        cur = await self._db.execute("SELECT * FROM api_keys WHERE token=?", (token,))
        return await cur.fetchone()

    async def rotate_key_token(self, key_id: int, token: str) -> bool:
        # Keep the ID so admitted work, usage and rate limits retain ownership.
        cur = await self._db.execute(
            "UPDATE api_keys SET token=? WHERE id=?", (token, key_id)
        )
        await self._db.commit()
        return cur.rowcount == 1

    async def update_key(self, key_id: int, fields: dict[str, Any]) -> None:
        allowed = {
            "name", "enabled", "daily_images", "daily_anlas", "daily_v5", "monthly_anlas",
            "daily_text_tokens", "rpm", "allow_anlas", "allow_img2img", "exclude_global_v5", "image_model_scope", "expires_at", "features",
            "is_test",
        }
        sets, vals = [], []
        for k, v in fields.items():
            if k not in allowed:
                continue
            sets.append(f"{k}=?")
            if k in ("enabled", "allow_anlas", "allow_img2img", "exclude_global_v5", "is_test"):
                v = 1 if v else 0
            vals.append(v)
        if not sets:
            return
        vals.append(key_id)
        await self._db.execute(f"UPDATE api_keys SET {', '.join(sets)} WHERE id=?", vals)
        await self._db.commit()

    async def delete_key(self, key_id: int) -> None:
        # The trigger archives the V5 flag and removes offsets atomically.
        # Counters/logs also accept late settlement from already admitted work.
        await self._db.execute("DELETE FROM key_sources WHERE key_id=?", (key_id,))
        await self._db.execute("DELETE FROM key_idle_reminders WHERE key_id=?", (key_id,))
        await self._db.execute("DELETE FROM api_keys WHERE id=?", (key_id,))
        await self._db.commit()

    async def inactive_key_ids(self, cutoff: float) -> list[int]:
        """Return inactive keys without falsifying their last-used timestamps."""
        raw_grace = await self.get_setting("key_inactivity_grace_started_at", 0)
        try:
            grace_started_at = float(raw_grace)
        except (TypeError, ValueError):
            return []  # Malformed reset marker must never trigger mass deletion.
        if not math.isfinite(grace_started_at) or grace_started_at < 0:
            return []
        cur = await self._db.execute(
            """SELECT id FROM api_keys
               WHERE is_admin=0 AND is_test=0 AND enabled=1 AND MAX(COALESCE(last_used_at, created_at), ?) < ?
               ORDER BY id ASC""",
            (grace_started_at, cutoff),
        )
        return [int(row["id"]) for row in await cur.fetchall()]

    async def keys_due_for_idle_reminder(self, remind_before: float) -> list[dict[str, Any]]:
        """闲置回收前的提醒对象：Discord 自助领取、仍启用、最后活动早于 remind_before、本轮尚未提醒。"""
        raw_grace = await self.get_setting("key_inactivity_grace_started_at", 0)
        try:
            grace = float(raw_grace)
        except (TypeError, ValueError):
            return []
        if not math.isfinite(grace) or grace < 0:
            return []
        cur = await self._db.execute(
            """SELECT k.id AS key_id, k.name AS name, r.discord_id AS discord_id,
                      MAX(COALESCE(k.last_used_at, k.created_at), ?) AS activity,
                      EXISTS (SELECT 1 FROM usage_log u WHERE u.key_id=k.id AND u.status='ok') AS ever_used
               FROM api_keys k JOIN discord_registrations r ON r.key_id=k.id
               LEFT JOIN key_idle_reminders m ON m.key_id=k.id
               WHERE k.is_admin=0 AND k.enabled=1
                 AND MAX(COALESCE(k.last_used_at, k.created_at), ?) < ?
                 AND (m.key_id IS NULL OR m.activity <> MAX(COALESCE(k.last_used_at, k.created_at), ?))""",
            (grace, grace, remind_before, grace))
        return [dict(r) for r in await cur.fetchall()]

    async def mark_idle_reminded(self, key_id: int, activity: float) -> None:
        await self._db.execute(
            "INSERT OR REPLACE INTO key_idle_reminders (key_id, activity, sent_at) VALUES (?,?,?)",
            (key_id, activity, time.time()))
        await self._db.commit()

    async def list_keys(self) -> list[aiosqlite.Row]:
        cur = await self._db.execute("SELECT * FROM api_keys ORDER BY id DESC")
        return list(await cur.fetchall())

    async def touch_key(self, key_id: int) -> None:
        await self._db.execute(
            "UPDATE api_keys SET last_used_at=? WHERE id=?", (time.time(), key_id)
        )
        await self._db.commit()

    # ---------- counters ----------
    async def bump_counters(
        self, key_id: int, day: str,
        images: int = 0, anlas: float = 0.0, text_tokens: int = 0, requests: int = 1,
        v5: int = 0, legacy_free_images: int = 0,
    ) -> None:
        await self._db.execute(
            _UPSERT_COUNTERS,
            (key_id, day, images, anlas, text_tokens, requests, v5, legacy_free_images),
        )
        await self._db.commit()

    async def get_counter(self, key_id: int, day: str) -> dict[str, Any]:
        cur = await self._db.execute(
            """SELECT c.*, COALESCE(o.anlas, 0) AS _quota_offset_anlas,
                      COALESCE(o.v5, 0) AS _quota_offset_v5
               FROM counters AS c
               LEFT JOIN daily_quota_offsets AS o ON o.key_id=c.key_id AND o.day=c.day
               WHERE c.key_id=? AND c.day=?""", (key_id, day)
        )
        row = await cur.fetchone()
        if row:
            result = dict(row)
            result["anlas"] = max(0, result["anlas"] - result.pop("_quota_offset_anlas"))
            result["v5"] = max(0, result["v5"] - result.pop("_quota_offset_v5"))
            return result
        return {"images": 0, "legacy_free_images": 0, "anlas": 0.0, "text_tokens": 0, "requests": 0, "v5": 0}

    async def reset_daily_image_quota(self, key_id: int, day: str) -> None:
        """重置单个 Key 当日的 V5 与 Anlas 可用额度基线。

        保留原始记账用量，以重置时的累计量更新基线。
        """
        await self._db.execute(
            """INSERT INTO daily_quota_offsets (key_id, day, anlas, v5)
               SELECT key_id, day, anlas, v5 FROM counters WHERE key_id=? AND day=?
               ON CONFLICT(key_id, day) DO UPDATE SET anlas=excluded.anlas, v5=excluded.v5""",
            (key_id, day),
        )
        await self._db.commit()

    async def month_anlas(self, key_id: int, month: str) -> float:
        cur = await self._db.execute(
            "SELECT COALESCE(SUM(anlas),0) AS a FROM counters WHERE key_id=? AND substr(day,1,7)=?",
            (key_id, month),
        )
        row = await cur.fetchone()
        return float(row["a"] or 0)

    async def day_v5_total(self, day: str) -> int:
        """全站当日 V5 消耗，不包含明确配置为独立额度的 Key。"""
        cur = await self._db.execute(
            """SELECT COALESCE(SUM(c.v5),0) AS c
               FROM counters AS c
               LEFT JOIN api_keys AS k ON k.id = c.key_id
               LEFT JOIN deleted_key_usage_flags AS d ON d.key_id = c.key_id
               WHERE c.day=? AND COALESCE(k.exclude_global_v5, d.exclude_global_v5, 1)=0""",
            (day,),
        )
        row = await cur.fetchone()
        return int(row["c"] or 0)

    async def month_anlas_all(self, month: str) -> float:
        """全站所有 Key 本月的 Anlas 消耗（用于全站预算总闸）。"""
        cur = await self._db.execute(
            "SELECT COALESCE(SUM(anlas),0) AS a FROM counters WHERE substr(day,1,7)=?",
            (month,),
        )
        row = await cur.fetchone()
        return float(row["a"] or 0)

    # ---------- site settings ----------
    async def get_setting(self, key: str, default: Any = None) -> Any:
        cur = await self._db.execute(
            "SELECT value FROM site_settings WHERE key=?", (key,))
        row = await cur.fetchone()
        return row["value"] if row else default

    async def set_setting(self, key: str, value: Any) -> None:
        await self._db.execute(
            """INSERT INTO site_settings (key, value) VALUES (?,?)
               ON CONFLICT(key) DO UPDATE SET value=excluded.value""",
            (key, str(value)),
        )
        await self._db.commit()

    async def set_settings_bulk(self, values: dict[str, Any]) -> None:
        """Persist a validated group of runtime controls in one transaction."""
        await self._db.executemany(
            """INSERT INTO site_settings (key, value) VALUES (?,?)
               ON CONFLICT(key) DO UPDATE SET value=excluded.value""",
            [(key, str(value)) for key, value in values.items()],
        )
        await self._db.commit()

    # ---------- logs ----------
    async def record_success(
        self, key_id: int, key_name: str, kind: str, model: str, day: str, *,
        images: int = 0, anlas: float = 0.0, tokens: int = 0, v5: int = 0,
        legacy_free_images: int = 0, detail: str = "", unconfirmed_anlas: float = 0.0,
        wait_ms: int = 0, dur_ms: int = 0, client: str = "", up_status: int = 0, rid: str = "", src: str = "",
    ) -> None:
        """成功日志、额度与使用时间一起提交；写入失败时整笔回退。"""
        # 不使用共享连接，避免其他请求的 commit 提前保存半笔记账。
        async with self._record_lock, self._open_connection() as db:
            try:
                await db.execute("BEGIN IMMEDIATE")
                now = time.time()
                await db.execute(_INSERT_LOG, (
                    now, key_id, key_name, kind, model, "ok", images, anlas,
                    tokens, detail[:500], unconfirmed_anlas, max(0, int(wait_ms)),
                    max(0, int(dur_ms)), client[:60], int(up_status), rid[:16], src[:40],
                ))
                await db.execute(_UPSERT_COUNTERS, (
                    key_id, day, images, anlas, tokens, 1, v5, legacy_free_images,
                ))
                await db.execute("UPDATE api_keys SET last_used_at=? WHERE id=?", (now, key_id))
                await db.commit()
            except BaseException:
                await db.rollback()
                raise

    async def forget_registration_for_key(self, key_id: int) -> list[str]:
        """删除该 Key 对应的领取记录，返回被删记录的 Discord ID（用于摘除身份组）。"""
        rows = await self._db.execute_fetchall(
            "SELECT discord_id FROM discord_registrations WHERE key_id=?", (key_id,))
        await self._db.execute("DELETE FROM discord_registrations WHERE key_id=?", (key_id,))
        await self._db.commit()
        return [str(r[0]) for r in rows]

    async def execute_fetchall_compat(self, sql: str, args: tuple = ()) -> list:
        return list(await (await self._db.execute(sql, args)).fetchall())

    async def add_audit(self, key_id, key_name: str, kind: str, model: str, status: str,
                        prompt: str, negative: str, thumb: Optional[bytes]) -> None:
        await self._db.execute(
            """INSERT INTO generation_audit (ts,key_id,key_name,kind,model,status,prompt,negative,thumb)
               VALUES (?,?,?,?,?,?,?,?,?)""",
            (time.time(), key_id, key_name[:80], kind, model[:80], status, prompt, negative, thumb))
        await self._db.commit()

    async def list_audit(self, limit: int, offset: int, key_id: Optional[int] = None) -> tuple[list, int]:
        where, args = ("WHERE key_id=?", (key_id,)) if key_id is not None else ("", ())
        total = (await (await self._db.execute(f"SELECT COUNT(*) FROM generation_audit {where}", args)).fetchone())[0]
        rows = await (await self._db.execute(
            f"""SELECT id,ts,key_id,key_name,kind,model,status,prompt,negative,
                       (thumb IS NOT NULL) AS has_thumb
                FROM generation_audit {where} ORDER BY id DESC LIMIT ? OFFSET ?""",
            (*args, limit, offset))).fetchall()
        return rows, int(total)

    async def audit_thumb(self, audit_id: int) -> Optional[bytes]:
        row = await (await self._db.execute("SELECT thumb FROM generation_audit WHERE id=?", (audit_id,))).fetchone()
        return bytes(row["thumb"]) if row and row["thumb"] is not None else None

    async def touch_key_source(self, key_id: int, net_hash: str, label: str,
                               now: float, window_start: float) -> bool:
        """记录来源网段；返回该网段在窗口内是否为新出现（用于决定是否检查分享告警）。"""
        cur = await self._db.execute(
            "SELECT last_seen FROM key_sources WHERE key_id=? AND net_hash=?", (key_id, net_hash))
        row = await cur.fetchone()
        await self._db.execute(
            """INSERT INTO key_sources (key_id, net_hash, label, first_seen, last_seen, hits)
               VALUES (?,?,?,?,?,1)
               ON CONFLICT(key_id, net_hash) DO UPDATE SET last_seen=excluded.last_seen,
                   label=excluded.label, hits=hits+1""",
            (key_id, net_hash, label, now, now))
        await self._db.commit()
        return row is None or row[0] < window_start

    async def key_source_labels(self, key_id: int, since: float) -> list[str]:
        # 按打码标签（IPv4 /16、IPv6 /32）去重：WARP、手机运营商等同一网络内频繁换 /24 的不算多个来源
        cur = await self._db.execute(
            """SELECT label FROM key_sources WHERE key_id=? AND last_seen>=?
               GROUP BY label ORDER BY MAX(last_seen) DESC""", (key_id, since))
        return [r[0] for r in await cur.fetchall()]

    async def key_source_summary(self, since: float) -> dict[int, list[str]]:
        """每把 Key 在 since 之后出现过的来源网段标签（最近的在前）。"""
        cur = await self._db.execute(
            """SELECT key_id, label FROM key_sources WHERE last_seen>=?
               GROUP BY key_id, label ORDER BY MAX(last_seen) DESC""", (since,))
        out: dict[int, list[str]] = {}
        for key_id, label in await cur.fetchall():
            out.setdefault(int(key_id), []).append(label)
        return out

    async def add_admin_action(self, actor: str, action: str, target: str, detail: str, ok: bool) -> None:
        await self._db.execute(
            "INSERT INTO admin_actions (ts, actor, action, target, detail, ok) VALUES (?,?,?,?,?,?)",
            (time.time(), actor, action, target, detail, 1 if ok else 0))
        await self._db.commit()

    async def list_admin_actions(self, limit: int = 30, offset: int = 0) -> list[dict[str, Any]]:
        cur = await self._db.execute(
            "SELECT * FROM admin_actions ORDER BY id DESC LIMIT ? OFFSET ?",
            (max(1, min(int(limit), 200)), max(0, int(offset))))
        return [dict(r) for r in await cur.fetchall()]

    async def count_admin_actions(self) -> int:
        row = await (await self._db.execute("SELECT COUNT(*) FROM admin_actions")).fetchone()
        return int(row[0])

    async def purge_admin_actions(self, older_than: float) -> int:
        cur = await self._db.execute("DELETE FROM admin_actions WHERE ts<?", (older_than,))
        await self._db.commit()
        return cur.rowcount or 0

    async def purge_key_sources(self, older_than: float) -> int:
        cur = await self._db.execute("DELETE FROM key_sources WHERE last_seen<?", (older_than,))
        await self._db.commit()
        return cur.rowcount or 0

    async def purge_usage_log(self, older_than: float) -> int:
        """只清理明细日志；每日计数器和账本不受影响。"""
        await self._db.execute("DELETE FROM req_features WHERE ts<?", (max(older_than, time.time() - 30 * 86400),))
        await self._db.execute("DELETE FROM share_evidence WHERE ts<?", (older_than,))
        cur = await self._db.execute("DELETE FROM usage_log WHERE ts<?", (older_than,))
        await self._db.commit()
        return cur.rowcount

    async def purge_audit(self, older_than: float) -> int:
        cur = await self._db.execute("DELETE FROM generation_audit WHERE ts<?", (older_than,))
        await self._db.commit()
        return cur.rowcount

    async def member_usage(self, since_day: str) -> dict[int, dict]:
        """每个 Key 近一段时间的用量汇总（来自每日计数器）。"""
        rows = await (await self._db.execute(
            """SELECT key_id, COALESCE(SUM(images),0) AS images, COALESCE(SUM(v5),0) AS v5,
                      COALESCE(SUM(anlas),0) AS anlas, COALESCE(SUM(text_tokens),0) AS text_tokens,
                      COALESCE(SUM(requests),0) AS requests
               FROM counters WHERE day>=? GROUP BY key_id""", (since_day,))).fetchall()
        return {int(r["key_id"]): dict(r) for r in rows}

    async def add_log(
        self, key_id: Optional[int], key_name: str, kind: str, model: str,
        status: str, images: int = 0, anlas: float = 0.0, tokens: int = 0,
        detail: str = "", unconfirmed_anlas: float = 0.0,
        wait_ms: int = 0, dur_ms: int = 0, client: str = "", up_status: int = 0, rid: str = "", src: str = "",
    ) -> None:
        await self._db.execute(
            _INSERT_LOG,
            (time.time(), key_id, key_name[:80], kind, model[:80], status,
             images, anlas, tokens, detail[:500], unconfirmed_anlas,
             max(0, int(wait_ms)), max(0, int(dur_ms)), client[:60], int(up_status), rid[:16], src[:40]),
        )
        await self._db.commit()

    @staticmethod
    def _log_filter(key_id: Optional[int], kinds: Optional[list[str]],
                    hide_test: bool = False, rid: str = "") -> tuple[str, tuple]:
        clauses, args = [], []
        if rid:
            clauses.append("rid=?")
            args.append(rid)
        if hide_test:
            clauses.append(NOT_TEST)
        if key_id:
            clauses.append("key_id=?")
            args.append(key_id)
        if kinds is not None:
            clauses.append("kind IN (%s)" % ",".join("?" * len(kinds)) if kinds else "0")
            args.extend(kinds)
        return (" WHERE " + " AND ".join(clauses)) if clauses else "", tuple(args)

    async def list_logs(self, limit: int = 20, offset: int = 0,
                        key_id: Optional[int] = None,
                        kinds: Optional[list[str]] = None, hide_test: bool = False,
                        rid: str = "") -> list[aiosqlite.Row]:
        limit = max(1, min(int(limit), 500))
        offset = max(0, int(offset))
        where, args = self._log_filter(key_id, kinds, hide_test, rid)
        cur = await self._db.execute(
            f"SELECT * FROM usage_log{where} ORDER BY id DESC LIMIT ? OFFSET ?", args + (limit, offset))
        return list(await cur.fetchall())

    async def count_logs(self, key_id: Optional[int] = None,
                         kinds: Optional[list[str]] = None, hide_test: bool = False,
                         rid: str = "") -> int:
        where, args = self._log_filter(key_id, kinds, hide_test, rid)
        cur = await self._db.execute(f"SELECT COUNT(*) AS c FROM usage_log{where}", args)
        row = await cur.fetchone()
        return int(row["c"])

    async def member_milestones(self, since: float) -> dict[int, dict[str, Any]]:
        """每把 Key：首次 / 最近一次成功出图时间，以及 since 之后被拒次数（用于成员筛选）。
        基于用量日志，超过日志保留期的早期记录不计入。"""
        out: dict[int, dict[str, Any]] = {}
        cur = await self._db.execute(
            """SELECT key_id, MIN(ts), MAX(ts) FROM usage_log
               WHERE status='ok' AND kind IN ('image','image_stream') AND key_id IS NOT NULL GROUP BY key_id""")
        for key_id, first, last in await cur.fetchall():
            out[int(key_id)] = {"first_image_at": first, "last_image_at": last, "rejected_24h": 0}
        cur = await self._db.execute(
            """SELECT key_id, COUNT(*) FROM usage_log WHERE status='rejected' AND ts>=? AND key_id IS NOT NULL
               GROUP BY key_id""", (since,))
        for key_id, n in await cur.fetchall():
            out.setdefault(int(key_id), {"first_image_at": None, "last_image_at": None})["rejected_24h"] = int(n)
        return out

    async def image_perf_rows(self, since: float) -> list[tuple]:
        """出图请求的 (ts, model, status, images, wait_ms, dur_ms, up_status, detail)，供上游表现分析。"""
        cur = await self._db.execute(
            """SELECT ts, model, status, images, wait_ms, dur_ms, up_status, detail FROM usage_log
               WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error')
                 AND """ + NOT_TEST + """
               ORDER BY ts""", (since,))
        return [tuple(r) for r in await cur.fetchall()]

    async def shadow_rows(self, since: float) -> list[tuple]:
        """调度影子模式回放用：出图请求的 (ts, key_id, key_name, status, images, wait_ms, dur_ms)，不含测试 Key。"""
        cur = await self._db.execute(
            """SELECT ts, key_id, key_name, status, images, wait_ms, dur_ms FROM usage_log
               WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error')
                 AND """ + NOT_TEST + """
               ORDER BY ts""", (since,))
        return [tuple(r) for r in await cur.fetchall()]

    async def image_stability(self, since: float) -> dict[str, int]:
        """since 之后出图请求的成功 / 上游失败次数（不含参数错误等被拒请求），用于首页稳定性图标。"""
        cur = await self._db.execute(
            """SELECT status, COUNT(*) FROM usage_log
               WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error')
                 AND """ + NOT_TEST + """
               GROUP BY status""", (since,))
        out = {"ok": 0, "error": 0}
        for status, n in await cur.fetchall():
            out[status] = int(n)
        return out

    async def usage_by_kind(self, since: float) -> list[dict[str, Any]]:
        """按 kind / status 汇总 since 之后的用量日志（日志保留期内有效）。"""
        cur = await self._db.execute(
            """SELECT kind, status, COUNT(*) AS n, COALESCE(SUM(images),0) AS images,
                      COALESCE(SUM(tokens),0) AS tokens, COALESCE(SUM(anlas),0) AS anlas,
                      COUNT(DISTINCT key_id) AS users
               FROM usage_log WHERE ts>=? AND """ + NOT_TEST + """ GROUP BY kind, status""", (since,))
        return [dict(r) for r in await cur.fetchall()]

    async def generated_image_totals(self, key_id: Optional[int] = None) -> dict[int, int]:
        """Count successful generations (including completed stream images) per Key."""
        sql = """SELECT key_id, COALESCE(SUM(images), 0) AS images
                 FROM usage_log
                 WHERE key_id IS NOT NULL AND kind IN ('image', 'image_stream')
                   AND status='ok'"""
        args: tuple = ()
        if key_id is not None:
            sql += " AND key_id=?"
            args = (key_id,)
        sql += " GROUP BY key_id"
        rows = await (await self._db.execute(sql, args)).fetchall()
        return {int(row["key_id"]): int(row["images"]) for row in rows}

    # ---------- overview ----------
    async def overview(self, today: str, week_days: list[str]) -> dict[str, Any]:
        from datetime import datetime, timedelta
        from zoneinfo import ZoneInfo

        async def one(sql: str, args: tuple = ()) -> Any:
            cur = await self._db.execute(sql, args)
            row = await cur.fetchone()
            return row[0] if row else 0

        today_images = await one(
            "SELECT COALESCE(SUM(images),0) FROM counters WHERE day=?", (today,)
        )
        today_anlas = await one(
            "SELECT COALESCE(SUM(anlas),0) FROM counters WHERE day=?", (today,)
        )
        today_tokens = await one(
            "SELECT COALESCE(SUM(text_tokens),0) FROM counters WHERE day=?", (today,)
        )
        today_requests = await one(
            "SELECT COALESCE(SUM(requests),0) FROM counters WHERE day=?", (today,)
        )
        today_v5 = await one(
            """SELECT COALESCE(SUM(c.v5),0)
               FROM counters AS c
               LEFT JOIN api_keys AS k ON k.id = c.key_id
               LEFT JOIN deleted_key_usage_flags AS d ON d.key_id = c.key_id
               WHERE c.day=? AND COALESCE(k.exclude_global_v5, d.exclude_global_v5, 1)=0""",
            (today,),
        )
        keys_total = await one("SELECT COUNT(*) FROM api_keys")
        keys_active = await one(
            "SELECT COUNT(*) FROM api_keys WHERE enabled=1 AND (expires_at IS NULL OR expires_at>?)",
            (time.time(),),
        )
        # 日志按时间戳保存；统计边界必须与配额使用同一时区。
        day_start = datetime.fromisoformat(today).replace(tzinfo=ZoneInfo(self.tz))
        anomaly = {}
        for period, start in (("today", day_start), ("month", day_start.replace(day=1))):
            row = await (await self._db.execute(
                """SELECT COUNT(*), COALESCE(SUM(unconfirmed_anlas),0) FROM usage_log
                   WHERE unconfirmed_anlas>0 AND ts>=? AND ts<?""",
                (start.timestamp(), (day_start + timedelta(days=1)).timestamp()),
            )).fetchone()
            anomaly[period] = {"unconfirmed_requests": int(row[0]),
                               "unconfirmed_anlas": round(float(row[1]), 2)}
        ph = ",".join("?" * len(week_days))
        cur = await self._db.execute(
            f"""SELECT day,
                       SUM(images) AS images, SUM(anlas) AS anlas,
                       SUM(text_tokens) AS text_tokens, SUM(requests) AS requests, SUM(v5) AS v5
                FROM counters WHERE day IN ({ph}) GROUP BY day""",
            tuple(week_days),
        )
        by_day = {r["day"]: dict(r) for r in await cur.fetchall()}
        week = []
        for d in week_days:
            r = by_day.get(d, {})
            week.append({
                "day": d,
                "images": int(r.get("images") or 0),
                "anlas": float(r.get("anlas") or 0),
                "text_tokens": int(r.get("text_tokens") or 0),
                "requests": int(r.get("requests") or 0),
                "v5": int(r.get("v5") or 0),
            })
        return {
            "today": {
                "images": int(today_images),
                # 成员出图：不含测试 Key 和已删除的 Key（例如早先的冒烟测试）；上面的总数是账号真实消耗
                "member_images": int(await one(
                    """SELECT COALESCE(SUM(c.images),0) FROM counters c JOIN api_keys k ON k.id=c.key_id
                       WHERE c.day=? AND k.is_test=0""", (today,))),
                "anlas": round(float(today_anlas), 2),
                "text_tokens": int(today_tokens),
                "requests": int(today_requests),
                "v5": int(today_v5),
                **anomaly["today"],
            },
            "keys_total": int(keys_total),
            "keys_active": int(keys_active),
            "week": week,
            "month": {"anlas": round(float(await one(
                "SELECT COALESCE(SUM(anlas),0) FROM counters WHERE substr(day,1,7)=?",
                (today[:7],))), 2), **anomaly["month"]},
        }
