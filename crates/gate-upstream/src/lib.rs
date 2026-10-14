//! NovelAI 上游客户端。
//!
//! - [`headers`]：请求头集合与顺序（与旧版逐项一致）+ 浏览器指纹池；
//! - [`limiter`]：每把 Token 的图片并发槽（可缩容、FIFO、取消不漏槽）；
//! - [`token_store`]：后台管理的上游 Token 文件；
//! - [`client`]：Token 池、选号、出图间隔、429 / 403 / 401 处理、结算。

pub mod headers;
pub mod limiter;
pub mod token_store;
