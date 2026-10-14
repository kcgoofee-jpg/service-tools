//! 上游 Token 的增删改（后台「接管」后令牌池以 `data/upstream_tokens.json` 为准）与 Anlas 核对。
//!
//! 上游 Token 的变更都是敏感操作：完整 Token 永远不出现在响应、日志和操作记录里（响应里只有打码版本），
//! 每次变更都会私信站长（「如果这不是你操作的，请立刻更换后台密码」）。

use crate::body::read_json_body;
use crate::error::{ApiError, ApiResult, HttpError, InternalError, ok_json};
use crate::reconciliation::{self, RunError};
use crate::session::{ADMIN_COOKIE, AdminSession, cookie};
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::routing::{get, post, put};
use gate_core::{pyc, pyext, pyval};
use gate_upstream::{headers::token_id, token_store};
use serde_json::{Map, Value, json};

const BODY_LIMIT: usize = 1024 * 1024;
/// 旧版 `_token_payload` 的请求体上限：一个 Token 绰绰有余。
const TOKEN_BODY_LIMIT: usize = 4096;
const NOT_FOUND: &str = "上游 Token 不存在";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/upstream-tokens", post(add_token))
        .route(
            "/admin/api/upstream-tokens/{token_id}",
            put(replace_token).delete(remove_token),
        )
        .route(
            "/admin/api/upstream-tokens/{token_id}/v5-limit",
            put(set_v5_limit),
        )
        .route(
            "/admin/api/upstream-tokens/{token_id}/image-concurrency",
            put(set_image_concurrency),
        )
        .route(
            "/admin/api/upstream-tokens/{token_id}/enabled",
            put(set_enabled),
        )
        .route(
            "/admin/api/reconciliation",
            get(reconciliation_status).post(reconcile),
        )
}

// ------------------------------------------------------------------ 令牌池状态

/// 令牌池的当前状态（旧版 `nai.status()`）：Token 打码，另附当天用量。
pub(super) async fn pool_status(state: &AppState) -> Result<Vec<Value>, ApiError> {
    let now = state.now();
    let mut out = Vec::new();
    for (t, f) in state.nai.status_rows().await {
        let c = state
            .db
            .get_upstream_counter(&t.token_id, &state.day())
            .await?;
        out.push(json!({
            "token_id": t.token_id,
            "position": f.position,
            "token": gate_core::policy::mask_token(&t.token, 6),
            "usable": t.usable(now),
            "admin_enabled": f.admin_enabled,
            "disabled": f.disabled,
            "fails": f.fails,
            "blocked_for": ((f.blocked_until - now) as i64).max(0),
            "last_ok": pyval::float(f.last_ok),
            "allow_anlas": f.allow_anlas,
            "image_concurrency": t.image_slots.limit(),
            "image_active": t.image_slots.active(),
            "v5_daily_limit": f.v5_daily_limit,
            "v5_used": c.v5,
            "images_today": c.images,
        }));
    }
    Ok(out)
}

/// 把令牌池写回 `upstream_tokens.json`（权限 600，不进数据库）。
async fn persist_pool(state: &AppState) -> Result<(), ApiError> {
    let entries: Vec<token_store::Entry> = state
        .nai
        .tokens()
        .await
        .iter()
        .map(|t| token_store::Entry {
            token: t.token.clone(),
            allow_anlas: t.fields().allow_anlas,
        })
        .collect();
    token_store::save(&state.env.upstream_tokens_path(), &entries)
        .map_err(|e| InternalError(format!("上游 Token 文件写入失败：{e}")))?;
    Ok(())
}

async fn exists(state: &AppState, id: &str) -> bool {
    state.nai.tokens().await.iter().any(|t| t.token_id == id)
}

fn announce(state: &AppState, text: &str) {
    state.alerter.notify("upstream_token_changed", text, 0.0);
}

/// 请求体里的 Token（去掉首尾空白）：必须是 `pst-` 开头的持久 API Token。
async fn token_payload(
    parts: &Parts,
    body: Body,
) -> Result<(String, Map<String, Value>), ApiError> {
    let body = read_json_body(&parts.headers, body, TOKEN_BODY_LIMIT).await?;
    let token = body
        .get("token")
        .map(|v| pyc::strip(&pyext::py_str(v)).to_string())
        .unwrap_or_default();
    if !token_store::valid_token(&token) {
        return Err(HttpError::new(
            422,
            "Token 格式不对：应以 pst- 开头（在 NovelAI 的 账号设置 → Get Persistent API Token 里获取）",
        )
        .into());
    }
    Ok((token, body))
}

/// 向 NovelAI 查订阅信息确认 Token 有效（只读，不消耗额度）。返回订阅等级。
async fn verify(state: &AppState, token: &str) -> Result<i64, ApiError> {
    state
        .nai
        .verify_token(token)
        .await
        .map_err(|msg| HttpError::new(422, msg).into())
}

// ------------------------------------------------------------------ 增删改

/// 添加一把上游 Token：先向 NovelAI 验证，通过后加入令牌池。
async fn add_token(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let (token, body) = token_payload(&parts, body).await?;
    let tier = verify(&state, &token).await?;
    let id = token_id(&token);
    let saved_v5 = state
        .db
        .get_upstream_token_limits()
        .await?
        .into_iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| v);
    let saved_enabled = state
        .db
        .get_upstream_token_enabled()
        .await?
        .into_iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| v);
    let allow_anlas = body.get("allow_anlas").is_some_and(pyval::truthy);
    state
        .nai
        .add_token(token, allow_anlas, saved_v5, saved_enabled)
        .await
        .map_err(|msg| HttpError::new(409, msg))?;
    persist_pool(&state).await?;
    announce(
        &state,
        &format!(
            "后台添加了一把上游 Token（订阅等级 {tier}）。如果这不是你操作的，请立刻更换后台密码。"
        ),
    );
    Ok(ok_json(
        &json!({"ok": true, "tier": tier, "pool": pool_status(&state).await?}),
    ))
}

/// 替换某个槽位的 Token（例如在 NovelAI 重置了 Token 后粘贴新的）；V5 日限、启停、并发和当天计数延续。
async fn replace_token(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(old_id): Path<String>,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let (token, _) = token_payload(&parts, body).await?;
    let tier = verify(&state, &token).await?;
    match state.nai.replace_token(&old_id, token).await {
        Ok((new, previous)) => {
            state
                .db
                .move_upstream_token(&previous, &new.token_id)
                .await?;
            persist_pool(&state).await?;
        }
        Err(msg) if exists(&state, &old_id).await => return Err(HttpError::new(409, msg).into()),
        Err(_) => return Err(HttpError::new(404, NOT_FOUND).into()),
    }
    announce(
        &state,
        "后台替换了一把上游 Token。如果这不是你操作的，请立刻更换后台密码。",
    );
    Ok(ok_json(
        &json!({"ok": true, "tier": tier, "pool": pool_status(&state).await?}),
    ))
}

async fn remove_token(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(id): Path<String>,
) -> ApiResult {
    match state.nai.remove_token(&id).await {
        Err(msg) => return Err(HttpError::new(409, msg).into()),
        Ok(false) => return Err(HttpError::new(404, NOT_FOUND).into()),
        Ok(true) => {}
    }
    persist_pool(&state).await?;
    announce(&state, "后台删除了一把上游 Token。");
    Ok(ok_json(
        &json!({"ok": true, "pool": pool_status(&state).await?}),
    ))
}

async fn set_v5_limit(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let limit = match body.get("v5_daily_limit").and_then(pyval::strict_int) {
        Some(n) if (0..=100_000).contains(&n) => n,
        _ => {
            return Err(
                HttpError::new(422, "上游 V5 日限额必须是 0～100000 的整数（0 为不限）").into(),
            );
        }
    };
    if !exists(&state, &id).await {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    state.db.set_upstream_token_limit(&id, limit).await?;
    if !state.nai.set_v5_daily_limit(&id, limit).await {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    Ok(ok_json(&json!({"ok": true, "v5_daily_limit": limit})))
}

async fn set_image_concurrency(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let limit = match body.get("image_concurrency").and_then(pyval::strict_int) {
        Some(n) if (1..=4).contains(&n) => n,
        _ => return Err(HttpError::new(422, "上游图片并发必须是 1～4 的整数").into()),
    };
    if !exists(&state, &id).await {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    // 库里存实际生效的并发（SINGLE_IMAGE_SLOT_ENFORCED 时恒为 1），响应回请求的值（与旧版一致）
    let effective = if state.env.single_image_slot_enforced {
        1
    } else {
        limit
    };
    state
        .db
        .set_upstream_token_image_concurrency(&id, effective)
        .await?;
    if state.nai.set_image_concurrency(&id, limit).await.is_none() {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    Ok(ok_json(&json!({"ok": true, "image_concurrency": limit})))
}

async fn set_enabled(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return Err(HttpError::new(422, "enabled 必须是布尔值").into());
    };
    if !exists(&state, &id).await {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    state.db.set_upstream_token_enabled(&id, enabled).await?;
    // 停用时会等这把 Token 手上的图跑完才返回：之后发出的请求一定能看到新状态
    if !state.nai.set_admin_enabled(&id, enabled).await {
        return Err(HttpError::new(404, NOT_FOUND).into());
    }
    Ok(ok_json(&json!({"ok": true, "enabled": enabled})))
}

// ------------------------------------------------------------------ Anlas 核对

/// 会话绑定的 CSRF 令牌：用管理员会话密钥对「reconciliation:<会话 Cookie>」做 HMAC（旧版 `_reconciliation_csrf`）。
/// 密钥推导与 `session::admin_session_key` 相同（SECRET_KEY + ":" + sha256(密码哈希或 ADMIN_PASSWORD)），
/// 所以改密码 / 换会话都会让旧令牌失效。
fn csrf_for(secret_key: &str, password_material: &str, session_cookie: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};
    let key = format!(
        "{secret_key}:{}",
        hex::encode(Sha256::digest(password_material.as_bytes()))
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("hmac key");
    mac.update(format!("reconciliation:{session_cookie}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn csrf_token(state: &AppState, session_cookie: &str) -> String {
    let material = state
        .admin_pw_hash
        .read()
        .expect("pw")
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.env.admin_password.clone());
    csrf_for(&state.secret_key, &material, session_cookie)
}

fn session_cookie(parts: &Parts) -> String {
    cookie(&parts.headers, ADMIN_COOKIE).unwrap_or_default()
}

fn run_error(e: RunError) -> ApiResult {
    match e {
        RunError::Internal(e) => Err(e),
        RunError::Refused(r) => {
            use axum::response::IntoResponse;
            let mut resp = HttpError::new(r.status, r.message).into_response();
            if r.retry_after != 0
                && let Ok(v) = r.retry_after.to_string().parse()
            {
                resp.headers_mut().insert("retry-after", v);
            }
            Ok(resp)
        }
    }
}

async fn reconciliation_status(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> ApiResult {
    let (parts, _) = req.into_parts();
    let mut out = match reconciliation::status(&state).await {
        Ok(Value::Object(m)) => m,
        Ok(_) => Map::new(),
        Err(e) => return run_error(e),
    };
    out.insert(
        "csrf_token".into(),
        Value::String(csrf_token(&state, &session_cookie(&parts))),
    );
    Ok(ok_json(&Value::Object(out)))
}

async fn reconcile(State(state): State<AppState>, _s: AdminSession, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let expected = csrf_token(&state, &session_cookie(&parts));
    // 会话绑定的 CSRF：反向代理 / TLS 终止后也成立，且隔离同级站点
    let given = crate::auth::header_str(&parts.headers, "x-nai-admin-csrf");
    {
        use subtle::ConstantTimeEq;
        if !bool::from(given.as_bytes().ct_eq(expected.as_bytes())) {
            return Err(HttpError::new(403, "会话校验失败，请刷新后台后重试").into());
        }
    }
    let ct = crate::auth::header_str(&parts.headers, "content-type");
    if pyc::strip(ct.split(';').next().unwrap_or("")).to_lowercase() != "application/json" {
        return Err(HttpError::new(415, "请使用 JSON 请求").into());
    }
    if !read_json_body(&parts.headers, body, 1024).await?.is_empty() {
        return Err(HttpError::new(400, "本操作不接受自定义账号或地址").into());
    }
    let mut out = match reconciliation::run(&state).await {
        Ok(Value::Object(m)) => m,
        Ok(_) => Map::new(),
        Err(e) => return run_error(e),
    };
    out.insert("csrf_token".into(), Value::String(expected));
    Ok(ok_json(&Value::Object(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_matches_python() {
        // python: hmac.new((SECRET + ":" + sha256(ADMIN_PASSWORD).hexdigest()).encode(),
        //                  b"reconciliation:1900000000.abc", sha256).hexdigest()
        assert_eq!(
            csrf_for(
                "test-secret-key-test-secret-key-0123456789",
                "dev-admin-password-123",
                "1900000000.abc"
            ),
            "c3b4dff9435fcebb29ab289c86573f7fa42746d3fae0a2df9f3ce485e376df58"
        );
    }
}
