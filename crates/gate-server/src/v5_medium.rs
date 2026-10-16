//! V5 Medium 降档：读设置、读上游余量缓存、算出此刻要不要把 V5 Full 请求改走 Medium。
//! 判定逻辑在 `gate_core::v5_medium`（纯函数）；这里只负责取数和保存自动档的当前状态。
//!
//! 取数只读缓存快照（`allowance.snapshot`），不发任何上游请求：Medium 请求同样走「免费 V5」的余量检查，
//! 缓存照常会被真实出图刷新，所以自动档不会因为降档后没人刷新而卡在旧读数上。

use crate::allowance::read_alert_threshold;
use crate::state::AppState;
use gate_core::allowance::DEFAULT_THRESHOLD;
use gate_core::site_flags::V5_MEDIUM_MODE;
use gate_core::v5_medium::{Decision, Mode, decide, lowest_percent};
use serde_json::Value;
use std::sync::atomic::Ordering;

/// 站长设置的模式。读库失败按 `off`：降档开关出问题不能把出图拖垮。
async fn read_mode(state: &AppState) -> Mode {
    match state.db.get_setting(V5_MEDIUM_MODE.key).await {
        Ok(raw) => Mode::from_raw(raw.as_deref()),
        Err(e) => {
            tracing::warn!(error = %e, "读取 v5_medium_mode 失败，按 off 处理");
            Mode::Off
        }
    }
}

/// 可用上游账号里最低的 V5 剩余百分比（只读缓存；没读到过返回 None）。
async fn usable_lowest_percent(state: &AppState) -> Option<i64> {
    let now = state.now();
    let ids: Vec<String> = state
        .nai
        .tokens()
        .await
        .iter()
        .filter(|t| t.usable(now))
        .map(|t| t.token_id.clone())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let snap = state.allowance.snapshot(&ids).await.ok()?;
    let accounts = snap.get("accounts").and_then(Value::as_array)?;
    lowest_percent(
        accounts
            .iter()
            .map(|a| a.get("percent").and_then(Value::as_i64)),
    )
}

async fn decision(state: &AppState, with_percent: bool) -> Decision {
    let mode = read_mode(state).await;
    let flag = &state.ops.v5_medium_auto;
    let threshold = if mode == Mode::Auto || with_percent {
        read_alert_threshold(&state.db)
            .await
            .unwrap_or(DEFAULT_THRESHOLD)
    } else {
        DEFAULT_THRESHOLD
    };
    let lowest = if mode == Mode::Auto || with_percent {
        usable_lowest_percent(state).await
    } else {
        None
    };
    let d = decide(mode, flag.load(Ordering::Relaxed), lowest, threshold);
    // 只有自动档才有「当前状态」；换到别的档位就清掉，下次切回 auto 从关开始
    let was = flag.swap(mode == Mode::Auto && d.active, Ordering::Relaxed);
    if mode == Mode::Auto && was != d.active {
        tracing::info!(
            on = d.active,
            lowest = ?d.lowest_percent,
            threshold = d.threshold,
            "V5 Medium 自动档切换"
        );
    }
    d
}

/// 出图热路径用：此刻要不要把 V5 Full 改走 Medium。
pub async fn active(state: &AppState) -> bool {
    decision(state, false).await.active
}

/// 后台展示用：模式、此刻是否生效、最低余量和提示线（不论哪个模式都带上读数）。
pub async fn describe(state: &AppState) -> Decision {
    decision(state, true).await
}
