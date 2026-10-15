//! 动态额度与自动驾驶的 IO 层：按 gate-core 文档里的清单取数 → 调 `decide` → 把有序的 `effects` 逐条落库。
//!
//! 取数在 `decide` 之前一次做完（旧版是边算边读，中间没有别的写入者，结果等价）；
//! 写入严格按 `decide` 给出的顺序执行，每一步都是独立提交（与旧版每次 `set_setting` 都提交一致），
//! 出错时之前已发生的写入保留。

use super::KernelError;
use crate::action_log::log_action;
use crate::state::AppState;
use gate_core::params::Params;
use gate_core::quota::{Effect, GuardInfo, QuietInputs, QuotaInputs, QuotaParams, quota_cfg};
use gate_core::site_flags::{self, GLOBAL_DAILY_V5};
use gate_core::{autopilot, pyval, quota};
use serde_json::Value;
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

/// 全站 V5 没设过时的起点值（`site_flags::GLOBAL_DAILY_V5` 的默认 150；2.x 取环境变量，已删除，#70）。
fn global_daily_v5_default() -> i64 {
    let v = site_flags::default_of(&GLOBAL_DAILY_V5, None);
    pyval::number_like(&v).map_or(0, |f| f.trunc() as i64)
}

/// 私密参数 `allocation.*` → [`QuotaParams`]（`P(name, 起点值)`：类型跟起点值一致，转不了回到起点值）。
///
/// 3.0 的 V5 公式不再读 `allocation.v5_k_table` / `v5_k_floor` / `min_people` / `v5_flood_*` /
/// `v5_tighten_below`（docs/v3-deviations.md #63）；文件里残留的这些键直接忽略，不报错。
fn quota_params(p: &Params) -> QuotaParams {
    let d = QuotaParams::default();
    QuotaParams {
        min_coverage_hours: p.int("allocation.min_coverage_hours", d.min_coverage_hours),
    }
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
        let cap = state.guard.with(|g| g.values["account_daily_cap"]);
        inp.guard = Some(GuardInfo {
            account_daily_cap: cap,
        });
        inp.pool_len = Some(state.nai.tokens().await.len());
        inp.global_daily_v5_default = global_daily_v5_default();
        inp.members = db.quota_member_count().await?;
        inp.params = quota_params(&state.params.get());
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
            .quota_active_v5(&quota::active_v5_days(clock, now, quota::V5_ACTIVE_DAYS))
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

/// 自动驾驶一轮（旧版 `autopilot.run(state, registrar)`；「名额 +5」已删除，docs/v3-deviations.md #68）。
pub async fn run_autopilot(state: &AppState) -> Result<Value, KernelError> {
    let now = state.now();
    let clock = &state.clock;
    let db = &state.db;
    let settings = read_settings(state, &autopilot::SETTING_KEYS_READ).await?;
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
