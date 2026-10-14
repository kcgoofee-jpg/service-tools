//! 运维命令集合（二进制 `nai-ops`）。每个子命令一个模块，`main.rs` 只做参数解析与退出码。
//!
//! 约定：
//! - 只读分析（巡检 / 提示词统计 / 列事件）用 `mode=ro` 直接打开数据库；
//! - 写业务表的只有 [`incident`] 里的两个函数，走 gate-db 的写线程；
//! - Discord REST 的基地址读环境变量 `DISCORD_API_BASE`，测试里指向本机假服务，不会真的发消息。

pub mod announce;
pub mod discord;
pub mod dm_broadcast;
pub mod incident;
pub mod patrol;
pub mod prompt_stats;
pub mod prompt_terms;
pub mod sanitize;
pub mod smoke;
pub mod snapshot;
pub mod util;
