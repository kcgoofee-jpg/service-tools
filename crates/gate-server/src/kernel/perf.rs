//! 上游表现分析的取数（旧版 `perf.collect(state, now)`）：日志 + 当前并发 / 间隔设置 → gate-core 的 `analyze`。
//! 后台「上游」页、维护循环的变慢提醒、风险检查都用它。

use super::KernelError;
use crate::state::AppState;
use gate_core::perf::{self, BASELINE_DAYS, DAY, PerfParams, PerfReport, PerfRow};

/// 从网关状态读取日志和当前并发 / 间隔设置，返回 `analyze()` 结果。
pub async fn collect_perf(state: &AppState, now: f64) -> Result<PerfReport, KernelError> {
    let rows = state
        .db
        .image_perf_rows(now - (BASELINE_DAYS * DAY) as f64)
        .await?;
    let rows: Vec<PerfRow> = rows
        .into_iter()
        .map(
            |(ts, model, status, images, wait_ms, dur_ms, up_status, detail)| PerfRow {
                ts,
                model: Some(model),
                status,
                images,
                wait_ms,
                dur_ms,
                up_status,
                detail: Some(detail),
            },
        )
        .collect();
    let tokens = state.nai.tokens().await;
    let wall = state.now();
    let usable: Vec<_> = tokens.iter().filter(|t| t.usable(wall)).collect();
    // `sum(t.image_slots.limit for t in usable) or 1`
    let slots = match usable
        .iter()
        .map(|t| t.image_slots.limit() as i64)
        .sum::<i64>()
    {
        0 => 1,
        n => n,
    };
    let hourly_cap =
        state.guard.with(|g| g.hourly_cap(now)) * std::cmp::max(1, usable.len()) as i64;
    let limits = state.limits();
    Ok(perf::analyze(
        &rows,
        now,
        &PerfParams {
            slots,
            site_interval: limits.image_min_interval,
            // 每把 Key 的出图间隔已删除（docs/v3-deviations.md #64）：同一把 Key 一次只生成一张，
            // 单人最快也受账号出图间隔约束，所以「单人产能」按账号间隔算
            key_interval: limits.image_min_interval,
            hourly_cap,
        },
    ))
}
