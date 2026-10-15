//! NovelAI 上游客户端的门面：[`NaiClient`] 与调用方用到的类型。
//!
//! 实现按职责分在几个模块里（行为与旧版 `app/nai.py` 的 `NaiClient` 一致，见各模块说明）：
//! - [`crate::pool`]：Token、令牌池、后台增删改、选号与 V5 预留、健康 / 冷却标记；
//! - [`crate::scheduler`]：出图节奏——账号出图间隔 + 开始抖动、出图后静默、等槽位；
//! - [`crate::http`]：wreq 客户端（Chrome 指纹）、组请求、读正文；
//! - [`crate::account`]：订阅 / 额度查询、Token 校验；
//! - [`crate::dispatch`]：一次请求的完整流程（`request` / `image_stream`）与取消安全的结算。

use crate::http::Http;
use crate::pool::Pool;
use crate::scheduler::Scheduler;
use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use wreq::header::HeaderMap;

pub use crate::dispatch::{ImageStream, request_images};
pub use crate::error::UpstreamError;
pub use crate::hooks::{Allowance, BoxFuture, HookResult, Hooks, NoObserver, Observer};
pub use crate::pool::{Token, TokenFields};

#[derive(Debug, Clone)]
pub struct Config {
    pub image_host: String,
    pub proxy: Option<String>,
    pub custom_user_agent: Option<String>,
    pub http2: bool,
    pub post_jitter_min: f64,
    pub post_jitter_max: f64,
    pub single_slot_enforced: bool,
    pub image_min_interval: f64,
}

pub(crate) struct Inner<H: Hooks> {
    pub(crate) cfg: Config,
    pub(crate) hooks: H,
    pub(crate) pool: Pool,
    pub(crate) scheduler: Scheduler,
    /// 按指纹档位分的 HTTP 客户端（同一档位共享连接池）。
    pub(crate) http: Http,
}

/// 上游客户端句柄（克隆成本很低）。
pub struct NaiClient<H: Hooks> {
    pub(crate) inner: Arc<Inner<H>>,
}

impl<H: Hooks> Clone for NaiClient<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// 上游响应（已读完正文）。
#[derive(Debug, Clone)]
pub struct UpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// 请求选项（旧版 `request()` 的关键字参数）。
pub struct RequestOpts<'a> {
    pub method: wreq::Method,
    pub url: String,
    pub json: Option<&'a Value>,
    pub accept: &'a str,
    pub requires_anlas: bool,
    pub v5_free: bool,
    pub image_count: usize,
    pub image_lane: bool,
    pub wait_for_image_slot: bool,
    pub max_response_bytes: Option<usize>,
    pub queue_timeout: f64,
}

impl<'a> RequestOpts<'a> {
    /// 无请求体的 GET（标签补全）。
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: wreq::Method::GET,
            json: None,
            ..Self::post(url, &Value::Null)
        }
    }

    pub fn post(url: impl Into<String>, json: &'a Value) -> Self {
        Self {
            method: wreq::Method::POST,
            url: url.into(),
            json: Some(json),
            accept: "*/*",
            requires_anlas: false,
            v5_free: false,
            image_count: 0,
            image_lane: false,
            wait_for_image_slot: true,
            max_response_bytes: None,
            queue_timeout: 90.0,
        }
    }
}

/// V5 续费确认回调（参数：官方免费额度是否已耗尽）。
pub type ResolveV5<'a> =
    Box<dyn Fn(bool) -> BoxFuture<'static, Result<(), UpstreamError>> + Send + Sync + 'a>;

/// 调用方回调（旧版的 `on_rate_limited` / `resolve_v5_cost` / `before_dispatch` / `on_dispatch`）。
#[derive(Default)]
pub struct Callbacks<'a> {
    /// 上游 429：参数是解析后的 Retry-After 秒数（未钳制，与旧版一致由回调自己处理）。
    pub on_rate_limited: Option<Box<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync + 'a>>,
    /// V5 是否已按 Anlas 计费的最终确认（参数：官方免费额度是否已耗尽）。回调可以拒绝（额度复核不过），
    /// 拒绝原样返回给调用方（`UpstreamError::local`）。
    pub resolve_v5_cost: Option<ResolveV5<'a>>,
    /// 真正发出前的最后检查（全站冷却）；返回 Err 则不发。
    pub before_dispatch: Option<Box<dyn Fn() -> Result<(), UpstreamError> + Send + Sync + 'a>>,
    /// 已决定发出（图片流用来标记「已占用上游」）。
    pub on_dispatch: Option<Box<dyn Fn() + Send + Sync + 'a>>,
}

impl<H: Hooks> NaiClient<H> {
    /// `tokens`：(Token, 每日免费 V5 上限, 是否允许 Anlas)。
    pub fn new(
        cfg: Config,
        hooks: H,
        tokens: Vec<(String, i64, bool)>,
    ) -> Result<Self, wreq::Error> {
        let http = Http::new(
            cfg.proxy.as_deref(),
            cfg.http2,
            cfg.custom_user_agent.as_deref(),
        )?;
        let pool = Pool::new(
            tokens,
            cfg.custom_user_agent.clone(),
            cfg.single_slot_enforced,
        );
        let scheduler = Scheduler::new(
            cfg.image_min_interval,
            (cfg.post_jitter_min, cfg.post_jitter_max),
        );
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                hooks,
                pool,
                scheduler,
                http,
            }),
        })
    }

    pub fn hooks(&self) -> &H {
        &self.inner.hooks
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    pub fn image_host(&self) -> &str {
        self.inner.cfg.image_host.trim_end_matches('/')
    }

    /// 客户端启动以来的单调秒数（`TokenFields::image_next_at` 用的就是这个时钟；首页 / 后台算「还要等几秒」要用）。
    pub fn mono(&self) -> f64 {
        self.inner.scheduler.mono()
    }

    /// 当前令牌池快照。
    pub async fn tokens(&self) -> Vec<Arc<Token>> {
        self.inner.pool.tokens().await
    }

    pub async fn configured(&self) -> bool {
        !self.tokens().await.is_empty()
    }

    // ---------------- 令牌池维护（后台增删改，见 pool.rs） ----------------

    /// 应用库里保存的每 Token 设置（V5 日限 / 启停 / 并发）。
    pub async fn apply_saved(
        &self,
        v5_limits: &HashMap<String, i64>,
        enabled: &HashMap<String, bool>,
        concurrency: &HashMap<String, i64>,
    ) {
        self.inner
            .pool
            .apply_saved(v5_limits, enabled, concurrency)
            .await;
    }

    /// 新增一把 Token（重复返回 Err）。调用方负责持久化 token_store 与写库。
    pub async fn add_token(
        &self,
        token: String,
        allow_anlas: bool,
        saved_v5: Option<i64>,
        saved_enabled: Option<bool>,
    ) -> Result<Arc<Token>, String> {
        self.inner
            .pool
            .add(token, allow_anlas, saved_v5, saved_enabled)
            .await
    }

    /// 替换某个槽位的 Token（在 NovelAI 重置了 Token）：设置与启停延续。
    pub async fn replace_token(
        &self,
        token_id: &str,
        token: String,
    ) -> Result<(Arc<Token>, String), String> {
        self.inner.pool.replace(token_id, token).await
    }

    /// 删除（至少保留一把）。
    pub async fn remove_token(&self, token_id: &str) -> Result<bool, String> {
        self.inner.pool.remove(token_id).await
    }

    pub async fn set_v5_daily_limit(&self, token_id: &str, limit: i64) -> bool {
        self.inner.pool.set_v5_daily_limit(token_id, limit).await
    }

    /// 停用 / 启用。停用会等这把 Token 手上的图跑完才返回（之后发出的请求一定能看到新状态）。
    pub async fn set_admin_enabled(&self, token_id: &str, enabled: bool) -> bool {
        self.inner.pool.set_admin_enabled(token_id, enabled).await
    }

    pub async fn set_image_concurrency(&self, token_id: &str, limit: i64) -> Option<i64> {
        self.inner.pool.set_image_concurrency(token_id, limit).await
    }

    /// 后台「上游令牌池」的状态行（不含当天计数，由调用方补）。
    pub async fn status_rows(&self) -> Vec<(Arc<Token>, TokenFields)> {
        self.inner.pool.status_rows().await
    }

    // ---------------- 选号、预留与节奏（见 pool.rs / scheduler.rs） ----------------

    /// 调大出图间隔时，正在冷却的 Token 也立即按新间隔顺延。
    pub async fn set_image_min_interval(&self, seconds: f64) {
        let tokens = self.tokens().await;
        self.inner.scheduler.set_interval(&tokens, seconds);
    }

    /// 选一把符合费用策略的 Token；V5 免费额度在这里原子预留。
    pub async fn pick_token(
        &self,
        requires_anlas: bool,
        v5_free: bool,
        image_job: bool,
    ) -> Result<Option<Arc<Token>>, UpstreamError> {
        let inner = &self.inner;
        inner
            .pool
            .pick(&inner.hooks, requires_anlas, v5_free, image_job)
            .await
    }

    /// 只把成功完成的免费 V5 图计入该 Token 的日额度；无论成败都释放预留。
    pub async fn finish_v5_reservation(&self, token: &Token, succeeded: bool, v5_free: bool) {
        let inner = &self.inner;
        inner
            .pool
            .finish_v5_reservation(&inner.hooks, token, succeeded, v5_free)
            .await;
    }

    /// 按上游实际成功的张数记账（失败、拒绝、限流不计）。
    pub async fn record_successful_images(&self, token: &Token, image_count: usize) {
        crate::dispatch::record_images(&self.inner.hooks, token, image_count).await;
    }

    /// 等到这把 Token 的出图间隔到点，然后占位（`now + 间隔 + 抖动`）。取消等待不会挤占后续生图。
    pub async fn wait_for_token_image_slot(&self, token: &Token) {
        self.inner
            .scheduler
            .wait_turn(token, &self.inner.hooks)
            .await;
    }

    /// 没有可用 Token 时给成员的说明：先区分「冷却 / 停用」和「额度真的用完」。
    pub async fn unavailable(&self, requires_anlas: bool, v5_free: bool) -> UpstreamError {
        let inner = &self.inner;
        inner
            .pool
            .unavailable(&inner.hooks, requires_anlas, v5_free)
            .await
    }

    // ---------------- 健康标记（见 pool.rs） ----------------

    pub fn mark_rate_limited(&self, token: &Token, retry_after: f64) {
        token.mark_rate_limited(&self.inner.hooks, retry_after);
    }

    /// 连续 3 次 403（中间没有成功）→ 冷却并告警；冷却按轮次 5 → 15 → 45 → 60 分钟退避。
    pub fn mark_forbidden(&self, token: &Token) {
        token.mark_forbidden(&self.inner.hooks);
    }

    pub fn mark_unauthorized(&self, token: &Token) {
        token.mark_unauthorized();
    }

    pub fn mark_ok(&self, token: &Token) {
        token.mark_ok(&self.inner.hooks);
    }

    // ---------------- 账号查询（见 account.rs） ----------------

    /// 只读查询（订阅 / Anlas 余额 / Token 校验）：与出图同一套浏览器请求头，不碰调度。
    pub async fn get_json_with(
        &self,
        token: &str,
        profile_index: usize,
        url: &str,
        timeout: Duration,
        max_bytes: usize,
        follow_redirects: bool,
    ) -> Result<(u16, HeaderMap, Bytes), String> {
        crate::account::get_json(
            &self.inner.http,
            profile_index,
            token,
            url,
            timeout,
            max_bytes,
            follow_redirects,
        )
        .await
    }

    /// 后台「添加 / 替换 Token」前的有效性检查：查订阅（只读，不消耗额度，不记录 Token）。
    pub async fn verify_token(&self, token: &str) -> Result<i64, String> {
        crate::account::verify(&self.inner.http, 0, self.image_host(), token).await
    }
}
