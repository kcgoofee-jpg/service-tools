//! 应用状态：启动时构造一次，`Arc` 共享给所有请求与后台任务（没有全局变量）。
//! 启动顺序复刻旧版 `main.lifespan`：建表迁移 → 运行期限制 → 上游 Token 标识迁移 → Token 设置 →
//! 账号保护 → 回填最近 3 小时出图 → Key 暂停 → 全站冷却 → 停机检测 → 后台密码。

use crate::alerts::Alerter;
use crate::bugs::Bugs;
use crate::env::EnvConfig;
use crate::guard::Guard;
use crate::hooks::GateHooks;
use crate::middleware::StatusStats;
use crate::params::ParamsStore;
use crate::registration::{MemberWeb, RegistrationService};
use crate::runtime::{
    AuthGuard, ImageCooldown, Inflight, KeyPacing, KeyPauses, SlidingWindow, TagQueue,
    UpstreamHealth,
};
use crate::web::Assets;
use gate_core::clock::Clock;
use gate_db::Db;
use gate_upstream::client::{Config as UpConfig, NaiClient};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 运行期可在后台调的四个限制（旧版 RUNTIME_LIMIT_BOUNDS）。
pub const RUNTIME_LIMIT_BOUNDS: [(&str, i64, i64); 4] = [
    ("queue_timeout", 15, 300),
    ("key_image_min_interval", 15, 120),
    ("image_min_interval", 15, 120),
    ("image_429_cooldown_seconds", 60, 3600),
];

/// 运行期限制的当前值（启动时 env，之后可被后台覆盖）。
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub queue_timeout: f64,
    pub key_image_min_interval: f64,
    pub image_min_interval: f64,
    pub image_429_cooldown_seconds: f64,
}

/// 一次出图的额度预留（排队等待期间就占住，防止并发请求一起超额）。
#[derive(Debug, Clone)]
pub struct Reservation {
    pub key_id: i64,
    pub exclude_global_v5: bool,
    pub anlas: f64,
    pub v5: i64,
    pub legacy: i64,
}

#[derive(Default, Clone)]
pub struct Budget {
    pub next_id: u64,
    pub reservations: HashMap<u64, Reservation>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub env: EnvConfig,
    pub db: Db,
    pub clock: Clock,
    pub assets: Assets,
    pub status_stats: StatusStats,
    pub version: &'static str,
    pub params: Arc<ParamsStore>,
    pub alerter: Arc<Alerter>,
    pub announcer: Arc<Alerter>,
    pub bugs: Arc<Bugs>,
    pub guard: Arc<Guard>,
    pub nai: NaiClient<GateHooks>,
    pub limits: RwLock<RuntimeLimits>,
    pub rpm: SlidingWindow<i64>,
    pub login_attempts: SlidingWindow<String>,
    pub auth_guard: AuthGuard,
    pub pacing: KeyPacing,
    pub cooldown: ImageCooldown,
    pub pauses: KeyPauses,
    pub inflight: Inflight,
    pub tags: TagQueue,
    /// 后台运营接口的进程内状态（测试告警节流、运行期限制写锁、Anlas 核对互斥）。
    pub ops: crate::ops::Shared,
    /// V5 官方免费额度缓存（只有真实出图会刷新）。
    pub allowance: crate::allowance::ServerAllowance,
    pub health: UpstreamHealth,
    /// 文本等非图片请求的全站并发（旧版 GLOBAL_CONCURRENCY）。
    pub global_sem: Arc<tokio::sync::Semaphore>,
    pub key_sems: Mutex<HashMap<i64, Arc<tokio::sync::Semaphore>>>,
    pub global_waiting: AtomicI64,
    pub global_active: AtomicI64,
    /// 额度预留：只在核对 / 修改预留时持锁，绝不跨上游 HTTP 持有。
    pub budget: tokio::sync::Mutex<Budget>,
    pub budget_idle: tokio::sync::Notify,
    /// 后台改过的密码（scrypt 哈希）；None 时用 ADMIN_PASSWORD。
    pub admin_pw_hash: RwLock<Option<String>>,
    /// 会话签名密钥（SECRET_KEY 或 data/secret_key）。
    pub secret_key: String,
    /// 记账等必须写完的后台任务（关停时等待）。
    pub tasks: TaskTracker,
    pub shutdown: CancellationToken,
    /// 近 1 分钟的请求结果（首页实时图）。
    pub live_events: Mutex<std::collections::VecDeque<(f64, String)>>,
    /// 最近 30 分钟上游 5xx：(时间, key_id)。只有不止一把 Key 遇到才算上游故障（见 `note_upstream_5xx`）。
    pub recent_5xx: Mutex<std::collections::VecDeque<(f64, i64)>>,
    /// 实时看板快照（`/public/live` 与 SSE 共用）、`/public/status` 缓存、前端报错限流。
    pub live: crate::live::Hub,
    /// Discord 自助领 Key（`env.registration` 配齐才有；否则 `/self-register/*` 回 503）。
    pub registration: Option<Arc<RegistrationService>>,
    /// 成员网页接口的进程内限流状态（捐赠提交、原图打包）。
    pub member_web: MemberWeb,
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// 会话签名密钥：SECRET_KEY 优先；否则读 data/secret_key（≥ 32 字符），缺失或损坏就生成新的并原子写入。
pub fn resolve_secret(env: &EnvConfig) -> std::io::Result<String> {
    if !env.secret_key.is_empty() {
        return Ok(env.secret_key.clone());
    }
    let f = env.data_dir.join("secret_key");
    let existing = std::fs::read_to_string(&f)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if existing.chars().count() >= 32 {
        return Ok(existing);
    }
    let fresh = hex::encode(rand::random::<[u8; 32]>());
    crate::fsutil::write_private(&f, fresh.as_bytes())?;
    Ok(fresh)
}

impl AppState {
    /// 构造并完成启动步骤。
    pub async fn build(env: EnvConfig, assets: Assets) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&env.data_dir)?;
        let now = crate::time::now();
        let db = Db::open(env.db_path(), now, gate_core::reasons::BACKFILL)?;
        db.set_rule_version(VERSION);
        let clock = Clock::new(&env.tz, env.day_reset_hour);
        let params = Arc::new(ParamsStore::new(env.private_params_path()));
        let alerter = Arc::new(Alerter::owner(&env));
        let announcer = Arc::new(Alerter::announcer(&env));
        let tasks = TaskTracker::new();
        let bugs = Arc::new(Bugs::new(db.clone(), alerter.clone(), tasks.clone()));
        let guard = Arc::new(Guard::new(
            db.clone(),
            &env.tz,
            env.day_reset_hour,
            params.clone(),
        ));

        // 上游 Token：后台接管过就以 upstream_tokens.json 为准，否则用 NAI_TOKENS
        let managed = gate_upstream::token_store::load(&env.upstream_tokens_path());
        let tokens: Vec<(String, i64, bool)> = match &managed {
            Some(entries) => entries
                .iter()
                .map(|e| (e.token.clone(), 0, e.allow_anlas))
                .collect(),
            None => env
                .nai_tokens
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    (
                        t.clone(),
                        env.nai_token_v5_daily_limits.get(i).copied().unwrap_or(0),
                        env.nai_token_allow_anlas.get(i).copied().unwrap_or(true),
                    )
                })
                .collect(),
        };
        let hooks = GateHooks::new(db.clone(), clock.clone(), guard.clone(), alerter.clone());
        let nai = NaiClient::new(
            UpConfig {
                image_host: env.image_host.clone(),
                text_host: env.text_host.clone(),
                legacy_text_host: env.text_host_legacy.clone(),
                proxy: Some(env.upstream_proxy.clone()).filter(|s| !s.is_empty()),
                custom_user_agent: Some(env.upstream_user_agent.clone()).filter(|s| !s.is_empty()),
                http2: env.upstream_http2,
                post_jitter_min: env.post_request_jitter_min,
                post_jitter_max: env.post_request_jitter_max,
                single_slot_enforced: env.single_image_slot_enforced,
                image_min_interval: env.image_min_interval,
            },
            hooks,
            tokens,
        )?;
        let limits = RuntimeLimits {
            queue_timeout: env.queue_timeout as f64,
            key_image_min_interval: env.key_image_min_interval,
            image_min_interval: env.image_min_interval,
            image_429_cooldown_seconds: env.image_429_cooldown_seconds,
        };
        let secret_key = resolve_secret(&env)?;
        let global = env.global_concurrency.max(1) as usize;
        let allowance = crate::allowance::ServerAllowance::new(db.clone(), alerter.clone());
        let registration = RegistrationService::from_env(&env, db.clone()).map(Arc::new);
        let state = Self(Arc::new(Inner {
            clock,
            assets,
            status_stats: StatusStats::default(),
            version: VERSION,
            params,
            alerter,
            announcer,
            bugs,
            guard,
            nai,
            limits: RwLock::new(limits),
            rpm: SlidingWindow::new(1024),
            login_attempts: SlidingWindow::new(2048),
            auth_guard: AuthGuard::default(),
            pacing: KeyPacing::default(),
            cooldown: ImageCooldown::default(),
            pauses: KeyPauses::default(),
            inflight: Inflight::default(),
            tags: TagQueue::default(),
            ops: crate::ops::Shared::default(),
            allowance,
            health: UpstreamHealth::default(),
            global_sem: Arc::new(tokio::sync::Semaphore::new(global)),
            key_sems: Mutex::new(HashMap::new()),
            global_waiting: AtomicI64::new(0),
            global_active: AtomicI64::new(0),
            budget: tokio::sync::Mutex::new(Budget::default()),
            budget_idle: tokio::sync::Notify::new(),
            admin_pw_hash: RwLock::new(None),
            secret_key,
            tasks,
            shutdown: CancellationToken::new(),
            live_events: Mutex::new(std::collections::VecDeque::new()),
            recent_5xx: Mutex::new(std::collections::VecDeque::new()),
            live: crate::live::Hub::new(),
            registration,
            member_web: MemberWeb::default(),
            env,
            db,
        }));
        state.startup(now).await?;
        Ok(state)
    }

    async fn startup(&self, now: f64) -> anyhow::Result<()> {
        self.load_runtime_limits().await?;
        let tokens = self.nai.tokens().await;
        let ids: Vec<String> = tokens.iter().map(|t| t.token_id.clone()).collect();
        self.db.migrate_upstream_token_ids(&ids).await?;
        let limits = self.db.get_upstream_token_limits().await?;
        let enabled = self.db.get_upstream_token_enabled().await?;
        let conc = self.db.get_upstream_token_image_concurrency().await?;
        self.nai
            .apply_saved(
                &limits.into_iter().collect(),
                &enabled.into_iter().collect(),
                &conc.into_iter().collect(),
            )
            .await;
        self.guard
            .load()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Err(e) = self.guard.seed_hour(&ids, now).await {
            self.bugs.capture(crate::bugs::Report {
                source: "guard_seed",
                error: Some(("GuardIoError", &e.to_string())),
                ..Default::default()
            });
        }
        self.pauses.load(self.db.active_pauses(now).await?);
        self.cooldown.load(
            self.db
                .get_setting("image_cooldown_until")
                .await?
                .as_deref(),
            now,
        );
        // 停机检测：最后一条请求日志距今超过 1 小时 → 从现在重新起算闲置天数（不然停机 ≥ 回收天数后一重启就把所有人删光）
        if let Some(last) = self.db.last_log_ts().await?
            && now - last > 3600.0
        {
            self.db
                .set_setting(
                    "key_inactivity_grace_started_at",
                    &gate_core::pyc::float_repr(now),
                )
                .await?;
            self.db
                .add_admin_action(
                    "系统",
                    "闲置计时重置",
                    "",
                    &format!(
                        "网关停机约 {:.1} 小时，闲置天数从现在重新计算",
                        (now - last) / 3600.0
                    ),
                    true,
                    now,
                )
                .await?;
        }
        *self.admin_pw_hash.write().expect("pw") =
            self.db.get_setting("admin_password_hash").await?;
        if self.env.admin_password.is_empty() {
            tracing::warn!("未设置 ADMIN_PASSWORD，/admin 管理端将无法登录！");
        }
        if tokens.is_empty() {
            tracing::warn!("未设置 NAI_TOKENS，所有生成请求将返回 503");
        }
        Ok(())
    }

    /// 后台保存过的运行期限制在每次重启后优先于 .env（越界值忽略）。
    pub async fn load_runtime_limits(&self) -> anyhow::Result<()> {
        let mut values = Vec::new();
        for (name, lo, hi) in RUNTIME_LIMIT_BOUNDS {
            if let Some(raw) = self.db.get_setting(&format!("runtime_{name}")).await?
                && let Some(v) = gate_core::pyc::parse_int(&raw).filter(|v| (lo..=hi).contains(v))
            {
                values.push((name, v));
            }
        }
        self.apply_runtime_limits(&values).await;
        Ok(())
    }

    pub async fn apply_runtime_limits(&self, values: &[(&str, i64)]) {
        for (name, v) in values {
            let v = *v as f64;
            match *name {
                "key_image_min_interval" => {
                    let old = self.limits.read().expect("limits").key_image_min_interval;
                    if v > old {
                        self.pacing.extend(v).await;
                    }
                    self.limits.write().expect("limits").key_image_min_interval = v;
                }
                "image_min_interval" => {
                    self.nai.set_image_min_interval(v).await;
                    self.limits.write().expect("limits").image_min_interval = v;
                }
                "queue_timeout" => self.limits.write().expect("limits").queue_timeout = v,
                "image_429_cooldown_seconds" => {
                    self.limits
                        .write()
                        .expect("limits")
                        .image_429_cooldown_seconds = v
                }
                _ => {}
            }
        }
    }

    pub fn limits(&self) -> RuntimeLimits {
        *self.limits.read().expect("limits")
    }

    pub fn now(&self) -> f64 {
        crate::time::now()
    }

    /// 当前服务日（7 点切换）。
    pub fn day(&self) -> String {
        self.clock.day(self.now())
    }

    pub fn month(&self) -> String {
        self.clock.month(self.now())
    }

    /// 每把 Key 的请求并发（旧版 key_sem）。
    pub fn key_sem(&self, key_id: i64) -> Arc<tokio::sync::Semaphore> {
        let n = self.env.key_concurrency.max(1) as usize;
        self.key_sems
            .lock()
            .expect("key_sems")
            .entry(key_id)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(n)))
            .clone()
    }

    /// 首页「近 1 分钟」的请求结果。
    pub fn live_note(&self, status: &str) {
        let mut e = self.live_events.lock().expect("live");
        e.push_back((self.now(), status.to_string()));
        while e.len() > 5000 {
            e.pop_front();
        }
    }

    /// 上游 429：暂停全站图片请求（写库 + AIMD + 告警），返回剩余冷却秒数。
    pub async fn block_image_generation(&self, retry_after: f64) -> i64 {
        let now = self.now();
        let until = self.cooldown.block(retry_after, now);
        if let Err(e) = self
            .db
            .set_setting("image_cooldown_until", &gate_core::pyc::float_repr(until))
            .await
        {
            self.bugs.capture(crate::bugs::Report {
                source: "cooldown_save",
                error: Some(("DbError", &e.to_string())),
                ..Default::default()
            });
        }
        let remaining = ((until - now) as i64).max(1);
        match self.guard.on_upstream_429(now).await {
            Ok(Some((old, new))) => self.alerter.notify(
                "guard_hourly_down",
                &format!("上游限流：每小时出图上限自动从 {old} 降到 {new}，之后连续一天没有限流会每天 +10。"),
                0.0,
            ),
            Ok(None) => {}
            Err(e) => {
                self.bugs.capture(crate::bugs::Report { source: "guard_429", error: Some(("GuardIoError", &e.to_string())), ..Default::default() });
            }
        }
        self.alerter.notify(
            "upstream_429",
            &format!("NovelAI 对图片请求返回 429（限流），全站生图已暂停 {remaining} 秒。如果频繁出现，请降低成员人数或调大图片间隔。"),
            1800.0,
        );
        remaining
    }

    /// 记一次上游 5xx；最近 30 分钟内至少 2 把不同的 Key 遇到时返回 true（该告警站长了）。
    /// 旧版每个 500 都告警：一位成员反复提交上游处理不了的请求（2026-10-14 夜，V5 + 超长负面提示词）
    /// 就会每半小时私信站长一次「上游可能故障」，而同时段其他成员一切正常。
    pub fn note_upstream_5xx(&self, key_id: i64) -> bool {
        let now = self.now();
        let mut q = self.recent_5xx.lock().expect("5xx");
        q.push_back((now, key_id));
        while q.front().is_some_and(|(t, _)| now - t > 1800.0) {
            q.pop_front();
        }
        let first = q.front().map(|(_, k)| *k);
        q.iter().any(|(_, k)| Some(*k) != first)
    }

    /// 记一次上游调用结果；状态翻转时告警。
    pub fn record_upstream(&self, ok: bool) {
        match self.health.record(ok, self.now()) {
            Some(crate::runtime::HealthChange::Degraded) => self.alerter.notify(
                "upstream_degraded",
                "NovelAI 上游近 10 分钟失败率偏高（上游可能在故障），成员的生图会受影响。",
                1800.0,
            ),
            Some(crate::runtime::HealthChange::Recovered) => {
                self.alerter
                    .notify("upstream_recovered", "NovelAI 上游已恢复正常。", 0.0)
            }
            None => {}
        }
    }
}
