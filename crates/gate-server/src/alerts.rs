//! 站长告警：推送到 Discord（私信 / 频道 / Webhook 任选）。
//! - 只发事件类型和一句话，不含 Key、提示词或成员内容；
//! - 同一 kind 在冷却期内只发一次（只写日志的也一样，免得日志每 5 分钟刷一条）；
//! - **只有重大事故才私信**（站长 10-15：有后台，非重大事故不私信）——成员现在用不了、
//!   或账号 / 数据有风险，且需要站长动手。其余只写日志，后台总览的「有 N 件事要处理」能看到同样的情况。

use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// 会私信站长的事件。新增之前先问：成员现在是不是用不了 / 账号或数据是不是有风险 / 站长是不是必须动手？
/// 上游 5xx / 限流 / 变慢、V5 余量、Bug、刷 Key 的 IP 都不算：要么会自动恢复，要么站长做不了什么，后台都能看到。
const PAGE_KINDS: [&str; 6] = [
    // 上游 Token 失效：生图和文本全部不可用
    "upstream_401",
    // 连续 403：账号可能被上游封控
    "upstream_403_cooldown",
    "upstream_403_text",
    // 上游 Token 被改动（安全事件）
    "upstream_token_changed",
    // 磁盘剩余不足 10%
    "disk_low",
    // 后台「发送测试告警」
    "test",
];

pub fn pages(kind: &str) -> bool {
    PAGE_KINDS.contains(&kind)
}

pub struct Alerter {
    bot_token: String,
    user_id: String,
    channel_id: String,
    webhook_url: String,
    api_base: String,
    last: Mutex<HashMap<String, Instant>>,
    dm_channel: tokio::sync::Mutex<Option<String>>,
    http: reqwest::Client,
    pub sent: std::sync::atomic::AtomicU64,
}

impl Alerter {
    pub fn new(
        api_base: &str,
        bot_token: &str,
        user_id: &str,
        channel_id: &str,
        webhook_url: &str,
    ) -> Self {
        Self {
            bot_token: bot_token.to_string(),
            user_id: user_id.trim().to_string(),
            channel_id: channel_id.trim().to_string(),
            webhook_url: webhook_url.trim().to_string(),
            api_base: api_base.trim_end_matches('/').to_string(),
            last: Mutex::new(HashMap::new()),
            dm_channel: tokio::sync::Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            sent: Default::default(),
        }
    }

    /// 站长告警通道。
    pub fn owner(env: &crate::env::EnvConfig) -> Self {
        Self::new(
            &env.discord_api_base,
            &env.discord_bot_token,
            &env.alert_user_id,
            &env.alert_channel_id,
            &env.alert_webhook_url,
        )
    }

    /// 成员公告通道（同一个机器人发到公告频道）。
    pub fn announcer(env: &crate::env::EnvConfig) -> Self {
        Self::new(
            &env.discord_api_base,
            &env.discord_bot_token,
            "",
            &env.announce_channel_id,
            "",
        )
    }

    pub fn configured(&self) -> bool {
        !self.webhook_url.is_empty()
            || (!self.bot_token.is_empty()
                && (!self.user_id.is_empty() || !self.channel_id.is_empty()))
    }

    /// 告警（不阻塞调用方）。
    pub fn notify(self: &std::sync::Arc<Self>, kind: &str, message: &str, cooldown: f64) {
        {
            let mut last = self.last.lock().expect("alerts");
            let now = Instant::now();
            if last
                .get(kind)
                .is_some_and(|t| now.duration_since(*t).as_secs_f64() < cooldown)
            {
                return;
            }
            last.insert(kind.to_string(), now);
        }
        if !pages(kind) {
            tracing::info!(target: "alert-log", kind, "{message}");
            return;
        }
        if !self.configured() {
            return;
        }
        let me = self.clone();
        let text = format!("⚠ **猫头鹰公益站告警**（{kind}）\n{message}");
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { me.send(&text).await });
        }
    }

    /// 不带告警前缀的普通通知（如给成员的公告）；无冷却。
    pub fn post(self: &std::sync::Arc<Self>, text: &str) {
        if !self.configured() {
            return;
        }
        let me = self.clone();
        let text = text.to_string();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { me.send(&text).await });
        }
    }

    async fn send(&self, text: &str) {
        let content: String = gate_core::pyc::take_chars(text, 1900).to_string();
        let body = json!({"content": content, "allowed_mentions": {"parse": []}});
        let result: Result<(), String> = async {
            if !self.webhook_url.is_empty() {
                self.http
                    .post(&self.webhook_url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .error_for_status()
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
            let auth = format!("Bot {}", self.bot_token);
            let channel = if !self.channel_id.is_empty() {
                self.channel_id.clone()
            } else {
                let mut dm = self.dm_channel.lock().await;
                if dm.is_none() {
                    let created: serde_json::Value = self
                        .http
                        .post(format!("{}/users/@me/channels", self.api_base))
                        .header("Authorization", &auth)
                        .json(&json!({"recipient_id": self.user_id}))
                        .send()
                        .await
                        .map_err(|e| e.to_string())?
                        .error_for_status()
                        .map_err(|e| e.to_string())?
                        .json()
                        .await
                        .map_err(|e| e.to_string())?;
                    *dm = created
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                dm.clone().ok_or("no dm channel")?
            };
            self.http
                .post(format!("{}/channels/{channel}/messages", self.api_base))
                .header("Authorization", &auth)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                self.sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // 告警失败不能影响服务；不打印错误详情（可能含 URL / Token）
            Err(_) => tracing::warn!("alert delivery failed"),
        }
    }
}
