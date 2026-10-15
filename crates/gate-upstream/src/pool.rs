//! 令牌池：每把上游 Token 的状态、后台增删改、选号（轮询 + 账号保护 + V5 预留）、健康 / 冷却标记。
//!
//! 池锁（`tokio::sync::Mutex`）就是旧版 `self._lock`：选号、V5 预留与释放、后台增删改在它下面串行，
//! 会跨 await 持有（选号时要问账号保护和库里的 V5 计数）。出图节奏不在这里（见 [`crate::scheduler`]）。

use crate::error::UpstreamError;
use crate::headers::{self, BrowserProfile};
use crate::hooks::Hooks;
use crate::limiter::AdjustableLimiter;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    /// 单调时钟（客户端启动以来的秒）：下一张图最早何时可以发。只由 [`crate::scheduler`] 改。
    pub image_next_at: f64,
    pub forbidden_streak: i64,
    pub forbidden_trips: i64,
    pub forbidden_last: f64,
}

pub struct Token {
    pub token: String,
    pub token_id: String,
    pub profile: BrowserProfile,
    profile_index: usize,
    pub image_slots: AdjustableLimiter,
    /// 与「停用 Token」串行：之后发出的请求一定能看到新的启停状态（请求本身不持锁）。
    pub(crate) dispatch_lock: tokio::sync::Mutex<()>,
    fields: Mutex<TokenFields>,
}

impl Token {
    fn new(token: String, index: usize, v5_daily_limit: i64, allow_anlas: bool) -> Self {
        Self {
            token_id: headers::token_id(&token),
            token,
            profile: headers::profile_for(index),
            profile_index: index % headers::POOL.len(),
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
            }),
        }
    }

    pub fn fields(&self) -> TokenFields {
        self.fields.lock().expect("token").clone()
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut TokenFields) -> R) -> R {
        f(&mut self.fields.lock().expect("token"))
    }

    /// `admin_enabled and not disabled and now >= blocked_until`
    pub fn usable(&self, now: f64) -> bool {
        self.with(|f| f.admin_enabled && !f.disabled && now >= f.blocked_until)
    }

    /// 这把 Token 用的指纹序号（取 Token 加入时的位置；删除 / 重排后位置会变，指纹不变）。
    pub fn profile_index(&self) -> usize {
        self.profile_index
    }

    // ---------------- 健康标记 ----------------

    pub(crate) fn mark_rate_limited(&self, hooks: &dyn Hooks, retry_after: f64) {
        let until = hooks.now() + gate_core::upstream_policy::clamp_retry_after_f64(retry_after);
        self.with(|f| {
            f.blocked_until = until;
            f.fails += 1;
        });
    }

    /// 连续 3 次 403（中间没有成功）→ 冷却并告警；冷却按轮次 5 → 15 → 45 → 60 分钟退避，
    /// 退避档位只在 24 小时内没有 403 时归零（间歇性 403 不会被一次成功打回 5 分钟）。
    pub(crate) fn mark_forbidden(&self, hooks: &dyn Hooks) {
        let now = hooks.now();
        let tripped = self.with(|f| {
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
            hooks.event(
                "upstream_403_cooldown",
                &format!("NovelAI 连续返回 403，账号可能被上游封控。该 Token（{}）冷却 {minutes} 分钟后自动重试（第 {trips} 轮），请检查上游账号状态。", self.token_id),
                1800.0,
            );
        }
    }

    pub(crate) fn mark_unauthorized(&self) {
        self.with(|f| f.disabled = true);
    }

    pub(crate) fn mark_ok(&self, hooks: &dyn Hooks) {
        let now = hooks.now();
        self.with(|f| {
            f.fails = 0;
            f.forbidden_streak = 0;
            f.last_ok = now;
            // forbidden_trips 故意不清零（见 mark_forbidden）
        });
    }
}

/// 402 / 403：提醒站长检查上游账号。
pub(crate) fn warn_account(hooks: &dyn Hooks, status: u16) {
    if status == 402 || status == 403 {
        let reason = if status == 402 {
            "需要付费 / 订阅或 Anlas 不足"
        } else {
            "拒绝访问，账号可能受限"
        };
        hooks.event(
            &format!("upstream_{status}"),
            &format!("NovelAI 返回 {status}（{reason}），请检查上游账号状态。"),
            1800.0,
        );
    }
}

pub(crate) fn event_401(hooks: &dyn Hooks) {
    hooks.event(
        "upstream_401",
        "NovelAI 返回 401：上游 Token 已失效，生图全部不可用，请尽快更换 Token。",
        1800.0,
    );
}

struct PoolState {
    tokens: Vec<Arc<Token>>,
    rr: usize,
}

pub(crate) struct Pool {
    state: tokio::sync::Mutex<PoolState>,
}

impl Pool {
    /// `tokens`：(Token, 每日免费 V5 上限, 是否允许 Anlas)。
    pub(crate) fn new(tokens: Vec<(String, i64, bool)>) -> Self {
        let tokens = tokens
            .into_iter()
            .enumerate()
            .map(|(i, (t, v5, anlas))| Arc::new(Token::new(t, i, v5, anlas)))
            .collect();
        Self {
            state: tokio::sync::Mutex::new(PoolState { tokens, rr: 0 }),
        }
    }

    pub(crate) async fn tokens(&self) -> Vec<Arc<Token>> {
        self.state.lock().await.tokens.clone()
    }

    pub(crate) async fn find(&self, token_id: &str) -> Option<Arc<Token>> {
        let pool = self.state.lock().await;
        pool.tokens.iter().find(|t| t.token_id == token_id).cloned()
    }

    /// 后台设的出图并发：1～4（这是唯一的开关，没有全局覆盖）。
    fn effective_concurrency(limit: i64) -> i64 {
        limit.clamp(1, 4)
    }

    // ---------------- 后台增删改 ----------------

    pub(crate) async fn apply_saved(
        &self,
        v5_limits: &HashMap<String, i64>,
        enabled: &HashMap<String, bool>,
        concurrency: &HashMap<String, i64>,
    ) {
        let pool = self.state.lock().await;
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
                t.image_slots
                    .resize(Self::effective_concurrency(*c) as usize);
            }
        }
    }

    pub(crate) async fn add(
        &self,
        token: String,
        allow_anlas: bool,
        saved_v5: Option<i64>,
        saved_enabled: Option<bool>,
    ) -> Result<Arc<Token>, String> {
        let mut pool = self.state.lock().await;
        let new = Token::new(token, pool.tokens.len(), 0, allow_anlas);
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

    pub(crate) async fn replace(
        &self,
        token_id: &str,
        token: String,
    ) -> Result<(Arc<Token>, String), String> {
        let mut pool = self.state.lock().await;
        let Some(index) = pool.tokens.iter().position(|t| t.token_id == token_id) else {
            return Err("令牌不存在".into());
        };
        let old = pool.tokens[index].clone();
        let of = old.fields();
        let new = Token::new(token, index, of.v5_daily_limit, of.allow_anlas);
        if new.token_id != old.token_id && pool.tokens.iter().any(|t| t.token_id == new.token_id) {
            return Err("这把 Token 已经在令牌池的另一个位置".into());
        }
        new.with(|f| f.admin_enabled = of.admin_enabled);
        new.image_slots.resize(old.image_slots.limit());
        let new = Arc::new(new);
        pool.tokens[index] = new.clone();
        renumber(&pool.tokens);
        Ok((new, old.token_id.clone()))
    }

    pub(crate) async fn remove(&self, token_id: &str) -> Result<bool, String> {
        let mut pool = self.state.lock().await;
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

    pub(crate) async fn set_v5_daily_limit(&self, token_id: &str, limit: i64) -> bool {
        let pool = self.state.lock().await;
        match pool.tokens.iter().find(|t| t.token_id == token_id) {
            Some(t) => {
                t.with(|f| f.v5_daily_limit = limit);
                true
            }
            None => false,
        }
    }

    /// 停用 / 启用。停用会等这把 Token 手上的图跑完才返回（之后发出的请求一定能看到新状态）。
    pub(crate) async fn set_admin_enabled(&self, token_id: &str, enabled: bool) -> bool {
        let Some(t) = self.find(token_id).await else {
            return false;
        };
        {
            let _d = t.dispatch_lock.lock().await;
            let _p = self.state.lock().await;
            t.with(|f| f.admin_enabled = enabled);
        }
        if !enabled {
            t.image_slots.wait_idle().await;
        }
        true
    }

    pub(crate) async fn set_image_concurrency(&self, token_id: &str, limit: i64) -> Option<i64> {
        let pool = self.state.lock().await;
        let t = pool.tokens.iter().find(|t| t.token_id == token_id)?;
        let eff = Self::effective_concurrency(limit);
        t.image_slots.resize(eff as usize);
        Some(eff)
    }

    // ---------------- 选号与预留 ----------------

    /// 选一把符合费用策略的 Token；V5 免费额度在这里原子预留。
    pub(crate) async fn pick(
        &self,
        hooks: &dyn Hooks,
        requires_anlas: bool,
        v5_free: bool,
        image_job: bool,
    ) -> Result<Option<Arc<Token>>, UpstreamError> {
        let mut pool = self.state.lock().await;
        let now = hooks.now();
        let mut usable: Vec<Arc<Token>> = pool
            .tokens
            .iter()
            .filter(|t| t.usable(now) && (!requires_anlas || t.with(|f| f.allow_anlas)))
            .cloned()
            .collect();
        if usable.is_empty() {
            return Ok(None);
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
    pub(crate) async fn finish_v5_reservation(
        &self,
        hooks: &dyn Hooks,
        token: &Token,
        succeeded: bool,
        v5_free: bool,
    ) {
        if !v5_free {
            return;
        }
        let _pool = self.state.lock().await;
        if succeeded {
            let day = hooks.day();
            if let Err(e) = hooks.bump_upstream_v5(&token.token_id, &day).await {
                tracing::error!(error = %e, "记录上游 V5 计数失败");
            }
        }
        token.with(|f| f.pending_v5 = (f.pending_v5 - 1).max(0));
    }

    /// 没有可用 Token 时给成员的说明：先区分「冷却 / 停用」和「额度真的用完」。
    pub(crate) async fn unavailable(
        &self,
        hooks: &dyn Hooks,
        requires_anlas: bool,
        v5_free: bool,
    ) -> UpstreamError {
        let pool = self.state.lock().await;
        let now = hooks.now();
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
                let wait = ((earliest - hooks.now()) as i64).max(1);
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

    /// 后台「上游令牌池」的状态行（不含当天计数，由调用方补）。
    pub(crate) async fn status_rows(&self) -> Vec<(Arc<Token>, TokenFields)> {
        let pool = self.state.lock().await;
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
