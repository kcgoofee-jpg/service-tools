//! 脱敏只读快照（移植自 deploy/ops/ro_sanitize.py）：给协作 AI 做只读分析用。
//!
//! 去掉：成员 API Key 原文（换成不可逆短哈希，同一把 Key 仍能对上）、盐 / 密码哈希等密钥类设置、
//! 请求词条哈希、报错堆栈、提示词 / 负面词 / 附加参数原文（成员创作内容）、缩略图、头像。
//! 保留：用量、请求特征哈希、网段、Discord ID 等分析需要的字段。原地改写，最后 VACUUM
//! （不 VACUUM 的话，被清掉的内容还留在空闲页里，用 strings 就能翻出来）。
//!
//! 相对旧版额外清掉的（旧版在这些表出现之前写的，漏了）：
//! - `key_donations.token`：捐赠者提交的 NovelAI Token 原文，同样换成短哈希；`contact` / `message` 清空；
//! - `discord_bans.avatar`：头像哈希。

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// `"sha256:" + sha256(str(tok)).hexdigest()[:16]`
pub fn short_hash(token: &str) -> String {
    format!(
        "sha256:{}",
        &hex::encode(Sha256::digest(token.as_bytes()))[..16]
    )
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!(
        "PRAGMA table_info(\"{}\")",
        table.replace('"', "\"\"")
    ))?;
    stmt.query_map([], |r| r.get::<_, String>(1))?.collect()
}

/// 在 `path` 上原地脱敏，返回处理的 Key 数。任何一步失败都整体回滚、返回错误（调用方不要发布这份文件）。
pub fn sanitize(path: &Path) -> Result<usize> {
    let mut conn = Connection::open(path).with_context(|| format!("打不开 {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(15))?;
    let tx = conn.transaction()?;

    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare("SELECT id, token FROM api_keys")?;
        stmt.query_map([], |r| {
            let v: rusqlite::types::Value = r.get(1)?;
            Ok((r.get(0)?, crate::util::sv_str(&v)))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (id, tok) in &rows {
        tx.execute(
            "UPDATE api_keys SET token=? WHERE id=?",
            rusqlite::params![short_hash(tok), id],
        )?;
    }
    // 名字里带密码 / 哈希 / 盐 / 密钥 / token 的设置一律删掉（10/11 审查：admin_password_hash 曾漏掉）
    tx.execute(
        "DELETE FROM site_settings WHERE key IN ('key_source_salt','admin_password_hash') OR key LIKE '%password%' \
         OR key LIKE '%hash%' OR key LIKE '%salt%' OR key LIKE '%secret%' OR key LIKE '%token%'",
        [],
    )?;
    // req_features.toks 是提示词词条的无盐短哈希，用标签字典能还原词条，清空；fp / sig 保留给分析
    // （req_features 表已随防分享体系移除（2026-10-13）：新快照没有这张表，老快照才有）
    if table_exists(&tx, "req_features")? {
        tx.execute("UPDATE req_features SET toks=''", [])?;
    }
    // 报错堆栈可能夹带请求内容，清空
    tx.execute("UPDATE error_events SET detail=''", [])?;
    let cols = columns(&tx, "generation_audit")?;
    for c in ["prompt", "negative", "extra"] {
        if cols.contains(c) {
            tx.execute(&format!("UPDATE generation_audit SET {c}=''"), [])?;
        }
    }
    if cols.contains("thumb") {
        tx.execute("UPDATE generation_audit SET thumb=NULL", [])?;
    }
    tx.execute("UPDATE discord_registrations SET avatar=''", [])?;

    // —— 以下是相对旧版新增的（见模块文档）——
    if table_exists(&tx, "key_donations")? {
        let donated: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, token FROM key_donations")?;
            stmt.query_map([], |r| {
                let v: rusqlite::types::Value = r.get(1)?;
                Ok((r.get(0)?, crate::util::sv_str(&v)))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        for (id, tok) in &donated {
            tx.execute(
                "UPDATE key_donations SET token=?, contact='', message='' WHERE id=?",
                rusqlite::params![short_hash(tok), id],
            )?;
        }
    }
    if table_exists(&tx, "discord_bans")? {
        tx.execute("UPDATE discord_bans SET avatar=''", [])?;
    }

    let left: i64 = tx.query_row(
        "SELECT COUNT(*) FROM api_keys WHERE token NOT LIKE 'sha256:%'",
        [],
        |r| r.get(0),
    )?;
    if left != 0 {
        bail!("api_keys.token 没脱敏干净（剩 {left} 条）");
    }
    tx.commit()?;
    conn.execute_batch("VACUUM")?;
    conn.close().map_err(|(_, e)| e)?;
    Ok(rows.len())
}
