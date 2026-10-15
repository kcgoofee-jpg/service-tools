//! 额度与保护类设置：Anlas / V5 预算、图片排队与冷却、账号保护与排队。

use super::{Kind, strict_num};
use crate::allowance::read_alert_threshold;
use crate::auth::{site_flag, site_flag_f64};
use crate::body::read_json_body;
use crate::error::{ApiError, ApiResult, HttpError, InternalError, ok_json};
use crate::guard::GuardIoError;
use crate::session::AdminSession;
use crate::state::{AppState, RUNTIME_LIMIT_BOUNDS};
use axum::Router;
use axum::extract::{Request, State};
use axum::routing::get;
use gate_core::allowance::SETTING as ALERT_THRESHOLD;
use gate_core::guard::GuardError;
use gate_core::pyval;
use gate_core::site_flags::{self, GLOBAL_DAILY_V5, GLOBAL_MONTHLY_ANLAS, MEMBER_DAILY_ANLAS};
use serde_json::{Value, json};

const BODY_LIMIT: usize = 1024 * 1024;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/settings", get(get_settings).put(put_settings))
        .route(
            "/admin/api/runtime-limits",
            get(get_runtime_limits).put(put_runtime_limits),
        )
        .route("/admin/api/guard", get(guard_get).put(guard_put))
}

// ------------------------------------------------------------------ 预算

async fn get_settings(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    Ok(ok_json(&json!({
        "global_monthly_anlas": pyval::float(site_flag_f64(&state, &GLOBAL_MONTHLY_ANLAS).await?),
        "global_daily_v5": crate::ops::algo_owned(&state.db).await?.global_daily_v5,
        "member_daily_anlas": site_flag(&state, &MEMBER_DAILY_ANLAS).await?,
        ALERT_THRESHOLD: read_alert_threshold(&state.db).await?,
    })))
}

async fn put_settings(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    // 全站 V5 归动态额度管：开着时不能手改（docs/v3-deviations.md #70）
    let managed: Vec<&str> = [GLOBAL_DAILY_V5.key]
        .into_iter()
        .filter(|k| body.contains_key(*k))
        .collect();
    if let Some(m) = crate::ops::managed_error(&state.db, &managed).await? {
        return Err(HttpError::new(400, m).into());
    }
    let threshold = match body.get(ALERT_THRESHOLD) {
        Some(v) => v.clone(),
        None => Value::from(read_alert_threshold(&state.db).await?),
    };
    let threshold = match pyval::strict_int(&threshold) {
        Some(n) if (1..=100).contains(&n) => n,
        _ => {
            return Err(HttpError::new(422, "V5 告警阈值必须为 1～100 的整数百分比").into());
        }
    };
    // 缺省字段保持原值（以前缺省会被写成 0 = 不限）
    async fn field(
        state: &AppState,
        body: &serde_json::Map<String, Value>,
        spec: &site_flags::Spec,
        kind: Kind,
        hi: f64,
    ) -> Result<Value, ApiError> {
        let value = match body.get(spec.key) {
            Some(v) => v.clone(),
            None if std::ptr::eq(spec, &GLOBAL_DAILY_V5) => {
                Value::from(crate::ops::algo_owned(&state.db).await?.global_daily_v5)
            }
            None => site_flag(state, spec).await?,
        };
        Ok(strict_num(&value, kind, 0.0, hi, spec.key)?)
    }
    let monthly = field(
        &state,
        &body,
        &GLOBAL_MONTHLY_ANLAS,
        Kind::Float,
        1_000_000.0,
    )
    .await?;
    let daily_v5 = field(&state, &body, &GLOBAL_DAILY_V5, Kind::Int, 100_000.0).await?;
    let member = field(&state, &body, &MEMBER_DAILY_ANLAS, Kind::Int, 100_000.0).await?;
    for (spec, v) in [
        (&GLOBAL_MONTHLY_ANLAS, &monthly),
        (&GLOBAL_DAILY_V5, &daily_v5),
        (&MEMBER_DAILY_ANLAS, &member),
    ] {
        let text = site_flags::encode(spec, v).map_err(|e| InternalError(e.0))?;
        state.db.set_setting(spec.key, &text).await?;
    }
    state
        .db
        .set_setting(ALERT_THRESHOLD, &threshold.to_string())
        .await?;
    Ok(ok_json(&json!({
        "ok": true,
        "global_monthly_anlas": monthly,
        "global_daily_v5": daily_v5,
        "member_daily_anlas": member,
        ALERT_THRESHOLD: threshold,
    })))
}

// ------------------------------------------------------------------ 排队与冷却

/// 当前生效的三个运行期限制（取整，与旧版 `int(getattr(settings, name))` 一致；
/// 每 Key 出图间隔 `key_image_min_interval` 已删除，docs/v3-deviations.md #64）。
fn runtime_limits_snapshot(state: &AppState) -> Value {
    let l = state.limits();
    json!({
        "queue_timeout": l.queue_timeout as i64,
        "image_min_interval": l.image_min_interval as i64,
        "image_429_cooldown_seconds": l.image_429_cooldown_seconds as i64,
    })
}

async fn get_runtime_limits(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    Ok(ok_json(&runtime_limits_snapshot(&state)))
}

async fn put_runtime_limits(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    if body.is_empty()
        || body
            .keys()
            .any(|k| !RUNTIME_LIMIT_BOUNDS.iter().any(|(n, _, _)| n == k))
    {
        return Err(HttpError::new(422, "包含未知或空的运行限制设置").into());
    }
    let mut values: Vec<(&'static str, i64)> = Vec::new();
    for (name, value) in &body {
        let &(name, lo, hi) = RUNTIME_LIMIT_BOUNDS
            .iter()
            .find(|(n, _, _)| n == name)
            .expect("上面已校验过名字");
        match pyval::strict_int(value) {
            Some(n) if (lo..=hi).contains(&n) => values.push((name, n)),
            _ => {
                return Err(HttpError::new(422, format!("{name} 必须是 {lo}～{hi} 的整数")).into());
            }
        }
    }
    // 写库 + 生效必须连在一起（旧版 `_runtime_limits_lock`）
    let shared = &state.ops;
    let _lock = shared.runtime_limits.lock().await;
    let writes: Vec<(String, String)> = values
        .iter()
        .map(|(k, v)| (format!("runtime_{k}"), v.to_string()))
        .collect();
    state.db.set_settings_bulk(&writes).await?;
    state.apply_runtime_limits(&values).await;
    Ok(ok_json(&runtime_limits_snapshot(&state)))
}

// ------------------------------------------------------------------ 账号保护

/// 账号保护与排队的当前设置和实时用量。
async fn guard_view(state: &AppState) -> Result<Value, ApiError> {
    let now = state.now();
    let mut data = state.guard.with(|g| g.describe(now));
    let day = state.day();
    let mut accounts = Vec::new();
    for (t, f) in state.nai.status_rows().await {
        let c = state.db.get_upstream_counter(&t.token_id, &day).await?;
        accounts.push(json!({
            "position": f.position,
            "usable": t.usable(now),
            "today": c.images,
            "this_hour": state.guard.with(|g| g.hour_count(&t.token_id, now)),
        }));
    }
    let queued: i64 = state.guard.with(|g| g.image_inflight.values().sum());
    if let Value::Object(m) = &mut data {
        m.insert("accounts".into(), Value::Array(accounts));
        m.insert("queued_images".into(), Value::from(queued));
    }
    Ok(data)
}

async fn guard_get(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    Ok(ok_json(&guard_view(&state).await?))
}

async fn guard_put(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    match state.guard.save(&body).await {
        Ok(_) => {}
        // 旧版：ValueError → 422 原文；其余（库写失败 / OverflowError）是 500
        Err(GuardIoError::Guard(GuardError::Value(msg))) => {
            return Err(HttpError::new(422, msg).into());
        }
        Err(e) => return Err(InternalError(e.to_string()).into()),
    }
    Ok(ok_json(&guard_view(&state).await?))
}
