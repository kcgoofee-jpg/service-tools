//! 路由表。按接口分组：公开页面与状态、成员接口（出图 / 文本 / 兼容）、成员页、机器人桥接、后台、状态页。

pub mod admin;
mod image;
mod pages;
mod public;
pub mod status;
mod text;

use crate::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::router())
        .merge(image::router())
        .merge(status::router())
        .merge(public::router())
        .merge(text::router())
        .merge(admin::router(state.clone()))
        // 与 Starlette 一致：没有的路径 / 不支持的方法回 JSON
        .fallback(|| async { crate::error::HttpError::new(404, "Not Found") })
        .method_not_allowed_fallback(|| async {
            crate::error::HttpError::new(405, "Method Not Allowed")
        })
        .with_state(state)
}
