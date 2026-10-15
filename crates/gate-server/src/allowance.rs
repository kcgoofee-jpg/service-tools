//! V5 官方免费额度缓存的 IO 层（旧版 `app/allowance.py` 的 `AllowanceCache`）。判定逻辑在
//! `gate_core::allowance::AllowanceCore`（golden 对拍过）；这里负责按账号串行、读阈值、请求订阅接口、
//! 回写恢复周期读数；额度偏低时写一行日志（后台「上游」页另有「低」标记）。
//!
//! 只有真实的出图请求会刷新缓存（订阅接口本身不消耗额度，但频繁查询像机器人）。

use gate_core::allowance::{self as core, AllowanceCore, Begin, Fetched};
use gate_db::Db;
use gate_upstream::client::{Allowance, BoxFuture, Hooks, NaiClient, Token};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ServerAllowance {
    db: Db,
    core: Mutex<AllowanceCore>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 单调时钟起点：缓存新鲜度按单调时钟算（旧版 `time.monotonic()`）。
    epoch: std::time::Instant,
}

/// `read_alert_threshold(db)`：库里存的是文本，1～3 位十进制数字才当整数，否则用默认 20。
pub async fn read_alert_threshold(db: &Db) -> Result<i64, gate_db::DbError> {
    let raw = db.get_setting(core::SETTING).await?;
    Ok(match raw {
        None => core::DEFAULT_THRESHOLD,
        Some(s) => core::normalize_alert_threshold(&Value::String(s)),
    })
}

impl ServerAllowance {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            core: Mutex::new(AllowanceCore::new()),
            locks: Mutex::new(HashMap::new()),
            epoch: std::time::Instant::now(),
        }
    }

    fn mono(&self) -> f64 {
        // 加一个大偏移：旧版 `row.get("at", -1e9)` 依赖单调时钟远大于 0
        self.epoch.elapsed().as_secs_f64() + 1e6
    }

    /// 绑定上游客户端，得到可以交给 `NaiClient::request` / `image_stream` 的 [`Allowance`]。
    pub fn with<'a, H: Hooks>(&'a self, nai: &'a NaiClient<H>) -> Bound<'a, H> {
        Bound { cache: self, nai }
    }

    /// 后台「V5 额度」面板（`snapshot(pool)`）。
    pub async fn snapshot(&self, token_ids: &[String]) -> Result<Value, gate_db::DbError> {
        let threshold = read_alert_threshold(&self.db).await?;
        let now_wall = crate::time::now();
        let now_mono = self.mono();
        let c = self.core.lock().expect("allowance");
        Ok(c.snapshot(
            token_ids.iter().map(String::as_str),
            threshold,
            now_wall,
            now_mono,
        ))
    }

    /// 首页 / 后台用的「每天恢复多少 %」：48 小时内最长的一次读数。
    pub fn period(&self, token_id: &str) -> Option<i64> {
        self.core
            .lock()
            .expect("allowance")
            .period(token_id, crate::time::now())
    }

    async fn resolve_with<H: Hooks>(
        &self,
        nai: &NaiClient<H>,
        token: &Token,
    ) -> Result<bool, String> {
        let lock = self
            .locks
            .lock()
            .expect("allowance")
            .entry(token.token_id.clone())
            .or_default()
            .clone();
        let _g = lock.lock().await;
        let unavailable = || core::UNAVAILABLE_MSG.to_string();
        let threshold = read_alert_threshold(&self.db)
            .await
            .map_err(|_| unavailable())?;
        match self.core.lock().expect("allowance").check_cache(
            &token.token_id,
            self.mono(),
            threshold,
        ) {
            Begin::Unavailable => return Err(unavailable()),
            Begin::Cached => return Ok(false),
            Begin::Fetch => {}
        }
        // 与生图同一套浏览器请求头；旧版固定用第 0 套浏览器特征，原样保留
        let url = format!(
            "{}/user/subscription",
            nai.image_host().trim_end_matches('/')
        );
        let fetched = tokio::time::timeout(
            Duration::from_secs(8),
            nai.get_json_with(
                &token.token,
                0,
                &url,
                Duration::from_secs(8),
                core::MAX_BODY,
                false,
            ),
        )
        .await;
        let (status, retry_after, body) = match &fetched {
            Ok(Ok((status, headers, body))) => (
                Some(*status),
                headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string(),
                Some(body.clone()),
            ),
            // 响应体超过 65536 字节时拿不到状态码：按「没拿到响应」处理，退避同样是 60 秒
            _ => (None, String::new(), None),
        };
        let f = match status {
            None => Fetched::Failed,
            Some(s) => Fetched::Response {
                status: s,
                retry_after: &retry_after,
                body: if s == 200 { body.as_deref() } else { None },
            },
        };
        let resolved = self.core.lock().expect("allowance").finish_resolve(
            &token.token_id,
            &f,
            crate::time::now(),
            self.mono(),
            threshold,
        );
        if let Some(seconds) = resolved.note_period {
            self.note_period(&token.token_id, seconds).await;
        }
        if resolved.low {
            tracing::warn!(account = %token.token_id, "V5 low allowance");
        }
        resolved.result.map_err(|()| unavailable())
    }

    /// `_note_period`：第一次用时从库里读样本，写失败只忽略（旧版 `except Exception: pass`）。
    async fn note_period(&self, token_id: &str, seconds: i64) {
        let loaded = self.core.lock().expect("allowance").periods_loaded();
        if !loaded {
            let raw = self.db.get_setting(core::PERIOD_KEY).await.ok().flatten();
            self.core
                .lock()
                .expect("allowance")
                .load_periods(raw.as_deref());
        }
        let text =
            self.core
                .lock()
                .expect("allowance")
                .note_period(token_id, seconds, crate::time::now());
        if let Ok(text) = text {
            let _ = self.db.set_setting(core::PERIOD_KEY, &text).await;
        }
    }
}

/// 绑定了上游客户端的额度查询。
pub struct Bound<'a, H: Hooks> {
    cache: &'a ServerAllowance,
    nai: &'a NaiClient<H>,
}

impl<H: Hooks> Allowance for Bound<'_, H> {
    fn resolve<'a>(&'a self, token: &'a Token) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(self.cache.resolve_with(self.nai, token))
    }
}
