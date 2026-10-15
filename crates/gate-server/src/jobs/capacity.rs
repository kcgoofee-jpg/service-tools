//! 两个容量相关的定时任务：
//!
//! - 「上游 V5 快照」：距上次快照满 1 小时就主动读一次订阅，写一行 `upstream_snapshots`
//!   （动态额度的安静时段测速要用：夜里没人用 V5 时余量的变化就是真实的恢复速度）；
//! - 「每小时上限自动调整」：`guard.adapt_daily`——平稳一天且顶到过上限就 +10，挨过 429 后天花板压到当时的 80%，
//!   只升不降，24 小时最多一次。

use super::JobError;
use super::quota::{allowance_accounts, refresh_allowance};
use crate::action_log::log_action;
use crate::state::AppState;
use gate_core::guard::parse_setting_float;
use gate_core::quota::allowance_summary;

/// 距离上次快照 ≥ 1 小时（或从未记录）才记一次。`last` = `SELECT MAX(ts) FROM upstream_snapshots`。
pub fn snapshot_due(last: Option<f64>, now: f64) -> bool {
    match last {
        // 0.0 也算没有
        None | Some(0.0) => true,
        Some(t) => now - t >= 3600.0,
    }
}

/// 一位小数，去掉末尾的 `.0`（63.0 → 63，11.5 → 11.5）。
fn short(x: f64) -> String {
    let s = format!("{x:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// 快照任务的摘要：「已记录：剩余 63%，恢复 11.0%/天」。
pub fn snapshot_summary(pct: Option<f64>, rate: Option<f64>, fresh: bool) -> String {
    let Some(p) = pct else {
        return "已记录：没有读数（上游账号都读不到）".to_string();
    };
    let mut s = format!("已记录：剩余 {}%", short(p));
    if let Some(r) = rate {
        s.push_str(&format!("，恢复 {r:.1}%/天"));
    }
    if !fresh {
        s.push_str("（这次没读到，用的是缓存）");
    }
    s
}

pub(super) async fn upstream_snapshot(state: &AppState) -> Result<String, JobError> {
    let now = state.now();
    let last = state.db.latest_snapshot_ts().await?;
    if !snapshot_due(last, now) {
        return Ok("距上次不到 1 小时".to_string());
    }
    // 主动读一次，夜里没人用 V5 时也是真实值
    let fresh = refresh_allowance(state).await;
    let (pct, rate) = allowance_summary(&allowance_accounts(state).await?);
    state
        .db
        .insert_upstream_snapshot(now, pct, rate, 0, i64::from(fresh))
        .await?;
    Ok(snapshot_summary(pct, rate, fresh))
}

/// 每小时上限任务的摘要。`last_step` / `last_429` 是调整之前的 `guard_hourly_adapted_at` / `guard_last_upstream_429`。
pub fn hourly_summary(
    cap: i64,
    changed: Option<(i64, i64)>,
    last_step: f64,
    last_429: f64,
    max: i64,
    now: f64,
) -> String {
    if let Some((a, b)) = changed {
        return format!("{a} → {b}");
    }
    let why = if last_step == 0.0 {
        "第一次运行，开始计时".to_string()
    } else if now - last_step < 86400.0 {
        "24 小时内调整或检查过，不动".to_string()
    } else if now - last_429 < 86400.0 {
        "24 小时内挨过上游 429，不动".to_string()
    } else if cap >= max {
        format!("已到自动调整的最高值 {max}")
    } else {
        "没顶到过上限、数据不足 20 小时或已到 429 后的天花板，不加".to_string()
    };
    format!("{cap}（{why}）")
}

pub(super) async fn hourly_cap(state: &AppState) -> Result<String, JobError> {
    let now = state.now();
    let setting = |k: &'static str| async move {
        let raw = state.db.get_setting(k).await?;
        // 读不懂时 adapt_daily 自己会报错，这里只影响摘要
        Ok::<f64, JobError>(parse_setting_float(raw.as_deref()).unwrap_or(0.0))
    };
    let last_step = setting("guard_hourly_adapted_at").await?;
    let last_429 = setting("guard_last_upstream_429").await?;
    let changed = state.guard.adapt_daily(now).await?;
    if let Some((a, b)) = changed {
        log_action(
            &state.db,
            "系统",
            "自动调整：每小时上限",
            "",
            &format!("一天没有上游限流：{a} → {b}"),
            true,
            state.now(),
        )
        .await;
    }
    let (cap, max) = state
        .guard
        .with(|g| (g.values["account_hourly_cap"], g.values["hourly_max"]));
    Ok(hourly_summary(cap, changed, last_step, last_429, max, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_due_once_an_hour() {
        assert!(snapshot_due(None, 100.0));
        assert!(snapshot_due(Some(0.0), 100.0));
        assert!(!snapshot_due(Some(1000.0), 1000.0 + 3599.0));
        assert!(snapshot_due(Some(1000.0), 1000.0 + 3600.0));
    }

    #[test]
    fn summaries() {
        assert_eq!(
            snapshot_summary(Some(63.0), Some(11.0), true),
            "已记录：剩余 63%，恢复 11.0%/天"
        );
        assert_eq!(
            snapshot_summary(Some(62.5), None, false),
            "已记录：剩余 62.5%（这次没读到，用的是缓存）"
        );
        assert_eq!(
            snapshot_summary(None, None, false),
            "已记录：没有读数（上游账号都读不到）"
        );
        let now = 1_000_000.0;
        assert_eq!(
            hourly_summary(160, Some((150, 160)), 1.0, 0.0, 160, now),
            "150 → 160"
        );
        assert_eq!(
            hourly_summary(150, None, 0.0, 0.0, 160, now),
            "150（第一次运行，开始计时）"
        );
        assert_eq!(
            hourly_summary(150, None, now - 3600.0, 0.0, 160, now),
            "150（24 小时内调整或检查过，不动）"
        );
        assert_eq!(
            hourly_summary(150, None, now - 90000.0, now - 60.0, 160, now),
            "150（24 小时内挨过上游 429，不动）"
        );
        assert_eq!(
            hourly_summary(160, None, now - 90000.0, 0.0, 160, now),
            "160（已到自动调整的最高值 160）"
        );
    }
}
