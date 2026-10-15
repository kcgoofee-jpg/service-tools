//! 动态额度与自动驾驶的 IO 层：按 gate-core 文档里的清单取数 → 调 `decide` → 把有序的 `effects` 逐条落库。
//!
//! 取数在 `decide` 之前一次做完（旧版是边算边读，中间没有别的写入者，结果等价）；
//! 写入严格按 `decide` 给出的顺序执行，每一步都是独立提交（与旧版每次 `set_setting` 都提交一致），
//! 出错时之前已发生的写入保留。

use super::KernelError;
use crate::action_log::log_action;
use crate::state::AppState;
use gate_core::params::Params;
use gate_core::quota::{
    DecideError, Effect, GuardInfo, QuietInputs, QuotaInputs, QuotaParams, quota_cfg,
};
use gate_core::site_flags::{self, GLOBAL_DAILY_V5};
use gate_core::{autopilot, pyval, quota};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// 取一组设置键的当前值（缺失的键不放进 map）。
async fn read_settings(
    state: &AppState,
    keys: &[&str],
) -> Result<HashMap<String, String>, KernelError> {
    let mut out = HashMap::new();
    for k in keys {
        if let Some(v) = state.db.get_setting(k).await? {
            out.insert((*k).to_string(), v);
        }
    }
    Ok(out)
}

/// `site_flags.default_of(GLOBAL_DAILY_V5, settings)`（环境变量 `GLOBAL_DAILY_V5` 钳到 0..100000）。
fn global_daily_v5_default(state: &AppState) -> i64 {
    let v = site_flags::default_of(&GLOBAL_DAILY_V5, Some(&state.env.settings_view()));
    pyval::number_like(&v).map_or(0, |f| f.trunc() as i64)
}

/// 私密参数 `allocation.*` → [`QuotaParams`]（`P(name, 起点值)`：类型跟起点值一致，转不了回到起点值）。
///
/// 已知差异：`allocation.v5_k_table` 被写成不是「[[下限, 系数], …]」的形状时，旧版只在读到有 V5 剩余读数
/// 且迭代到坏条目时才抛 TypeError；这里在取参时就报错（私密参数文件写坏才会碰到）。
fn quota_params(p: &Params) -> Result<QuotaParams, DecideError> {
    let d = QuotaParams::default();
    let v5_k_table = match p.as_map().get("allocation.v5_k_table") {
        None => d.v5_k_table.clone(),
        Some(Value::Array(rows)) => {
            let mut table = Vec::new();
            for row in rows {
                let pair = row.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
                    DecideError::Type("allocation.v5_k_table 的条目必须是 [下限, 系数]".into())
                })?;
                let floor = pyval::number_like(&pair[0]).ok_or_else(|| {
                    DecideError::Type("allocation.v5_k_table 的下限必须是数字".into())
                })?;
                let k = match &pair[1] {
                    Value::String(s) => gate_core::pyc::parse_float(s).ok_or_else(|| {
                        DecideError::Value("allocation.v5_k_table 的系数不是数字".into())
                    })?,
                    other => pyval::number_like(other).ok_or_else(|| {
                        DecideError::Type("allocation.v5_k_table 的系数必须是数字".into())
                    })?,
                };
                table.push((floor, k));
            }
            table
        }
        Some(_) => {
            return Err(DecideError::Type("allocation.v5_k_table 必须是列表".into()));
        }
    };
    Ok(QuotaParams {
        v5_k_table,
        v5_k_floor: p.float("allocation.v5_k_floor", d.v5_k_floor),
        min_people: p.int("allocation.min_people", d.min_people),
        min_coverage_hours: p.int("allocation.min_coverage_hours", d.min_coverage_hours),
        v5_flood_pct: p.int("allocation.v5_flood_pct", d.v5_flood_pct),
        v5_flood_min_each: p.int("allocation.v5_flood_min_each", d.v5_flood_min_each),
        v5_flood_global: p.int("allocation.v5_flood_global", d.v5_flood_global),
        v5_tighten_below: p.int("allocation.v5_tighten_below", d.v5_tighten_below),
    })
}

/// `_allowance(state)`：账号 V5 剩余 % 与恢复 %/天（多个账号取平均）。刚重启时缓存里还没有读数，主动读一次。
pub(super) async fn allowance_accounts(
    state: &AppState,
) -> Result<Vec<quota::AccountAllowance>, KernelError> {
    let src = state.kernel.allowance_source();
    let mut accounts = src
        .snapshot(state)
        .await
        .map_err(|e| KernelError::new("AllowanceError", e))?;
    if accounts.iter().all(|a| a.percent.is_none()) {
        src.resolve_usable(state).await;
        accounts = src
            .snapshot(state)
            .await
            .map_err(|e| KernelError::new("AllowanceError", e))?;
    }
    Ok(accounts)
}

/// 把 `decide` 给出的写入按顺序执行。
pub(crate) async fn apply_effects(
    state: &AppState,
    effects: Vec<Effect>,
) -> Result<(), KernelError> {
    for e in effects {
        match e {
            Effect::Set { key, value } => state.db.set_setting(&key, &value).await?,
            Effect::Bulk { pairs } => state.db.set_settings_bulk(&pairs).await?,
            Effect::Log {
                actor,
                action,
                target,
                detail,
            } => {
                log_action(
                    &state.db,
                    &actor,
                    &action,
                    &target,
                    &detail,
                    true,
                    state.now(),
                )
                .await;
            }
            Effect::UpdateKeys {
                daily_images,
                daily_v5,
            } => state.db.quota_apply_to_keys(daily_images, daily_v5).await?,
            Effect::GuardSaveBase { base_daily_images } => {
                let mut body = Map::new();
                body.insert("base_daily_images".into(), json!(base_daily_images));
                state.guard.save(&body).await?;
            }
            // ops.set_registration(db, {"max_users": n, "notify": False}, ...)：只落库，不发公告
            Effect::SetRegistrationMaxUsers { max_users } => {
                state
                    .db
                    .set_settings_bulk(&[("register_max_users".to_string(), max_users.to_string())])
                    .await?;
            }
            Effect::TripBreaker {
                seconds,
                reason,
                now,
            } => {
                state
                    .guard
                    .with(|g| g.trip_breaker(seconds as f64, &reason, now))
                    .map_err(|e| KernelError::new("ValueError", format!("{e:?}")))?;
            }
            // 临时暂停单把 Key：写 share_state 表（现在只存这个，表名为兼容旧库保留）并更新内存里的暂停表
            Effect::PauseKey {
                key_id,
                seconds,
                reason,
                now,
            } => {
                let until = now + (seconds as f64).max(0.0);
                state.db.upsert_pause(key_id, until, &reason).await?;
                state.pauses.set(key_id, until, reason);
            }
        }
    }
    Ok(())
}

fn decide_failure(ty: String) -> KernelError {
    // gate-core 只带回 Python 异常类名，不带消息
    KernelError::new(ty, "内核输入数据异常（算法按旧版在此处抛异常）")
}

/// 重算并应用动态额度（旧版 `quota_algo.run(state)`）。
pub async fn run_quota(state: &AppState) -> Result<Value, KernelError> {
    let _serial = state.kernel.quota_guard().await;
    let now = state.now();
    let clock = &state.clock;
    let db = &state.db;
    let settings = read_settings(state, &quota::SETTING_KEYS_READ).await?;
    // 总开关关着（或设置读不懂）：旧版在这里就返回 / 抛异常，之后什么也不读不写——包括不向上游发订阅查询
    let skip = match quota_cfg(&settings) {
        Ok(c) => c.quota_auto_enabled == 0,
        Err(_) => true,
    };
    let mut inp = QuotaInputs {
        now,
        settings,
        guard: None,
        pool_len: None,
        global_daily_v5_default: 0,
        members: 0,
        daily: None,
        accounts: Vec::new(),
        quiet: QuietInputs::default(),
        active_v5: 0,
        managed_keys: Vec::new(),
        params: QuotaParams::default(),
    };
    if !skip {
        let (cap, base) = state
            .guard
            .with(|g| (g.values["account_daily_cap"], g.values["base_daily_images"]));
        inp.guard = Some(GuardInfo {
            account_daily_cap: cap,
            base_daily_images: Some(base),
        });
        inp.pool_len = Some(state.nai.tokens().await.len());
        inp.global_daily_v5_default = global_daily_v5_default(state);
        inp.members = db.quota_member_count().await?;
        inp.params = quota_params(&state.params.get())?;
        let today = clock.day(now);
        if quota::daily_adjust_due(inp.settings.get(quota::DAY_KEY).map(String::as_str), &today) {
            let yday = clock.day(now - 86400.0);
            if let Ok((start, end)) = quota::day_window(clock, &yday) {
                inp.daily = Some(db.quota_daily_inputs(&yday, start, end).await?);
            }
        }
        inp.accounts = allowance_accounts(state).await?;
        inp.quiet = db.quota_quiet_inputs(now).await?;
        inp.active_v5 = db
            .quota_active_v5(&quota::active_v5_days(clock, now, 3))
            .await?;
        inp.managed_keys = db.quota_managed_keys().await?;
    }
    let out = quota::decide(&inp, clock);
    apply_effects(state, out.effects).await?;
    match (out.result, out.error) {
        (Some(v), _) => Ok(v),
        (None, Some(ty)) => Err(decide_failure(ty)),
        (None, None) => Err(KernelError::new("RuntimeError", "额度算法没有返回结果")),
    }
}

/// 自动驾驶一轮（旧版 `autopilot.run(state, registrar)`）。
pub async fn run_autopilot(state: &AppState) -> Result<Value, KernelError> {
    let now = state.now();
    let clock = &state.clock;
    let db = &state.db;
    let settings = read_settings(state, &autopilot::SETTING_KEYS_READ).await?;
    let registrar = match state.kernel.registrar() {
        Some(r) => {
            let cfg = r
                .settings()
                .await
                .map_err(|e| KernelError::new("RegistrarError", e))?;
            let active = r
                .count_active()
                .await
                .map_err(|e| KernelError::new("RegistrarError", e))?;
            Some(autopilot::Registrar {
                max_users: cfg.max_users,
                active,
            })
        }
        None => None,
    };
    // 上一轮复盘里 queue_full 的统计窗口；读不懂的话 decide 自己会按旧版报同样的错
    let queue_hours = match autopilot::queue_window(
        settings.get("quota_algo_history").map(String::as_str),
        clock,
    ) {
        Ok(Some((start, end))) => {
            db.autopilot_queue_hours(start, end, autopilot::QUEUE_BUSY_REJECTS)
                .await?
        }
        _ => 0,
    };
    let breaker = match autopilot::breaker_since(
        now,
        settings.get("autopilot_breaker_until").map(String::as_str),
    ) {
        Ok(since) => db.autopilot_breaker_rows(since).await?,
        Err(_) => autopilot::BreakerRows {
            fails: None,
            total: None,
            fail_keys: 0,
        },
    };
    let rejects = db
        .autopilot_rejects(now - 3600.0, &autopilot::KEY_GUARD_SKIP_REASONS)
        .await?;
    let keys = db.autopilot_keys().await?;
    let breaker_until = state.guard.with(|g| g.breaker_until);
    let paused: HashMap<i64, f64> = state
        .pauses
        .snapshot()
        .into_iter()
        .map(|(k, (until, _))| (k, until))
        .collect();
    let inp = autopilot::AutopilotInputs {
        now,
        settings,
        registrar,
        global_daily_v5_default: global_daily_v5_default(state),
        queue_hours,
        breaker,
        guard: Some(autopilot::GuardState { breaker_until }),
        pauses: Some(autopilot::PausedKeys { paused }),
        rejects,
        keys,
    };
    let out = autopilot::decide(&inp, clock);
    apply_effects(state, out.effects).await?;
    match (out.result, out.error) {
        (Some(v), _) => Ok(v),
        (None, Some(ty)) => Err(decide_failure(ty)),
        (None, None) => Err(KernelError::new("RuntimeError", "自动驾驶没有返回结果")),
    }
}
