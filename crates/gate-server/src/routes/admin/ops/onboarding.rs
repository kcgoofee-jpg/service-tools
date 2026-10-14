//! 自助领 Key、全局功能开关、生成记录开关，以及首页「加入 Discord」按钮、交叉校验的「站长已确认」。

use super::upstream_health;
use crate::action_log::log_action;
use crate::audit::{audit_flags, audit_image_cap_mb, audit_image_days, audit_image_keep_hours};
use crate::body::read_json_body;
use crate::error::{ApiError, ApiResult, HttpError, ok_json};
use crate::ops;
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::routing::{get, post, put};
use gate_core::features::{self, FEATURES};
use gate_core::site_flags::{self, HOME_JOIN_OPEN};
use gate_core::{pyext, pyval};
use serde_json::{Map, Value, json};

const BODY_LIMIT: usize = 1024 * 1024;

/// 交叉校验的「站长已确认」键 → 说明（旧版 `_ACK_CHECKS`）。
const ACK_CHECKS: [(&str, &str); 2] = [
    ("v5_oversell", "V5 每人额度×人数 ≤ 全站上限"),
    ("registration_ready", "每人承诺 ≤ 全站日容量"),
];

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/ops", get(ops_get))
        .route("/admin/api/ops/registration", put(set_registration))
        .route("/admin/api/ops/features", put(set_features))
        .route("/admin/api/ops/audit", put(set_audit))
        .route(
            "/admin/api/home/join",
            get(home_join_get).post(home_join_set),
        )
        .route("/admin/api/checks/ack", post(checks_ack))
}

/// 后台「新建 Key」弹窗的默认功能：沿用「新成员默认开通的功能」（默认只有文生图），而不是全部功能。
async fn new_key_features(state: &AppState) -> Result<Value, ApiError> {
    let reg = ops::registration_settings(state).await?;
    // 站长明确把默认设为「全部已开放功能」→ null
    if reg.features.is_none()
        && state.db.get_setting("register_features").await?.as_deref() == Some("*")
    {
        return Ok(Value::Null);
    }
    let names = reg
        .features
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| vec!["image".to_string()]);
    let dumped = features::dump(Some(names)).unwrap_or_default();
    Ok(match features::parse_list(&Value::String(dumped)) {
        Some(list) => json!(list),
        None => Value::Null,
    })
}

/// 运行中开关的当前状态（`GET /ops` 与三个 `PUT /ops/*` 共用）。
async fn snapshot(state: &AppState) -> Result<Value, ApiError> {
    let reg = ops::registration_settings(state).await?;
    let mut registration = reg.to_map();
    registration.insert("active".into(), json!(ops::active_count(state).await?));
    registration.insert(
        "reset_at".into(),
        json!(
            state
                .env
                .registration
                .as_ref()
                .map_or("", |r| r.reset_at.as_str())
        ),
    );
    registration.insert("v5_capacity".into(), ops::v5_capacity(state).await?);
    let (prompts, thumbs, days) = audit_flags(&state.db, &state.env).await?;
    let labels: Map<String, Value> = FEATURES
        .iter()
        .map(|(n, l)| ((*n).to_string(), json!(l)))
        .collect();
    Ok(json!({
        "registration": registration,
        "features": {
            "global": ops::global_flags(state).await?,
            "labels": labels,
            // 后台「新建 Key」弹窗的默认功能，与服务端缺省逻辑一致
            "new_key": new_key_features(state).await?,
        },
        "audit": {
            "prompts": prompts,
            "thumbs": thumbs,
            "retention_days": days,
            "image_retention_days": audit_image_days(&state.db).await?,
            "image_cap_mb": audit_image_cap_mb(&state.db).await?,
            "image_bytes": state.db.audit_image_bytes().await?,
            "image_keep_hours": audit_image_keep_hours(&state.db).await?,
            // 兼容字段：公告渠道仍用于其他公告
            "announce_configured": state.announcer.configured(),
        },
        "upstream": upstream_health(state),
    }))
}

async fn ops_get(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    Ok(ok_json(&snapshot(&state).await?))
}

/// 开放 / 关闭自助注册、名额上限、新成员默认功能与额度；机器人下一条命令即生效。
async fn set_registration(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    match body.get("features") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list))
            if list.iter().all(|v| {
                v.as_str()
                    .is_some_and(|n| FEATURES.iter().any(|(f, _)| *f == n))
            }) => {}
        Some(_) => return Err(HttpError::new(422, "features 必须是功能名列表").into()),
    }
    ops::set_registration(&state, &body).await?;
    Ok(ok_json(&snapshot(&state).await?))
}

/// 全局功能开关：`{"flags": {"text": false, ...}}`。关闭后所有成员（管理员 Key 除外）立即不可用。
async fn set_features(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let flags = match body.get("flags") {
        Some(Value::Object(m))
            if !m.is_empty()
                && m.iter()
                    .all(|(k, v)| FEATURES.iter().any(|(f, _)| f == k) && v.is_boolean()) =>
        {
            m
        }
        _ => return Err(HttpError::new(422, "flags 必须是 {功能名: true/false}").into()),
    };
    ops::set_global_features(&state, flags).await?;
    Ok(ok_json(&snapshot(&state).await?))
}

/// 开关生成记录与保留天数。
async fn set_audit(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    ops::set_audit(&state, &body).await?;
    Ok(ok_json(&snapshot(&state).await?))
}

// ------------------------------------------------------------------ 首页加入按钮 / 交叉校验确认

/// 首页「加入 Discord」按钮开关（独立于领 Key 开关 register_open）。
async fn home_join_get(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let open = crate::auth::site_flag(&state, &HOME_JOIN_OPEN).await?;
    Ok(ok_json(&json!({"open": open})))
}

async fn home_join_set(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let on = body.get("open").is_none_or(pyval::truthy);
    let text = site_flags::encode(&HOME_JOIN_OPEN, &Value::Bool(on))
        .map_err(|e| crate::error::InternalError(e.0))?;
    state.db.set_setting(HOME_JOIN_OPEN.key, &text).await?;
    // 与旧版一致：这里自己记一条，中间件还会再记一条「POST /home/join」
    log_action(
        &state.db,
        &super::super::actor(&state, &parts),
        &format!(
            "首页加入按钮{}",
            if on {
                "开放"
            } else {
                "关闭（置灰「关闭注册」）"
            }
        ),
        "",
        "",
        true,
        state.now(),
    )
    .await;
    Ok(ok_json(&json!({"ok": true, "open": on})))
}

/// 结构性交叉校验的「站长已确认」：确认后该校验转绿（数字仍显示），不再每 10 分钟记一条 warn。
async fn checks_ack(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let key = body.get("key").map(pyext::py_str).unwrap_or_default();
    let Some((_, label)) = ACK_CHECKS.iter().find(|(k, _)| *k == key) else {
        return Err(HttpError::new(422, "未知的校验键").into());
    };
    let on = body.get("on").is_none_or(pyval::truthy);
    // 记下确认的时间（服务器本地时间，如「10-15 00:19」）；取消确认写空串
    let stamp = if on {
        state.clock.format(state.now(), "%m-%d %H:%M")
    } else {
        String::new()
    };
    state.db.set_setting(&format!("ack_{key}"), &stamp).await?;
    log_action(
        &state.db,
        &super::super::actor(&state, &parts),
        if on {
            "站长确认交叉校验"
        } else {
            "取消确认交叉校验"
        },
        label,
        "",
        true,
        state.now(),
    )
    .await;
    Ok(ok_json(&json!({"ok": true, "on": on})))
}
