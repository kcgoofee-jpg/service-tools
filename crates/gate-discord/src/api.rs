//! Discord REST：用 twilight-http 发请求（含它自带的限流器），请求体自己拼成 JSON。
//!
//! 请求体为什么不用 twilight 的 typed builder：
//! - 要和 discord.py 发出的 JSON 逐键一致（`tts:false`、`flags:64`、`allowed_mentions` 的写法），
//!   对话式 builder 会多发或少发默认字段；
//! - 弹窗用 Label(18) 包下拉，discord.py 的字段（`disabled:false` 等）也要照发；
//! - 纯函数拼体，不连 Discord 就能断言「发出去的就是 discord.py 发的」（见 `tests/flows.rs`）。
//!
//! 交互回调 / 跟进消息是「令牌即鉴权」的接口，不带机器人 Authorization（与 discord.py 一致）。

use serde_json::{Value, json};
use twilight_http::{Client, request::Request, routing::Route};

/// 消息标志位：EPHEMERAL（只有触发者可见）。
pub const EPHEMERAL: u64 = 64;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("无效的 ID：{0}")]
    BadId(String),
    #[error("Discord 请求失败：{0}")]
    Http(#[from] twilight_http::Error),
}

fn id(s: &str) -> Result<u64, ApiError> {
    s.parse().map_err(|_| ApiError::BadId(s.to_owned()))
}

// ---------- 请求体（纯函数） ----------

/// `interaction.response.defer(...)`：
/// - 斜杠命令：类型 5（DEFERRED_CHANNEL_MESSAGE），`ephemeral=True` 时带 `flags: 64`；
/// - 弹窗提交：discord.py 在 `thinking=False` 时发类型 6（DEFERRED_UPDATE_MESSAGE），不带任何 data
///   （`ephemeral` 对它无效，后面的跟进消息自己带 `flags: 64`）。
pub fn defer_body(ephemeral: bool, modal_submit: bool) -> Value {
    if modal_submit {
        json!({"type": 6})
    } else if ephemeral {
        json!({"type": 5, "data": {"flags": EPHEMERAL}})
    } else {
        json!({"type": 5})
    }
}

/// `interaction.response.send_message(content, ephemeral=...)`：类型 4。
pub fn message_body(content: &str, ephemeral: bool) -> Value {
    let mut data = json!({"tts": false, "content": content});
    if ephemeral {
        data["flags"] = json!(EPHEMERAL);
    }
    json!({"type": 4, "data": data})
}

/// `interaction.response.send_modal(modal)`：类型 9。
pub fn modal_body(modal: Value) -> Value {
    json!({"type": 9, "data": modal})
}

/// `interaction.followup.send(content, ephemeral=...)`。
pub fn followup_body(content: &str, ephemeral: bool) -> Value {
    let mut body = json!({"content": content, "tts": false});
    if ephemeral {
        body["flags"] = json!(EPHEMERAL);
    }
    body
}

/// 「@奶妹」回复：`message.reply(text, allowed_mentions=AllowedMentions(users=[owner], everyone=False,
/// roles=False, replied_user=False))`。只允许提到服务器主人这一个用户，不 @everyone / 身份组，也不 ping 被回复的人
/// （`replied_user` 为假时 discord.py 不发这个键，Discord 默认就是不 ping）。
///
/// 键与顺序取自 discord.py 2.7 实际发出的请求（见 golden `mention_replies_to_owner`）：
/// `components: []`、随机 `nonce`（`secrets.randbits(64)` 的十进制字符串）+ `enforce_nonce`，
/// `message_reference` 里 ID 是数字、没有 `fail_if_not_exists`。
pub fn mention_reply_body(
    content: &str,
    message_id: &str,
    channel_id: &str,
    guild_id: Option<&str>,
    owner_id: u64,
    nonce: u64,
) -> Value {
    let mut reference = serde_json::Map::new();
    reference.insert("message_id".into(), json!(message_id.parse::<u64>().ok()));
    reference.insert("channel_id".into(), json!(channel_id.parse::<u64>().ok()));
    if let Some(g) = guild_id {
        reference.insert("guild_id".into(), json!(g.parse::<u64>().ok()));
    }
    json!({
        "content": content,
        "components": [],
        "nonce": nonce.to_string(),
        "enforce_nonce": true,
        "message_reference": reference,
        "tts": false,
        "allowed_mentions": {"users": [owner_id], "parse": []},
    })
}

// ---------- 发送 ----------

#[derive(Clone)]
pub struct DiscordApi {
    client: std::sync::Arc<Client>,
}

impl DiscordApi {
    /// `proxy` 仅测试用：把所有 REST 请求指到本机假 Discord（`host:port`，明文 HTTP）。
    pub fn new(token: &str, proxy: Option<String>) -> Self {
        let mut builder = Client::builder().token(token.to_owned());
        if let Some(host) = proxy {
            // 假服务器没有限流头，关掉限流器；生产环境不传 proxy，保留 twilight 默认限流器
            builder = builder.proxy(host, true).ratelimiter(None);
        }
        DiscordApi {
            client: std::sync::Arc::new(builder.build()),
        }
    }

    async fn send(&self, request: Result<Request, twilight_http::Error>) -> Result<(), ApiError> {
        self.client.request::<()>(request?).await?;
        Ok(())
    }

    /// `POST /interactions/{id}/{token}/callback?with_response=true`
    pub async fn callback(
        &self,
        interaction_id: &str,
        token: &str,
        body: &Value,
    ) -> Result<(), ApiError> {
        let route = Route::InteractionCallback {
            interaction_id: id(interaction_id)?,
            interaction_token: token,
            with_response: true,
        };
        self.send(
            Request::builder(&route)
                .json(body)
                .use_authorization_token(false)
                .build(),
        )
        .await
    }

    /// `POST /webhooks/{application_id}/{token}?wait=true`（交互跟进消息）。
    pub async fn followup(
        &self,
        application_id: &str,
        token: &str,
        body: &Value,
    ) -> Result<(), ApiError> {
        let route = Route::ExecuteWebhook {
            thread_id: None,
            token,
            wait: Some(true),
            with_components: Some(false),
            webhook_id: id(application_id)?,
        };
        self.send(
            Request::builder(&route)
                .json(body)
                .use_authorization_token(false)
                .build(),
        )
        .await
    }

    /// `POST /channels/{id}/messages`
    pub async fn create_message(&self, channel_id: &str, body: &Value) -> Result<(), ApiError> {
        let route = Route::CreateMessage {
            channel_id: id(channel_id)?,
        };
        self.send(Request::builder(&route).json(body).build()).await
    }

    /// `PUT /applications/{app}/guilds/{guild}/commands`（整体覆盖，等同 `tree.sync(guild=...)`）。
    pub async fn sync_guild_commands(
        &self,
        application_id: &str,
        guild_id: u64,
        definitions: &[Value],
    ) -> Result<(), ApiError> {
        let route = Route::SetGuildCommands {
            application_id: id(application_id)?,
            guild_id,
        };
        self.send(Request::builder(&route).json(&definitions).build())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defer_bodies() {
        assert_eq!(
            defer_body(true, false).to_string(),
            r#"{"type":5,"data":{"flags":64}}"#
        );
        assert_eq!(defer_body(false, false).to_string(), r#"{"type":5}"#);
        // 弹窗提交：ephemeral 对类型 6 无效
        assert_eq!(defer_body(true, true).to_string(), r#"{"type":6}"#);
    }

    #[test]
    fn message_and_followup_bodies() {
        assert_eq!(
            message_body("你好", true).to_string(),
            r#"{"type":4,"data":{"tts":false,"content":"你好","flags":64}}"#
        );
        assert_eq!(
            followup_body("你好", true).to_string(),
            r#"{"content":"你好","tts":false,"flags":64}"#
        );
    }

    #[test]
    fn mention_reply_only_allows_the_owner() {
        let b = mention_reply_body(
            "我在床上陪主人呢 <@900000000000000002>",
            "900000000000000031",
            "900000000000000004",
            Some("900000000000000001"),
            900000000000000002,
            7,
        );
        assert_eq!(
            b["allowed_mentions"],
            json!({"users": [900000000000000002u64], "parse": []})
        );
        assert_eq!(b["message_reference"]["message_id"], 900000000000000031u64);
        assert_eq!(b["nonce"], "7");
    }
}
