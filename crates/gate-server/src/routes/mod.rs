//! 路由表。按接口分组：公开页面与状态、成员接口（出图 / 文本 / 兼容）、成员页、机器人桥接、后台、状态页。

pub mod admin;
mod image;
mod member;
mod pages;
mod public;
mod registration;
pub mod status;
mod text;

use crate::AppState;
use axum::Router;

/// 成员自助打包：单次最多打包最新的 200MB 原图。
pub const EXPORT_MAX_BYTES: i64 = 200 * 1024 * 1024;
/// 成员自助打包：每人两次打包的最短间隔（秒）。
pub const EXPORT_PER_MEMBER_SECS: f64 = 60.0;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::router())
        .merge(image::router())
        .merge(status::router())
        .merge(public::router())
        .merge(text::router())
        .merge(registration::router())
        .merge(member::router())
        .merge(admin::router(state.clone()))
        // 与 Starlette 一致：没有的路径 / 不支持的方法回 JSON
        .fallback(|| async { crate::error::HttpError::new(404, "Not Found") })
        .method_not_allowed_fallback(|| async {
            crate::error::HttpError::new(405, "Method Not Allowed")
        })
        .with_state(state)
}
