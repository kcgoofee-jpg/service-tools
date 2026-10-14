//! 猫头鹰公益站的两个 Discord 机器人（3.0 Rust 版，替换 `integration/discord_bot.py` 与 `warden_bot.py`）：
//!
//! - **奶妹**（`nai-bots naimei`）：`/register /quota /resetkey /help /反馈` 与管理员命令，
//!   以及每分钟一次的配置同步 / 心跳；
//! - **燕双鹰**（`nai-bots warden`）：零 Intent、不接命令、不发私信，只保持在线并显示「正在观看 一人一 Key」。
//!
//! 重要约束（Discord 曾因 OAuth 突发 + 群发私信 + 机器人在 NSFW 论坛自动点赞给应用打过标记，错误码 680009）：
//! 与 Python 版相比，**对 Discord 可见的行为必须完全一致**——命令名 / 描述 / 选项 / 权限、公会级注册、
//! ephemeral 回复、Intent（奶妹只有 GUILDS + GUILD_MESSAGES，没有 MESSAGE_CONTENT；燕双鹰为 0）、
//! 在线状态与 allowed_mentions；没有私信；没有论坛点赞 / AI 评论。

pub mod api;
pub mod bridge;
pub mod commands;
pub mod config;
pub mod events;
pub mod handlers;
pub mod naimei;
pub mod pystr;
pub mod warden;
