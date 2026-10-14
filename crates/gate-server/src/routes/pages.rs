//! 页面与静态资源：`/`、`/admin`（含 `/admin/*` 前端路由，`/admin/api/*` 除外）、`/survey`、`/_owl/*`、图标、健康检查。

use crate::AppState;
use crate::error::ok_json;
use crate::web::Page;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(landing))
        .route("/survey", get(survey))
        .route("/admin", get(admin))
        .route("/admin/", get(admin))
        .route("/admin/{*rest}", get(admin_spa))
        .route("/_owl/{*path}", get(asset))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
}

/// 不返回版本号：匿名接口给出精确版本等于给攻击者指纹定位。
async fn healthz(State(st): State<AppState>) -> Response {
    let _ = &st;
    ok_json(&json!({"ok": true, "upstream": true}))
}

async fn landing(State(st): State<AppState>) -> Response {
    st.assets.page(Page::Landing, None)
}

async fn survey(State(st): State<AppState>) -> Response {
    st.assets.page(Page::Survey, None)
}

async fn admin(State(st): State<AppState>) -> Response {
    st.assets.page(Page::Admin, None)
}

async fn admin_spa(State(st): State<AppState>, Path(rest): Path<String>) -> Response {
    if rest.starts_with("api/") || rest == "api" {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "Not Found"})),
        )
            .into_response();
    }
    st.assets.page(Page::Admin, None)
}

async fn asset(State(st): State<AppState>, Path(path): Path<String>) -> Response {
    st.assets.asset(&path)
}

async fn favicon(State(st): State<AppState>) -> Response {
    let mut resp = st.assets.asset("favicon.svg");
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    resp
}
