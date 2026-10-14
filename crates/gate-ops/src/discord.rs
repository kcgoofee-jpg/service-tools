//! Discord REST（用 bot token，不走网关连接）：公告、逐条私信、巡检告警共用。
//!
//! 基地址从环境变量 `DISCORD_API_BASE` 读（默认官方地址）——测试里指向本机假服务，
//! 绝不会真的发消息。Discord 前面有 Cloudflare：默认 UA 会被判成机器人直接 403（error code 1010），
//! 所以统一用 patrol.py 里实测可用的那组 UA（2026-10-13）。

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";
pub const USER_AGENT: &str = "DiscordBot (https://gate.davidzhao.top, 1.0)";

/// `DISCORD_API_BASE`（去掉末尾斜杠）。
pub fn api_base_from_env() -> String {
    crate::util::env_nonempty("DISCORD_API_BASE")
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// 一次 REST 调用的结果；传输层错误用 `Err` 返回。
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
    pub fn ok(&self) -> bool {
        matches!(self.status, 200 | 201)
    }
}

pub struct Discord {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Discord {
    pub fn new(base: &str, token: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Resp> {
        let resp = req
            .header("Authorization", format!("Bot {}", self.token))
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(Resp { status, body })
    }

    /// 往频道发消息。
    pub async fn post_message(&self, channel: &str, body: &Value) -> Result<Resp> {
        let url = format!("{}/channels/{channel}/messages", self.base);
        self.send(self.http.post(url).json(body)).await
    }

    /// 打开（或取回）与某人的私信频道。
    pub async fn create_dm(&self, user_id: &str) -> Result<Resp> {
        let url = format!("{}/users/@me/channels", self.base);
        let body = serde_json::json!({"recipient_id": user_id});
        self.send(self.http.post(url).json(&body)).await
    }

    /// 原位编辑已发出的消息。
    pub async fn edit_message(&self, channel: &str, message: &str, body: &Value) -> Result<Resp> {
        let url = format!("{}/channels/{channel}/messages/{message}", self.base);
        self.send(self.http.patch(url).json(body)).await
    }
}

/// 不允许任何 @ 提及的消息体（公告 / 私信文案里的 @everyone 也不会生效）。
pub fn message_body(text: &str) -> Value {
    serde_json::json!({
        "content": gate_core::pyc::take_chars(text, 1900),
        "allowed_mentions": {"parse": []},
    })
}
