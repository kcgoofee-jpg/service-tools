//! 路由表。按接口分组：公开页面与状态、成员接口（出图 / 文本 / 兼容）、成员页、机器人桥接、后台、状态页。

pub mod admin;
mod image;
mod pages;
mod public;
pub mod status;

use crate::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::router())
        .merge(image::router())
        .merge(status::router())
        .merge(public::router())
        .merge(admin::router(state.clone()))
        .with_state(state)
}
