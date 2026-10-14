//! 路由表。按接口分组：公开页面与状态、成员接口（出图 / 文本 / 兼容）、成员页、机器人桥接、后台、状态页。

mod pages;

use crate::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    Router::new().merge(pages::router()).with_state(state)
}
