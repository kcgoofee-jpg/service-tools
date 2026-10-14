//! 成员问卷与首页「捐赠 Key」。

use crate::{Db, Result, value};
use gate_core::pyc;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

/// `save_survey` 的各项回答（保存时按旧版的长度截断）。
#[derive(Debug, Clone, Default)]
pub struct Survey {
    pub client: String,
    pub understand: String,
    pub improve: String,
    pub improve_other: String,
    pub donate: String,
}

/// `list_donations` 的结果：未处理的在前（`handled` 列升序，再按 ts 倒序），各自最多共 200 条。
#[derive(Debug, Clone, Serialize)]
pub struct Donations {
    pub unhandled: Vec<Value>,
    pub handled: Vec<Value>,
}

impl Db {
    /// 成员问卷：一人一份，重复提交覆盖（取最新想法）。
    pub async fn save_survey(&self, key_id: i64, s: &Survey, now: f64) -> Result<()> {
        let client = pyc::take_chars(&s.client, 60).to_string();
        let understand = pyc::take_chars(&s.understand, 20).to_string();
        let improve = pyc::take_chars(&s.improve, 200).to_string();
        let improve_other = pyc::take_chars(&s.improve_other, 300).to_string();
        let donate = pyc::take_chars(&s.donate, 20).to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO member_survey(key_id, client, understand, improve, improve_other, donate, ts) \
                 VALUES (?,?,?,?,?,?,?) ON CONFLICT(key_id) DO UPDATE SET client=excluded.client, \
                 understand=excluded.understand, improve=excluded.improve, improve_other=excluded.improve_other, \
                 donate=excluded.donate, ts=excluded.ts",
                params![key_id, client, understand, improve, improve_other, donate, now],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_survey(&self) -> Result<Vec<Value>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT f.*, k.name, r.discord_id FROM member_survey f \
                 LEFT JOIN api_keys k ON k.id=f.key_id \
                 LEFT JOIN discord_registrations r ON r.key_id=f.key_id ORDER BY f.ts DESC",
            )?;
            stmt.query_map([], value::row_json)?.collect()
        })
        .await
    }

    /// 首页捐赠 Key：原文入库等站长人工接入，返回 id。Token 绝不写日志。
    pub async fn save_donation(
        &self,
        token: &str,
        message: &str,
        contact: &str,
        now: f64,
    ) -> Result<i64> {
        let token = token.to_string();
        let message = pyc::take_chars(message, 500).to_string();
        let contact = pyc::take_chars(contact, 100).to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO key_donations(ts, token, message, contact) VALUES (?,?,?,?)",
                params![now, token, message, contact],
            )?;
            Ok(tx.last_insert_rowid())
        })
        .await
    }

    pub async fn list_donations(&self) -> Result<Donations> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, token, message, contact, handled, handled_at, handled_note FROM key_donations \
                 ORDER BY handled, ts DESC LIMIT 200",
            )?;
            let rows = stmt.query_map([], |r| Ok((value::flag(r, "handled")?, value::row_json(r)?)))?;
            let mut out = Donations {
                unhandled: Vec::new(),
                handled: Vec::new(),
            };
            for row in rows {
                let (handled, json) = row?;
                if handled {
                    out.handled.push(json);
                } else {
                    out.unhandled.push(json);
                }
            }
            Ok(out)
        })
        .await
    }

    pub async fn count_unhandled_donations(&self) -> Result<i64> {
        self.read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM key_donations WHERE handled=0",
                [],
                |r| value::i64_or(r, 0, 0),
            )
        })
        .await
    }

    pub async fn handle_donation(&self, did: i64, note: &str, now: f64) -> Result<bool> {
        let note = pyc::take_chars(note, 200).to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE key_donations SET handled=1, handled_at=?, handled_note=? WHERE id=? AND handled=0",
                params![now, note, did],
            )?;
            Ok(n > 0)
        })
        .await
    }
}
