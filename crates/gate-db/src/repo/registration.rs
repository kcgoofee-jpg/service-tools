//! Discord 领取记录（`discord_registrations`）的查询（旧版 registration.py 里的裸 SQL）。

use crate::{Db, Result};

impl Db {
    /// 这个 Discord 账号领到的 Key（`SELECT key_id FROM discord_registrations WHERE discord_id=?`）。
    pub async fn registration_key_id(&self, discord_id: &str) -> Result<Option<i64>> {
        let id = discord_id.to_string();
        self.read(move |c| {
            let mut stmt =
                c.prepare("SELECT key_id FROM discord_registrations WHERE discord_id=?")?;
            let mut rows = stmt.query([id])?;
            Ok(match rows.next()? {
                Some(r) => r.get::<_, Option<i64>>(0)?,
                None => None,
            })
        })
        .await
    }

    /// `key_row_for(discord_id)`：领到的 Key 行；没领过 / Key 已删为 None。
    pub async fn key_row_for_discord(&self, discord_id: &str) -> Result<Option<serde_json::Value>> {
        match self.registration_key_id(discord_id).await? {
            Some(id) => self.get_key(id).await,
            None => Ok(None),
        }
    }
}
