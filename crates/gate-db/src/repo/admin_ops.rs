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

    /// 拒绝与失败的原因分布：`(原因码, 状态 rejected|error, 详情前 60 字, 次数)`，按次数从多到少，最多 400 行。
    /// 没有原因码的（上游错误、老记录）带上详情前缀，交给调用方按首句归类。
    pub async fn failure_reason_distribution(
        &self,
        since: i64,
    ) -> Result<Vec<(String, String, String, i64)>> {
        self.read(move |c| {
            let mut stmt = c.prepare(
                "SELECT reason, status, CASE WHEN reason='' THEN substr(detail,1,60) ELSE '' END, COUNT(*) \
                 FROM usage_log WHERE ts>=? AND status!='ok' GROUP BY 1,2,3 ORDER BY 4 DESC LIMIT 400",
            )?;
            stmt.query_map([since], |r| {
                Ok((
                    value::text_or(r, 0, "")?,
                    value::text_or(r, 1, "")?,
                    value::text_or(r, 2, "")?,
                    value::i64_or(r, 3, 0)?,
                ))
            })?
            .collect()
        })
        .await
    }
}
