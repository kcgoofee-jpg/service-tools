//! 公开页面（首页首屏 / 实时看板）用到的两条查询（旧版 registration.py 里的裸 SQL）。

use crate::{Db, Result, value};

/// 领取记录里的 Discord 资料：(username, display_name, avatar)，任一项可能为 NULL。
pub type DiscordProfileRow = (Option<String>, Option<String>, Option<String>);

impl Db {
    /// 首页「我」里的 Discord 资料（旧版 `registration_profile` 的查询）：没有登记行返回 None。
    pub async fn discord_profile_row(&self, discord_id: &str) -> Result<Option<DiscordProfileRow>> {
        let id = discord_id.to_string();
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT username, display_name, avatar FROM discord_registrations WHERE discord_id=?",
            )?;
            let mut rows = stmt.query([id])?;
            Ok(match rows.next()? {
                Some(r) => Some((
                    value::text_opt(r, 0)?,
                    value::text_opt(r, 1)?,
                    value::text_opt(r, 2)?,
                )),
                None => None,
            })
        })
        .await
    }

    /// 占用名额的人数（旧版 `registration.count_registered`）：持有未过期 Key 的已注册用户。
    /// Key 过期或被删则名额释放；被站长停用（暂停）的成员仍占名额。
    pub async fn registered_member_count(&self, now: f64) -> Result<i64> {
        self.read(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id \
                 WHERE k.expires_at IS NULL OR k.expires_at > ?",
                [now],
                |r| value::i64_or(r, 0, 0),
            )
        })
        .await
    }
}
