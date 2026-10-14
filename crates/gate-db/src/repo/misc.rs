//! 杂项。

use super::json_to_sql;
use crate::{Db, Result, value};
use serde_json::Value;

impl Db {
    /// 旧版 `execute_fetchall_compat(sql, args)`：给外部模块跑一条只读 SELECT 取全部行
    /// （目前只有 `registration.count_registered` 用）。走只读连接，所以这里**不支持写语句**；
    /// 参数是 JSON 标量（bool 当整数），行按 `row_json` 返回。
    pub async fn execute_fetchall_compat(&self, sql: &str, args: &[Value]) -> Result<Vec<Value>> {
        let sql = sql.to_string();
        let args = args.iter().map(json_to_sql).collect::<Result<Vec<_>>>()?;
        self.read(move |c| {
            let mut stmt = c.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(args.iter()), value::row_json)?
                .collect()
        })
        .await
    }
}
