//! 动态额度（旧 `app/quota_algo.py` + `app/quota_policies.py` 里线上用到的 `simple` / `water_fill`）。
//!
//! 评估周期内额度结果**一个单位都不许变**：怪癖照抄，并注明「与旧版一致」。
//!
//! # 结构
//!
//! 旧版 `run(state)` 把数据库查询和决策混在一起。这里拆成：
//! - 纯函数（`water_fill` / `simple` / `choose_rate` / `v5_plan` / `quota_notice` /
//!   `day_coverage` / `yesterday_stats` / `v5_used_equiv` / `quiet_recovery` / `allowance_summary` …），
//!   每个都有快照测试；
//! - 决策核心 [`decide`]：`QuotaInputs`（`run` 读到的一切）→ `QuotaOutputs`（`run` 做的每一次写入，
//!   按发生顺序，设置值是**精确字符串**）。服务层只负责「按下面的清单取数 → 调 `decide` → 按顺序执行 `effects`」。
//!
//! 出错路径也复刻：`decide` 遇到旧版会抛异常的地方返回 `error`（异常类型名与 Python 一致），
//! 且 `effects` 里保留异常之前**已经发生**的写入（旧版每次 `set_setting` 都已提交）。
//!
//! # 服务层要实现的查询（`QuotaInputs` 各字段的来源）
//!
//! 参数里的 `yday` = `clock.day(now - 86400)`；`start` = `clock.service_day_start(yday)`（本地 0 点 + 重置小时，
//! 对应 `time.mktime(time.strptime(day, "%Y-%m-%d")) + reset_hour*3600`），`end` = `start + 86400`。
//! 「每日微调」只在 `quota_algo_day != today` 时发生：服务层可用 [`daily_adjust_due`] 判断，
//! 不需要时 `daily` 传 `None`（传了也会被忽略）。
//!
//! | 字段 | SQL | 参数 |
//! |---|---|---|
//! | `members` | `SELECT id FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0 AND quota_auto=1`（只用行数） | — |
//! | `daily.yesterday.counters` | `SELECT c.key_id, c.images, k.daily_images, c.legacy_free_images, k.quota_auto FROM counters c JOIN api_keys k ON k.id=c.key_id WHERE c.day=? AND k.is_test=0 AND k.is_admin=0` | `(yday)` |
//! | `daily.yesterday.hourly_block_hours` | `SELECT DISTINCT CAST(u.ts/3600 AS INT) FROM usage_log u JOIN api_keys k ON k.id=u.key_id WHERE u.ts>=? AND u.ts<? AND u.status='rejected' AND k.is_test=0 AND k.is_admin=0 AND u.reason=?` | `(start, end, 'hourly_cap')`（旧版按文案 `LIKE '%出图量已达上限%'` 数） |
//! | `daily.yesterday.paced_blocks` | `SELECT COUNT(*) FROM usage_log u JOIN api_keys k ON k.id=u.key_id WHERE u.ts>=? AND u.ts<? AND u.status='rejected' AND k.is_test=0 AND k.is_admin=0 AND u.reason=?` | `(start, end, 'paced')` |
//! | `daily.coverage_first_ts` | `SELECT MIN(ts) FROM usage_log`（无记录为 NULL → `None`） | — |
//! | `daily.member_usage` | `SELECT c.legacy_free_images FROM counters c JOIN api_keys k ON k.id=c.key_id WHERE c.day=? AND k.is_test=0 AND k.is_admin=0 AND k.quota_auto=1 AND c.legacy_free_images>0` | `(yday)` |
//! | `daily.v5_rows` | `SELECT detail, images FROM usage_log WHERE ts>=? AND ts<? AND status='ok' AND model LIKE '%diffusion-5%'` | `(start, start+86400)` |
//! | `accounts` | 上游账号 V5 缓存快照（`allowance.snapshot(pool)["accounts"]`：每个账号的 `percent`、`recharge_per_day`）。刚重启、全部账号都没有 `percent` 时，旧版会对每个 usable 账号主动读一次订阅信息再取一次快照，这一步在服务层做 | — |
//! | `quiet.snaps` | `SELECT ts, v5_percent FROM upstream_snapshots WHERE ts>? AND fresh=1 AND v5_percent IS NOT NULL ORDER BY ts` | `(now - 72*3600)` |
//! | `quiet.counts[i]` | `SELECT COUNT(*) FROM usage_log WHERE ts>? AND ts<=? AND status='ok' AND model LIKE '%diffusion-5%'`（相邻两条快照之间；间隔 > 3 小时的旧版不查，值任意） | `(snaps[i].ts, snaps[i+1].ts)` |
//! | `active_v5` | `SELECT COUNT(DISTINCT c.key_id) FROM counters c JOIN api_keys k ON k.id=c.key_id WHERE c.v5>0 AND k.is_test=0 AND k.is_admin=0 AND c.day IN (?,?,?)` | [`active_v5_days`]`(clock, now, 3)`：今天之前的 3 个**完整服务日**（旧版是含今天的 3 个自然日） |
//! | `managed_keys` | `SELECT daily_images, daily_v5, image_model_scope FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0`（旧版直接 `COUNT(*)` 带 `daily_images<>? OR daily_v5<>? OR image_model_scope<>'all'`，这里改成取出行再在 Rust 里数，三列都是 NOT NULL，结果相同） | — |
//! | `settings` | `SELECT value FROM site_settings WHERE key=?`，读到的键见 [`SETTING_KEYS_READ`] | — |
//! | `guard` / `pool_len` / `global_daily_v5_default` | 内存里的 Guard（`values["account_daily_cap"]`）、上游账号池大小、`site_flags.default_of(GLOBAL_DAILY_V5)`（3.0 固定 150，不再取环境变量） | — |
//!
//! # `effects` 的执行方式（按顺序）
//!
//! - [`Effect::Set`]：`db.set_setting(key, value)`（`INSERT … ON CONFLICT(key) DO UPDATE`，立即提交）；
//! - [`Effect::Bulk`]：`db.set_settings_bulk(pairs)`（同一事务）；
//! - [`Effect::Log`]：`log_action(db, actor, action, target, detail)`（落库时截断 80/80/120/400 个码点，失败只告警）；
//! - [`Effect::UpdateKeys`]：`UPDATE api_keys SET daily_images=?, daily_v5=?, image_model_scope='all' WHERE quota_auto=1 AND is_admin=0 AND is_test=0` 并提交。
//!
//! # 3.0 删掉的「保底」
//!
//! 2.x 在每人上限 A 之外还有「保底 B」（`quota_base*` / `quota_base_now` 设置、`guard_base_daily_images`、
//! 复盘里的 `base_blocks`），超过 B 只在全站空闲时放行。算法早就把 B 同步成 A，这一层从不生效；3.0 整层删除：
//! 复盘的 `from` / `to` 只记 `[A]`，结果里没有 `base`，不再写 `guard_base_daily_images`。
//!
//! # 算法参数 [`QuotaParams`]
//!
//! 2.x 的 `allocation.*` 来自私密参数文件 `data/private_params.json`；3.0 取消了这个文件，
//! 服务层直接用 [`QuotaParams::default`]（代码里的公开起点值）。当时只剩 `allocation.min_coverage_hours`；
//! V5 的系数表 / 洪峰 / 收紧 / 人数下限参数随旧的 V5 公式删除。
//!
//! # V5
//!
//! 全站 V5 池 G = ⌊恢复速度 × 账号数 × [`V5_IMAGES_PER_PERCENT`]⌋，每人上限 = max(3, ⌊G ÷ max(10, N)⌋)，
//! N = 前 3 个完整服务日里用过 V5 的人数。两者都在服务日第一次计算时（07:00 后）定下，当天不再改。
//! 每人上限只是上限：全站池先到先得。

use crate::clock::Clock;
use crate::{jsondump, pyc, pyval};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// 常量（与旧版一致）
// ---------------------------------------------------------------------------

/// 官方 17.3 张/1% 按 23 步估算；成员多用 28 步，按步数折算 17.3×23/28。
pub const V5_IMAGES_PER_PERCENT: f64 = 14.2;
/// 拿不到读数时用的恢复速度（%/天）。
pub const V5_FALLBACK_RATE: f64 = 5.0;
/// 恢复速度的合理范围（超出就当读数有问题）。
pub const V5_RATE_RANGE: (f64, f64) = (1.0, 15.0);
/// 还没核对过（没有实测值）时，不超过保守值。
pub const V5_SAFE_RATE: f64 = 5.0;
/// 接口值和实测值相差超过 50% 算对不上。
pub const V5_RATE_MISMATCH: f64 = 0.5;

pub const STATE_KEY: &str = "quota_algo_last";
pub const HISTORY_KEY: &str = "quota_algo_history";
pub const DAY_KEY: &str = "quota_algo_day";
pub const NOTICE_KEY: &str = "algo_notice";
pub const V5_DAY_KEY: &str = "quota_v5_day_plan";

/// `quota_algo.DEFAULTS`（键名 → 起点值），顺序与旧版一致。
/// V5 的 `quota_v5_min` / `quota_v5_max`（3～30 夹取）随旧 V5 公式删除。
pub const DEFAULTS: [(&str, i64); 5] = [
    ("quota_auto_enabled", 1),
    ("quota_target_avg", 150),
    ("quota_ceiling_max", 300),
    ("quota_base_min_fair", 10),
    ("quota_step", 25),
];

/// `decide` 会读的 `site_settings` 键（服务层据此取 `QuotaInputs::settings`，缺失的键不放进 map）。
pub const SETTING_KEYS_READ: [&str; 11] = [
    "quota_auto_enabled",
    "quota_target_avg",
    "quota_ceiling_max",
    "quota_base_min_fair",
    "quota_step",
    "quota_ceiling",
    DAY_KEY,
    HISTORY_KEY,
    V5_DAY_KEY,
    "global_daily_v5",
    "member_daily_anlas",
];

// ---------------------------------------------------------------------------
// 错误与副作用
// ---------------------------------------------------------------------------

/// 旧版会抛出的异常（只区分类型名）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecideError {
    #[error("ValueError: {0}")]
    Value(String),
    #[error("JSONDecodeError: {0}")]
    Json(String),
    #[error("TypeError: {0}")]
    Type(String),
    #[error("KeyError: {0}")]
    Key(String),
    #[error("AttributeError: {0}")]
    Attribute(String),
    #[error("OverflowError: {0}")]
    Overflow(String),
    #[error("ZeroDivisionError: {0}")]
    ZeroDivision(String),
    /// 服务层没按约定提供某组输入（旧版没有对应异常）。
    #[error("缺少输入: {0}")]
    MissingInput(String),
}

impl DecideError {
    /// Python 异常类名。
    pub fn py_type(&self) -> &'static str {
        match self {
            Self::Value(_) => "ValueError",
            Self::Json(_) => "JSONDecodeError",
            Self::Type(_) => "TypeError",
            Self::Key(_) => "KeyError",
            Self::Attribute(_) => "AttributeError",
            Self::Overflow(_) => "OverflowError",
            Self::ZeroDivision(_) => "ZeroDivisionError",
            Self::MissingInput(_) => "MissingInput",
        }
    }
}

/// 一次写入 / 动作，按旧版发生的顺序排列（额度与自动驾驶共用）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Effect {
    /// `db.set_setting(key, value)`（值已是 `str(...)` 之后的精确字符串）。
    Set { key: String, value: String },
    /// `db.set_settings_bulk({...})`（保持字典顺序）。
    Bulk { pairs: Vec<(String, String)> },
    /// `log_action(db, actor, action, target, detail)`。
    Log {
        actor: String,
        action: String,
        target: String,
        detail: String,
    },
    /// 把算法结果下发给所有算法管理的成员 Key（见模块文档）。
    UpdateKeys { daily_images: i64, daily_v5: i64 },
    /// 自动驾驶：`guard.trip_breaker(seconds, reason, now)`（内存态）。
    TripBreaker {
        seconds: i64,
        reason: String,
        now: f64,
    },
    /// 自动驾驶：临时暂停单把 Key（旧版 `share.pause_key(key_id, seconds, reason, now)`；落 `share_state` 表并更新内存暂停表）。
    PauseKey {
        key_id: i64,
        seconds: i64,
        reason: String,
        now: f64,
    },
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// Python `min(a, b)`：先到先得，`b < a` 才换（NaN 行为与 Python 一致）。
pub(crate) fn py_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// Python `max(a, b)`：`b > a` 才换。
pub(crate) fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `f"{x:.1f}"`（Rust 对 NaN 写 `NaN`，Python 写 `nan`）。
pub(crate) fn fmt_prec(x: f64, n: usize) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x.is_infinite() {
        if x > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        format!("{x:.n$}")
    }
}

pub(crate) fn opt_f(x: Option<f64>) -> Value {
    x.map_or(Value::Null, pyval::float)
}

/// `int(float(s))`：非法 → ValueError，NaN → ValueError，inf → OverflowError。
/// 超出 i64 的数饱和（Python 是大整数；设置值有范围校验，不会真走到这里）。
fn int_via_float(s: &str) -> Result<i64, DecideError> {
    match pyc::parse_float(s) {
        None => Err(DecideError::Value(format!(
            "could not convert string to float: {s:?}"
        ))),
        Some(f) if f.is_nan() => Err(DecideError::Value(
            "cannot convert float NaN to integer".into(),
        )),
        Some(f) if f.is_infinite() => Err(DecideError::Overflow(
            "cannot convert float infinity to integer".into(),
        )),
        Some(f) => Ok(f.trunc() as i64),
    }
}

/// `site_flags.parse(spec, raw, fallback)`（int 类型、带 lo/hi 钳制）。
pub(crate) fn site_int(raw: Option<&str>, lo: f64, hi: f64, fallback: i64) -> i64 {
    let Some(raw) = raw else { return fallback };
    if pyc::strip(raw).is_empty() {
        return fallback;
    }
    match pyc::parse_float(pyc::strip(raw)) {
        None => fallback,
        Some(num) if num.is_nan() => fallback,
        Some(num) => {
            let num = py_max(lo, num);
            let num = py_min(hi, num);
            num.trunc() as i64
        }
    }
}

/// `json.loads(get_setting(key, default) or default)`。
pub(crate) fn loads_setting(raw: Option<&str>, default: &str) -> Result<Value, DecideError> {
    let s = match raw {
        Some(s) if !s.is_empty() => s,
        _ => default,
    };
    jsondump::loads(s).ok_or_else(|| DecideError::Json(format!("无法解析 {s:?}")))
}

/// 历史列表（第二次读取，只会被 `reversed(...)` 遍历并对每项调 `.get`）。
/// 不是数组时的行为跟 Python 一致：空字符串 / 空对象遍历为空，不会报错；
/// 非空字符串 / 对象遍历出的是字符 / 键，`.get` 抛 `AttributeError`；数字 / None 不可 `reversed` → `TypeError`。
fn as_history(v: Value) -> Result<Vec<Value>, DecideError> {
    match v {
        Value::Array(a) => Ok(a),
        Value::Object(o) if o.is_empty() => Ok(Vec::new()),
        Value::String(s) if s.is_empty() => Ok(Vec::new()),
        Value::Object(_) | Value::String(_) => Err(DecideError::Attribute(
            "'str' object has no attribute 'get'".into(),
        )),
        other => Err(DecideError::Type(format!("历史不可 reversed: {other}"))),
    }
}

/// `h.get(key)`（h 必须是 dict，否则 AttributeError）。
pub(crate) fn hget<'a>(h: &'a Value, key: &str) -> Result<Option<&'a Value>, DecideError> {
    match h {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(DecideError::Attribute(format!(
            "{other} 没有 get（历史条目不是 dict）"
        ))),
    }
}

/// `h.get(key) is not None`。
pub(crate) fn hget_nn<'a>(h: &'a Value, key: &str) -> Result<Option<&'a Value>, DecideError> {
    Ok(hget(h, key)?.filter(|v| !v.is_null()))
}

pub(crate) fn num(v: &Value) -> Result<f64, DecideError> {
    pyval::number_like(v).ok_or_else(|| DecideError::Type(format!("不是数字: {v}")))
}

/// Python `==`（数字跨 int / float / bool 比较，容器逐项比较）。
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| py_eq(p, q)))
        }
        (Value::Bool(_) | Value::Number(_), Value::Bool(_) | Value::Number(_)) => match (a, b) {
            (Value::Number(x), Value::Number(y)) if !x.is_f64() && !y.is_f64() => x == y,
            _ => pyval::number_like(a) == pyval::number_like(b),
        },
        _ => a == b,
    }
}

/// `int(v or 0)`（`quota_notice` 读 `v5["each"]` / `v5["global"]`）。
fn int_or0(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map_or(0, |f| f.trunc() as i64)),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// quota_policies：water_fill / simple
// ---------------------------------------------------------------------------

/// 最大最小公平（water-filling）：找最小的 b，使 Σ min(用量ᵢ, b) ≥ budget。
/// 用得少的人用多少算多少，剩下的平分给用得多的人；需求不到 budget 时返回最大用量（人人都满足）。
pub fn water_fill(usage: &[i64], budget: f64) -> i64 {
    let mut us: Vec<i64> = usage.iter().copied().filter(|u| *u > 0).collect();
    us.sort_unstable();
    if us.is_empty() {
        return 0;
    }
    let mut left = budget;
    let n = us.len();
    for (i, &u) in us.iter().enumerate() {
        let share = left / (n - i) as f64;
        if u as f64 >= share {
            return std::cmp::max(1, share.trunc() as i64);
        }
        left -= u as f64;
    }
    us[n - 1]
}

/// `quota_algo._cfg` 解析后的整数配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCfg {
    pub quota_auto_enabled: i64,
    pub quota_target_avg: i64,
    pub quota_ceiling_max: i64,
    pub quota_base_min_fair: i64,
    pub quota_step: i64,
}

impl Default for QuotaCfg {
    fn default() -> Self {
        Self {
            quota_auto_enabled: 1,
            quota_target_avg: 150,
            quota_ceiling_max: 300,
            quota_base_min_fair: 10,
            quota_step: 25,
        }
    }
}

/// `v2.18 极简模式`（线上）：返回 `(新的每人额度 N, 理由)`。理由已含「每人每天 N 张。」前缀。
/// `prev_a` = `prev.get("a")`；**为 0 或缺失时退回 `quota_target_avg`**（`prev.get("a") or cfg[...]`，与旧版一致）。
///
/// `usage` 是被复盘那个服务日每人实际用掉的 V4.5 免费图张数。「有人用满 A」就是有人当天实际用到了 A：
/// 2.x 的每人注水让当天最多只能用到约 126 张，A > 126 时这条永远不成立、A 不会再 +25；
/// 注水删除后用满 A 才真的可能发生。
pub fn simple(prev_a: Option<i64>, usage: &[i64], budget: i64, cfg: &QuotaCfg) -> (i64, String) {
    let a = match prev_a {
        Some(x) if x != 0 => x,
        _ => cfg.quota_target_avg,
    };
    let (lo, hi) = (cfg.quota_base_min_fair, cfg.quota_ceiling_max);
    let total: i64 = usage.iter().sum();
    let (n, why) = if !usage.is_empty() && (total as f64) >= (budget as f64) * 0.95 {
        let n = std::cmp::max(lo, std::cmp::min(hi, water_fill(usage, budget as f64)));
        (
            n,
            format!("昨天总需求 {total} 张已接近可分的 {budget} 张，按需求平分"),
        )
    } else if !usage.is_empty() && usage.iter().copied().max().unwrap_or(0) >= a {
        let n = std::cmp::min(hi, a + cfg.quota_step);
        let full = usage.iter().filter(|u| **u >= a).count();
        (
            n,
            format!("还有余量（昨天共 {total} / {budget} 张），{full} 人用满 {a} 张：放宽"),
        )
    } else {
        let n = std::cmp::max(lo, std::cmp::min(hi, a));
        (
            n,
            format!("还有余量（昨天共 {total} / {budget} 张），没人用满：不变"),
        )
    };
    (n, format!("每人每天 {n} 张。{why}"))
}

// ---------------------------------------------------------------------------
// V5：恢复速度、全站 / 每人额度
// ---------------------------------------------------------------------------

/// 算法参数。`Default` = 代码里的公开起点值（线上就用它；字段留着是为了让对拍夹具里用过别的值的用例仍能重放）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaParams {
    /// 昨天的日志至少覆盖多少小时才调整每人额度（2.x 的私密参数 `allocation.min_coverage_hours`）
    pub min_coverage_hours: i64,
}

impl Default for QuotaParams {
    fn default() -> Self {
        Self {
            min_coverage_hours: 20,
        }
    }
}

/// 每人 V5 上限的人数下限：前 3 个完整服务日里用过 V5 的人少于这个数时按它算（站长 10-15 批准的公式）。
pub const V5_MIN_PEOPLE: i64 = 10;
/// 每人 V5 上限的下限：全站池本来就先到先得（PVP），个人上限只防一个人吃光，不再保证人人有份（站长 10-17 定 3）。
pub const V5_MIN_EACH: i64 = 3;
/// 统计「用过 V5 的人」的完整服务日个数（不含今天）。
pub const V5_ACTIVE_DAYS: usize = 3;

/// 决定用哪个恢复速度。返回 `(速度, 说明)`；说明显示在后台，并给观测模块做自检。
pub fn choose_rate(api_rate: Option<f64>, measured: Option<f64>) -> (f64, Map<String, Value>) {
    let (lo, hi) = V5_RATE_RANGE;
    let mut notes: Vec<String> = Vec::new();
    let in_range = |x: f64| lo <= x && x <= hi;
    let mut api_rate = api_rate;
    let mut measured = measured;
    if let Some(a) = api_rate
        && !in_range(a)
    {
        notes.push(format!(
            "接口读数 {}%/天 超出合理范围 1～15，不用",
            fmt_prec(a, 1)
        ));
        api_rate = None;
    }
    if let Some(m) = measured
        && !in_range(m)
    {
        notes.push(format!("实测 {}%/天 超出合理范围，不用", fmt_prec(m, 1)));
        measured = None;
    }
    let mismatch = match (api_rate, measured) {
        (Some(a), Some(m)) => (a - m).abs() > V5_RATE_MISMATCH * m,
        _ => false,
    };
    let (rate, status) = if let Some(m) = measured {
        let rate = match api_rate {
            Some(a) => py_min(m, a),
            None => m,
        };
        let status = if mismatch { "对不上" } else { "已核对" };
        if mismatch {
            notes.push(format!(
                "接口 {} 和实测 {} 相差超过一半，取较小的",
                fmt_prec(api_rate.unwrap_or(0.0), 1),
                fmt_prec(m, 1)
            ));
        }
        (rate, status)
    } else {
        let rate = match api_rate {
            Some(a) => py_min(a, V5_SAFE_RATE),
            None => V5_SAFE_RATE,
        };
        notes.push("还没有实测值核对，不超过保守值 5%/天".to_string());
        (rate, "未核对")
    };
    let rate = py_max(lo, py_min(hi, rate));
    let mut info = Map::new();
    info.insert("api".into(), opt_f(api_rate));
    info.insert("measured".into(), opt_f(measured));
    info.insert("used".into(), pyval::float(pyc::round(rate, 1)));
    info.insert("status".into(), json!(status));
    info.insert("notes".into(), json!(notes));
    (rate, info)
}

/// `v5_plan` 的返回值。字段顺序即字典顺序：rate, percent, global, people, each。
#[derive(Debug, Clone, PartialEq)]
pub struct V5Plan {
    pub rate: f64,
    /// 定方案时账号 V5 剩余 %（只用于显示，不参与计算）。
    pub percent: Option<f64>,
    pub global: i64,
    /// max(10, N)：分池子用的人数。
    pub people: i64,
    pub each: i64,
}

impl V5Plan {
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("rate".into(), pyval::float(self.rate));
        m.insert("percent".into(), opt_f(self.percent));
        m.insert("global".into(), json!(self.global));
        m.insert("people".into(), json!(self.people));
        m.insert("each".into(), json!(self.each));
        m
    }
}

/// 全站 V5 池 G 与每人上限：
/// G = ⌊恢复速度 × 账号数 × 每 1% 的张数⌋，每人 = max(3, ⌊G ÷ max(10, N)⌋)。
/// `rate` = 采用的恢复 %/天（`None` / ≤ 0 / NaN 用保守的 5）；`active` = N（前 3 个完整服务日用过 V5 的人数）。
/// 返回的 `rate` 已 `round(rate, 1)`。旧版的电量系数、3～30 夹取、洪峰放宽都已删除。
pub fn v5_plan(percent: Option<f64>, rate: Option<f64>, active: i64) -> V5Plan {
    v5_plan_n(percent, rate, active, 1)
}

/// 多个上游账号时全站池按账号数相加（站长 10-17）：恢复速度、剩余 % 都是「单个账号的平均」，
/// 池 = ⌊速度 × 账号数 × 每 1% 的张数⌋。`accounts` < 1 按 1。
pub fn v5_plan_n(percent: Option<f64>, rate: Option<f64>, active: i64, accounts: i64) -> V5Plan {
    let rate = match rate {
        Some(r) if r > 0.0 => r,
        _ => V5_FALLBACK_RATE,
    };
    let pool = (rate * std::cmp::max(1, accounts) as f64 * V5_IMAGES_PER_PERCENT).floor() as i64;
    let people = std::cmp::max(V5_MIN_PEOPLE, active);
    let each = std::cmp::max(V5_MIN_EACH, pool.div_euclid(people));
    V5Plan {
        rate: pyc::round(rate, 1),
        percent,
        global: pool,
        people,
        each,
    }
}

/// 首页那句「今日额度」。
///
/// `v5` 是 V5 方案字典（读 `each` / `global`），`people` 是前 3 个完整服务日用过 V5 的人数
/// （超卖判定按活跃人数，不按全部 Key），
/// `review` 是当天的复盘字典（读 `from` / `to` / `reasons`）。
pub fn quota_notice(
    a: i64,
    v5: &Value,
    people: i64,
    today: &str,
    review: Option<&Value>,
    reset_hour: i64,
    anlas: i64,
) -> Result<String, DecideError> {
    let each = int_or0(pyval::get(v5, "each"));
    let global_v5 = int_or0(pyval::get(v5, "global"));
    let oversell = global_v5 != 0 && each * std::cmp::max(1, people) > global_v5;
    let mut v5_status = format!(" · V5 每人 {each} 张");
    if oversell {
        v5_status.push_str(&format!("（全站今日共 {global_v5} 张，先到先得）"));
    }
    if anlas > 0 {
        v5_status.push_str(&format!(" · Anlas 每人 {anlas}/天"));
    }
    // 2.x 的每人注水（每小时恢复 4 张、当天最多约 126 张）已删除：A 就是当天能用到的数
    let mut note = format!("今日额度：V4.5 每人 {a} 张/天（每日 {reset_hour} 点重置）{v5_status}");
    if let Some(r) = review
        && pyval::truthy(r)
    {
        let from = pyval::get(r, "from");
        let to = pyval::get(r, "to");
        let differ = match (from, to) {
            (None, None) => false,
            (Some(x), Some(y)) => !py_eq(x, y),
            _ => true,
        };
        if differ {
            let reasons = pyval::get(r, "reasons")
                .ok_or_else(|| DecideError::Key("'reasons'".into()))?
                .as_array()
                .ok_or_else(|| DecideError::Type("reasons 不是列表".into()))?;
            let strs: Result<Vec<&str>, _> = reasons
                .iter()
                .map(|x| {
                    x.as_str()
                        .ok_or_else(|| DecideError::Type("reasons 里有非字符串".into()))
                })
                .collect();
            // today[5:].replace('-', '月')
            let md: String = today.chars().skip(5).collect::<String>().replace('-', "月");
            note = format!("{md}日 算法调整：{}。{note}", strs?.join("；"));
        }
    }
    Ok(note)
}

// ---------------------------------------------------------------------------
// SQL 派生量的纯数学
// ---------------------------------------------------------------------------

/// `day_coverage`：这一天有日志覆盖的小时数（从系统开始记录算起）。`first_ts` = `SELECT MIN(ts) FROM usage_log`。
pub fn day_coverage(clock: &Clock, day: &str, first_ts: Option<f64>) -> Result<f64, DecideError> {
    let start = clock
        .service_day_start(day)
        .ok_or_else(|| DecideError::Value(format!("time data {day:?} does not match format")))?;
    let Some(first) = first_ts else {
        return Ok(0.0);
    };
    Ok(py_max(
        0.0,
        (start + 86400.0 - py_max(start, first)) / 3600.0,
    ))
}

/// 服务日 `day` 的查询窗口 `(start, end)`（`_yesterday` / `_v5_used_equiv` 用）。
pub fn day_window(clock: &Clock, day: &str) -> Result<(f64, f64), DecideError> {
    let start = clock
        .service_day_start(day)
        .ok_or_else(|| DecideError::Value(format!("time data {day:?} does not match format")))?;
    Ok((start, start + 86400.0))
}

/// 前一天某个 Key 的计数行（`counters JOIN api_keys`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterRow {
    pub images: i64,
    pub daily_images: i64,
    pub legacy_free_images: i64,
    pub quota_auto: i64,
}

/// `_yesterday` 的查询结果。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YesterdayInputs {
    pub counters: Vec<CounterRow>,
    pub hourly_block_hours: Vec<i64>,
    pub paced_blocks: i64,
}

/// `_yesterday` 的返回值（字段顺序即字典顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YesterdayStats {
    pub used: i64,
    pub ceiling_hits: i64,
    pub hourly_blocks: i64,
    pub paced_blocks: i64,
}

impl YesterdayStats {
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("used".into(), json!(self.used));
        m.insert("ceiling_hits".into(), json!(self.ceiling_hits));
        m.insert("hourly_blocks".into(), json!(self.hourly_blocks));
        m.insert("paced_blocks".into(), json!(self.paced_blocks));
        m
    }
}

/// `_yesterday`：成员出图总数、顶到上限的人数、被每小时上限拦的小时数等。
/// 旧版的 `used` 还按当天账号层面「节约模式折算值 / 实际张数」换算；节约模式早已删除，3.0 就是实际张数。
pub fn yesterday_stats(y: &YesterdayInputs) -> YesterdayStats {
    let used: i64 = y.counters.iter().map(|r| r.images).sum();
    // 顶格：V4.5 上限 A 管的是 V4.5 免费图（legacy_free_images）；手动额度（quota_auto != 1）的 Key 不参与
    let hits = y
        .counters
        .iter()
        .filter(|r| {
            r.daily_images != 0 && r.quota_auto == 1 && r.legacy_free_images >= r.daily_images
        })
        .count() as i64;
    let hours: HashSet<i64> = y.hourly_block_hours.iter().copied().collect();
    YesterdayStats {
        used,
        ceiling_hits: hits,
        hourly_blocks: hours.len() as i64,
        paced_blocks: y.paced_blocks,
    }
}

/// 某一天 `status='ok'` 的 V5 日志行（`detail`, `images`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V5Row {
    pub detail: String,
    pub images: i64,
}

/// `_v5_used_equiv`：这一天全站用掉的 V5，折成 28 步的张数（按每张图自己的步数 `/NNstep`，缺省 28）。
pub fn v5_used_equiv(rows: &[V5Row]) -> f64 {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"/(\d+)step").expect("正则合法"));
    let mut total = 0.0_f64;
    for r in rows {
        let steps = match re.captures(&r.detail) {
            // 超出 i64 的超长数字：Python 是大整数，夹到 28 后结果相同
            Some(c) => pyc::parse_int(&c[1]).unwrap_or(i64::MAX),
            None => 28,
        };
        let images = if r.images != 0 { r.images } else { 1 };
        let st = steps.clamp(1, 28);
        total += (images * st) as f64 / 28.0;
    }
    total
}

/// 一条上游快照（`upstream_snapshots`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietSnap {
    pub ts: f64,
    pub percent: f64,
}

/// `quiet_recovery` 的输入：按 `ts` 升序的快照 + 相邻两条之间的 V5 出图条数。
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct QuietInputs {
    pub snaps: Vec<QuietSnap>,
    /// `counts[i]` = `snaps[i]` 到 `snaps[i+1]` 之间的 V5 成功日志条数（间隔 > 3 小时的不会被读，值任意）。
    pub counts: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Quiet {
    pub rate: f64,
    pub hours: f64,
    pub gain: f64,
}

impl Quiet {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("rate".into(), pyval::float(self.rate));
        m.insert("hours".into(), pyval::float(self.hours));
        m.insert("gain".into(), pyval::float(self.gain));
        Value::Object(m)
    }
}

/// `quiet_recovery` 回看的窗口（72 小时）。
pub const QUIET_WINDOW_SECS: f64 = 72.0 * 3600.0;

/// 服务层查快照的下界参数：`ts > now - 72h`。
pub fn quiet_snaps_since(now: f64) -> f64 {
    now - QUIET_WINDOW_SECS
}

/// 旧版会去数 V5 条数的相邻快照对（间隔 ≤ 3 小时）的下标 `i`（即 `snaps[i]` → `snaps[i+1]`）。
/// 服务层只需要对这些下标执行 `quiet.counts[i]` 那条查询，其余位置填 0 即可。
pub fn quiet_count_pairs(snaps: &[QuietSnap]) -> Vec<usize> {
    snaps
        .windows(2)
        .enumerate()
        .filter(|(_, w)| {
            let skipped = w[1].ts - w[0].ts > 3.0 * 3600.0;
            !skipped
        })
        .map(|(i, _)| i)
        .collect()
}

/// 独立测量恢复速度：相邻两次「主动读」的快照之间一张 V5 都没出 → 剩余的上涨就是纯恢复。
/// 累计 ≥ 4 个安静小时才给结果。
pub fn quiet_recovery(q: &QuietInputs) -> Option<Quiet> {
    let mut hours = 0.0_f64;
    let mut gain = 0.0_f64;
    for (i, w) in q.snaps.windows(2).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        if b.ts - a.ts > 3.0 * 3600.0 {
            continue;
        }
        if q.counts.get(i).copied().unwrap_or(0) == 0 {
            hours += (b.ts - a.ts) / 3600.0;
            gain += b.percent - a.percent;
        }
    }
    if hours < 4.0 {
        return None;
    }
    Some(Quiet {
        rate: pyc::round(gain / hours * 24.0, 2),
        hours: pyc::round(hours, 1),
        gain,
    })
}

/// Python 的 `int` 或 `float`（`sum()` 对两者的处理不同，见 [`py_sum`]）。
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum PyNum {
    Int(i64),
    Float(f64),
}

impl PyNum {
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }

    /// `bool(x)`
    pub fn truthy(self) -> bool {
        self.as_f64() != 0.0
    }

    /// `str(x)`：int 原样，float 用 `repr`。
    pub fn py_str(self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Float(f) => pyc::float_repr(f),
        }
    }

    /// 从 JSON 数字（int / float，不含 bool）转换。
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) if !n.is_f64() => n.as_i64().map(Self::Int),
            Value::Number(n) => n.as_f64().map(Self::Float),
            _ => None,
        }
    }
}

/// CPython 3.12 的 `sum(items)`（起始值 int 0）：
/// - 先走整数快路径，全是 int 时结果是精确的 int；
/// - 遇到第一个 float：`float(整数部分) + x` 后进入浮点阶段，浮点项用 Neumaier 补偿，
///   **夹在中间的 int 项只做朴素相加、不进补偿**（与 [`pyc::sum`] 对纯 float 列表的结果一致，混合列表才有差别）。
pub fn py_sum(items: &[PyNum]) -> PyNum {
    let mut i_result: i64 = 0;
    let mut idx = 0;
    while idx < items.len() {
        match items[idx] {
            PyNum::Int(v) => {
                i_result = i_result.saturating_add(v);
                idx += 1;
            }
            PyNum::Float(_) => break,
        }
    }
    if idx == items.len() {
        return PyNum::Int(i_result);
    }
    let PyNum::Float(first) = items[idx] else {
        unreachable!("循环在第一个 float 处停下")
    };
    let mut f = i_result as f64 + first;
    let mut c = 0.0_f64;
    for item in &items[idx + 1..] {
        match *item {
            PyNum::Float(x) => {
                let t = f + x;
                if f.abs() >= x.abs() {
                    c += (f - t) + x;
                } else {
                    c += (x - t) + f;
                }
                f = t;
            }
            PyNum::Int(v) => f += v as f64,
        }
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    PyNum::Float(f)
}

/// 一个上游账号的 V5 缓存（`allowance.snapshot` 里的一项）。
/// 线上 `percent` 一定是 int（`allowance.resolve` 拒绝非 int），`recharge_per_day` 是 `round(86400/秒数, 1)` 的 float。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountAllowance {
    pub percent: Option<PyNum>,
    pub recharge_per_day: Option<PyNum>,
}

/// `_allowance` 的纯部分：账号 V5 剩余 % 与恢复 %/天（多个账号取平均；没有读数返回 `(None, None)`）。
pub fn allowance_summary(accounts: &[AccountAllowance]) -> (Option<f64>, Option<f64>) {
    let rows: Vec<&AccountAllowance> = accounts.iter().filter(|a| a.percent.is_some()).collect();
    if rows.is_empty() {
        return (None, None);
    }
    let pcts: Vec<PyNum> = rows.iter().filter_map(|a| a.percent).collect();
    let pct = py_sum(&pcts).as_f64() / rows.len() as f64;
    // `if a.get("recharge_per_day")`：0 / None 不算
    let rates: Vec<PyNum> = rows
        .iter()
        .filter_map(|a| a.recharge_per_day)
        .filter(|r| r.truthy())
        .collect();
    let rate = if rates.is_empty() {
        None
    } else {
        Some(py_sum(&rates).as_f64() / rates.len() as f64)
    };
    (Some(pct), rate)
}

/// `_active_v5` 的日期参数：今天之前的 `days` 个**完整服务日**（从旧到新，不含今天）。
/// 旧版是含今天的 `days` 个自然日（本地日期，不按重置点偏移），今天还没过完、人数一天里会变；
/// 3.0 只看已经过完的服务日，人数当天不变。
pub fn active_v5_days(clock: &Clock, now: f64, days: usize) -> Vec<String> {
    let mut all = clock.last_days(now, days + 1);
    all.pop(); // 今天
    all
}

/// 今天是否还没做过每日微调（`get_setting(DAY_KEY, None) != today`）。
pub fn daily_adjust_due(day_key_setting: Option<&str>, today: &str) -> bool {
    day_key_setting != Some(today)
}

// ---------------------------------------------------------------------------
// 决策核心
// ---------------------------------------------------------------------------

/// 内存里的 Guard（只用到两个值）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardInfo {
    /// `guard.values["account_daily_cap"]`
    pub account_daily_cap: i64,
}

/// 算法管理的成员 Key 当前下发的值。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedKey {
    pub daily_images: i64,
    pub daily_v5: i64,
    pub image_model_scope: String,
}

/// 「每日微调」要读的数据（只在 `daily_adjust_due` 时需要）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyInputs {
    pub yesterday: YesterdayInputs,
    pub coverage_first_ts: Option<f64>,
    pub member_usage: Vec<i64>,
    pub v5_rows: Vec<V5Row>,
}

/// `run` 读到的一切。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaInputs {
    pub now: f64,
    /// `site_settings` 里 [`SETTING_KEYS_READ`] 各键的当前值（字符串；缺失的键不放）。
    pub settings: HashMap<String, String>,
    pub guard: Option<GuardInfo>,
    /// `len(state.nai.pool)`；`state.nai` 不存在时为 `None`。
    pub pool_len: Option<usize>,
    pub global_daily_v5_default: i64,
    pub members: usize,
    pub daily: Option<DailyInputs>,
    pub accounts: Vec<AccountAllowance>,
    pub quiet: QuietInputs,
    pub active_v5: i64,
    pub managed_keys: Vec<ManagedKey>,
    #[serde(default)]
    pub params: QuotaParams,
}

/// `run` 的全部输出。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaOutputs {
    /// 按旧版发生顺序的写入（出错时只含出错之前已发生的）。
    pub effects: Vec<Effect>,
    /// `run` 的返回值（`{"enabled": False}` 或完整结果字典，同时也是写进 `quota_algo_last` 的内容）。
    pub result: Option<Value>,
    /// 旧版会抛异常时的 Python 异常类名。
    pub error: Option<String>,
}

/// 一轮 `run` 的可变现场：设置表（旧代码会读回自己刚写的值）+ 按序累积的写入。
pub(crate) struct Cx {
    pub(crate) settings: HashMap<String, String>,
    pub(crate) effects: Vec<Effect>,
}

impl Cx {
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    pub(crate) fn set(&mut self, key: &str, value: String) {
        self.settings.insert(key.to_string(), value.clone());
        self.effects.push(Effect::Set {
            key: key.to_string(),
            value,
        });
    }

    pub(crate) fn bulk(&mut self, pairs: Vec<(String, String)>) {
        for (k, v) in &pairs {
            self.settings.insert(k.clone(), v.clone());
        }
        self.effects.push(Effect::Bulk { pairs });
    }

    pub(crate) fn log(&mut self, action: &str, target: String, detail: String) {
        self.effects.push(Effect::Log {
            actor: "系统".into(),
            action: action.into(),
            target,
            detail,
        });
    }
}

/// `_cfg`：把 `DEFAULTS` 里的每个键读成整数（解析失败回到起点值；`inf` 抛 OverflowError，与旧版一致）。
pub fn quota_cfg(settings: &HashMap<String, String>) -> Result<QuotaCfg, DecideError> {
    let mut cfg = QuotaCfg::default();
    let slots: [&mut i64; 5] = [
        &mut cfg.quota_auto_enabled,
        &mut cfg.quota_target_avg,
        &mut cfg.quota_ceiling_max,
        &mut cfg.quota_base_min_fair,
        &mut cfg.quota_step,
    ];
    for (slot, (name, default)) in slots.into_iter().zip(DEFAULTS) {
        // `_cfg`：except (TypeError, ValueError) → 起点值；OverflowError（inf）不在其中，会抛出
        *slot = match settings.get(name).map(String::as_str) {
            None => default,
            Some(raw) => match int_via_float(raw) {
                Ok(v) => v,
                Err(DecideError::Value(_)) => default,
                Err(e) => return Err(e),
            },
        };
    }
    Ok(cfg)
}

/// `run(state, now)` 的决策核心。`clock` 的重置小时即 `state.settings.day_reset_hour`。
pub fn decide(inp: &QuotaInputs, clock: &Clock) -> QuotaOutputs {
    let mut cx = Cx {
        settings: inp.settings.clone(),
        effects: Vec::new(),
    };
    let r = run_inner(inp, clock, &mut cx);
    match r {
        Ok(v) => QuotaOutputs {
            effects: cx.effects,
            result: Some(v),
            error: None,
        },
        Err(e) => QuotaOutputs {
            effects: cx.effects,
            result: None,
            error: Some(e.py_type().to_string()),
        },
    }
}

fn run_inner(inp: &QuotaInputs, clock: &Clock, cx: &mut Cx) -> Result<Value, DecideError> {
    let now = inp.now;
    let params = &inp.params;
    let cfg = quota_cfg(&cx.settings)?;
    if cfg.quota_auto_enabled == 0 {
        return Ok(json!({"enabled": false}));
    }
    // 按账号总数算（不看此刻是否可用）；无账号池时按 1 个
    let accounts = match inp.pool_len {
        Some(n) => std::cmp::max(1, n) as i64,
        None => 1,
    };
    let cap = inp.guard.as_ref().map_or(1000, |g| g.account_daily_cap) * accounts;
    let mut a = match cx.get("quota_ceiling") {
        Some(s) => int_via_float(s)?,
        None => cfg.quota_target_avg,
    };
    let reset_hour = clock.reset_hour();
    let members = inp.members;

    // ---- 每日微调（一天一次，看前一天）----
    let today = clock.day(now);
    let mut review: Option<Map<String, Value>> = None;
    if daily_adjust_due(cx.get(DAY_KEY), &today) {
        let d = inp
            .daily
            .as_ref()
            .ok_or_else(|| DecideError::MissingInput("daily".into()))?;
        let yday = clock.day(now - 86400.0);
        let stats = yesterday_stats(&d.yesterday);
        let covered = day_coverage(clock, &yday, d.coverage_first_ts)?;
        let v5_global = site_int(
            cx.get("global_daily_v5"),
            0.0,
            100_000.0,
            inp.global_daily_v5_default,
        );
        let budget = std::cmp::max(0, cap - v5_global);
        let (n, reasons) = if covered < params.min_coverage_hours as f64 {
            (
                a,
                vec![format!(
                    "昨天只有 {} 小时数据（不足 20 小时），每人额度不变",
                    fmt_prec(covered, 1)
                )],
            )
        } else {
            let (n, why) = simple(Some(a), &d.member_usage, budget, &cfg);
            (
                n,
                vec![format!(
                    "{why}（可分 = 账号日上限 {cap} − 全站 V5 {v5_global}）"
                )],
            )
        };
        let a2 = n;
        let mut r = Map::new();
        r.insert("day".into(), json!(yday));
        r.extend(stats.to_map());
        r.insert(
            "coverage_hours".into(),
            pyval::float(pyc::round(covered, 1)),
        );
        r.insert("cap".into(), json!(cap));
        r.insert("budget".into(), json!(budget));
        r.insert("from".into(), json!([a]));
        r.insert("to".into(), json!([a2]));
        r.insert("reasons".into(), json!(reasons));
        let mut history = as_history_slice(loads_setting(cx.get(HISTORY_KEY), "[]")?)?;
        history.push(Value::Object(r.clone()));
        cx.bulk(vec![
            ("quota_ceiling".into(), a2.to_string()),
            (DAY_KEY.into(), today.clone()),
            (HISTORY_KEY.into(), jsondump::dumps(&Value::Array(history))),
        ]);
        if a2 != a {
            cx.log("动态额度调整", format!("上限 {a}→{a2}"), reasons.join("；"));
        }
        review = Some(r);
        a = a2;
    }

    // ---- V5 ----
    let (pct, rate_api) = allowance_summary(&inp.accounts);
    // 有读数的上游账号数：剩余 % 与恢复速度是单个账号的平均，用掉的张数是全站合计
    let n_accounts = std::cmp::max(
        1,
        inp.accounts.iter().filter(|a| a.percent.is_some()).count() as i64,
    );
    let mut hist = as_history(loads_setting(cx.get(HISTORY_KEY), "[]")?)?;
    if let (Some(rev), Some(p)) = (review.as_mut(), pct) {
        // 每天记一笔账号 V5 剩余和接口给的恢复速度，并用「余量变化 + 前一天用掉的量」反推实测恢复速度
        rev.insert("v5_pct".into(), pyval::float(pyc::round(p, 1)));
        rev.insert(
            "v5_rate".into(),
            match rate_api {
                Some(r) if r != 0.0 => pyval::float(pyc::round(r, 1)),
                _ => Value::Null,
            },
        );
        let rev_day = rev["day"].clone();
        let same_day = match hist.last() {
            Some(h) => hget(h, "day")?.is_some_and(|d| py_eq(d, &rev_day)),
            None => false,
        };
        let prev: Option<Value> = if same_day {
            let n = hist.len();
            let mut found = None;
            for h in hist[..n - 1].iter().rev() {
                if hget_nn(h, "v5_pct")?.is_some() {
                    found = Some(h.clone());
                    break;
                }
            }
            found
        } else {
            None
        };
        if let Some(prev) = prev {
            let d = inp
                .daily
                .as_ref()
                .ok_or_else(|| DecideError::MissingInput("daily".into()))?;
            let used = v5_used_equiv(&d.v5_rows);
            let used_pct = pyc::round(used / V5_IMAGES_PER_PERCENT / n_accounts as f64, 2);
            rev.insert("v5_used_pct".into(), pyval::float(used_pct));
            let prev_pct = num(hget_nn(&prev, "v5_pct")?.expect("上面刚判过非空"))?;
            rev.insert(
                "v5_rate_measured".into(),
                pyval::float(pyc::round(p - prev_pct + used_pct, 2)),
            );
        }
        if same_day {
            let last = hist.len() - 1;
            hist[last] = Value::Object(rev.clone());
            cx.set(HISTORY_KEY, jsondump::dumps(&Value::Array(hist.clone())));
        }
    }
    let mut measured: Option<f64> = None;
    for h in hist.iter().rev() {
        if let Some(v) = hget_nn(h, "v5_rate_measured")? {
            measured = Some(num(v)?);
            break;
        }
    }
    let quiet = quiet_recovery(&inp.quiet);
    if let Some(q) = &quiet {
        // 安静时段直接量到的恢复速度最可靠：优先用它
        measured = Some(q.rate);
    }
    let (rate, mut rate_info) = choose_rate(rate_api, measured);
    if let Some(q) = &quiet {
        rate_info.insert("quiet".into(), q.to_value());
    }
    let stored = loads_setting(cx.get(V5_DAY_KEY), "{}")?;
    let stored = match stored {
        Value::Object(m) => m,
        other => {
            return Err(DecideError::Attribute(format!(
                "{other} 没有 get（V5 方案不是 dict）"
            )));
        }
    };
    // 当天的方案在服务日第一次计算时（07:00 后）定下，当天不再改：
    // 旧版的电量系数、3～30 夹取、电量 ≥ 85% 的洪峰放宽、白天的放宽 / 收紧都已删除。
    let stored_plan = match (stored.get("day"), stored.get("plan")) {
        (Some(Value::String(d)), Some(p)) if *d == today && pyval::truthy(p) => Some(p.clone()),
        _ => None,
    };
    let mut v5: Map<String, Value> = match stored_plan {
        Some(Value::Object(sp)) => sp,
        Some(_) => return Err(DecideError::Type("V5 方案不是 dict".into())),
        None => {
            let mut fresh = v5_plan_n(pct, Some(rate), inp.active_v5, n_accounts).to_map();
            fresh.insert("rate_info".into(), Value::Object(rate_info.clone()));
            let v = json!({"day": today, "plan": Value::Object(fresh.clone())});
            cx.set(V5_DAY_KEY, jsondump::dumps(&v));
            fresh
        }
    };

    // 当天的方案一天只定一次，但显示用的剩余要用实时值；核对状态每 10 分钟更新
    if let Some(p) = pct {
        v5.insert("percent".into(), pyval::float(p));
    }
    v5.insert("rate_check".into(), Value::Object(rate_info));

    // ---- 应用：所有由算法管理的成员同一套额度 ----
    let each = {
        let e = v5
            .get("each")
            .ok_or_else(|| DecideError::Key("'each'".into()))?;
        pyval::int_like(e).ok_or_else(|| DecideError::Type(format!("each 不是整数: {e}")))?
    };
    let global = {
        let g = v5
            .get("global")
            .ok_or_else(|| DecideError::Key("'global'".into()))?;
        pyval::int_like(g).ok_or_else(|| DecideError::Type(format!("global 不是整数: {g}")))?
    };
    let changed = inp
        .managed_keys
        .iter()
        .filter(|k| k.daily_images != a || k.daily_v5 != each || k.image_model_scope != "all")
        .count() as i64;
    if changed != 0 {
        cx.effects.push(Effect::UpdateKeys {
            daily_images: a,
            daily_v5: each,
        });
    }
    cx.bulk(vec![
        ("global_daily_v5".into(), global.to_string()),
        ("register_daily_images".into(), a.to_string()),
        ("register_daily_v5".into(), each.to_string()),
        ("register_image_scope".into(), "all".into()),
    ]);
    // 每人 V5 额度 × 活跃人数 > 全站 V5 池时，个人额度是「先到先得」的上限而不是承诺。
    // 人数按前 3 个完整服务日用过 V5 的人算，不按全部算法管理的 Key；
    // 键名 `members` 沿用（后台显示「每人 × 人数」）
    let people = inp.active_v5;
    let oversell = global != 0 && each * std::cmp::max(1, people) > global;
    v5.insert(
        "oversell".into(),
        if oversell {
            json!({"each": each, "members": people, "global": global})
        } else {
            Value::Null
        },
    );
    let member_anlas = site_int(cx.get("member_daily_anlas"), 0.0, 100_000.0, 50);
    let review_value = review.clone().map(Value::Object);
    let v5_value = Value::Object(v5.clone());
    let note = quota_notice(
        a,
        &v5_value,
        people,
        &today,
        review_value.as_ref(),
        reset_hour,
        member_anlas,
    )?;
    cx.set(NOTICE_KEY, note);
    let mut result = Map::new();
    result.insert("enabled".into(), json!(true));
    result.insert("ceiling".into(), json!(a));
    result.insert("v5".into(), v5_value);
    result.insert("members".into(), json!(members));
    result.insert("cap".into(), json!(cap));
    result.insert("updated_at".into(), pyval::float(now));
    result.insert("changed_keys".into(), json!(changed));
    result.insert("review".into(), review_value.unwrap_or(Value::Null));
    let result = Value::Object(result);
    cx.set(STATE_KEY, jsondump::dumps(&result));
    if changed != 0 {
        let pct_s = pct.map_or("None".to_string(), pyc::float_repr);
        let rate_s = pyval::py_str(
            v5.get("rate")
                .ok_or_else(|| DecideError::Key("'rate'".into()))?,
        );
        cx.log(
            "动态额度应用",
            format!("{changed} 把 Key"),
            format!(
                "V4.5 上限 {a} · V5 每人 {each}，全站 {global}（剩余 {pct_s}%，恢复 {rate_s}%/天）"
            ),
        );
    }
    Ok(result)
}

/// `json.loads(...)[-29:]`：只保留最近 29 条。
/// 不是列表时的异常与 Python 3.12 一致：dict 用 slice 当键 → `KeyError`（3.12 起 slice 可哈希），
/// 其余（字符串切片后再 `+ [review]`、数字 / None 不可下标）→ `TypeError`。
fn as_history_slice(v: Value) -> Result<Vec<Value>, DecideError> {
    match v {
        Value::Array(mut a) => {
            let n = a.len();
            if n > 29 {
                a.drain(..n - 29);
            }
            Ok(a)
        }
        Value::Object(_) => Err(DecideError::Key("slice(-29, None, None)".into())),
        other => Err(DecideError::Type(format!("历史不是列表: {other}"))),
    }
}
