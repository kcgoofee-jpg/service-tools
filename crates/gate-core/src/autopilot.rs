//! 自动驾驶（旧 `app/autopilot.py`）：全站熔断、单个 Key 限流。
//!
//! 与 [`crate::quota`] 同样的拆法：纯规则函数 + 决策核心 [`decide`]（`AutopilotInputs` → 有序 `effects` + 返回值）。
//! 评估周期内处罚结果不许变：判定与文案照旧版。
//!
//! # 服务层要实现的查询 / 内存读取（`AutopilotInputs` 各字段的来源）
//!
//! | 字段 | 来源 | 参数 |
//! |---|---|---|
//! | `settings` | `SELECT value FROM site_settings WHERE key=?`，键见 [`SETTING_KEYS_READ`] | — |
//! | `breaker` | `SELECT SUM(CASE WHEN status='error' AND (up_status>=500 OR up_status BETWEEN 200 AND 299) THEN 1 ELSE 0 END), COUNT(*) FROM usage_log WHERE ts>=? AND kind LIKE 'image%' AND status IN ('ok','error')`（SUM 可能为 NULL） | `(since)`，见 [`breaker_since`] |
//! | `rejects` | `SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' AND key_id IS NOT NULL AND reason NOT IN (<`[`KEY_GUARD_SKIP_REASONS`]`>) GROUP BY key_id` | `(now - 3600)` |
//! | `keys` | `SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0`（**保持数据库返回顺序**：决定 `decisions` 的顺序） | — |
//! | `guard` | 内存里的 Guard：`breaker_until`；没有 Guard 传 `None` | — |
//! | `pauses` | 内存里的 Key 暂停表（`state.pauses`）：`paused`（`key_id → 暂停到期时间`）；没有传 `None`。夹具里这个字段叫 `share`（2.x 的 ShareGuard），冻结不改，解析时当别名收 | — |
//!
//! # `effects` 的执行方式（按顺序）
//!
//! 同 [`crate::quota::Effect`]；自动驾驶额外用到：
//! - `TripBreaker`：`guard.trip_breaker(seconds, reason, now)`（内存态，`breaker_until = now + seconds`）；
//! - `PauseKey`：暂停单把 Key（旧版 `share.pause_key(key_id, seconds, reason, now)`）：写 `share_state` 表
//!   （现在它只是每把 Key 的临时暂停存储，表名为兼容旧库保留）并更新内存暂停表。
//!   旧版只有 `pause_key` 返回 True（此前不在暂停中）才记日志、标 `applied`；这一步已在 `decide` 里
//!   用 `pauses.paused` 提前判过，只有会成功的才会产出 `PauseKey` 与对应日志。
//!
//! # 已删除的规则
//!
//! - 2.x 的单 Key 守护还有「24 小时内多次来回切换网段 → 暂停 24 小时」「3 种以上客户端且几乎全天在用 → 重置 Key」
//!   两条，输入来自防分享证据（`events`）。防分享体系在 2.x 就删了，输入恒为空，这两条永远不会触发；3.0 连同
//!   `events` 输入一起删除（docs/v3-deviations.md #51）。现在 `key_guard` 只剩「1 小时被拒 ≥ 60 次 → 暂停 1 小时」。
//! - 「名额 +5」（`slots`：名额快满且昨天还有余量就把注册名额 +5，最多 100）连同它的输入（注册服务的名额 / 人数、
//!   上一轮额度复盘、`queue_full` 拥挤小时数）删除；每条规则的 `off` / `observe` / `enforce` 模式（设置
//!   `autopilot_<规则>`）也删除，熔断与单 Key 限流一律执行（返回值里的 `mode` 固定为 `enforce`），
//!   不再有「自动驾驶（观察）」日志（docs/v3-deviations.md #68）。

use crate::clock::Clock;
use crate::quota::{Cx, DecideError, Effect, py_max};
use crate::reasons::{
    BREAKER, BURST_CAP, CAP_3H, COOLDOWN, DAILY_CAP, HOURLY_CAP, KEY_PAUSED, PACED, QUEUE_FULL,
    QUEUE_TIMEOUT, QUIET_CAP, SITE_QUOTA, V5_EXHAUSTED,
};
use crate::{jsondump, pyc, pyval};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub const STATE_KEY: &str = "autopilot_last";
pub const HISTORY_KEY: &str = "autopilot_history";

/// 规则一律执行（旧版的 `off` / `observe` 已删除，#68）；返回值里照旧写出模式，后台据此显示。
pub const MODE: &str = "enforce";

/// 单 Key 限流统计要排除的拒绝原因码：已暂停（`KEY_PAUSED`）+ 全站层面的拒绝（原因在站点：排队满、
/// 账号上限、熔断、冷却……，不是这个成员的客户端出了问题）。忙时自动重试的正常成员不能被误判为死循环。
///
/// 旧版另有一份 `reasons.SITE_LEVEL`（就是去掉 `KEY_PAUSED` 的这份名单），只有这里用，3.0 并成这一份。
pub const KEY_GUARD_SKIP_REASONS: [&str; 13] = [
    KEY_PAUSED,
    QUEUE_FULL,
    HOURLY_CAP,
    QUIET_CAP,
    BURST_CAP,
    CAP_3H,
    DAILY_CAP,
    PACED,
    BREAKER,
    COOLDOWN,
    V5_EXHAUSTED,
    QUEUE_TIMEOUT,
    SITE_QUOTA,
];

/// `decide` 会读的 `site_settings` 键。
pub const SETTING_KEYS_READ: [&str; 2] = ["autopilot_breaker_until", HISTORY_KEY];

// ---------------------------------------------------------------------------
// 纯规则
// ---------------------------------------------------------------------------

/// 最近 15 分钟上游失败 ≥ 5 次且占比 ≥ 30% → 熔断。
pub fn breaker_rule(fails: i64, total: i64) -> (bool, String) {
    if fails >= 5 && total != 0 && (fails as f64 / total as f64) >= 0.3 {
        return (
            true,
            format!("最近 15 分钟上游失败 {fails}/{total}：暂停出图 5 分钟"),
        );
    }
    (false, format!("最近 15 分钟上游失败 {fails}/{total}：正常"))
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

// ---------------------------------------------------------------------------
// 决策核心
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerRows {
    /// `SUM(CASE …)`，没有行时为 NULL。
    pub fails: Option<i64>,
    pub total: Option<i64>,
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

/// `run(state, now)` 的决策核心。
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

fn run_inner(inp: &AutopilotInputs, clock: &Clock, cx: &mut Cx) -> Result<Value, DecideError> {
    let now = inp.now;
    let mut rules = Map::new();

    // ---- 熔断：只看上次熔断结束之后的上游失败 ----
    let _since = breaker_since(now, cx.get("autopilot_breaker_until"))?;
    let fails = inp.breaker.fails.unwrap_or(0);
    let total = inp.breaker.total.unwrap_or(0);
    let (trip, why) = breaker_rule(fails, total);
    let mut breaker_v = Map::new();
    breaker_v.insert("mode".into(), json!(MODE));
    breaker_v.insert("value".into(), json!(trip));
    breaker_v.insert("why".into(), json!(why));
    if trip
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
    let kg_why = if decisions.is_empty() {
        "没有 Key 触发".to_string()
    } else {
        format!("{} 把 Key 触发", decisions.len())
    };
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
    let kg_value = Value::Array(decisions.iter().cloned().map(Value::Object).collect());
    let mut kg = Map::new();
    kg.insert("mode".into(), json!(MODE));
    kg.insert("value".into(), kg_value);
    kg.insert("why".into(), json!(kg_why));
    rules.insert("key_guard".into(), Value::Object(kg));

    let mut out = Map::new();
    out.insert("at".into(), pyval::float(now));
    out.insert("rules".into(), Value::Object(rules.clone()));
    let out = Value::Object(out);
    cx.set(STATE_KEY, jsondump::dumps(&out));

    // ---- 每小时留一条快照，保留 7 天 ----
    let hour = clock.format(now, "%Y-%m-%d %H:00");
    let hist = crate::quota::loads_setting(cx.get(HISTORY_KEY), "[]")?;
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
        Some(h) => {
            !crate::quota::hget(h, "hour")?.is_some_and(|x| x.as_str() == Some(hour.as_str()))
        }
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
