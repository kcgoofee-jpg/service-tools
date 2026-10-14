//! NovelAI 上游客户端：逐段移植 `app/nai.py` 的 `NaiClient`。
//!
//! 评估周内这里是**行为 1:1**：选号轮询、V5 预留、出图间隔 + 抖动、请求后抖动、
//! 429 / 403 / 401 的处理与文案、计费不确定性的判定，全部照旧版写。差别只在两处：
//! - TLS / HTTP2 指纹换成 wreq 的 Chrome 模拟（站长 10-14 批准）；
//! - 取消安全用 Drop 守卫实现：请求 future 在任何点被丢弃，V5 预留、计数、请求后抖动与
//!   并发槽都会照常结算（旧版用 `asyncio.shield` + `CancelScope(shield=True)` 达到同样效果）。

use crate::headers::{self, BrowserProfile};
use crate::limiter::{AdjustableLimiter, Permit};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wreq::header::HeaderMap;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type HookResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 上游错误：`status` 是回给成员的 HTTP 状态，`message` 是成员可见的中文说明。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{status} {message}")]
pub struct UpstreamError {
    pub status: u16,
    pub message: String,
    /// 请求可能已经在上游开始生成（可能已扣费），调用方据此决定记账口径。
    pub billing_uncertain: bool,
    /// 非出图通道的传输层错误：旧版直接把 httpx 异常抛给路由（变成 500 / 502），这里单独标记。
    pub transport: bool,
    /// 拒绝原因码（账号保护挡住时带上 gate_core::reasons 的码；旧版靠 str 子类 Reason 携带）。
    pub code: String,
}

impl UpstreamError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            billing_uncertain: false,
            transport: false,
            code: String::new(),
        }
    }
    fn uncertain(status: u16, message: impl Into<String>, billing_uncertain: bool) -> Self {
        Self {
            status,
            message: message.into(),
            billing_uncertain,
            transport: false,
            code: String::new(),
        }
    }
    fn internal(e: impl std::fmt::Display) -> Self {
        Self {
            status: 500,
            message: format!("内部错误：{e}"),
            billing_uncertain: false,
            transport: true,
            code: String::new(),
        }
    }
}

/// 网关注入给客户端的外部能力：时间、事件告警、账号保护（guard）、上游计数（数据库）、结果校验。
/// 全部由 gate-server 实现；测试里用内存版。
pub trait Hooks: Send + Sync + 'static {
    /// 当前服务日（`state.day()`）。
    fn day(&self) -> String;
    /// 墙钟时间（Unix 秒）。
    fn now(&self) -> f64;
    /// 告警事件（同类有冷却，由实现方处理）。
    fn event(&self, kind: &str, message: &str, cooldown: f64);

    /// 是否接了账号保护（旧版 `self.guard is not None`）。
    fn has_guard(&self) -> bool;
    /// 账号保护：这把 Token 现在能不能出图；不能时返回 (给成员的原因, 原因码)。
    fn token_block_reason<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<Option<(String, String)>>>;
    /// 账号保护：记一次出图开始（按张数）。
    fn record_start(&self, token_id: &str, images: i64);
    /// 账号保护：本次出图间隔的随机抖动（秒）。
    fn jitter(&self) -> f64;

    fn upstream_v5_counter<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<i64>>;
    fn bump_upstream_v5<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<()>>;
    fn bump_upstream_images<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
        n: i64,
    ) -> BoxFuture<'a, HookResult<()>>;

    /// 校验上游返回的出图结果是否完整（`image_tools.validate_result`），在阻塞线程里跑。
    fn validate_image_result(
        &self,
        body: Bytes,
        expected_images: usize,
    ) -> BoxFuture<'static, bool>;
}

/// 单次请求的观察者（旧版 `request_timing` 的 contextvars）：记录「已发往上游」与上游状态码。
pub trait Observer: Send + Sync {
    fn sent(&self) {}
    fn status(&self, _code: u16) {}
}

pub struct NoObserver;
impl Observer for NoObserver {}

#[derive(Debug, Clone)]
pub struct Config {
    pub image_host: String,
    pub text_host: String,
    pub legacy_text_host: String,
    pub proxy: Option<String>,
    pub custom_user_agent: Option<String>,
    pub http2: bool,
    pub post_jitter_min: f64,
    pub post_jitter_max: f64,
    pub single_slot_enforced: bool,
    pub image_min_interval: f64,
}

/// 每把上游 Token 的可变状态（旧版 `TokenState` 的 slots）。
#[derive(Debug, Clone)]
pub struct TokenFields {
    pub position: usize,
    pub v5_daily_limit: i64,
    pub allow_anlas: bool,
    pub pending_v5: i64,
    pub admin_enabled: bool,
    pub fails: i64,
    /// 墙钟时间：429 / 403 冷却到何时。
    pub blocked_until: f64,
    pub disabled: bool,
    pub last_ok: f64,
    /// 单调时钟（客户端启动以来的秒）：下一张图最早何时可以发。
    pub image_next_at: f64,
    pub forbidden_streak: i64,
    pub forbidden_trips: i64,
    pub forbidden_last: f64,
    pub text_blocked_until: f64,
    pub text_forbidden: i64,
}

pub struct Token {
    pub token: String,
    pub token_id: String,
    pub profile: BrowserProfile,
    profile_index: usize,
    pub image_slots: AdjustableLimiter,
    dispatch_lock: tokio::sync::Mutex<()>,
    fields: Mutex<TokenFields>,
}

impl Token {
    fn new(
        token: String,
        index: usize,
        v5_daily_limit: i64,
        allow_anlas: bool,
        custom_ua: Option<&str>,
    ) -> Self {
        Self {
            token_id: headers::token_id(&token),
            token,
            profile: headers::profile_for(index, custom_ua),
            profile_index: index % 3,
            image_slots: AdjustableLimiter::new(1),
            dispatch_lock: tokio::sync::Mutex::new(()),
            fields: Mutex::new(TokenFields {
                position: index + 1,
                v5_daily_limit: v5_daily_limit.max(0),
                allow_anlas,
                pending_v5: 0,
                admin_enabled: true,
                fails: 0,
                blocked_until: 0.0,
                disabled: false,
                last_ok: 0.0,
                image_next_at: 0.0,
                forbidden_streak: 0,
                forbidden_trips: 0,
                forbidden_last: 0.0,
                text_blocked_until: 0.0,
                text_forbidden: 0,
            }),
        }
    }

    pub fn fields(&self) -> TokenFields {
        self.fields.lock().expect("token").clone()
    }

    fn with<R>(&self, f: impl FnOnce(&mut TokenFields) -> R) -> R {
        f(&mut self.fields.lock().expect("token"))
    }

    /// `admin_enabled and not disabled and now >= blocked_until`
    pub fn usable(&self, now: f64) -> bool {
        self.with(|f| f.admin_enabled && !f.disabled && now >= f.blocked_until)
    }
}

struct PoolState {
    tokens: Vec<Arc<Token>>,
    rr: usize,
    image_min_interval: f64,
}

struct Inner<H: Hooks> {
    cfg: Config,
    hooks: H,
    /// 旧版 `self._lock`：选号、预留、间隔计算都在它下面串行（会跨 await 持有）。
    pool: tokio::sync::Mutex<PoolState>,
    /// 按指纹档位分的 HTTP 客户端（同一档位共享连接池）。
    http: Vec<wreq::Client>,
    mono_base: tokio::time::Instant,
}

/// 上游客户端句柄（克隆成本很低）。
pub struct NaiClient<H: Hooks> {
    inner: Arc<Inner<H>>,
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

/// 调用方回调（旧版的 `on_rate_limited` / `resolve_v5_cost` / `before_dispatch` / `on_dispatch`）。
#[derive(Default)]
pub struct Callbacks<'a> {
    /// 上游 429：参数是解析后的 Retry-After 秒数（未钳制，与旧版一致由回调自己处理）。
    pub on_rate_limited: Option<Box<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync + 'a>>,
    /// V5 是否已按 Anlas 计费的最终确认（参数：官方免费额度是否已耗尽）。
    pub resolve_v5_cost: Option<Box<dyn Fn(bool) -> BoxFuture<'static, ()> + Send + Sync + 'a>>,
    /// 真正发出前的最后检查（全站冷却）；返回 Err 则不发。
    pub before_dispatch: Option<Box<dyn Fn() -> Result<(), UpstreamError> + Send + Sync + 'a>>,
    /// 已决定发出（图片流用来标记「已占用上游」）。
    pub on_dispatch: Option<Box<dyn Fn() + Send + Sync + 'a>>,
}

/// V5 额度确认（`AllowanceCache.resolve`）：由 gate-server 实现（需要库里的阈值与周期样本）。
pub trait Allowance: Send + Sync {
    /// 返回官方免费额度是否已耗尽；查询失败返回 Err(给成员的文案)。
    fn resolve<'a>(&'a self, token: &'a Token) -> BoxFuture<'a, Result<bool, String>>;
}

/// 请求进行中的结算守卫：正常路径调用 `finish().await`；被取消（Drop）时在后台补做同样的结算。
struct InFlight<H: Hooks> {
    inner: Arc<Inner<H>>,
    token: Arc<Token>,
    succeeded: bool,
    v5_free: bool,
    image_count: usize,
    send_started: bool,
    permit: Option<Permit>,
    done: bool,
}

impl<H: Hooks> InFlight<H> {
    async fn finish(mut self) {
        self.done = true;
        let permit = self.permit.take();
        settle(
            &self.inner,
            &self.token,
            self.succeeded,
            self.v5_free,
            self.image_count,
            self.send_started,
        )
        .await;
        drop(permit); // 先结算（含请求后抖动）再放槽：下一位一定能看到新的 image_next_at
    }
}

impl<H: Hooks> Drop for InFlight<H> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let inner = self.inner.clone();
        let token = self.token.clone();
        let permit = self.permit.take();
        let (succeeded, v5_free, image_count, send_started) = (
            self.succeeded,
            self.v5_free,
            self.image_count,
            self.send_started,
        );
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                settle(
                    &inner,
                    &token,
                    succeeded,
                    v5_free,
                    image_count,
                    send_started,
                )
                .await;
                drop(permit);
            });
        }
    }
}

async fn settle<H: Hooks>(
    inner: &Arc<Inner<H>>,
    token: &Arc<Token>,
    succeeded: bool,
    v5_free: bool,
    image_count: usize,
    send_started: bool,
) {
    let client = NaiClient {
        inner: inner.clone(),
    };
    client
        .finish_v5_reservation(token, succeeded, v5_free)
        .await;
    if succeeded {
        client.record_successful_images(token, image_count).await;
    }
    // 拟人化的请求后静默：只对真正发往上游、且间隔策略正常（> 0.5 秒）的请求生效；测试环境自动跳过
    let pool = inner.pool.lock().await;
    let interval = pool.image_min_interval;
    drop(pool);
    let (jmin, jmax) = (
        inner.cfg.post_jitter_min.max(0.0),
        inner
            .cfg
            .post_jitter_max
            .max(inner.cfg.post_jitter_min.max(0.0)),
    );
    if send_started && interval > 0.5 && jmax > 0.0 {
        let now = client.mono();
        let jitter_max = jmax.min(interval);
        let jitter_min = jmin.min(jitter_max);
        let post_delay = if jitter_max > 0.0 {
            uniform(jitter_min, jitter_max)
        } else {
            0.0
        };
        if post_delay > 0.0 {
            let _guard = inner.pool.lock().await;
            token.with(|f| f.image_next_at = f.image_next_at.max(now + post_delay));
        }
    }
}

/// Python `random.uniform(a, b)` = `a + (b - a) * random()`。
fn uniform(a: f64, b: f64) -> f64 {
    a + (b - a) * rand::random::<f64>()
}

/// `parameters.n_samples`（至少 1）：限速计数按张数记，批量请求才绕不过突发窗口。
pub fn request_images(body: Option<&Value>) -> i64 {
    let n = body
        .and_then(|b| b.get("parameters"))
        .and_then(|p| p.get("n_samples"))
        .cloned()
        .unwrap_or(Value::Null);
    let parsed = match &n {
        Value::Null => Some(1),
        v if !gate_core::pyval::truthy(v) => Some(1),
        Value::Number(x) if !x.is_f64() => x.as_i64(),
        Value::Number(x) => x.as_f64().and_then(gate_core::pyc::int_of),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => gate_core::pyc::parse_int(s),
        _ => None,
    };
    parsed.unwrap_or(1).max(1)
}

impl<H: Hooks> NaiClient<H> {
    /// `tokens`：(Token, 每日免费 V5 上限, 是否允许 Anlas)。
    pub fn new(
        cfg: Config,
        hooks: H,
        tokens: Vec<(String, i64, bool)>,
    ) -> Result<Self, wreq::Error> {
        let custom = cfg.custom_user_agent.clone();
        let pool: Vec<Arc<Token>> = tokens
            .into_iter()
            .enumerate()
            .map(|(i, (t, v5, anlas))| Arc::new(Token::new(t, i, v5, anlas, custom.as_deref())))
            .collect();
        let mut http = Vec::with_capacity(3);
        for p in headers::browser_profiles() {
            let emu = wreq_util::Emulation::builder()
                .profile(p.chrome)
                .platform(p.platform)
                .http2(cfg.http2)
                .headers(false)
                .build();
            let mut b = wreq::Client::builder()
                .emulation(emu)
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(300))
                // 旧版 keepalive_expiry=120：出图间隔 15 秒，默认 5 秒会让每张图都重新 TLS 握手
                .pool_idle_timeout(Duration::from_secs(120))
                .pool_max_idle_per_host(8)
                .redirect(wreq::redirect::Policy::limited(20));
            if !cfg.http2 {
                b = b.http1_only();
            }
            if let Some(proxy) = cfg.proxy.as_deref().filter(|p| !p.is_empty()) {
                b = b.proxy(wreq::Proxy::all(proxy)?);
            }
            http.push(b.build()?);
        }
        let interval = cfg.image_min_interval.max(0.0);
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                hooks,
                pool: tokio::sync::Mutex::new(PoolState {
                    tokens: pool,
                    rr: 0,
                    image_min_interval: interval,
                }),
                http,
                mono_base: tokio::time::Instant::now(),
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
    pub fn text_host(&self) -> &str {
        self.inner.cfg.text_host.trim_end_matches('/')
    }
    pub fn legacy_text_host(&self) -> &str {
        self.inner.cfg.legacy_text_host.trim_end_matches('/')
    }

    fn mono(&self) -> f64 {
        self.inner.mono_base.elapsed().as_secs_f64()
    }

    fn now(&self) -> f64 {
        self.inner.hooks.now()
    }

    /// 当前令牌池快照。
    pub async fn tokens(&self) -> Vec<Arc<Token>> {
        self.inner.pool.lock().await.tokens.clone()
    }

    pub async fn configured(&self) -> bool {
        !self.inner.pool.lock().await.tokens.is_empty()
    }

    fn http_for(&self, token: &Token) -> &wreq::Client {
        &self.inner.http[token.profile_index]
    }

    /// 按旧版顺序构造请求（头顺序见 headers.rs）。
    fn build(
        &self,
        token: &Token,
        method: wreq::Method,
        url: &str,
        body: Option<&Value>,
        accept: &str,
    ) -> wreq::RequestBuilder {
        let has_body = !matches!(method, wreq::Method::GET | wreq::Method::HEAD);
        let list = headers::ordered(&token.profile, Some(&token.token), accept, has_body);
        let (map, orig) = headers::to_wreq(&list);
        let mut req = self
            .http_for(token)
            .request(method, url)
            .default_headers(false)
            .headers(map)
            .orig_headers(orig);
        if let Some(b) = body {
            req = req.body(gate_core::pyjson::wire(b));
        }
        req
    }

    // ---------------- 令牌池维护（后台增删改） ----------------

    /// 应用库里保存的每 Token 设置（V5 日限 / 启停 / 并发）。
    pub async fn apply_saved(
        &self,
        v5_limits: &HashMap<String, i64>,
        enabled: &HashMap<String, bool>,
        concurrency: &HashMap<String, i64>,
    ) {
        let pool = self.inner.pool.lock().await;
        for t in &pool.tokens {
            t.with(|f| {
                if let Some(v) = v5_limits.get(&t.token_id) {
                    f.v5_daily_limit = *v;
                }
                if let Some(e) = enabled.get(&t.token_id) {
                    f.admin_enabled = *e;
                }
            });
            if let Some(c) = concurrency.get(&t.token_id) {
                let eff = if self.inner.cfg.single_slot_enforced {
                    1
                } else {
                    (*c).clamp(1, 4) as usize
                };
                t.image_slots.resize(eff);
            }
        }
    }

    /// 新增一把 Token（重复返回 Err）。调用方负责持久化 token_store 与写库。
    pub async fn add_token(
        &self,
        token: String,
        allow_anlas: bool,
        saved_v5: Option<i64>,
        saved_enabled: Option<bool>,
    ) -> Result<Arc<Token>, String> {
        let mut pool = self.inner.pool.lock().await;
        let new = Token::new(
            token,
            pool.tokens.len(),
            0,
            allow_anlas,
            self.inner.cfg.custom_user_agent.as_deref(),
        );
        if pool.tokens.iter().any(|t| t.token_id == new.token_id) {
            return Err("这把 Token 已经在令牌池里".into());
        }
        new.with(|f| {
            if let Some(v) = saved_v5 {
                f.v5_daily_limit = v;
            }
            if let Some(e) = saved_enabled {
                f.admin_enabled = e;
            }
        });
        new.image_slots.resize(1);
        let new = Arc::new(new);
        pool.tokens.push(new.clone());
        renumber(&pool.tokens);
        Ok(new)
    }

    /// 替换某个槽位的 Token（在 NovelAI 重置了 Token）：设置与启停延续。
    pub async fn replace_token(
        &self,
        token_id: &str,
        token: String,
    ) -> Result<(Arc<Token>, String), String> {
        let mut pool = self.inner.pool.lock().await;
        let Some(index) = pool.tokens.iter().position(|t| t.token_id == token_id) else {
            return Err("令牌不存在".into());
        };
        let old = pool.tokens[index].clone();
        let of = old.fields();
        let new = Token::new(
            token,
            index,
            of.v5_daily_limit,
            of.allow_anlas,
            self.inner.cfg.custom_user_agent.as_deref(),
        );
        if new.token_id != old.token_id && pool.tokens.iter().any(|t| t.token_id == new.token_id) {
            return Err("这把 Token 已经在令牌池的另一个位置".into());
        }
        new.with(|f| f.admin_enabled = of.admin_enabled);
        new.image_slots
            .resize(if self.inner.cfg.single_slot_enforced {
                1
            } else {
                old.image_slots.limit()
            });
        let new = Arc::new(new);
        pool.tokens[index] = new.clone();
        renumber(&pool.tokens);
        Ok((new, old.token_id.clone()))
    }

    /// 删除（至少保留一把）。
    pub async fn remove_token(&self, token_id: &str) -> Result<bool, String> {
        let mut pool = self.inner.pool.lock().await;
        if pool.tokens.len() <= 1 {
            return Err("至少需要保留一把上游 Token".into());
        }
        let before = pool.tokens.len();
        pool.tokens.retain(|t| t.token_id != token_id);
        if pool.tokens.len() == before {
            return Ok(false);
        }
        renumber(&pool.tokens);
        Ok(true)
    }

    pub async fn set_v5_daily_limit(&self, token_id: &str, limit: i64) -> bool {
        let pool = self.inner.pool.lock().await;
        match pool.tokens.iter().find(|t| t.token_id == token_id) {
            Some(t) => {
                t.with(|f| f.v5_daily_limit = limit);
                true
            }
            None => false,
        }
    }

    /// 停用 / 启用。停用会等这把 Token 手上的图跑完才返回（之后发出的请求一定能看到新状态）。
    pub async fn set_admin_enabled(&self, token_id: &str, enabled: bool) -> bool {
        let token = {
            let pool = self.inner.pool.lock().await;
            pool.tokens.iter().find(|t| t.token_id == token_id).cloned()
        };
        let Some(t) = token else { return false };
        {
            let _d = t.dispatch_lock.lock().await;
            let _p = self.inner.pool.lock().await;
            t.with(|f| f.admin_enabled = enabled);
        }
        if !enabled {
            t.image_slots.wait_idle().await;
        }
        true
    }

    pub async fn set_image_concurrency(&self, token_id: &str, limit: i64) -> Option<i64> {
        let pool = self.inner.pool.lock().await;
        let t = pool.tokens.iter().find(|t| t.token_id == token_id)?;
        let eff = if self.inner.cfg.single_slot_enforced {
            1
        } else {
            limit.clamp(1, 4)
        };
        t.image_slots.resize(eff as usize);
        Some(eff)
    }

    /// 调大出图间隔时，正在冷却的 Token 也立即按新间隔顺延。
    pub async fn set_image_min_interval(&self, seconds: f64) {
        let mut pool = self.inner.pool.lock().await;
        let now = self.mono();
        if seconds > pool.image_min_interval {
            for t in &pool.tokens {
                t.with(|f| {
                    if f.image_next_at > now {
                        f.image_next_at = f.image_next_at.max(now + seconds);
                    }
                });
            }
        }
        pool.image_min_interval = seconds;
    }

    // ---------------- 选号与预留 ----------------

    /// 选一把符合费用策略的 Token；V5 免费额度在这里原子预留。
    pub async fn pick_token(
        &self,
        requires_anlas: bool,
        v5_free: bool,
        image_job: bool,
        text_job: bool,
    ) -> Result<Option<Arc<Token>>, UpstreamError> {
        let hooks = &self.inner.hooks;
        let mut pool = self.inner.pool.lock().await;
        let now = self.now();
        let mut usable: Vec<Arc<Token>> = pool
            .tokens
            .iter()
            .filter(|t| t.usable(now) && (!requires_anlas || t.with(|f| f.allow_anlas)))
            .cloned()
            .collect();
        if usable.is_empty() {
            return Ok(None);
        }
        if text_job {
            let now = self.now();
            let open: Vec<Arc<Token>> = usable
                .iter()
                .filter(|t| t.with(|f| f.text_blocked_until <= now))
                .cloned()
                .collect();
            if open.is_empty() {
                let earliest = usable
                    .iter()
                    .map(|t| t.with(|f| f.text_blocked_until))
                    .fold(f64::INFINITY, f64::min);
                let wait = ((earliest - now) as i64).max(1);
                return Err(UpstreamError::new(
                    503,
                    format!("上游文本通道限流冷却中，约 {wait} 秒后恢复（生图不受影响）"),
                ));
            }
            usable = open;
        }
        if image_job && hooks.has_guard() {
            let day = hooks.day();
            let mut reasons = Vec::new();
            let mut open = Vec::new();
            for t in usable {
                match hooks
                    .token_block_reason(&t.token_id, &day)
                    .await
                    .map_err(UpstreamError::internal)?
                {
                    Some(r) => reasons.push(r),
                    None => open.push(t),
                }
            }
            if open.is_empty() {
                let (text, code) = reasons.into_iter().next().unwrap_or_default();
                let mut e = UpstreamError::new(429, text);
                e.code = code;
                return Err(e);
            }
            usable = open;
        }
        // 保持轮询，同时跳过当日 V5 已用完的 Token
        let n = usable.len();
        let start = (pool.rr + 1) % n;
        let mut chosen: Option<Arc<Token>> = None;
        for offset in 0..n {
            let candidate = &usable[(start + offset) % n];
            let (limit, pending) = candidate.with(|f| (f.v5_daily_limit, f.pending_v5));
            if v5_free && limit != 0 {
                let used = hooks
                    .upstream_v5_counter(&candidate.token_id, &hooks.day())
                    .await
                    .map_err(UpstreamError::internal)?;
                if used + pending >= limit {
                    continue;
                }
            }
            chosen = Some(candidate.clone());
            break;
        }
        let Some(chosen) = chosen else {
            return Ok(None);
        };
        pool.rr = usable
            .iter()
            .position(|t| Arc::ptr_eq(t, &chosen))
            .unwrap_or(0);
        if v5_free {
            chosen.with(|f| f.pending_v5 += 1);
        }
        Ok(Some(chosen))
    }

    /// 只把成功完成的免费 V5 图计入该 Token 的日额度；无论成败都释放预留。
    pub async fn finish_v5_reservation(&self, token: &Token, succeeded: bool, v5_free: bool) {
        if !v5_free {
            return;
        }
        let _pool = self.inner.pool.lock().await;
        if succeeded {
            let day = self.inner.hooks.day();
            if let Err(e) = self
                .inner
                .hooks
                .bump_upstream_v5(&token.token_id, &day)
                .await
            {
                tracing::error!(error = %e, "记录上游 V5 计数失败");
            }
        }
        token.with(|f| f.pending_v5 = (f.pending_v5 - 1).max(0));
    }

    /// 按上游实际成功的张数记账（失败、拒绝、限流不计）。
    pub async fn record_successful_images(&self, token: &Token, image_count: usize) {
        if image_count > 0 {
            let day = self.inner.hooks.day();
            if let Err(e) = self
                .inner
                .hooks
                .bump_upstream_images(&token.token_id, &day, image_count as i64)
                .await
            {
                tracing::error!(error = %e, "记录上游出图计数失败");
            }
        }
    }

    /// 等到这把 Token 的出图间隔到点，然后占位（`now + 间隔 + 抖动`）。取消等待不会挤占后续生图。
    pub async fn wait_for_token_image_slot(&self, token: &Token) {
        loop {
            let wait = {
                let pool = self.inner.pool.lock().await;
                let now = self.mono();
                let wait = (token.with(|f| f.image_next_at) - now).max(0.0);
                if wait == 0.0 {
                    let jitter = if self.inner.hooks.has_guard() {
                        self.inner.hooks.jitter()
                    } else {
                        0.0
                    };
                    let interval = pool.image_min_interval;
                    token.with(|f| f.image_next_at = now + interval + jitter);
                    return;
                }
                wait
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    // ---------------- 健康标记 ----------------

    pub fn mark_rate_limited(&self, token: &Token, retry_after: f64) {
        let until = self.now() + gate_core::upstream_policy::clamp_retry_after_f64(retry_after);
        token.with(|f| {
            f.blocked_until = until;
            f.fails += 1;
        });
    }

    /// 连续 3 次 403（中间没有成功）→ 冷却并告警；冷却按轮次 5 → 15 → 45 → 60 分钟退避，
    /// 退避档位只在 24 小时内没有 403 时归零（间歇性 403 不会被一次成功打回 5 分钟）。
    pub fn mark_forbidden(&self, token: &Token) {
        let now = self.now();
        let tripped = token.with(|f| {
            f.fails += 1;
            if f.forbidden_last != 0.0 && now - f.forbidden_last > 86400.0 {
                f.forbidden_trips = 0;
            }
            f.forbidden_last = now;
            f.forbidden_streak += 1;
            if f.forbidden_streak >= 3 {
                let minutes =
                    60_i64.min(5 * 3_i64.saturating_pow(f.forbidden_trips.clamp(0, 30) as u32));
                f.forbidden_trips += 1;
                f.forbidden_streak = 0;
                f.blocked_until = f.blocked_until.max(now + minutes as f64 * 60.0);
                Some((minutes, f.forbidden_trips))
            } else {
                None
            }
        });
        if let Some((minutes, trips)) = tripped {
            self.inner.hooks.event(
                "upstream_403_cooldown",
                &format!("NovelAI 连续返回 403，账号可能被上游封控。该 Token（{}）冷却 {minutes} 分钟后自动重试（第 {trips} 轮），请检查上游账号状态。", token.token_id),
                1800.0,
            );
        }
    }

    fn warn_account(&self, status: u16) {
        if status == 402 || status == 403 {
            let reason = if status == 402 {
                "需要付费 / 订阅或 Anlas 不足"
            } else {
                "拒绝访问，账号可能受限"
            };
            self.inner.hooks.event(
                &format!("upstream_{status}"),
                &format!("NovelAI 返回 {status}（{reason}），请检查上游账号状态。"),
                1800.0,
            );
        }
    }

    pub fn mark_unauthorized(&self, token: &Token) {
        token.with(|f| f.disabled = true);
    }

    pub fn mark_ok(&self, token: &Token) {
        let now = self.now();
        token.with(|f| {
            f.fails = 0;
            f.forbidden_streak = 0;
            f.last_ok = now;
            // forbidden_trips 故意不清零（见 mark_forbidden）
        });
    }

    fn event_401(&self) {
        self.inner.hooks.event(
            "upstream_401",
            "NovelAI 返回 401：上游 Token 已失效，生图和文本全部不可用，请尽快更换 Token。",
            1800.0,
        );
    }

    /// 没有可用 Token 时给成员的说明：先区分「冷却 / 停用」和「额度真的用完」。
    pub async fn unavailable(&self, requires_anlas: bool, v5_free: bool) -> UpstreamError {
        let pool = self.inner.pool.lock().await;
        let now = self.now();
        let usable: Vec<_> = pool.tokens.iter().filter(|t| t.usable(now)).collect();
        if usable.is_empty() {
            let cooling: Vec<f64> = pool
                .tokens
                .iter()
                .filter_map(|t| {
                    t.with(|f| {
                        (f.admin_enabled && !f.disabled && now < f.blocked_until)
                            .then_some(f.blocked_until)
                    })
                })
                .collect();
            if !cooling.is_empty() {
                let earliest = cooling.into_iter().fold(f64::INFINITY, f64::min);
                let wait = ((earliest - self.now()) as i64).max(1);
                return UpstreamError::new(
                    503,
                    format!(
                        "上游账号暂时限流冷却中，约 {wait} 秒后恢复（不是额度用完），请稍后再试"
                    ),
                );
            }
            return UpstreamError::new(503, "上游账号暂时不可用（不是额度用完），请稍后再试");
        }
        if requires_anlas {
            return UpstreamError::new(503, "没有允许使用 Anlas 的上游令牌，无法生成此图片");
        }
        if v5_free {
            return UpstreamError::new(429, "可用上游令牌的今日 V5 免费图片额度已用完");
        }
        UpstreamError::new(503, "上游令牌全部被限流或不可用，请稍后再试")
    }

    // ---------------- 请求 ----------------

    /// 普通请求（图片工具 / 补全 / 非流式出图）。出图请求不自动重试；普通请求只对明确的 429 换 Token 一次。
    pub async fn request(
        &self,
        mut o: RequestOpts<'_>,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
    ) -> Result<UpResponse, UpstreamError> {
        let mut attempts = 0;
        while attempts < 2 {
            attempts += 1;
            let Some(token) = self
                .pick_token(
                    o.requires_anlas,
                    o.v5_free,
                    o.image_lane && o.wait_for_image_slot,
                    false,
                )
                .await?
            else {
                return Err(self.unavailable(o.requires_anlas, o.v5_free).await);
            };
            let mut fl = InFlight {
                inner: self.inner.clone(),
                token: token.clone(),
                succeeded: false,
                v5_free: o.v5_free,
                image_count: o.image_count,
                send_started: false,
                permit: None,
                done: false,
            };
            let outcome = self
                .request_once(&mut o, cb, allowance, obs, &token, &mut fl)
                .await;
            fl.finish().await;
            match outcome {
                Attempt::Done(r) => return r,
                Attempt::Retry => continue,
            }
        }
        Err(UpstreamError::new(429, "上游限流(429)，请降低频率后重试"))
    }

    async fn request_once(
        &self,
        o: &mut RequestOpts<'_>,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
        token: &Arc<Token>,
        fl: &mut InFlight<H>,
    ) -> Attempt {
        let mut response_status: Option<u16> = None;
        if o.image_lane {
            let wait = async {
                let permit = token.image_slots.acquire().await;
                fl.permit = Some(permit);
                self.wait_for_token_image_slot(token).await;
            };
            if tokio::time::timeout(Duration::from_secs_f64(o.queue_timeout.max(0.0)), wait)
                .await
                .is_err()
            {
                return Attempt::Done(Err(UpstreamError::new(
                    429,
                    "上游图片任务排队超时，请稍后再试",
                )));
            }
        }
        if o.v5_free
            && let (Some(resolve), Some(a)) = (cb.resolve_v5_cost.as_ref(), allowance)
        {
            match self.resolve_v5_cost(token, a, resolve).await {
                Ok(true) => {
                    self.finish_v5_reservation(token, false, true).await;
                    o.v5_free = false;
                    fl.v5_free = false;
                    o.requires_anlas = true;
                }
                Ok(false) => {}
                Err(e) => return Attempt::Done(Err(e)),
            }
        }
        // 与 set_admin_enabled 串行：之后发出的请求一定能看到新的启停状态（请求本身不持锁）
        drop(token.dispatch_lock.lock().await);
        if !token.with(|f| f.admin_enabled) {
            return Attempt::Retry;
        }
        if let Some(check) = cb.before_dispatch.as_ref()
            && let Err(e) = check()
        {
            return Attempt::Done(Err(e));
        }
        fl.send_started = true;
        obs.sent();
        if o.image_lane && o.wait_for_image_slot && self.inner.hooks.has_guard() {
            self.inner
                .hooks
                .record_start(&token.token_id, request_images(o.json));
        }
        let req = self.build(token, o.method.clone(), &o.url, o.json, o.accept);
        let sent = async {
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            response_status = Some(status);
            let headers = resp.headers().clone();
            match o.max_response_bytes {
                None => Ok::<_, wreq::Error>(Ok(UpResponse {
                    status,
                    headers,
                    body: resp.bytes().await?,
                })),
                Some(cap) => {
                    let mut data = Vec::new();
                    let mut stream = resp.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        if data.len() + chunk.len() > cap {
                            let uncertain =
                                o.image_lane && (status == 200 || status == 201 || status >= 500);
                            return Ok(Err(UpstreamError::uncertain(
                                502,
                                "上游图片工具结果过大",
                                uncertain,
                            )));
                        }
                        data.extend_from_slice(&chunk);
                    }
                    let mut headers = headers;
                    headers.remove(wreq::header::CONTENT_ENCODING);
                    headers.remove(wreq::header::CONTENT_LENGTH);
                    Ok(Ok(UpResponse {
                        status,
                        headers,
                        body: Bytes::from(data),
                    }))
                }
            }
        };
        let result = if o.max_response_bytes.is_some() {
            match tokio::time::timeout(Duration::from_secs(300), sent).await {
                Ok(r) => r.map_err(Transport::Http),
                Err(_) => Err(Transport::Timeout),
            }
        } else {
            sent.await.map_err(Transport::Http)
        };
        let resp = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Attempt::Done(Err(e)),
            Err(t) => {
                if !o.image_lane {
                    let mut e = UpstreamError::new(502, format!("上游请求失败：{t}"));
                    e.transport = true;
                    return Attempt::Done(Err(e));
                }
                // 已发送请求的读写故障可能产生扣款；明确的 4xx、连接没建立起来的除外
                let uncertain = fl.send_started
                    && response_status.is_none_or(|s| s == 200 || s == 201 || s >= 500)
                    && t.after_send();
                return Attempt::Done(Err(UpstreamError::uncertain(
                    502,
                    "上游图片请求连接中断或超时，未记费；请勿自动重试",
                    uncertain,
                )));
            }
        };
        obs.status(resp.status);
        if (resp.status == 200 || resp.status == 201)
            && o.image_count > 0
            && !self
                .inner
                .hooks
                .validate_image_result(resp.body.clone(), o.image_count)
                .await
        {
            return Attempt::Done(Err(UpstreamError::uncertain(
                502,
                "上游未返回完整有效的图片结果",
                true,
            )));
        }
        let succeeded = resp.status == 200 || resp.status == 201;
        fl.succeeded = succeeded;
        if resp.status == 429 {
            let retry_after = retry_after_or_default(
                header_str(&resp.headers, "retry-after").as_deref(),
                self.now(),
            );
            self.mark_rate_limited(token, retry_after);
            if let Some(cb) = cb.on_rate_limited.as_ref() {
                cb(retry_after).await;
            }
            if o.image_lane {
                return Attempt::Done(Err(UpstreamError::new(
                    429,
                    "上游限流(429)，全站图片生成已进入冷却",
                )));
            }
            return Attempt::Retry;
        }
        if resp.status == 403 && o.image_lane {
            // 「模型不存在」也是 403：不算账号被拒
            let peek = String::from_utf8_lossy(&resp.body[..resp.body.len().min(300)]).into_owned();
            if gate_core::upstream_policy::is_model_level_403(&peek) {
                return Attempt::Done(Err(UpstreamError::new(
                    400,
                    "上游不认识这个模型：请换用本站白名单里的模型",
                )));
            }
            self.mark_forbidden(token);
            return Attempt::Done(Err(UpstreamError::new(
                403,
                "上游拒绝了请求（HTTP 403），该账号已临时隔离保护",
            )));
        }
        self.warn_account(resp.status);
        if resp.status == 401 {
            self.mark_unauthorized(token);
            self.event_401();
            return Attempt::Done(Err(UpstreamError::new(
                502,
                "上游令牌已失效（401），请站长更换 NovelAI Token",
            )));
        }
        if succeeded {
            self.mark_ok(token);
        }
        Attempt::Done(Ok(resp))
    }

    async fn resolve_v5_cost(
        &self,
        token: &Token,
        allowance: &dyn Allowance,
        callback: &(dyn Fn(bool) -> BoxFuture<'static, ()> + Send + Sync + '_),
    ) -> Result<bool, UpstreamError> {
        let exhausted = {
            let _d = token.dispatch_lock.lock().await;
            if !token.with(|f| f.admin_enabled) {
                return Err(UpstreamError::new(
                    503,
                    "上游令牌已停用，未查询额度或发送生图",
                ));
            }
            allowance
                .resolve(token)
                .await
                .map_err(|m| UpstreamError::new(503, m))?
        };
        if exhausted && !token.with(|f| f.allow_anlas) {
            return Err(UpstreamError::new(
                503,
                "该上游账号 V5 额度已耗尽，且未允许使用 Anlas；未发送生图",
            ));
        }
        callback(exhausted).await;
        Ok(exhausted)
    }

    /// 打开图片流（不重试）。返回的 [`ImageStream`] 读完后调用 `finish(completed_images)`；
    /// 中途丢弃也会按已确认的张数结算。
    #[allow(clippy::too_many_arguments)] // 与旧版 image_stream 的参数一一对应
    pub async fn image_stream(
        &self,
        url: &str,
        body: &Value,
        requires_anlas: bool,
        v5_free: bool,
        queue_timeout: f64,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
    ) -> Result<ImageStream<H>, UpstreamError> {
        let Some(token) = self
            .pick_token(requires_anlas, v5_free, true, false)
            .await?
        else {
            return Err(self.unavailable(requires_anlas, v5_free).await);
        };
        let mut fl = InFlight {
            inner: self.inner.clone(),
            token: token.clone(),
            succeeded: false,
            v5_free,
            image_count: 0,
            send_started: false,
            permit: None,
            done: false,
        };
        let wait = async {
            fl.permit = Some(token.image_slots.acquire().await);
            self.wait_for_token_image_slot(&token).await;
        };
        if tokio::time::timeout(Duration::from_secs_f64(queue_timeout.max(0.0)), wait)
            .await
            .is_err()
        {
            fl.finish().await;
            return Err(UpstreamError::new(429, "上游图片任务排队超时，请稍后再试"));
        }
        if v5_free && let (Some(resolve), Some(a)) = (cb.resolve_v5_cost.as_ref(), allowance) {
            match self.resolve_v5_cost(&token, a, resolve).await {
                Ok(true) => {
                    // 与旧版一致：图片流只把这次改成「非免费 V5」，不改 requires_anlas
                    self.finish_v5_reservation(&token, false, true).await;
                    fl.v5_free = false;
                }
                Ok(false) => {}
                Err(e) => {
                    fl.finish().await;
                    return Err(e);
                }
            }
        }
        let accept = if body
            .get("parameters")
            .and_then(|p| p.get("stream"))
            .and_then(Value::as_str)
            == Some("msgpack")
        {
            "application/x-msgpack"
        } else {
            "text/event-stream"
        };
        let req = self.build(&token, wreq::Method::POST, url, Some(body), accept);
        // 与 set_admin_enabled 串行（只在检查启停时持锁，请求本身不持锁）
        drop(token.dispatch_lock.lock().await);
        if !token.with(|f| f.admin_enabled) {
            fl.finish().await;
            return Err(UpstreamError::new(503, "上游令牌已停用，未发送生图"));
        }
        if let Some(check) = cb.before_dispatch.as_ref()
            && let Err(e) = check()
        {
            fl.finish().await;
            return Err(e);
        }
        if let Some(on) = cb.on_dispatch.as_ref() {
            on();
        }
        fl.send_started = true;
        obs.sent();
        if self.inner.hooks.has_guard() {
            self.inner
                .hooks
                .record_start(&token.token_id, request_images(Some(body)));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let uncertain = fl.send_started && Transport::Http(e).after_send();
                fl.finish().await;
                return Err(UpstreamError::uncertain(
                    502,
                    "上游图片流连接中断，请检查任务结果后再决定是否重试",
                    uncertain,
                ));
            }
        };
        let status = resp.status().as_u16();
        obs.status(status);
        let err = if status == 429 {
            let ra = retry_after_or_default(
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
                self.now(),
            );
            self.mark_rate_limited(&token, ra);
            if let Some(cb) = cb.on_rate_limited.as_ref() {
                cb(ra).await;
            }
            Some(UpstreamError::new(
                429,
                "上游限流(429)，全站图片生成已进入冷却",
            ))
        } else if status == 403 {
            let peek = peek_body(resp).await;
            if gate_core::upstream_policy::is_model_level_403(&peek) {
                fl.finish().await;
                return Err(UpstreamError::new(
                    400,
                    "上游不认识这个模型：请换用本站白名单里的模型",
                ));
            }
            self.mark_forbidden(&token);
            fl.finish().await;
            return Err(UpstreamError::new(
                403,
                "上游拒绝了请求（HTTP 403），该账号已临时隔离保护",
            ));
        } else {
            self.warn_account(status);
            if status == 401 {
                self.mark_unauthorized(&token);
                self.event_401();
                Some(UpstreamError::new(
                    502,
                    "上游令牌已失效（401），请站长更换 NovelAI Token",
                ))
            } else if status != 200 && status != 201 {
                let s = if (400..=599).contains(&status) {
                    status
                } else {
                    502
                };
                Some(UpstreamError::uncertain(
                    s,
                    format!("上游图片流请求失败（HTTP {s}）"),
                    status >= 500,
                ))
            } else {
                None
            }
        };
        if let Some(e) = err {
            fl.finish().await;
            return Err(e);
        }
        Ok(ImageStream {
            response: Some(resp),
            flight: Some(fl),
            client: self.clone(),
            token,
            completed_images: 0,
        })
    }

    /// 打开文本流（不重试）；调用方持有并读取响应。
    pub async fn text_stream(
        &self,
        url: &str,
        body: &Value,
        obs: &dyn Observer,
    ) -> Result<wreq::Response, UpstreamError> {
        let Some(token) = self.pick_token(false, false, false, true).await? else {
            return Err(UpstreamError::new(
                503,
                "上游令牌全部被限流或不可用，请稍后再试",
            ));
        };
        let req = self.build(
            &token,
            wreq::Method::POST,
            url,
            Some(body),
            "text/event-stream",
        );
        let resp = {
            let _d = token.dispatch_lock.lock().await;
            if !token.with(|f| f.admin_enabled) {
                return Err(UpstreamError::new(503, "上游令牌已停用，未发送请求"));
            }
            req.send().await.map_err(|e| {
                let mut err = UpstreamError::new(502, format!("上游请求失败：{e}"));
                err.transport = true;
                err
            })?
        };
        let status = resp.status().as_u16();
        if status != 200 && status != 201 {
            if status == 401 {
                self.mark_unauthorized(&token);
            }
            obs.status(status);
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            // 文本 403 要窥探一小段判断是不是「模型不存在」；其余错误体按政策不读
            let peek = if status == 403 {
                peek_body(resp).await
            } else {
                drop(resp);
                String::new()
            };
            if status == 401 {
                self.event_401();
                return Err(UpstreamError::new(
                    502,
                    "上游令牌已失效（401），请站长更换 NovelAI Token",
                ));
            }
            if status == 403 {
                if gate_core::upstream_policy::is_model_level_403(&peek) {
                    return Err(UpstreamError::new(
                        400,
                        "上游不认识这个文本模型；本站可用的文本模型：llama-3-erato-v1、kayra-v1",
                    ));
                }
                let tripped = token.with(|f| {
                    f.fails += 1;
                    f.text_forbidden += 1;
                    if f.text_forbidden >= 3 {
                        f.text_forbidden = 0;
                        true
                    } else {
                        false
                    }
                });
                if tripped {
                    self.inner.hooks.event(
                        "upstream_403_text",
                        "NovelAI 对文本 / 语音请求连续返回 403。生图仍可用，但请检查上游账号状态。",
                        1800.0,
                    );
                }
                return Err(UpstreamError::new(
                    403,
                    "上游拒绝了请求（HTTP 403），该账号已临时隔离保护",
                ));
            }
            if status == 429 {
                let wait = gate_core::upstream_policy::clamp_retry_after_f64(
                    retry_after_or_default(retry_after.as_deref(), self.now()),
                );
                let now = self.now();
                token.with(|f| {
                    f.fails += 1;
                    f.text_blocked_until = f.text_blocked_until.max(now + wait);
                });
                return Err(UpstreamError::new(
                    429,
                    format!(
                        "上游限流(429)，文本 / 语音通道暂停约 {} 秒，请稍后再试",
                        wait as i64
                    ),
                ));
            }
            let s = if matches!(status, 400 | 422 | 503) {
                status
            } else {
                502
            };
            return Err(UpstreamError::new(
                s,
                format!("上游文本请求失败（HTTP {status}）"),
            ));
        }
        token.with(|f| f.text_forbidden = 0);
        self.mark_ok(&token);
        Ok(resp)
    }

    /// 只读查询（订阅 / Anlas 余额 / Token 校验）：与出图同一套浏览器请求头，不跟随重定向，不碰调度。
    pub async fn get_json_with(
        &self,
        token: &str,
        profile_index: usize,
        url: &str,
        timeout: Duration,
        max_bytes: usize,
        follow_redirects: bool,
    ) -> Result<(u16, HeaderMap, Bytes), String> {
        let profile =
            headers::profile_for(profile_index, self.inner.cfg.custom_user_agent.as_deref());
        let list = headers::ordered(&profile, Some(token), "application/json", false);
        let (map, orig) = headers::to_wreq(&list);
        let mut req = self.inner.http[profile_index % 3]
            .get(url)
            .default_headers(false)
            .headers(map)
            .orig_headers(orig)
            .timeout(timeout);
        if !follow_redirects {
            req = req.redirect(wreq::redirect::Policy::none());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let hdrs = resp.headers().clone();
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if data.len() + chunk.len() > max_bytes {
                return Err("响应过大".into());
            }
            data.extend_from_slice(&chunk);
        }
        Ok((status, hdrs, Bytes::from(data)))
    }

    /// 后台「添加 / 替换 Token」前的有效性检查：查订阅（只读，不消耗额度，不记录 Token）。
    pub async fn verify_token(&self, token: &str) -> Result<i64, String> {
        let url = format!("{}/user/subscription", self.image_host());
        let (status, _, body) = self
            .get_json_with(token, 0, &url, Duration::from_secs(12), 1 << 20, true)
            .await
            .map_err(|_| "无法连接 NovelAI 验证这把 Token，请稍后再试".to_string())?;
        if status == 401 || status == 403 {
            return Err("NovelAI 拒绝了这把 Token（无效，或已被重置）".into());
        }
        if status != 200 {
            return Err(format!("NovelAI 返回 {status}，暂时无法验证"));
        }
        let info: Value = serde_json::from_slice(&body)
            .map_err(|_| "NovelAI 返回了无法识别的订阅信息".to_string())?;
        let tier = info
            .get("tier")
            .and_then(gate_core::pyval::int_like)
            .ok_or("NovelAI 返回了无法识别的订阅信息")?;
        if !info.get("active").is_some_and(gate_core::pyval::truthy) {
            return Err("这把 Token 对应的账号没有有效订阅".into());
        }
        Ok(tier)
    }

    /// 后台「上游令牌池」的状态行（不含当天计数，由调用方补）。
    pub async fn status_rows(&self) -> Vec<(Arc<Token>, TokenFields)> {
        let pool = self.inner.pool.lock().await;
        pool.tokens
            .iter()
            .map(|t| (t.clone(), t.fields()))
            .collect()
    }
}

fn renumber(tokens: &[Arc<Token>]) {
    for (i, t) in tokens.iter().enumerate() {
        t.with(|f| f.position = i + 1);
    }
}

/// Retry-After 解析；旧版遇到超长数字 / 越界日期会抛异常导致冷却不生效（见 docs/v3-deviations.md #1），这里按默认 20 秒。
fn retry_after_or_default(header: Option<&str>, now: f64) -> f64 {
    gate_core::upstream_policy::parse_retry_after(header, now).unwrap_or(20.0)
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// 读上游错误响应体的一小段（≤ 300 字节、≤ 3 秒），**只用来判断 403 属于哪一类**，绝不回显给成员。
async fn peek_body(resp: wreq::Response) -> String {
    let read = async {
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            data.extend_from_slice(&chunk);
            if data.len() >= 300 {
                break;
            }
        }
        data.truncate(300);
        String::from_utf8_lossy(&data).into_owned()
    };
    tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .unwrap_or_default()
}

enum Attempt {
    Done(Result<UpResponse, UpstreamError>),
    Retry,
}

enum Transport {
    Http(wreq::Error),
    Timeout,
}

impl Transport {
    /// 是否发生在请求已经发出之后（读写中断 / 读超时 / 协议错误）——可能已扣费。
    /// 建连失败、建连超时、TLS 握手失败明确没有发出，不算。
    fn after_send(&self) -> bool {
        match self {
            Transport::Timeout => true,
            Transport::Http(e) => !e.is_connect() && !e.is_builder() && !e.is_redirect(),
        }
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Http(e) => write!(f, "{e}"),
            Transport::Timeout => write!(f, "超时"),
        }
    }
}

/// 已打开的上游图片流。调用方逐块读取，确认完整的最终图片后累加 `completed_images`，
/// 最后调用 [`ImageStream::finish`]；被丢弃时按已确认张数在后台结算。
pub struct ImageStream<H: Hooks> {
    response: Option<wreq::Response>,
    flight: Option<InFlight<H>>,
    client: NaiClient<H>,
    token: Arc<Token>,
    pub completed_images: usize,
}

impl<H: Hooks> ImageStream<H> {
    pub fn headers(&self) -> Option<&HeaderMap> {
        self.response.as_ref().map(|r| r.headers())
    }

    /// 取走正文字节流（只能取一次）。
    pub fn take_body(
        &mut self,
    ) -> Option<impl futures::Stream<Item = Result<Bytes, wreq::Error>> + use<H>> {
        self.response.take().map(|r| r.bytes_stream())
    }

    /// 结算：有确认的图就记成功并标记账号健康。
    pub async fn finish(mut self) {
        self.response = None;
        if let Some(mut fl) = self.flight.take() {
            let count = self.completed_images;
            fl.succeeded = count > 0;
            fl.image_count = count;
            fl.finish().await;
            if count > 0 {
                self.client.mark_ok(&self.token);
            }
        }
    }
}

impl<H: Hooks> Drop for ImageStream<H> {
    fn drop(&mut self) {
        if let Some(mut fl) = self.flight.take() {
            let count = self.completed_images;
            fl.succeeded = count > 0;
            fl.image_count = count;
            if count > 0 {
                self.client.mark_ok(&self.token);
            }
            drop(fl); // InFlight 的 Drop 在后台完成结算
        }
    }
}
