//! 自动驾驶（旧 `app/autopilot.py`）：名额 +5、全站熔断、单个 Key 限流。
//!
//! 与 [`crate::quota`] 同样的拆法：纯规则函数 + 决策核心 [`decide`]（`AutopilotInputs` → 有序 `effects` + 返回值）。
//! 评估周期内处罚结果不许变：怪癖照抄（见下方「与旧版一致的怪癖」）。
//!
//! # 服务层要实现的查询 / 内存读取（`AutopilotInputs` 各字段的来源）
//!
//! | 字段 | 来源 | 参数 |
//! |---|---|---|
//! | `settings` | `SELECT value FROM site_settings WHERE key=?`，键见 [`SETTING_KEYS_READ`] | — |
//! | `registrar` | `registrar.settings()["max_users"]`（`or 0`）与 `registrar.count_active()`（持有未过期 Key 的已注册用户数）；没有注册服务时传 `None` | — |
//! | `global_daily_v5_default` | `site_flags.default_of(GLOBAL_DAILY_V5, settings)`（只在上一轮复盘缺 `budget` 时用到） | — |
//! | `queue_hours` | `SELECT CAST(ts/3600 AS INT) h FROM usage_log WHERE ts>=? AND ts<? AND reason='queue_full' GROUP BY h HAVING COUNT(*)>=?` 的**行数**；仅当上一轮复盘有 `day` 时需要 | `(start, end, 5)`，见 [`queue_window`] |
//! | `breaker` | `SELECT SUM(CASE WHEN status='error' AND (up_status>=500 OR up_status BETWEEN 200 AND 299) THEN 1 ELSE 0 END), COUNT(*) FROM usage_log WHERE ts>=? AND kind LIKE 'image%' AND status IN ('ok','error')`（SUM 可能为 NULL）；3.0 另取这些失败里不同的非测试 Key 数 `fail_keys`（docs/v3-deviations.md #58） | `(since)`，见 [`breaker_since`] |
//! | `rejects` | `SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' AND key_id IS NOT NULL AND reason NOT IN (<`[`KEY_GUARD_SKIP_REASONS`]`>) GROUP BY key_id` | `(now - 3600)` |
//! | `keys` | `SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0`（**保持数据库返回顺序**：决定 `decisions` 的顺序） | — |
//! | `guard` | 内存里的 Guard：`breaker_until`；没有 Guard 传 `None` | — |
//! | `pauses` | 内存里的 Key 暂停表（`state.pauses`）：`paused`（`key_id → 暂停到期时间`）；没有传 `None`。夹具里这个字段叫 `share`（2.x 的 ShareGuard），冻结不改，解析时当别名收 | — |
//!
//! # `effects` 的执行方式（按顺序）
//!
//! 同 [`crate::quota::Effect`]；自动驾驶额外用到：
//! - `SetRegistrationMaxUsers`：`ops.set_registration(db, {"max_users": n, "notify": False}, state, registrar)`
//!   （= `set_settings_bulk({"register_max_users": n})`，不发公告）；
//! - `TripBreaker`：`guard.trip_breaker(seconds, reason, now)`（内存态，`breaker_until = now + seconds`）；
//! - `PauseKey`：暂停单把 Key（旧版 `share.pause_key(key_id, seconds, reason, now)`）：写 `share_state` 表
//!   （现在它只是每把 Key 的临时暂停存储，表名为兼容旧库保留）并更新内存暂停表。
//!   旧版只有 `pause_key` 返回 True（此前不在暂停中）才记日志、标 `applied`；这一步已在 `decide` 里
//!   用 `pauses.paused` 提前判过，只有会成功的才会产出 `PauseKey` 与对应日志。
//!
//! # 与旧版一致的怪癖
//!
//! - 上一轮复盘里 `queue_full` 的统计窗口是 `day` 的**本地 0 点起 24 小时**（`time.mktime(strptime(day))`），
//!   而复盘本身统计的是服务日（7 点到次日 7 点）：两者错开 7 小时。
//! - 没有注册服务时 `max_users` 视为 0，走「名额不限」分支。
//!
//! # 已删除的规则
//!
//! 2.x 的单 Key 守护还有「24 小时内多次来回切换网段 → 暂停 24 小时」「3 种以上客户端且几乎全天在用 → 重置 Key」
//! 两条，输入来自防分享证据（`events`）。防分享体系在 2.x 就删了，输入恒为空，这两条永远不会触发；3.0 连同
//! `events` 输入一起删除（docs/v3-deviations.md #51）。现在 `key_guard` 只剩「1 小时被拒 ≥ 60 次 → 暂停 1 小时」。

use crate::clock::Clock;
use crate::quota::{Cx, DecideError, Effect, fmt_prec, hget, loads_setting, num, py_max, site_int};
use crate::{jsondump, pyc, pyval};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub const STATE_KEY: &str = "autopilot_last";
pub const HISTORY_KEY: &str = "autopilot_history";

pub const SLOTS_MAX: i64 = 100;
pub const SLOTS_STEP: i64 = 5;
/// 每天最多加一次名额。
pub const SLOTS_COOLDOWN: f64 = 24.0 * 3600.0;
/// 一个小时里「排队的人太多」拒绝 ≥ 5 次，算这个小时高峰拥挤。
pub const QUEUE_BUSY_REJECTS: i64 = 5;
/// 账号 V5 剩余低于这个百分比：不加名额。
pub const SLOTS_V5_MIN: f64 = 50.0;

/// 单 Key 限流统计要排除的拒绝原因码：`KEY_PAUSED` + `reasons.SITE_LEVEL`（全站层面的拒绝不算成员的错）。
pub const KEY_GUARD_SKIP_REASONS: [&str; 14] = [
    "key_paused",
    "queue_full",
    "hourly_cap",
    "quiet_cap",
    "burst_cap",
    "cap_3h",
    "daily_cap",
    "paced",
    "over_base",
    "breaker",
    "cooldown",
    "v5_exhausted",
    "queue_timeout",
    "site_quota",
];

/// `decide` 会读的 `site_settings` 键。
pub const SETTING_KEYS_READ: [&str; 9] = [
    "autopilot_slots",
    "autopilot_breaker",
    "autopilot_key_guard",
    "autopilot_slots_at",
    "autopilot_breaker_until",
    "quota_algo_history",
    "quota_algo_last",
    HISTORY_KEY,
    "global_daily_v5",
];

// ---------------------------------------------------------------------------
// 纯规则
// ---------------------------------------------------------------------------

/// `f"{x:.0%}"`：先乘 100 再按 `.0f` 正确舍入，末尾带 `%`（CPython 的百分号格式就是这样做的）。
fn percent0(x: f64) -> String {
    format!("{}%", fmt_prec(x * 100.0, 0))
}

/// 名额快满（空位 ≤ 2）且账号昨天还有余量 → +5。只加不减。返回 `(新名额, 理由)`。
pub fn slots_rule(
    cap: i64,
    active: i64,
    day_util: f64,
    blocked_hours: i64,
    queue_hours: i64,
    v5_pct: Option<f64>,
) -> (i64, String) {
    if let Some(p) = v5_pct
        && p < SLOTS_V5_MIN
    {
        return (
            cap,
            format!("账号 V5 只剩 {}%：名额不再增加", fmt_prec(p, 0)),
        );
    }
    if blocked_hours >= 3 {
        return (
            cap,
            format!("昨天 {blocked_hours} 个小时被每小时上限拦过：名额不再增加"),
        );
    }
    if queue_hours >= 3 {
        return (
            cap,
            format!(
                "昨天 {queue_hours} 个小时高峰排队拥挤（每小时排队满被拒 ≥ {QUEUE_BUSY_REJECTS} 次）：名额不再增加"
            ),
        );
    }
    if day_util >= 0.6 {
        return (
            cap,
            format!("昨天用量 {}：名额不再增加", percent0(day_util)),
        );
    }
    if cap != 0 && cap < SLOTS_MAX && cap - active <= 2 {
        return (
            std::cmp::min(SLOTS_MAX, cap + SLOTS_STEP),
            format!(
                "名额 {active}/{cap}，昨天用量 {}：名额 +{SLOTS_STEP}",
                percent0(day_util)
            ),
        );
    }
    (
        cap,
        format!(
            "名额 {active}/{cap}、昨天用量 {}：名额不变",
            percent0(day_util)
        ),
    )
}

/// 熔断还要求失败来自至少这么多把不同的非测试 Key：一个成员反复发畸形请求、测试 Key 出错都不算上游故障
/// （与 docs/v3-deviations.md #7 / #8 的「≥ 2 把 Key 才算上游故障」同一口径，见 #58）。
pub const BREAKER_MIN_KEYS: i64 = 2;

/// 最近 15 分钟上游失败 ≥ 5 次且占比 ≥ 30% → 熔断（旧版规则，夹具对拍；3.0 另加 Key 数门槛，见 [`breaker_rule_keys`]）。
pub fn breaker_rule(fails: i64, total: i64) -> (bool, String) {
    if fails >= 5 && total != 0 && (fails as f64 / total as f64) >= 0.3 {
        return (
            true,
            format!("最近 15 分钟上游失败 {fails}/{total}：暂停出图 5 分钟"),
        );
    }
    (false, format!("最近 15 分钟上游失败 {fails}/{total}：正常"))
}

/// [`breaker_rule`] 加上 Key 数门槛：失败只来自不到 [`BREAKER_MIN_KEYS`] 把非测试 Key 时不熔断。
pub fn breaker_rule_keys(fails: i64, total: i64, fail_keys: i64) -> (bool, String) {
    let (trip, why) = breaker_rule(fails, total);
    if trip && fail_keys < BREAKER_MIN_KEYS {
        return (
            false,
            format!(
                "最近 15 分钟上游失败 {fails}/{total}，只来自 {fail_keys} 把 Key（多半是这把 Key 的请求有问题）：不熔断"
            ),
        );
    }
    (trip, why)
}

/// 单 Key 限流：1 小时内被拒 ≥ 这个次数，多半是客户端在死循环重试。
pub const KEY_GUARD_REJECTS: i64 = 60;
/// 触发后暂停这把 Key 的秒数。
pub const KEY_GUARD_PAUSE_SECS: i64 = 3600;

/// 单 Key 限流：1 小时内被拒 ≥ [`KEY_GUARD_REJECTS`] 次 → 返回暂停的原因；否则 `None`。
pub fn key_guard_rule(rejects_1h: i64) -> Option<String> {
    (rejects_1h >= KEY_GUARD_REJECTS)
        .then(|| format!("1 小时内被拒绝 {rejects_1h} 次（多半是客户端在死循环重试）：暂停 1 小时"))
}

/// 熔断统计的下界参数：`max(now - 900, float(autopilot_breaker_until or 0))`——只看上次熔断结束之后的数据。
pub fn breaker_since(now: f64, breaker_until_setting: Option<&str>) -> Result<f64, DecideError> {
    Ok(py_max(now - 900.0, setting_float(breaker_until_setting)?))
}

/// `float(get_setting(key, 0) or 0)`。
fn setting_float(raw: Option<&str>) -> Result<f64, DecideError> {
    match raw {
        None => Ok(0.0),
        Some("") => Ok(0.0),
        Some(s) => pyc::parse_float(s)
            .ok_or_else(|| DecideError::Value(format!("could not convert string to float: {s:?}"))),
    }
}

/// 上一轮复盘（`quota_algo_history` 的最后一条）里 `queue_full` 统计窗口 `(start, end)`；复盘没有 `day` 返回 `None`。
/// 注意是 `day` 的**本地 0 点**起 24 小时，不是服务日起点（与旧版一致）。
pub fn queue_window(
    history_raw: Option<&str>,
    clock: &Clock,
) -> Result<Option<(f64, f64)>, DecideError> {
    let hist = loads_setting(history_raw, "[]")?;
    let last = last_review(&hist)?;
    match hget(&last, "day")? {
        Some(d) if pyval::truthy(d) => {
            let day = d
                .as_str()
                .ok_or_else(|| DecideError::Type("strptime 需要字符串".into()))?;
            let start = clock.local_midnight(day).ok_or_else(|| {
                DecideError::Value(format!("time data {day:?} does not match format"))
            })?;
            Ok(Some((start, start + 86400.0)))
        }
        _ => Ok(None),
    }
}

/// `hist[-1] if hist else {}`（`hist` 来自 `json.loads`，可能不是列表）。
fn last_review(hist: &Value) -> Result<Value, DecideError> {
    match hist {
        Value::Array(a) => Ok(a.last().cloned().unwrap_or_else(|| json!({}))),
        // 真值的非列表：dict `[-1]` → KeyError；str 取到字符，下一步 `.get` → AttributeError；数字 / True → TypeError
        Value::Object(o) if !o.is_empty() => Err(DecideError::Key("-1".into())),
        Value::String(s) if !s.is_empty() => Err(DecideError::Attribute(
            "'str' object has no attribute 'get'".into(),
        )),
        Value::Number(n) if n.as_f64().is_some_and(|f| f != 0.0) => Err(DecideError::Type(
            "'int' object is not subscriptable".into(),
        )),
        Value::Bool(true) => Err(DecideError::Type(
            "'bool' object is not subscriptable".into(),
        )),
        _ => Ok(json!({})),
    }
}

// ---------------------------------------------------------------------------
// 决策核心
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registrar {
    /// `registrar.settings()["max_users"]`（`or 0`）。
    pub max_users: i64,
    /// `registrar.count_active()`
    pub active: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerRows {
    /// `SUM(CASE …)`，没有行时为 NULL。
    pub fails: Option<i64>,
    pub total: Option<i64>,
    /// 这些失败来自多少把不同的非测试 Key（`is_test=0`）。不到 [`BREAKER_MIN_KEYS`] 把不熔断
    /// （docs/v3-deviations.md #58）。冻结夹具里没有这个字段，缺省为 0。
    #[serde(default)]
    pub fail_keys: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardState {
    /// `guard.breaker_until`
    pub breaker_until: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PausedKeys {
    /// 当前的暂停表：`key_id → 暂停到期时间`
    pub paused: HashMap<i64, f64>,
}

/// `run` 读到的一切。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutopilotInputs {
    pub now: f64,
    pub settings: HashMap<String, String>,
    pub registrar: Option<Registrar>,
    pub global_daily_v5_default: i64,
    pub queue_hours: i64,
    pub breaker: BreakerRows,
    pub guard: Option<GuardState>,
    /// 夹具里叫 `share`（2.x 的 ShareGuard），冻结不改。
    #[serde(alias = "share")]
    pub pauses: Option<PausedKeys>,
    /// `(key_id, 被拒次数)`，查询返回顺序。
    pub rejects: Vec<(i64, i64)>,
    /// `(id, name)`，查询返回顺序。
    pub keys: Vec<(i64, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutopilotOutputs {
    pub effects: Vec<Effect>,
    /// `run` 的返回值（同时写进 `autopilot_last`）。
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// `_mode`：不认识的值回到 `observe`。
fn mode(cx: &Cx, rule: &str) -> String {
    match cx.get(&format!("autopilot_{rule}")) {
        Some(v @ ("observe" | "enforce" | "off")) => v.to_string(),
        _ => "observe".to_string(),
    }
}

/// `run(state, registrar, now)` 的决策核心。
pub fn decide(inp: &AutopilotInputs, clock: &Clock) -> AutopilotOutputs {
    let mut cx = Cx {
        settings: inp.settings.clone(),
        effects: Vec::new(),
    };
    match run_inner(inp, clock, &mut cx) {
        Ok(v) => AutopilotOutputs {
            effects: cx.effects,
            result: Some(v),
            error: None,
        },
        Err(e) => AutopilotOutputs {
            effects: cx.effects,
            result: None,
            error: Some(e.py_type().to_string()),
        },
    }
}

/// `int(v)`：数字截断、bool → 0/1、字符串按 `int()` 解析（失败 ValueError）、其余 TypeError。
fn py_int(v: &Value) -> Result<i64, DecideError> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => Ok(n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map_or(0, |f| f.trunc() as i64))),
        Value::String(s) => pyc::parse_int(s).ok_or_else(|| {
            DecideError::Value(format!("invalid literal for int() with base 10: {s:?}"))
        }),
        other => Err(DecideError::Type(format!("int() 不接受 {other}"))),
    }
}

/// `x.get(key, 0)` 当数字用：缺失 → 0；JSON null / 非数字 → TypeError。
fn num_or_zero(h: &Value, key: &str) -> Result<f64, DecideError> {
    match hget(h, key)? {
        None => Ok(0.0),
        Some(v) => num(v),
    }
}

fn run_inner(inp: &AutopilotInputs, clock: &Clock, cx: &mut Cx) -> Result<Value, DecideError> {
    let now = inp.now;
    let mut rules = Map::new();

    // ---- 名额 ----
    let (cap_cfg, active) = match &inp.registrar {
        Some(r) => (r.max_users, r.active),
        None => (0, 0),
    };
    let hist = loads_setting(cx.get("quota_algo_history"), "[]")?;
    let last = last_review(&hist)?;
    // 分母必须与额度算法同一口径：用算法自己记的 budget 优先，缺失时才用 cap - 全站 V5
    let budget_v = hget(&last, "budget")?;
    let budget: f64 = match budget_v {
        Some(v) if pyval::truthy(v) => num(v)?,
        _ => {
            let v5_global = site_int(
                cx.get("global_daily_v5"),
                0.0,
                100_000.0,
                inp.global_daily_v5_default,
            );
            let cap_v = match hget(&last, "cap")? {
                Some(v) if pyval::truthy(v) => num(v)?,
                _ => 0.0,
            };
            py_max(0.0, cap_v - v5_global as f64)
        }
    };
    let day_util = if budget != 0.0 {
        num_or_zero(&last, "used")? / budget
    } else {
        0.0
    };
    let cap_now = cap_cfg;
    let mut queue_hours = 0;
    if let Some(d) = hget(&last, "day")?
        && pyval::truthy(d)
    {
        // strptime 失败的异常由 queue_window 负责（服务层先调它取查询参数）
        let day = d
            .as_str()
            .ok_or_else(|| DecideError::Type("strptime 需要字符串".into()))?;
        clock.local_midnight(day).ok_or_else(|| {
            DecideError::Value(format!("time data {day:?} does not match format"))
        })?;
        queue_hours = inp.queue_hours;
    }
    let state_json = loads_setting(cx.get("quota_algo_last"), "{}")?;
    let v5_node = match &state_json {
        Value::Object(m) => m.get("v5"),
        other => {
            return Err(DecideError::Attribute(format!(
                "{other} 没有 get（额度状态不是 dict）"
            )));
        }
    };
    // `(state.get("v5") or {}).get("percent")`：真值的非 dict 会在 `.get` 处抛 AttributeError
    let v5_pct_raw: Option<&Value> = match v5_node {
        Some(v) if pyval::truthy(v) => match v {
            Value::Object(m) => m.get("percent").filter(|p| !p.is_null()),
            _ => return Err(DecideError::Attribute("v5 不是 dict".into())),
        },
        _ => None,
    };
    // 参数求值顺序与旧版一致：先 int(hourly_blocks)，再在 slots_rule 里拿 v5_pct 做比较
    let blocked_hours = match hget(&last, "hourly_blocks")? {
        None => 0,
        Some(v) => py_int(v)?,
    };
    let v5_pct: Option<f64> = match v5_pct_raw {
        Some(p) => Some(num(p)?),
        None => None,
    };
    let (mut slots, mut why) = slots_rule(
        cap_now,
        active,
        day_util,
        blocked_hours,
        queue_hours,
        v5_pct,
    );
    // 昨天数据不完整：不据此加名额
    let cov_v = hget(&last, "coverage_hours")?;
    let cov_num = match cov_v {
        None => 0.0,
        Some(v) => num(v)?,
    };
    if cov_num < 20.0 {
        slots = cap_now;
        why = match cov_v {
            Some(v) if !v.is_null() => format!("昨天只有 {} 小时数据，名额不变", pyval::py_str(v)),
            _ => {
                let day = match hget(&last, "day")? {
                    Some(d) => pyval::py_str(d),
                    None => "昨天".to_string(),
                };
                format!("{day} 的数据不完整（系统从那天中途开始记录），名额不变")
            }
        };
    }
    if cap_now == 0 {
        // 名额不限：没有上限可调，否则 0 → +5 会把「不限」改成 5
        slots = 0;
        why = "名额不限（站长设定），不调整；人多了由动态额度缩小每人份额".to_string();
    }
    let slots_mode = mode(cx, "slots");
    let mut slots_rule_v = Map::new();
    slots_rule_v.insert("mode".into(), json!(slots_mode));
    slots_rule_v.insert("value".into(), json!(slots));
    slots_rule_v.insert("why".into(), json!(why));
    if slots_mode == "enforce" && cap_now != 0 && slots > cap_now && inp.registrar.is_some() {
        let last_at = setting_float(cx.get("autopilot_slots_at"))?;
        if now - last_at >= SLOTS_COOLDOWN {
            cx.effects
                .push(Effect::SetRegistrationMaxUsers { max_users: slots });
            cx.set("autopilot_slots_at", pyc::float_repr(now));
            cx.log(
                "自动驾驶：名额",
                String::new(),
                format!("{cap_now} → {slots}：{why}"),
            );
            slots_rule_v.insert("applied".into(), json!(true));
        }
    }
    rules.insert("slots".into(), Value::Object(slots_rule_v));

    // ---- 熔断：只看上次熔断结束之后的上游失败（窗口由服务层按 `breaker_since` 查好）；失败须来自 ≥ 2 把非测试 Key ----
    // 旧版在这里再算一遍 `since` 只为在 `autopilot_breaker_until` 读不懂时抛 ValueError；3.0 不再因此整轮失败
    // （服务层读不懂时按没有失败处理，docs/v3-deviations.md #58）
    let fails = inp.breaker.fails.unwrap_or(0);
    let total = inp.breaker.total.unwrap_or(0);
    let (trip, why) = breaker_rule_keys(fails, total, inp.breaker.fail_keys);
    let breaker_mode = mode(cx, "breaker");
    let mut breaker_v = Map::new();
    breaker_v.insert("mode".into(), json!(breaker_mode));
    breaker_v.insert("value".into(), json!(trip));
    breaker_v.insert("why".into(), json!(why));
    if trip
        && breaker_mode == "enforce"
        && let Some(g) = &inp.guard
        && g.breaker_until <= now
    {
        cx.effects.push(Effect::TripBreaker {
            seconds: 300,
            reason: format!("上游连续出错，已暂停出图 5 分钟（{why}）"),
            now,
        });
        cx.set("autopilot_breaker_until", pyc::float_repr(now + 300.0));
        cx.log(
            "自动驾驶：熔断",
            String::new(),
            format!("{why}；全站暂停出图 5 分钟"),
        );
        breaker_v.insert("applied".into(), json!(true));
    }
    rules.insert("breaker".into(), Value::Object(breaker_v));

    // ---- 单个 Key ----
    let rejects: HashMap<i64, i64> = inp.rejects.iter().copied().collect();
    let mut decisions: Vec<Map<String, Value>> = Vec::new();
    // `{r[0]: r[1] for r in rows}`：重复的 id 位置取第一次、名字取最后一次（数据库主键不会重复，这里只是保险）
    let mut keys: Vec<(i64, &String)> = Vec::new();
    for (kid, name) in &inp.keys {
        match keys.iter_mut().find(|(k, _)| k == kid) {
            Some(slot) => slot.1 = name,
            None => keys.push((*kid, name)),
        }
    }
    for (kid, name) in &keys {
        if let Some(why) = key_guard_rule(rejects.get(kid).copied().unwrap_or(0)) {
            let mut m = Map::new();
            m.insert("key".into(), json!(kid));
            m.insert("name".into(), json!(name));
            m.insert("action".into(), json!("pause"));
            m.insert("seconds".into(), json!(KEY_GUARD_PAUSE_SECS));
            m.insert("why".into(), json!(why));
            decisions.push(m);
        }
    }
    let kg_mode = mode(cx, "key_guard");
    let kg_why = if decisions.is_empty() {
        "没有 Key 触发".to_string()
    } else {
        format!("{} 把 Key 触发", decisions.len())
    };
    if !decisions.is_empty() && kg_mode == "enforce" {
        for d in decisions.iter_mut() {
            let kid = d["key"].as_i64().unwrap_or(0);
            if let Some(pauses) = &inp.pauses {
                // 已在暂停中则不重复（旧版 pause_key 返回 False）
                let already = pauses.paused.get(&kid).copied().unwrap_or(0.0) > now;
                if !already {
                    cx.effects.push(Effect::PauseKey {
                        key_id: kid,
                        seconds: KEY_GUARD_PAUSE_SECS,
                        reason: "你的 Key 短时间内被大量拒绝，疑似客户端在反复重试，已暂停 1 小时"
                            .to_string(),
                        now,
                    });
                    let why = d["why"].as_str().unwrap_or("").to_string();
                    let name = pyval::py_str(&d["name"]);
                    cx.log("自动驾驶：Key 限流", name, why);
                    d.insert("applied".into(), json!(true));
                }
            }
        }
    }
    let kg_value = Value::Array(decisions.iter().cloned().map(Value::Object).collect());
    let mut kg = Map::new();
    kg.insert("mode".into(), json!(kg_mode));
    kg.insert("value".into(), kg_value);
    kg.insert("why".into(), json!(kg_why));
    rules.insert("key_guard".into(), Value::Object(kg));

    // ---- 观察模式：只记录；有「会执行的动作」时写操作日志，方便第二天审核 ----
    let mut notable: Vec<String> = Vec::new();
    for (r, v) in &rules {
        let observed = v["mode"] != "enforce";
        let hit = (r == "key_guard" && pyval::truthy(&v["value"]))
            || (r == "breaker" && pyval::truthy(&v["value"]));
        if observed && hit {
            notable.push(format!("{r}：{}", v["why"].as_str().unwrap_or("")));
        }
    }
    if !notable.is_empty() {
        let joined = notable.join("；");
        cx.log(
            "自动驾驶（观察）",
            String::new(),
            pyc::take_chars(&joined, 500).to_string(),
        );
    }
    let mut out = Map::new();
    out.insert("at".into(), pyval::float(now));
    out.insert("rules".into(), Value::Object(rules.clone()));
    let out = Value::Object(out);
    cx.set(STATE_KEY, jsondump::dumps(&out));

    // ---- 每小时留一条快照，保留 7 天 ----
    let hour = clock.format(now, "%Y-%m-%d %H:00");
    let hist = loads_setting(cx.get(HISTORY_KEY), "[]")?;
    let mut hist: Vec<Value> = match hist {
        Value::Array(a) => a,
        // 空 / 假值：`not hist` 为真 → `hist + [...]` 对非列表抛 TypeError；真值：`hist[-1]` 取到字符 / 抛 KeyError
        Value::Object(o) if !o.is_empty() => return Err(DecideError::Key("-1".into())),
        Value::String(s) if !s.is_empty() => {
            return Err(DecideError::Attribute(
                "'str' object has no attribute 'get'".into(),
            ));
        }
        other => return Err(DecideError::Type(format!("历史不是列表: {other}"))),
    };
    let need = match hist.last() {
        None => true,
        Some(h) => !hget(h, "hour")?.is_some_and(|x| x.as_str() == Some(hour.as_str())),
    };
    if need {
        let mut entry = Map::new();
        entry.insert("hour".into(), json!(hour));
        for (r, v) in &rules {
            let val = if r == "key_guard" {
                json!(v["value"].as_array().map_or(0, Vec::len))
            } else {
                v["value"].clone()
            };
            entry.insert(r.clone(), val);
        }
        hist.push(Value::Object(entry));
        let n = hist.len();
        if n > 168 {
            hist.drain(..n - 168);
        }
        cx.set(HISTORY_KEY, jsondump::dumps(&Value::Array(hist)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_needs_two_keys() {
        // 旧版规则成立时，失败只来自 0 / 1 把 Key 不熔断（docs/v3-deviations.md #58）
        assert!(breaker_rule(9, 10).0);
        for keys in [0, 1] {
            let (trip, why) = breaker_rule_keys(9, 10, keys);
            assert!(!trip);
            assert_eq!(
                why,
                format!(
                    "最近 15 分钟上游失败 9/10，只来自 {keys} 把 Key（多半是这把 Key 的请求有问题）：不熔断"
                )
            );
        }
        assert_eq!(breaker_rule_keys(9, 10, 2), breaker_rule(9, 10));
        assert_eq!(breaker_rule_keys(9, 10, 5), breaker_rule(9, 10));
        // 旧版就不熔断的，Key 再多也不熔断，文案不变
        assert_eq!(breaker_rule_keys(4, 10, 4), breaker_rule(4, 10));
        assert_eq!(breaker_rule_keys(5, 100, 5), breaker_rule(5, 100));
    }

    fn inputs(fail_keys: i64, breaker_until: Option<&str>) -> AutopilotInputs {
        let mut settings =
            HashMap::from([("autopilot_breaker".to_string(), "enforce".to_string())]);
        if let Some(v) = breaker_until {
            settings.insert("autopilot_breaker_until".into(), v.into());
        }
        AutopilotInputs {
            now: 1_791_950_400.0,
            settings,
            registrar: None,
            global_daily_v5_default: 0,
            queue_hours: 0,
            breaker: BreakerRows {
                fails: Some(8),
                total: Some(10),
                fail_keys,
            },
            guard: Some(GuardState { breaker_until: 0.0 }),
            pauses: None,
            rejects: vec![],
            keys: vec![],
        }
    }

    fn tripped(out: &AutopilotOutputs) -> bool {
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::TripBreaker { .. }))
    }

    #[test]
    fn one_key_cannot_stop_the_whole_site() {
        let clock = Clock::new("Asia/Shanghai", 7);
        let one = decide(&inputs(1, None), &clock);
        assert!(one.error.is_none(), "{:?}", one.error);
        assert!(!tripped(&one));
        assert_eq!(
            one.result.unwrap()["rules"]["breaker"]["value"],
            json!(false)
        );
        let two = decide(&inputs(2, None), &clock);
        assert!(tripped(&two));
        assert_eq!(
            two.result.unwrap()["rules"]["breaker"]["applied"],
            json!(true)
        );
    }

    #[test]
    fn unreadable_breaker_until_no_longer_fails_the_round() {
        // 旧版在熔断这一步抛 ValueError、整轮自动驾驶失败；3.0 照常往下走
        // （服务层读不懂时按没有失败处理，见 gate-server kernel::algo，docs/v3-deviations.md #58）
        let clock = Clock::new("Asia/Shanghai", 7);
        let out = decide(&inputs(2, Some("not-a-number")), &clock);
        assert!(out.error.is_none(), "{:?}", out.error);
        assert!(out.result.is_some());
    }
}
