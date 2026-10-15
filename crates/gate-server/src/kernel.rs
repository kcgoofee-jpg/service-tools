//! 自适应内核运行层（旧版 `app/kernel.py` + `modules.py` 的框架部分 + `quota_algo.run` / `autopilot.run` 的 IO）。
//!
//! 纯逻辑在 gate-core（`quota` / `autopilot` / `modules` / `perf`，都有 golden 对拍）；这里负责：
//! 取数（库、内存里的 Guard / 上游账号池 / Key 暂停表）、把 gate-core 算出的有序写入逐条落库、
//! 以及「模块内核」本身——每个模块回答一个问题，每 10 分钟跑一次，再用独立来源交叉核对。
//!
//! # 与旧版一致的约定
//!
//! - 评估周期内额度 / 处罚结果一个单位都不许变：算法一律走 gate-core 的 `decide`，这里不做任何判断。
//! - 每个模块的运行结果记进 `kernel_last`（保留最近 36 轮）。模块自己抛异常（tick 或校验函数出错）记进 Bug 追踪；
//!   校验「没通过」只在后台「模块」页标出，不记 Bug（docs/v3-deviations.md #44）。
//! - `kernel_last` 的键顺序、浮点格式都与旧版 `json.dumps(..., ensure_ascii=False)` 一致（上线前后混跑也不乱）。
//!
//! # 外部钩子（旧版 `kernel.extra`）
//!
//! 注册服务（领 Key）不属于内核：[`crate::jobs::startup`] 在注册服务配齐时自动接上（[`ServiceRegistrar`]），
//! 也可以用 [`Kernel::set_registrar`] 换掉。
//! 没接时等同旧版 `extra.get("registrar") is None`（自动驾驶按「名额不限」走，领 Key 模块不出校验）。

mod algo;
mod modules;
pub mod perf;
pub mod risk;

use crate::error::{ApiError, InternalError};
use crate::state::AppState;
use futures::future::BoxFuture;
use gate_core::jsondump;
use gate_core::modules::{Check, MODULES};
use gate_core::pyc::take_chars;
use gate_core::pyval;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, RwLock};

pub use algo::{run_autopilot as run_autopilot_checked, run_quota as run_quota_checked};
pub use perf::collect_perf;

/// 设置键：每个模块最近一轮的运行结果。
pub const STATE_KEY: &str = "kernel_last";
/// 每个模块保留最近 36 轮（6 小时）。
pub const RUNS: usize = 36;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 内核里一步出错：Python 异常类名 + 消息（写进 `kernel_last.error` 与 Bug 追踪，格式 `类型: 消息`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelError {
    pub ty: String,
    pub msg: String,
}

impl KernelError {
    pub fn new(ty: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            ty: ty.into(),
            msg: msg.into(),
        }
    }
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.ty, self.msg)
    }
}

impl std::error::Error for KernelError {}

impl From<gate_core::quota::DecideError> for KernelError {
    fn from(e: gate_core::quota::DecideError) -> Self {
        let ty = e.py_type();
        let text = e.to_string();
        let msg = text
            .strip_prefix(&format!("{ty}: "))
            .unwrap_or(&text)
            .to_string();
        Self::new(ty, msg)
    }
}

impl From<gate_db::DbError> for KernelError {
    fn from(e: gate_db::DbError) -> Self {
        Self::new("DbError", e.to_string())
    }
}

impl From<crate::guard::GuardIoError> for KernelError {
    fn from(e: crate::guard::GuardIoError) -> Self {
        Self::new("GuardError", e.to_string())
    }
}

impl From<KernelError> for ApiError {
    fn from(e: KernelError) -> Self {
        InternalError(e.to_string()).into()
    }
}

// ---------------------------------------------------------------------------
// 外部钩子
// ---------------------------------------------------------------------------

/// `registrar.settings()` 里内核用到的一项（已经 `or 0`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegistrarSettings {
    pub max_users: i64,
}

/// 注册服务（领 Key）在内核里的样子（旧版 `kernel.extra["registrar"]`）：内核模块与自动驾驶只用名额上限和占用人数。
/// 摘身份组 / 同步 / 退群回收 / 定时清空这些要碰 Discord 的后台工作不走这里，见 [`crate::registration`]。
/// 线上用 [`ServiceRegistrar`]（`startup` 自动接上）；对拍测试换成读库的假实现。
pub trait KernelRegistrar: Send + Sync {
    /// `registrar.settings()`：站长在后台保存的优先，其次是启动时的环境默认值。
    fn settings(&self) -> BoxFuture<'_, Result<RegistrarSettings, String>>;
    /// `registrar.count_active()`：占用名额的人数。
    fn count_active(&self) -> BoxFuture<'_, Result<i64, String>>;
}

/// 真正的注册服务（`state.registration`）接进内核。
pub struct ServiceRegistrar(pub Arc<crate::registration::RegistrationService>);

impl KernelRegistrar for ServiceRegistrar {
    fn settings(&self) -> BoxFuture<'_, Result<RegistrarSettings, String>> {
        Box::pin(async move {
            let r = self.0.settings().await.map_err(|e| e.to_string())?;
            Ok(RegistrarSettings {
                max_users: r.max_users,
            })
        })
    }

    fn count_active(&self) -> BoxFuture<'_, Result<i64, String>> {
        Box::pin(async move { self.0.count_active().await.map_err(|e| e.to_string()) })
    }
}

/// 一个上游账号的 V5 缓存（`allowance.snapshot(pool)["accounts"]` 里内核用到的两项）。
pub use gate_core::quota::AccountAllowance;

/// 账号 V5 剩余的来源（旧版 `quota_algo` 里读 `nai.allowance` 的几处）。线上用 [`LiveAllowance`]；对拍测试换成假的。
pub trait AllowanceSource: Send + Sync {
    /// `allowance.snapshot(pool)["accounts"]`（缓存里的读数，不发请求）。
    fn snapshot<'a>(
        &'a self,
        state: &'a AppState,
    ) -> BoxFuture<'a, Result<Vec<AccountAllowance>, String>>;
    /// 对每个可用账号主动读一次订阅信息（失败忽略）。
    fn resolve_usable<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()>;
    /// `refresh_allowance`：主动读一次，至少一个成功返回 true。
    fn refresh<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, bool>;
}

/// 真实的上游账号 V5 缓存（`state.allowance`）。
pub struct LiveAllowance;

impl AllowanceSource for LiveAllowance {
    fn snapshot<'a>(
        &'a self,
        state: &'a AppState,
    ) -> BoxFuture<'a, Result<Vec<AccountAllowance>, String>> {
        Box::pin(async move {
            let ids: Vec<String> = state
                .nai
                .tokens()
                .await
                .iter()
                .map(|t| t.token_id.clone())
                .collect();
            let snap = state
                .allowance
                .snapshot(&ids)
                .await
                .map_err(|e| e.to_string())?;
            let accounts = snap
                .get("accounts")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[]);
            Ok(accounts
                .iter()
                .map(|a| AccountAllowance {
                    percent: a
                        .get("percent")
                        .and_then(gate_core::quota::PyNum::from_value),
                    recharge_per_day: a
                        .get("recharge_per_day")
                        .and_then(gate_core::quota::PyNum::from_value),
                })
                .collect())
        })
    }

    fn resolve_usable<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            use gate_upstream::client::Allowance;
            let now = state.now();
            for t in state.nai.tokens().await {
                if t.usable(now) {
                    let _ = state.allowance.with(&state.nai).resolve(&t).await;
                }
            }
        })
    }

    fn refresh<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            use gate_upstream::client::Allowance;
            let now = state.now();
            let mut ok = false;
            for t in state.nai.tokens().await {
                if t.usable(now) && state.allowance.with(&state.nai).resolve(&t).await.is_ok() {
                    ok = true;
                }
            }
            ok
        })
    }
}

// ---------------------------------------------------------------------------
// Kernel
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Hooks {
    registrar: Option<Arc<dyn KernelRegistrar>>,
    allowance: Option<Arc<dyn AllowanceSource>>,
    /// 官方状态接口地址的覆盖（测试用）；None 用状态页模块里的 `OFFICIAL_API`。
    official_api: Option<String>,
}

/// 内核状态：每个模块最近一轮的运行记录 + 外部钩子。放在 [`AppState`] 里。
pub struct Kernel {
    /// 旧版 `Kernel.last`（也就是 `kernel_last` 设置的内容）。
    last: Mutex<Map<String, Value>>,
    hooks: RwLock<Hooks>,
    /// 同一时刻只跑一轮 `tick_all`（旧版是单事件循环里交错执行，这里串行化，结果是它的一种合法交错）。
    tick_lock: tokio::sync::Mutex<()>,
    /// 动态额度重算同理：后台定时、后台改参数、改 Key 额度都会调它，不能两份同时写。
    quota_lock: tokio::sync::Mutex<()>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            last: Mutex::new(Map::new()),
            hooks: RwLock::new(Hooks::default()),
            tick_lock: tokio::sync::Mutex::new(()),
            quota_lock: tokio::sync::Mutex::new(()),
        }
    }

    // ---- 钩子 ----

    pub fn set_registrar(&self, r: Option<Arc<dyn KernelRegistrar>>) {
        self.hooks.write().expect("kernel hooks").registrar = r;
    }

    pub fn registrar(&self) -> Option<Arc<dyn KernelRegistrar>> {
        self.hooks.read().expect("kernel hooks").registrar.clone()
    }

    /// 换掉账号 V5 缓存的来源（测试用）；`None` 恢复线上实现。
    pub fn set_allowance_source(&self, s: Option<Arc<dyn AllowanceSource>>) {
        self.hooks.write().expect("kernel hooks").allowance = s;
    }

    pub(crate) fn allowance_source(&self) -> Arc<dyn AllowanceSource> {
        self.hooks
            .read()
            .expect("kernel hooks")
            .allowance
            .clone()
            .unwrap_or_else(|| Arc::new(LiveAllowance))
    }

    /// 把「取官方状态」指到别的地址（测试用，保证不出本机）；`None` 恢复官方接口。
    pub fn set_official_api(&self, url: Option<String>) {
        self.hooks.write().expect("kernel hooks").official_api = url;
    }

    pub(crate) fn official_api(&self) -> Option<String> {
        self.hooks
            .read()
            .expect("kernel hooks")
            .official_api
            .clone()
    }

    pub(crate) async fn quota_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.quota_lock.lock().await
    }

    // ---- 运行记录 ----

    /// 启动时读回上次的运行记录，运行条不会因为重启清空。读不懂就当没有。
    pub async fn load(&self, state: &AppState) -> Result<(), KernelError> {
        let raw = state.db.get_setting(STATE_KEY).await?;
        let parsed = match raw.as_deref() {
            Some(s) if !s.is_empty() => jsondump::loads(s),
            _ => jsondump::loads("{}"),
        };
        *self.last.lock().expect("kernel last") = match parsed {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        Ok(())
    }

    /// 当前的运行记录（`kernel_last`）。
    pub fn last(&self) -> Map<String, Value> {
        self.last.lock().expect("kernel last").clone()
    }

    pub async fn enabled(&self, state: &AppState, name: &str) -> Result<bool, KernelError> {
        modules::enabled(state, name).await
    }

    pub async fn set_enabled(
        &self,
        state: &AppState,
        name: &str,
        on: bool,
    ) -> Result<(), KernelError> {
        modules::set_enabled(state, name, on).await
    }

    /// 模块是否存在（后台 `PUT /modules/{name}` 先查它）。
    pub fn has_module(name: &str) -> bool {
        MODULES.iter().any(|m| m.name == name)
    }

    /// 按登记顺序运行启用的模块，再做交叉校验；每个模块记一轮运行结果。单个模块出错不影响其他模块。
    pub async fn tick_all(&self, state: &AppState) -> Result<Map<String, Value>, KernelError> {
        let _serial = self.tick_lock.lock().await;
        let now = state.now();
        let mut last = self.last();
        for m in MODULES.iter() {
            let name = m.name;
            let on = self.enabled(state, name).await?;
            let entry = entry_mut(&mut last, name);
            entry.insert("enabled".into(), Value::Bool(on));
            pyval::pop(entry, "checks");
            if !on || !m.periodic {
                continue;
            }
            let t0 = state.now();
            match modules::tick(state, name).await {
                Ok(summary) => {
                    let ms = ((state.now() - t0) * 1000.0) as i64;
                    let entry = entry_mut(&mut last, name);
                    entry.insert("at".into(), pyval::float(t0));
                    entry.insert("ok".into(), Value::Bool(true));
                    entry.insert("ms".into(), json!(ms));
                    entry.insert("summary".into(), json!(take_chars(&summary, 300)));
                    entry.insert("error".into(), json!(""));
                }
                Err(e) => {
                    let entry = entry_mut(&mut last, name);
                    entry.insert("at".into(), pyval::float(t0));
                    entry.insert("ok".into(), Value::Bool(false));
                    entry.insert("error".into(), json!(take_chars(&e.to_string(), 300)));
                    let source = format!("module:{name}");
                    state.bugs.capture(crate::bugs::Report {
                        source: &source,
                        error: Some((&e.ty, &e.msg)),
                        ..Default::default()
                    });
                }
            }
        }
        self.check_all(state, &mut last).await?;
        for m in MODULES.iter() {
            let entry = entry_mut(&mut last, m.name);
            if !entry.get("enabled").is_some_and(pyval::truthy) {
                continue;
            }
            let checks: Vec<Value> = match entry.get("checks") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let any_level = |lv: &str| {
                checks
                    .iter()
                    .any(|c| c.get("level").and_then(Value::as_str) == Some(lv))
            };
            let level = if entry.get("ok") == Some(&Value::Bool(false)) || any_level("bad") {
                "bad"
            } else if any_level("warn") {
                "warn"
            } else {
                "ok"
            };
            let mut runs: Vec<Value> = match entry.get("runs") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let keep = RUNS - 1;
            if runs.len() > keep {
                runs.drain(..runs.len() - keep);
            }
            runs.push(json!({"at": pyval::float(now), "level": level}));
            entry.insert("runs".into(), Value::Array(runs));
        }
        state
            .db
            .set_setting(STATE_KEY, &jsondump::dumps(&Value::Object(last.clone())))
            .await?;
        *self.last.lock().expect("kernel last") = last.clone();
        Ok(last)
    }

    /// 交叉校验：对启用的、有校验的模块各跑一遍，结果写进 `last[name]["checks"]`；返回没通过的。
    pub async fn check_all(
        &self,
        state: &AppState,
        last: &mut Map<String, Value>,
    ) -> Result<Vec<(String, Check)>, KernelError> {
        let mut bad: Vec<(String, Check)> = Vec::new();
        for m in MODULES.iter() {
            if !m.has_checks || !self.enabled(state, m.name).await? {
                continue;
            }
            let mut checks = match modules::checks(state, m.name).await {
                Ok(v) => v,
                Err(e) => {
                    // 校验函数自己出错（取数失败、库里的数据被改坏）是模块代码的问题，和 tick 抛异常一样记 Bug
                    // （「注意」级，和旧版一致；后台「模块」页同时会标出「校验本身出错」）。
                    // 下面「校验没通过」不记：后台「模块」页和总览提醒已经标出，再记只会成为永久噪音
                    let source = format!("module:{}:checks", m.name);
                    state.bugs.capture(crate::bugs::Report {
                        source: &source,
                        error: Some((&e.ty, &e.msg)),
                        level: "warn",
                        ..Default::default()
                    });
                    vec![Check::new(
                        "校验本身出错",
                        false,
                        take_chars(&e.to_string(), 200),
                    )]
                }
            };
            for c in checks.iter_mut() {
                if c.level.is_empty() {
                    c.level = if c.ok { "ok" } else { "bad" }.to_string();
                }
            }
            entry_mut(last, m.name).insert(
                "checks".into(),
                Value::Array(checks.iter().map(check_value).collect()),
            );
            for c in checks {
                if !c.ok {
                    bad.push((m.name.to_string(), c));
                }
            }
        }
        Ok(bad)
    }

    /// 后台「模块」页：问题、开关、运行记录、关键参数当前值。
    pub async fn snapshot(&self, state: &AppState) -> Result<Value, KernelError> {
        let last = self.last();
        let mut out = Vec::new();
        for m in MODULES.iter() {
            let values = modules::param_values(state, m.name).await;
            let params: Vec<Value> = m
                .params
                .iter()
                .zip(values)
                .map(|((name, note), v)| json!({"name": name, "value": v, "note": note}))
                .collect();
            out.push(json!({
                "name": m.name,
                "title": m.title,
                "question": m.question,
                "enabled": self.enabled(state, m.name).await?,
                "hard_note": m.hard_note,
                "periodic": m.periodic,
                "last": last.get(m.name).cloned().unwrap_or_else(|| json!({})),
                "params": params,
            }));
        }
        Ok(Value::Array(out))
    }
}

/// `self.last.setdefault(name, {})`：条目不是对象（库里的数据被改坏）时换成空对象。
fn entry_mut<'a>(last: &'a mut Map<String, Value>, name: &str) -> &'a mut Map<String, Value> {
    let slot = last
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().expect("刚保证是对象")
}

/// `c.__dict__`：name, ok, detail, level, evidence。
fn check_value(c: &Check) -> Value {
    json!({
        "name": c.name,
        "ok": c.ok,
        "detail": c.detail,
        "level": c.level,
        "evidence": c.evidence,
    })
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 动态额度重算（旧版 `quota_algo.run(state)`），返回写进 `quota_algo_last` 的结果。
///
/// 每 10 分钟由内核的「分配」模块调用；后台改参数、改单个 Key 的额度/模型后也会立即调用一次。
pub async fn run_quota_algo(state: &AppState) -> Result<Value, ApiError> {
    Ok(algo::run_quota(state).await?)
}

/// 自动驾驶（旧版 `autopilot.run(state, registrar)`）：名额 +5、全站熔断、单 Key 限流。
pub async fn run_autopilot_once(state: &AppState) -> Result<Value, ApiError> {
    Ok(algo::run_autopilot(state).await?)
}
