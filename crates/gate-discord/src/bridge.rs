//! 网关桥接客户端：`/self-register/*`（命令）与 `/self-register/bot/*`（配置 / 心跳）。
//!
//! 鉴权：`Authorization: Bearer <REGISTRATION_BRIDGE_SECRET>`；请求体 / 超时与 Python 一致：
//! - 命令类（`backend()`）：POST JSON，8 秒；
//! - `/register`（`discord_registration.py`）：POST JSON，12 秒；
//! - 配置 / 上报（`bridge()`）：8 秒，任何失败返回 None，不影响机器人其他功能。
//!
//! httpx 的 `timeout=8` 是「连接 / 读 / 写 / 取连接」各 8 秒，reqwest 这里用整体 8 秒；
//! 网关在同机 docker 网络里，两者在实际行为上没有差别。

use std::time::Duration;

use serde_json::{Map, Value};

use crate::events::Interaction;
use crate::handlers::{Backend, HandlerError, interpret_backend};

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(12);

/// Python 的 `os.environ["REGISTRATION_BRIDGE_SECRET"]` 没配置时会抛 `KeyError`，
/// 命令里走 `tree.error`（回「出了点问题」），同步循环里被 `except Exception` 吞掉并打日志。
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("KeyError: 'REGISTRATION_BRIDGE_SECRET'")]
    NoSecret,
    #[error("{0}")]
    Handler(#[from] HandlerError),
}

#[derive(Clone)]
pub struct Bridge {
    http: reqwest::Client,
    backend: String,
    secret: Option<String>,
    command_timeout: Duration,
    register_timeout: Duration,
}

impl Bridge {
    /// `backend` 已去掉末尾的 `/`。
    pub fn new(backend: impl Into<String>, secret: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("nai-bots/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest 客户端配置固定，不会失败");
        Bridge {
            http,
            backend: backend.into(),
            secret,
            command_timeout: COMMAND_TIMEOUT,
            register_timeout: REGISTER_TIMEOUT,
        }
    }

    /// 换掉超时（只给测试用：生产环境一律用 8 秒 / 12 秒）。
    #[must_use]
    pub fn with_timeouts(mut self, command: Duration, register: Duration) -> Self {
        self.command_timeout = command;
        self.register_timeout = register;
        self
    }

    fn bearer(&self) -> Result<String, BridgeError> {
        self.secret
            .as_ref()
            .map(|s| format!("Bearer {s}"))
            .ok_or(BridgeError::NoSecret)
    }

    /// `backend(path, interaction, ...)`：POST 到网关，返回 `(status, data)`。
    /// 连不上 / 超时 / 返回体不是 JSON → `(0, "服务暂时不可用，请稍后再试。")`。
    pub async fn command(&self, path: &str, payload: &Value) -> Result<Backend, BridgeError> {
        let auth = self.bearer()?;
        let sent = self
            .http
            .post(format!("{}{path}", self.backend))
            .timeout(self.command_timeout)
            .header("Authorization", auth)
            .json(payload)
            .send()
            .await;
        let Ok(response) = sent else {
            return Ok(Backend::unavailable());
        };
        let status = response.status().as_u16();
        let Ok(body) = response.bytes().await else {
            return Ok(Backend::unavailable());
        };
        Ok(interpret_backend(status, &body)?)
    }

    /// `/register` 直发：返回网关的 `(status, body)`；传输层失败返回 `None`（调用方回「领 Key 暂不可用」）。
    pub async fn issue(&self, secret: &str, payload: &Value) -> Option<(u16, Vec<u8>)> {
        let response = self
            .http
            .post(format!("{}/self-register/issue", self.backend))
            .timeout(self.register_timeout)
            .header("Authorization", format!("Bearer {secret}"))
            .json(payload)
            .send()
            .await
            .ok()?;
        let status = response.status().as_u16();
        let body = response.bytes().await.ok()?;
        Some((status, body.to_vec()))
    }

    /// `bridge(method, path, payload)`：机器人专用接口 `/self-register/bot{path}`；
    /// 非 200 / 连不上 / 不是 JSON 一律 `Ok(None)`。
    pub async fn bot(
        &self,
        method: reqwest::Method,
        path: &str,
        payload: Option<&Value>,
    ) -> Result<Option<Value>, BridgeError> {
        let auth = self.bearer()?;
        let mut request = self
            .http
            .request(method, format!("{}/self-register/bot{path}", self.backend))
            .timeout(self.command_timeout)
            .header("Authorization", auth);
        if let Some(p) = payload {
            request = request.json(p);
        }
        let Ok(response) = request.send().await else {
            return Ok(None);
        };
        if response.status().as_u16() != 200 {
            return Ok(None);
        }
        Ok(response.json::<Value>().await.ok())
    }
}

/// `backend()` 里拼的请求体：`{discord_id, guild_id, actor_id, **extra}`。
/// `discord_id` 默认是发起者，传了 `extra_id`（非空）就用它（/revoke 用目标成员）；
/// `extra` 里的同名键覆盖前面的值但保持位置（Python dict 的 `**` 展开语义）。
pub fn who_payload(
    it: &Interaction,
    extra_id: Option<&str>,
    extra: &[(&str, Value)],
) -> Result<Value, HandlerError> {
    let user = it
        .actor()
        .ok_or_else(|| HandlerError::Type("interaction has no user".into()))?;
    let mut map = Map::new();
    let discord_id = extra_id.filter(|s| !s.is_empty()).unwrap_or(&user.id);
    map.insert("discord_id".into(), Value::String(discord_id.to_owned()));
    map.insert("guild_id".into(), Value::String(it.guild_id_str()));
    map.insert("actor_id".into(), Value::String(user.id.clone()));
    for (k, v) in extra {
        map.insert((*k).to_owned(), v.clone());
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn interaction() -> Interaction {
        serde_json::from_value(json!({
            "id": "900000000000000010", "type": 2, "token": "t", "guild_id": "900000000000000001",
            "member": {"user": {"id": "900000000000000005", "username": "alice"}},
            "data": {"name": "revoke"},
        }))
        .unwrap()
    }

    #[test]
    fn who_payload_default_and_extra() {
        let p = who_payload(
            &interaction(),
            None,
            &[("action", json!("open")), ("value", json!("on"))],
        )
        .unwrap();
        assert_eq!(
            p.to_string(),
            r#"{"discord_id":"900000000000000005","guild_id":"900000000000000001","actor_id":"900000000000000005","action":"open","value":"on"}"#
        );
    }

    #[test]
    fn who_payload_extra_id_targets_other_member() {
        let p = who_payload(&interaction(), Some("900000000000000077"), &[]).unwrap();
        assert_eq!(p["discord_id"], "900000000000000077");
        assert_eq!(p["actor_id"], "900000000000000005");
        // 空串回落到发起者（Python 的 `extra_id or ...`）
        let p = who_payload(&interaction(), Some(""), &[]).unwrap();
        assert_eq!(p["discord_id"], "900000000000000005");
    }

    #[test]
    fn who_payload_extra_overrides_in_place() {
        let p = who_payload(&interaction(), None, &[("actor_id", json!("x"))]).unwrap();
        assert_eq!(
            p.to_string(),
            r#"{"discord_id":"900000000000000005","guild_id":"900000000000000001","actor_id":"x"}"#
        );
    }
}
