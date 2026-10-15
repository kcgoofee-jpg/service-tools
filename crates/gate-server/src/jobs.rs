//! 定时任务（旧版 `app/background_tasks.py` + 内核模块的周期部分）与启动 / 关停序列。
//!
//! 每件事一个循环，全部挂在 `state.tasks` 上，启动后立即跑第一轮，睡觉时听 `state.shutdown`：
//!
//! | 任务 | 周期 | 做什么 |
//! |---|---|---|
//! | `quota` 动态额度 | 10 分钟 ±20% | `quota::decide`：服务日第一次跑时复盘昨天、定当天的 V5 方案，之后每轮把额度下发给算法管理的 Key |
//! | `upstream_snapshot` 上游 V5 快照 | 10 分钟 ±20%，满 1 小时才记 | 主动读一次订阅，写 `upstream_snapshots`（安静时段测速用） |
//! | `hourly_cap` 每小时上限自动调整 | 10 分钟 ±20% | `guard.adapt_daily`：平稳一天且顶到过上限 +10 |
//! | `breaker` 全站熔断 | 10 分钟 ±20% | 15 分钟内上游失败 ≥ 5 次、≥ 30%、来自 ≥ 2 把 Key → 停 5 分钟 |
//! | `key_guard` 单 Key 限流 | 10 分钟 ±20% | 1 小时被拒 ≥ 60 次 → 暂停这把 Key 1 小时 |
//! | `maintenance` 维护 | 5 分钟 ±20% | 清过期记录 / 状态码统计落库 / 原图保存期与容量上限 / 自动关闭不用处理的 Bug（最多 10 分钟一次）/ 注册服务的三个同步（`registration::registrar_jobs`）/ 磁盘自检 / 官方状态 |
//! | `idle_recycle` 闲置回收 | 1 小时 | 长期不用的 Key 删除并拉黑（站点出不了图的服务日不算，见 [`idle_recycle_window`]）；`KEY_INACTIVITY_DELETE_DAYS=0` 时**什么都不做** |
//!
//! 每轮的结果记进设置 `job_last`，后台「系统」页的「定时任务」卡片显示（[`registry`]）。
//! 一轮工作做到底才响应关停（只在睡觉时被打断）：额度重算这类多步写入不能被拦腰切断。

mod autopilot;
mod capacity;
pub mod quota;
pub mod registry;

pub use registry::{JOBS, Job, JobError, JobsState, Origin, find, guarded, run_once, spawn_job};

use crate::action_log::{RETENTION_DAYS as ADMIN_ACTION_RETENTION_DAYS, log_action};
use crate::audit::{audit_flags, audit_image_cap_mb, audit_image_keep_hours};
use crate::bugs::Report;
use crate::state::AppState;
use gate_core::idle;
use gate_db::repo::discord::BanInfo;
use std::time::Duration;

/// 关停时等待记账 / 写库任务的最长时间（docker `stop_grace_period` 是 120 秒）。
pub const SHUTDOWN_WAIT_SECONDS: u64 = 110;

/// 周期任务的固定节拍（每 300 / 600 秒整）是最容易被识别的机器特征之一：加 ±ratio 抖动。
/// 上游看到的订阅查询、状态轮询因此不再是「每 600.0 秒一次」的精确网格。
pub fn jittered(seconds: f64, ratio: f64) -> f64 {
    jittered_with(seconds, ratio, rand::random::<f64>())
}

/// `jittered` 的纯函数形式：`u` 是 [0, 1) 的随机数（`random.uniform(-ratio, ratio)` = `-ratio + 2*ratio*u`）。
pub fn jittered_with(seconds: f64, ratio: f64, u: f64) -> f64 {
    let uniform = -ratio + (ratio - -ratio) * u;
    (seconds * (1.0 + uniform)).max(30.0)
}

/// 睡 `seconds` 秒；睡眠中收到关停信号返回 false（调用方就该退出循环了）。
async fn sleep_or_shutdown(state: &AppState, seconds: f64) -> bool {
    if state.shutdown.is_cancelled() {
        return false;
    }
    tokio::select! {
        _ = state.shutdown.cancelled() => false,
        _ = tokio::time::sleep(Duration::from_secs_f64(seconds.max(0.0))) => true,
    }
}

fn capture(state: &AppState, source: &str, e: &JobError) {
    state.bugs.capture(Report {
        source,
        error: Some((&e.ty, &e.msg)),
        ..Default::default()
    });
}

// ---------------------------------------------------------------------------
// 启动 / 关停
// ---------------------------------------------------------------------------

/// 启动序列（在 `AppState::build` 之后调用）：读回定时任务的运行记录 → 拉起所有循环。
pub async fn startup(state: &AppState) -> Result<(), JobError> {
    state.jobs.load(&state.db).await?;
    spawn_all(state);
    Ok(())
}

/// 拉起所有定时任务的循环。每个循环都在 `state.shutdown` 取消后尽快退出。
pub fn spawn_all(state: &AppState) {
    for job in JOBS.iter() {
        spawn_job(state, job);
    }
}

/// 优雅关停的后半段（HTTP 服务已经停止接收新连接并等完在途请求之后）：
/// 取消循环 → 等 `state.tasks`（记账 / Bug 写库 / 循环收尾，最多 110 秒）→ 状态码统计落库 → WAL 并回主库。
pub async fn shutdown(state: &AppState) {
    state.shutdown.cancel();
    state.tasks.close();
    if tokio::time::timeout(
        Duration::from_secs(SHUTDOWN_WAIT_SECONDS),
        state.tasks.wait(),
    )
    .await
    .is_err()
    {
        tracing::warn!("等待后台任务超过 {SHUTDOWN_WAIT_SECONDS} 秒，放弃等待");
    }
    if let Err(e) = flush_status_stats(state).await {
        tracing::warn!("关停时状态码统计落库失败：{e}");
    }
    match state.db.wal_checkpoint().await {
        Ok((busy, log, done)) => tracing::info!(busy, log, done, "WAL 已并回主库"),
        Err(e) => tracing::warn!("关停时 WAL checkpoint 失败：{e}"),
    }
}

/// 把内存里累计的状态码分布写进 `status_hourly`；写失败放回去下次再写（旧版 `status_stats.flush`）。
pub async fn flush_status_stats(state: &AppState) -> Result<(), gate_db::DbError> {
    let items = state.status_stats.drain();
    if items.is_empty() {
        return Ok(());
    }
    if let Err(e) = state.db.flush_status_hourly(&items).await {
        for ((hour, code), n) in items {
            for _ in 0..n {
                state.status_stats.record(code, hour as f64);
            }
        }
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 维护
// ---------------------------------------------------------------------------

/// 维护的一轮：每一步各自出错、各自记 Bug（来源 `maintenance:<步骤>`），不影响其他步骤。
/// 返回出错的步骤（显示名, 错误）。
pub async fn maintenance_once(state: &AppState) -> Vec<(&'static str, JobError)> {
    let mut failed = Vec::new();
    macro_rules! step {
        ($name:literal, $label:literal, $fut:expr) => {
            if state.shutdown.is_cancelled() {
                return failed;
            }
            if let Err(e) = $fut.await {
                capture(state, concat!("maintenance:", $name), &e);
                failed.push(($label, e));
            }
        };
    }
    step!("purge", "清理过期记录", purge(state));
    // 清理之后再自动关 Bug：先关再清的话，久未出现的旧 Bug 会在同一轮里直接被删，来不及留下「自动处理」的痕迹
    step!("bug_triage", "自动关闭 Bug", bug_triage(state));
    // 同步身份组 / 补全资料 / 退群回收（注册服务自己逐步记错；没配齐就什么都不做）
    if state.shutdown.is_cancelled() {
        return failed;
    }
    crate::registration::registrar_jobs(state).await;
    step!("disk", "磁盘检查", disk_check(state));
    step!("official", "官方状态", official_status(state));
    failed
}

/// 定时任务「维护」：跑一轮 [`maintenance_once`]，出错的步骤合成一句（各步骤已经各自记过 Bug）。
pub(crate) async fn maintenance_job(state: &AppState) -> Result<String, JobError> {
    let failed = maintenance_once(state).await;
    if !failed.is_empty() {
        let text: Vec<String> = failed
            .iter()
            .map(|(label, e)| format!("{label}出错：{e}"))
            .collect();
        return Err(JobError::reported(text.join("；")));
    }
    if state.shutdown.is_cancelled() {
        return Ok("正在关停，本轮没做完".to_string());
    }
    Ok("全部完成".to_string())
}

/// 自动关闭不需要人处理的 Bug（见 [`crate::bugs::Bugs::auto_triage`]）。最多 10 分钟一次，启动后的第一轮维护就会跑。
pub async fn bug_triage(state: &AppState) -> Result<(), JobError> {
    let now = state.now();
    if state.bugs.triage_due(now) {
        state.bugs.auto_triage(now).await?;
    }
    Ok(())
}

/// 清理过期记录；原图的保存期与容量上限。
pub async fn purge(state: &AppState) -> Result<(), JobError> {
    let db = &state.db;
    // 0 = 长期保留：已向成员披露保留范围，用于防滥用、回测和优化算法
    let days = audit_flags(db).await?.2;
    if days > 0 {
        db.purge_audit(state.now() - days as f64 * 86400.0).await?;
    }
    let keep = state.env.usage_log_retention_days;
    if keep > 0 {
        db.purge_usage_log(state.now() - std::cmp::max(7, keep) as f64 * 86400.0)
            .await?;
    }
    db.purge_admin_actions(state.now() - ADMIN_ACTION_RETENTION_DAYS as f64 * 86400.0)
        .await?;
    flush_status_stats(state).await?;
    db.purge_status_hourly_older_than(state.now() - 30.0 * 86400.0)
        .await?;
    db.purge_error_events(state.now() - 30.0 * 86400.0).await?;
    // 原图：保存 N 小时后删（10-11 站长定 24 小时），另外总量超过容量上限时删最旧的；站长也可以在后台手动清空
    if let Err(e) = purge_audit_images(state).await {
        capture(state, "audit_image_purge", &e);
    }
    Ok(())
}

async fn purge_audit_images(state: &AppState) -> Result<(), JobError> {
    let db = &state.db;
    let keep_h = audit_image_keep_hours(db).await?;
    if keep_h > 0 {
        let n = db
            .purge_audit_images(state.now() - keep_h as f64 * 3600.0)
            .await?;
        if n > 0 {
            tracing::info!("[audit] 原图保存超过 {keep_h} 小时，已删 {n} 张");
        }
    }
    db.sweep_orphan_images(state.now() - 3600.0).await?;
    let cap_mb = audit_image_cap_mb(db).await?;
    if cap_mb > 0 {
        let n = db
            .purge_audit_images_to_cap(cap_mb * 1024 * 1024, 0.9)
            .await?;
        if n > 0 {
            tracing::info!("[audit] 原图超过容量上限 {cap_mb} MB，已清掉最旧的 {n} 张");
        }
    }
    Ok(())
}

/// 磁盘剩余不足 10% 提醒站长（同一条 6 小时一次）。
async fn disk_check(state: &AppState) -> Result<(), JobError> {
    let st = rustix::fs::statvfs(&state.env.data_dir)
        .map_err(|e| JobError::new("OSError", e.to_string()))?;
    // shutil.disk_usage：free = f_bavail * f_frsize，total = f_blocks * f_frsize
    let free = st.f_bavail.saturating_mul(st.f_frsize);
    let total = st.f_blocks.saturating_mul(st.f_frsize);
    if total == 0 {
        return Err(JobError::new("ZeroDivisionError", "division by zero"));
    }
    if (free as f64) / (total as f64) < 0.10 {
        state.alerter.notify(
            "disk_low",
            &format!(
                "服务器磁盘剩余不足 10%（剩 {} MB），请清理或扩容。",
                free / (1 << 20)
            ),
            6.0 * 3600.0,
        );
    }
    Ok(())
}

/// NovelAI 官方状态（status.io 公开接口）：状态页那边的 `fetch_official`，维护每轮取一次，失败保留上次的结果。
/// 地址默认是官方接口；测试用 `state.jobs.set_official_api(..)`（进程内）或环境变量 `OFFICIAL_STATUS_URL`（起真实二进制时）
/// 指到本地 / 不可达的地址，保证不出本机。
async fn official_status(state: &AppState) -> Result<(), JobError> {
    let url = state
        .jobs
        .official_api()
        .or_else(|| {
            std::env::var("OFFICIAL_STATUS_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .unwrap_or_else(|| crate::routes::status::OFFICIAL_API.to_string());
    crate::routes::status::fetch_official(&state.db, &url, state.now()).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 闲置 Key 回收
// ---------------------------------------------------------------------------

/// 定时任务「闲置回收」。2026-10-13 站长：取消闲置提醒私信。3 天闲置 = 直接删 Key 并拉黑名单，不再提前 24 小时通知。
pub(crate) async fn idle_recycle_job(state: &AppState) -> Result<String, JobError> {
    Ok(match recycle_inactive_keys(state).await? {
        Recycle::Off => "关闭（天数=0）".to_string(),
        Recycle::Skipped => "最近 60 分钟没出图，本轮跳过".to_string(),
        Recycle::Done {
            deleted,
            skipped_days,
        } => {
            if deleted > 0 {
                tracing::info!("deleted {deleted} inactive API key(s)");
            }
            if skipped_days > 0 {
                format!("回收 {deleted} 把（不算站点没在正常出图的 {skipped_days} 天）")
            } else {
                format!("回收 {deleted} 把")
            }
        }
    })
}

/// 一轮闲置回收的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recycle {
    /// `KEY_INACTIVITY_DELETE_DAYS=0`：关闭。
    Off,
    /// 最近 60 分钟全站没有成功出图（不在安静时段）：本轮跳过。
    Skipped,
    /// 删掉的 Key 数；`skipped_days` = 往回多数的、站点没在正常出图的服务日数。
    Done { deleted: usize, skipped_days: i64 },
}

/// 本轮闲置回收的截止时间；站点眼下就出不了图时返回 `None`（本轮不回收）。
///
/// 站点自己出不了图的服务日不算成员闲置：
/// - 已完结的服务日全站成功出图不到 [`idle::HEALTHY_DAY_MIN_IMAGES`] 张 → 跳过，往回多数一天（最多回看 30 天）；
/// - 当前服务日：不在安静时段、且最近 60 分钟全站一张都没出成功 → 本轮直接跳过。
///
/// 平时（近几天都在正常出图）截止时间就是「现在 − N 天」，与原来完全相同。
pub async fn idle_recycle_window(
    state: &AppState,
    days: i64,
    now: f64,
) -> Result<Option<idle::IdleWindow>, JobError> {
    let quiet = state.guard.with(|g| g.in_quiet(now));
    if !quiet {
        let recent = state
            .db
            .ok_image_counts(vec![(now - idle::RECENT_OK_WINDOW, f64::MAX)])
            .await?;
        if recent.first().copied().unwrap_or(0) == 0 {
            return Ok(None);
        }
    }
    // 已完结的服务日：用服务日工具取日期与起点，不自己做时区换算
    let lookback = idle::max_lookback_days(days) as usize;
    let names = state.clock.last_days(now, lookback + 1);
    let mut ranges = Vec::with_capacity(lookback);
    for pair in names.windows(2) {
        if let (Some(start), Some(end)) = (
            state.clock.service_day_start(&pair[0]),
            state.clock.service_day_start(&pair[1]),
        ) {
            ranges.push((start, end));
        }
    }
    let counts = state.db.ok_image_counts(ranges.clone()).await?;
    let served: Vec<idle::ServedDay> = ranges
        .into_iter()
        .zip(counts)
        .map(|((start, end), ok_images)| idle::ServedDay {
            start,
            end,
            ok_images,
        })
        .collect();
    Ok(Some(idle::idle_window(now, days, &served)))
}

/// 永久回收长期未使用的普通 Key，并拉入黑名单（由站长在后台管理与解封）。返回删掉的 Key 数，见 [`recycle_inactive_keys`]。
pub async fn delete_inactive_keys(state: &AppState) -> Result<usize, JobError> {
    Ok(match recycle_inactive_keys(state).await? {
        Recycle::Done { deleted, .. } => deleted,
        Recycle::Off | Recycle::Skipped => 0,
    })
}

/// 闲置回收的一轮。
///
/// **`KEY_INACTIVITY_DELETE_DAYS=0` 表示关闭：这时无论库里有什么都不会删任何 Key**（线上现在是 3，10-13 起开着）。
/// 「连续 N 天」不算站点自己出不了图的服务日，见 [`idle_recycle_window`]。
pub async fn recycle_inactive_keys(state: &AppState) -> Result<Recycle, JobError> {
    let days = std::cmp::max(0, state.env.key_inactivity_delete_days);
    if days == 0 {
        return Ok(Recycle::Off);
    }
    let now = state.now();
    let Some(window) = idle_recycle_window(state, days, now).await? else {
        tracing::info!("最近 60 分钟全站没有成功出图（不在安静时段），本轮不做闲置回收");
        return Ok(Recycle::Skipped);
    };
    if window.skipped_days > 0 {
        tracing::info!(
            skipped_days = window.skipped_days,
            "闲置回收跳过站点没在正常出图的服务日"
        );
    }
    let key_ids = state.db.inactive_key_ids(window.cutoff, now).await?;
    let detail = if window.skipped_days > 0 {
        format!(
            "连续 {days} 天未活动自动回收并拉黑（不算站点没在正常出图的 {} 天）",
            window.skipped_days
        )
    } else {
        format!("连续 {days} 天未活动自动回收并拉黑")
    };
    let registrar = state.registration.clone();
    for &key_id in &key_ids {
        let key_name = state
            .db
            .get_key(key_id)
            .await?
            .and_then(|k| k.get("name").map(gate_core::pyval::py_str))
            .unwrap_or_else(|| format!("Key #{key_id}"));
        let reg_rows = state.db.registration_profiles_for_key(key_id).await?;
        state.db.delete_key(key_id).await?;
        log_action(
            &state.db,
            "系统",
            "闲置回收 Key",
            &format!("Key #{key_id} {key_name}"),
            &detail,
            true,
            state.now(),
        )
        .await;
        for reg in reg_rows {
            state
                .db
                .ban_discord(
                    &reg.discord_id,
                    &BanInfo {
                        reason: gate_core::idle::ban_reason(days),
                        username: reg.username,
                        display_name: reg.display_name,
                        avatar: reg.avatar,
                        banned_by: "系统".into(),
                    },
                    state.now(),
                )
                .await?;
            if let Some(r) = &registrar {
                let _ = r.release_role(&reg.discord_id).await;
            }
        }
        // 释放领取记录
        for discord_id in state.db.forget_registration_for_key(key_id).await? {
            if let Some(r) = &registrar {
                r.release_role(&discord_id).await?;
            }
        }
    }
    Ok(Recycle::Done {
        deleted: key_ids.len(),
        skipped_days: window.skipped_days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_matches_uniform_formula() {
        // random.uniform(-0.2, 0.2) = -0.2 + 0.4 * u
        assert_eq!(jittered_with(600.0, 0.2, 0.0), 480.0);
        assert!((jittered_with(600.0, 0.2, 0.5) - 600.0).abs() < 1e-9);
        assert!((jittered_with(600.0, 0.2, 1.0) - 720.0).abs() < 1e-9);
        // 下限 30 秒
        assert_eq!(jittered_with(10.0, 0.2, 0.5), 30.0);
    }
}
