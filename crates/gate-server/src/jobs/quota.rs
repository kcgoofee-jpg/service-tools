//! 定时任务「动态额度」：按 gate-core 文档里的清单取数 → 调 `quota::decide` → 把有序的 `effects` 逐条落库。
//!
//! 服务日第一次跑时（07:00 后）复盘昨天、定当天的 V5 方案；之后每轮把每人上限下发给算法管理的 Key。
//! 后台改动态额度参数、把 Key 交回算法时，[`run_now`] 立即重算一次。
//!
//! 取数在 `decide` 之前一次做完；写入严格按 `decide` 给出的顺序执行，每一步都是独立提交，
//! 出错时之前已发生的写入保留。

use super::JobError;
use crate::action_log::log_action;
use crate::error::ApiError;
use crate::state::AppState;
use gate_core::pyval;
use gate_core::quota::{
    self, AccountAllowance, Effect, GuardInfo, PyNum, QuietInputs, QuotaInputs, QuotaParams,
    quota_cfg,
};
use gate_core::site_flags::{self, GLOBAL_DAILY_V5};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

/// 取一组设置键的当前值（缺失的键不放进 map）。
async fn read_settings(
    state: &AppState,
    keys: &[&str],
) -> Result<HashMap<String, String>, JobError> {
    let mut out = HashMap::new();
    for k in keys {
        if let Some(v) = state.db.get_setting(k).await? {
            out.insert((*k).to_string(), v);
        }
    }
    Ok(out)
}

/// 全站 V5 没设过时的起点值（`site_flags::GLOBAL_DAILY_V5` 的默认 150）。
fn global_daily_v5_default() -> i64 {
    let v = site_flags::default_of(&GLOBAL_DAILY_V5, None);
    pyval::number_like(&v).map_or(0, |f| f.trunc() as i64)
}

/// 上游账号 V5 缓存里的读数（`allowance.snapshot(pool)["accounts"]`；只读缓存，不发请求）。
async fn allowance_snapshot(state: &AppState) -> Result<Vec<AccountAllowance>, JobError> {
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
        .map_err(|e| JobError::new("AllowanceError", e.to_string()))?;
    let accounts = snap
        .get("accounts")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    Ok(accounts
        .iter()
        .map(|a| AccountAllowance {
            percent: a.get("percent").and_then(PyNum::from_value),
            recharge_per_day: a.get("recharge_per_day").and_then(PyNum::from_value),
        })
        .collect())
}

/// 对每个可用的上游账号主动读一次订阅信息（失败忽略）；至少一个成功返回 true。
pub(crate) async fn refresh_allowance(state: &AppState) -> bool {
    use gate_upstream::client::Allowance;
    let now = state.now();
    let mut ok = false;
    for t in state.nai.tokens().await {
        if t.usable(now) && state.allowance.with(&state.nai).resolve(&t).await.is_ok() {
            ok = true;
        }
    }
    ok
}

/// 账号 V5 剩余 % 与恢复 %/天。刚重启时缓存里还没有读数，主动读一次（读失败也不报错）。
pub(crate) async fn allowance_accounts(
    state: &AppState,
) -> Result<Vec<AccountAllowance>, JobError> {
    let mut accounts = allowance_snapshot(state).await?;
    if accounts.iter().all(|a| a.percent.is_none()) {
        refresh_allowance(state).await;
        accounts = allowance_snapshot(state).await?;
    }
    Ok(accounts)
}

/// 把 gate-core 给出的写入按顺序执行（动态额度与自动驾驶共用）。
pub(crate) async fn apply_effects(state: &AppState, effects: Vec<Effect>) -> Result<(), JobError> {
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
                    .map_err(|e| JobError::new("ValueError", format!("{e:?}")))?;
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

/// 重算并应用动态额度，返回写进 `quota_algo_last` 的结果（开关关着时是 `{"enabled": false}`）。
pub async fn compute(state: &AppState) -> Result<Value, JobError> {
    let _serial = state.jobs.quota_guard().await;
    let now = state.now();
    let clock = &state.clock;
    let db = &state.db;
    let settings = read_settings(state, &quota::SETTING_KEYS_READ).await?;
    // 总开关关着（或设置读不懂）：什么也不读不写——包括不向上游发订阅查询
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
        // gate-core 只带回异常类名，不带消息
        (None, Some(ty)) => Err(JobError::new(ty, "额度算法的输入数据异常（设置被改坏？）")),
        (None, None) => Err(JobError::new("RuntimeError", "额度算法没有返回结果")),
    }
}

/// 一轮结果的摘要：「每人 V4.5 95 张 · V5 3 张（全站 V5 156） · 同步 12 把 · 今日复盘：95→120」。
pub fn summary(r: &Value) -> String {
    if r.get("enabled") == Some(&Value::Bool(false)) {
        return "动态额度已关闭，未计算".to_string();
    }
    let text = |v: Option<&Value>| v.map_or("—".to_string(), pyval::py_str);
    let v5 = r.get("v5");
    let mut parts = vec![format!(
        "每人 V4.5 {} 张 · V5 {} 张（全站 V5 {}）",
        text(r.get("ceiling")),
        text(v5.and_then(|v| v.get("each"))),
        text(v5.and_then(|v| v.get("global")))
    )];
    if let Some(n) = r.get("changed_keys").and_then(Value::as_i64)
        && n > 0
    {
        parts.push(format!("同步 {n} 把"));
    }
    if let Some(rev) = r.get("review").filter(|v| v.is_object()) {
        let first = |k: &str| rev.get(k).and_then(|a| a.get(0));
        parts.push(format!(
            "今日复盘：{}→{}",
            text(first("from")),
            text(first("to"))
        ));
    }
    parts.join(" · ")
}

/// 定时任务的一轮。
pub(super) async fn tick(state: &AppState) -> Result<String, JobError> {
    Ok(summary(&compute(state).await?))
}

/// 立即重算一次（后台改动态额度参数、把 Key 交回算法时），同样记进 `job_last.quota`
/// （先记在内存里，随下一次定时任务一起落库，见 [`super::registry::Origin::Manual`]）。
/// 出错时不在这里记 Bug：调用方（接口返回 500 / 改 Key 的接口）各自已经记录。
pub async fn run_now(state: &AppState) -> Result<Value, ApiError> {
    let at = state.now();
    let t0 = Instant::now();
    let r = super::registry::guarded(compute(state)).await;
    let ms = i64::try_from(t0.elapsed().as_millis()).unwrap_or(i64::MAX);
    let shown = r.as_ref().map(summary).map_err(Clone::clone);
    state
        .jobs
        .record(
            state,
            "quota",
            at,
            ms,
            &shown,
            super::registry::Origin::Manual,
        )
        .await;
    Ok(r?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_texts() {
        assert_eq!(
            summary(&json!({"enabled": false})),
            "动态额度已关闭，未计算"
        );
        let r = json!({
            "enabled": true, "ceiling": 95, "v5": {"each": 3, "global": 156},
            "changed_keys": 0, "review": null,
        });
        assert_eq!(summary(&r), "每人 V4.5 95 张 · V5 3 张（全站 V5 156）");
        let r = json!({
            "enabled": true, "ceiling": 120, "v5": {"each": 3, "global": 156},
            "changed_keys": 12, "review": {"from": [95], "to": [120]},
        });
        assert_eq!(
            summary(&r),
            "每人 V4.5 120 张 · V5 3 张（全站 V5 156） · 同步 12 把 · 今日复盘：95→120"
        );
    }
}
