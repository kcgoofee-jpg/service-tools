//! 燕双鹰：猫头鹰公益站的执法 bot。零 Intent、不接命令、不发私信，只保持在线。
//! 公告由运维脚本经 REST 发到公告频道，这个进程不发任何消息。
//!
//! 没配 `WARDEN_BOT_TOKEN` 时不要反复登录失败重启（会被 Discord 限流）：打一行日志，然后睡到天荒地老。

use std::time::Duration;

use futures::StreamExt as _;
use serde_json::json;
use tracing::{info, warn};
use twilight_gateway::{Config as GatewayConfig, Message, Shard, ShardId};
use twilight_model::gateway::CloseFrame;

use crate::commands::WARDEN_INTENTS;
use crate::events::{self, Gateway};

/// 在线状态里显示的活动：「正在观看 一人一 Key」。
pub const ACTIVITY_NAME: &str = "一人一 Key";

/// READY 之后发的在线状态：opcode 3（PRESENCE_UPDATE）。
///
/// 载荷逐键取自 discord.py 实际发出的帧（Python：`change_presence(status=online,
/// activity=Activity(type=watching, name=...))`，见 golden `warden_presence`）：活动里带 `flags: 0`、
/// `buttons: []`，`since` 是浮点 `0.0`。所以不用 twilight 的 `UpdatePresence`（它会省略这些键、`since` 发 null），
/// 直接发 JSON 文本。
pub fn presence_payload() -> String {
    json!({
        "op": 3,
        "d": {
            "activities": [{"flags": 0, "type": ACTIVITY_WATCHING, "name": ACTIVITY_NAME, "buttons": []}],
            "afk": false,
            "since": 0.0,
            "status": "online",
        },
    })
    .to_string()
}

/// 活动类型 3 = Watching。
pub const ACTIVITY_WATCHING: u8 = 3;

/// 没配 Token：永远睡下去（不返回）。
pub async fn idle_forever() -> ! {
    info!("[warden] WARDEN_BOT_TOKEN 未设置，不启动");
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// 运行燕双鹰，直到收到停止信号（正常返回）或网关致命关闭（返回 Err）。
pub async fn run(token: String) -> anyhow::Result<()> {
    let config = GatewayConfig::new(token, WARDEN_INTENTS);
    let mut shard = Shard::with_config(ShardId::ONE, config);
    let shutdown = crate::naimei::wait_for_shutdown();
    tokio::pin!(shutdown);
    let mut closing = false;
    loop {
        let item = tokio::select! {
            item = shard.next() => item,
            () = &mut shutdown, if !closing => {
                info!("[warden] 收到停止信号，正在断开网关");
                shard.close(CloseFrame::NORMAL);
                closing = true;
                continue;
            }
        };
        match item {
            Some(Ok(Message::Text(raw))) => {
                if let Ok(Gateway::Ready(ready)) = events::parse(&raw) {
                    shard.send(presence_payload());
                    info!("[warden] online as {}", ready.user.display_tag());
                }
            }
            Some(Ok(Message::Close(frame))) => info!("[warden] 网关连接关闭：{frame:?}"),
            Some(Err(e)) => warn!("[warden] 网关消息出错：{e}"),
            None if closing => return Ok(()),
            None => anyhow::bail!(
                "网关连接被致命关闭（常见原因：WARDEN_BOT_TOKEN 无效），详见上面的关闭码"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_is_online_watching() {
        let json: serde_json::Value = serde_json::from_str(&presence_payload()).unwrap();
        let d = &json["d"];
        assert_eq!(json["op"], 3);
        assert_eq!(d["status"], "online");
        assert_eq!(d["afk"], false);
        assert_eq!(d["activities"][0]["name"], ACTIVITY_NAME);
        assert_eq!(d["activities"][0]["type"], 3);
    }
}
