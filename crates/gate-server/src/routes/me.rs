//! `GET /v1/me`：这把 Key 自己的额度、功能开关和闲置回收设置（旧版 `app/routers/text_routes.py` 里的 `/v1/me`）。
//!
//! 2026-10-15 文本生成 / 语音 / OpenAI 兼容聊天整套接口删除（docs/v3-deviations.md #81），这个文件原先叫
//! `text.rs`，只留下了 `/v1/me`（线上冒烟 `nai-ops smoke` 和用 Key 自查额度的脚本在用）。

use crate::auth::{authenticate, require_feature};
use crate::error::{ApiError, ApiResult, ok_json};
use crate::key::Key;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use gate_core::features::FEATURES;
use gate_core::pyc;
use gate_core::pyval::{self, truthy};
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/me", get(v1_me))
}

fn respond(r: ApiResult) -> Response {
    r.unwrap_or_else(IntoResponse::into_response)
}

/// `daily_base`：这把 Key 的每日保底张数；0 表示没有保底 / 借用之分（整天都按每日上限）。
fn daily_base(state: &AppState, key: &Key) -> i64 {
    let base = state
        .guard
        .with(|g| g.values.get("base_daily_images").copied().unwrap_or(0));
    if key.is_admin()
        || base == 0
        || !truthy(key.get("daily_images"))
        || base >= key.daily_images()
        || key.manual()
    {
        0
    } else {
        base
    }
}

/// `_feature_view`：该 Key 当前各项功能是否可用（已计入全局开关）。
async fn feature_view(state: &AppState, key: &Key) -> Result<Vec<Value>, ApiError> {
    let mut view = Vec::new();
    for (name, label) in FEATURES {
        // require_feature 只在被拒时返回 Gate 错误（对应旧版 `check(...) is not None`）
        let on = match require_feature(state, key, name).await {
            Ok(()) => true,
            Err(ApiError::Gate(_)) => false,
            Err(e) => return Err(e),
        };
        view.push(json!({"id": name, "label": label, "on": on}));
    }
    Ok(view)
}

async fn v1_me(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    respond(me(&state, &parts).await)
}

async fn me(state: &AppState, parts: &Parts) -> ApiResult {
    let key = authenticate(state, parts, false).await?;
    let c = state.db.get_counter(key.id(), &state.day()).await?;
    let generated = state.db.generated_image_totals(Some(key.id())).await?;
    let generated_total = generated
        .iter()
        .find(|(id, _)| *id == key.id())
        .map_or(0, |(_, n)| *n);
    // get_counter 对 ≤ 0 的 anlas 给整数 0；float(0) 之后仍是 0.0
    let anlas_today = if c.anlas > 0.0 { c.anlas } else { 0.0 };
    let anlas_month = state.db.month_anlas(key.id(), &state.month()).await?;
    let anlas_auto = pyval::number_like(key.get("anlas_auto")) == Some(1.0) && key.allow_anlas();
    let features = feature_view(state, &key).await?;
    Ok(ok_json(&json!({
        "name": key.name(),
        "is_admin": key.is_admin(),
        "generated_images_total": generated_total,
        "today": {
            "images": c.images,
            "daily_images": key.get("daily_images"),
            "daily_images_base": daily_base(state, &key),
            "legacy_free_images_today": c.legacy_free_images,
            "anlas_today": pyc::round(anlas_today, 2),
            "daily_anlas": key.get("daily_anlas"),
            "anlas_auto": anlas_auto,
            "v5_today": c.v5,
            "daily_v5": key.get("daily_v5"),
            "image_model_scope": key.get("image_model_scope"),
            "anlas_month": pyc::round(anlas_month, 2),
            "monthly_anlas": key.get("monthly_anlas"),
            "text_tokens": c.text_tokens,
            "daily_text_tokens": key.get("daily_text_tokens"),
            "requests": c.requests,
        },
        "expires_at": key.get("expires_at"),
        "inactivity_delete_days": if key.is_admin() { 0 } else { state.env.key_inactivity_delete_days },
        "features": features,
    })))
}
