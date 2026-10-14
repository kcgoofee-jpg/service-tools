//! 后台「运营 / 监控」几个接口用到的裸 SQL（旧版 `admin.py` / `registration.py` 里直接写在路由里的查询）。

use crate::{Db, Result, value};

impl Db {
    /// 上游实际返回的状态码分布（`usage_log.up_status`，`since`（时间戳）之后），次数多的在前；
    /// 0 = 没发到 NovelAI（网关自己拦下、排队超时或客户端断开）。次数相同的先后顺序由 SQLite 决定（旧版同样）。
    ///
    /// 旧版 SQL 原样：`SELECT up_status, COUNT(*) FROM usage_log WHERE ts>=? GROUP BY up_status ORDER BY 2 DESC`。
    pub async fn upstream_status_distribution(&self, since: i64) -> Result<Vec<(i64, i64)>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT up_status, COUNT(*) FROM usage_log WHERE ts>=? GROUP BY up_status ORDER BY 2 DESC",
            )?;
            stmt.query_map([since], |r| {
                Ok((value::i64_or(r, 0, 0)?, value::i64_or(r, 1, 0)?))
            })?
            .collect()
        })
        .await
    }

    /// 占用名额的人数：持有未过期 Key 的已注册用户（旧版 `registration.count_registered`）。
    /// 被站长停用的成员仍占名额；Key 过期或被删则名额释放。
    pub async fn admin_registered_count(&self, now: f64) -> Result<i64> {
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
