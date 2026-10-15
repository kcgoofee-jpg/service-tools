//! 监控类接口：成员接口状态码分布、总览、运行状态、上游表现、V5 官方额度。

use super::{query_param, upstream, upstream_health};
use crate::auth::site_flag_f64;
use crate::error::{ApiResult, InternalError, ok_json};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use gate_core::features::{FEATURES, KIND_FEATURE};
use gate_core::site_flags::GLOBAL_MONTHLY_ANLAS;
use gate_core::{pyc, pyext, pyval};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/status-codes", get(status_codes))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/status", get(server_status))
        .route("/admin/api/perf", get(upstream_perf))
        .route("/admin/api/allowance", get(allowance))
}

/// 成员接口返回的 HTTP 状态码含义（旧版 `status_stats.MEANING`）。
fn meaning(code: i64) -> &'static str {
    match code {
        200 => "成功",
        400 => "参数不对（尺寸、步数、不支持的功能等）",
        401 => "Key 错误、缺失或已失效",
        402 => "额度用完（每日 / V5 / Anlas）",
        403 => "功能没开放（Vibe、放大、图生图、文本等）",
        404 => "地址填错了",
        405 => "请求方法不对（多半是地址填错）",
        409 => "请求冲突",
        413 => "请求太大",
        422 => "参数格式不对",
        429 => "太频繁 / 排队满 / 每小时上限",
        499 => "客户端提前断开",
        500 => "网关出错",
        502 => "上游出错",
        503 => "上游不可用或冷却中",
        504 => "上游超时",
        _ => "",
    }
}

/// NovelAI 实际返回给网关的状态码含义（`usage_log.up_status`）。
fn upstream_meaning(code: i64) -> &'static str {
    match code {
        0 => "没发到 NovelAI（网关自己拦下、排队超时或客户端断开）",
        200 => "NovelAI 正常返回",
        400 => "NovelAI 说参数不对",
        401 => "NovelAI 说凭证无效",
        402 => "NovelAI 说需要 Anlas / 额度不足",
        403 => "NovelAI 拒绝（账号受限 / 风控）",
        429 => "NovelAI 限流",
        500 => "NovelAI 内部错误",
        502 => "NovelAI 网关错误",
        503 => "NovelAI 暂时不可用",
        504 => "NovelAI 超时",
        _ => "其他",
    }
}

/// 成员接口 HTTP 状态码分布（今日 / 最近 1 / 24 / 168 小时）。
async fn status_codes(State(state): State<AppState>, _s: AdminSession, uri: Uri) -> ApiResult {
    let hours = query_param(&uri, "hours").unwrap_or_else(|| "today".into());
    let now = state.now();
    let (h_val, since): (i64, i64) = if matches!(hours.to_lowercase().as_str(), "today" | "0") {
        // 今日 = 服务日那天本地 0 点起（不按 7 点重置，与旧版一致）
        let day = state.day();
        let start = state
            .clock
            .local_midnight(&day)
            .ok_or_else(|| InternalError(format!("无法换算服务日 {day}")))?;
        let since = start as i64;
        let h = pyc::floordiv_f(now - since as f64, 3600.0) as i64;
        (h.max(1), since)
    } else {
        let h = match pyext::py_int_str(&hours) {
            Ok(n) => n.clamp(1, 24 * 30) as i64,
            Err(_) => 24,
        };
        let since = pyc::floordiv_f(now - (h * 3600) as f64, 3600.0) as i64 * 3600;
        (h, since)
    };

    // 内存里还没落库的也算上
    let mut total: BTreeMap<i64, i64> = BTreeMap::new();
    for (code, n) in state.db.status_totals_since(since).await? {
        *total.entry(code).or_default() += n;
    }
    for ((hour, code), n) in state.status_stats.pending() {
        if hour >= since {
            *total.entry(i64::from(code)).or_default() += n;
        }
    }
    let all_n: i64 = total.values().sum();
    let codes: Vec<Value> = total
        .iter()
        .map(|(c, n)| {
            let pct = if all_n != 0 {
                pyc::round(*n as f64 / all_n as f64 * 100.0, 1)
            } else {
                0.0
            };
            json!({"code": c, "n": n, "pct": pct, "meaning": meaning(*c)})
        })
        .collect();

    // NovelAI 实际返回给网关的状态码（usage_log.up_status）
    let rows = state.db.upstream_status_distribution(since).await?;
    let up_total: i64 = rows.iter().map(|(_, n)| n).sum();
    let upstream: Vec<Value> = rows
        .iter()
        .map(|(c, n)| {
            let pct = if up_total != 0 {
                pyval::float(pyc::round((n * 100) as f64 / up_total as f64, 1))
            } else {
                json!(0)
            };
            json!({"code": c, "n": n, "pct": pct, "meaning": upstream_meaning(*c)})
        })
        .collect();
    Ok(ok_json(&json!({
        "hours": h_val,
        "since": since,
        "total": all_n,
        "codes": codes,
        "upstream": upstream,
        "upstream_total": up_total,
    })))
}

/// 每项功能今日 / 近 7 天的调用次数（成功 / 失败 / 拒绝）、图片、tokens、Anlas 与使用人数。
async fn feature_usage(state: &AppState) -> Result<Vec<Value>, crate::error::ApiError> {
    #[derive(Clone, Copy, Default)]
    struct Bucket {
        ok: i64,
        error: i64,
        rejected: i64,
        images: i64,
        tokens: i64,
        anlas: f64,
    }
    impl Bucket {
        fn json(self) -> Value {
            json!({
                "ok": self.ok, "error": self.error, "rejected": self.rejected,
                "images": self.images, "tokens": self.tokens, "anlas": self.anlas,
            })
        }
    }
    let day = state.day();
    let day_start = state
        .clock
        .local_midnight(&day)
        .ok_or_else(|| InternalError(format!("无法换算服务日 {day}")))?;
    let week_start = day_start - 6.0 * 86400.0;
    let mut buckets: Vec<[Bucket; 2]> = vec![[Bucket::default(); 2]; FEATURES.len()];
    for (period, since) in [(0_usize, day_start), (1, week_start)] {
        for row in state.db.usage_by_kind(since).await? {
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or_default();
            let Some(name) = KIND_FEATURE
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, f)| *f)
            else {
                continue;
            };
            let Some(idx) = FEATURES.iter().position(|(n, _)| *n == name) else {
                continue;
            };
            let int = |k: &str| row.get(k).and_then(pyval::int_like).unwrap_or(0);
            let status = row
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let b = &mut buckets[idx][period];
            match status {
                "ok" => b.ok += int("n"),
                "rejected" => b.rejected += int("n"),
                _ => b.error += int("n"),
            }
            if status == "ok" {
                b.images += int("images");
                b.tokens += int("tokens");
                let anlas = row.get("anlas").and_then(pyval::number_like).unwrap_or(0.0);
                b.anlas = pyc::round(b.anlas + anlas, 2);
            }
        }
    }
    Ok(FEATURES
        .iter()
        .zip(buckets)
        .map(|((id, label), [today, week])| {
            json!({"id": id, "label": label, "today": today.json(), "week": week.json()})
        })
        .collect())
}

async fn overview(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let now = state.now();
    let week = state.clock.last_days(now, 7);
    let data = state
        .db
        .overview(&state.day(), &week, now, &state.clock)
        .await?;
    let mut data = match serde_json::to_value(data) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    data.insert(
        "pool".into(),
        Value::Array(upstream::pool_status(&state).await?),
    );
    data.insert(
        "pool_configured".into(),
        Value::Bool(state.nai.configured().await),
    );
    data.insert(
        "anlas_budget".into(),
        pyval::float(site_flag_f64(&state, &GLOBAL_MONTHLY_ANLAS).await?),
    );
    data.insert(
        "v5_limit".into(),
        Value::from(crate::ops::algo_owned(&state.db).await?.global_daily_v5),
    );
    data.insert(
        "feature_usage".into(),
        Value::Array(feature_usage(&state).await?),
    );
    data.insert(
        "log_since".into(),
        state
            .db
            .first_log_ts()
            .await?
            .map_or(Value::Null, pyval::float),
    );
    Ok(ok_json(&Value::Object(data)))
}

/// 数据目录所在磁盘的 (剩余, 总量)，单位字节（旧版 `shutil.disk_usage`）。
fn disk_usage(path: &std::path::Path) -> Result<(u64, u64), InternalError> {
    let s =
        rustix::fs::statvfs(path).map_err(|e| InternalError(format!("读取磁盘用量失败：{e}")))?;
    Ok((
        s.f_bavail.saturating_mul(s.f_frsize),
        s.f_blocks.saturating_mul(s.f_frsize),
    ))
}

/// 告警与记录功能的当前状态（不含任何密钥）。
async fn server_status(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let env = &state.env;
    let (prompts, thumbs, days) = crate::audit::audit_flags(&state.db, env).await?;
    let (free, total) = disk_usage(&env.data_dir)?;
    Ok(ok_json(&json!({
        "alerts": {
            "configured": state.alerter.configured(),
            "sent": state.alerter.sent.load(std::sync::atomic::Ordering::Relaxed),
        },
        "audit": {
            "prompts": prompts,
            "thumbs": thumbs,
            "retention_days": days,
            "image_cap_mb": crate::audit::audit_image_cap_mb(&state.db).await?,
            "image_bytes": state.db.audit_image_bytes().await?,
            "image_keep_hours": crate::audit::audit_image_keep_hours(&state.db).await?,
        },
        "upstream": upstream_health(&state),
        "protection": {
            "auth_fail_max": env.auth_fail_max,
            "auth_fail_window": env.auth_fail_window,
            "auth_block_seconds": env.auth_block_seconds,
            "login_max_attempts": env.login_max_attempts,
        },
        "disk": {"free_mb": free >> 20, "total_mb": total >> 20},
    })))
}

/// 上游表现：V4.5 / V5 的生成耗时、排队、限流、成功率、产能，与过去 7 天对比。
async fn upstream_perf(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let report = crate::kernel::collect_perf(&state, state.now()).await?;
    Ok(ok_json(
        &serde_json::to_value(report).map_err(|e| InternalError(e.to_string()))?,
    ))
}

/// V5 官方免费额度（只有真实出图会刷新缓存，这里只读）。
async fn allowance(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let ids: Vec<String> = state
        .nai
        .tokens()
        .await
        .iter()
        .map(|t| t.token_id.clone())
        .collect();
    Ok(ok_json(&state.allowance.snapshot(&ids).await?))
}
