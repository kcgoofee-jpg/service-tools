//! 快照：Rust 版发给 Discord 的命令定义 / Intent / 在线状态，必须和旧 Python（discord.py）实际发出的逐字节一致。
//! 夹具 `discord_commands.json` 由 `tools/parity/golden/discord_commands.py` 生成：
//! 真跑 `build_client()` + `tree.sync()`，在 HTTP 层截下 `PUT /applications/{app}/guilds/{guild}/commands` 的请求体。

mod common;

use common::{APP, GUILD, MockServer};
use gate_discord::api::DiscordApi;
use gate_discord::commands::{self, NAIMEI_INTENTS, WARDEN_INTENTS};
use gate_discord::warden;
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/discord_commands.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn command_definitions_equal_discordpy_sync_body() {
    let expected = &golden()["sync_request"]["json"];
    let actual = Value::Array(commands::definitions());
    // 逐字符比较（含键顺序）：JSON 文本完全一致才算过
    assert_eq!(actual.to_string(), expected.to_string());
    // 名字 / 描述 / 选项 / 权限的结构化比较，出错时更好读
    assert_eq!(actual, *expected);
}

#[test]
fn command_names_and_permissions_summary() {
    let defs = commands::definitions();
    let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "register", "quota", "反馈", "resetkey", "help", "open", "limit", "ban", "unban",
            "slots", "revoke"
        ]
    );
    // 管理员命令 = manage_guild(32)；成员命令 = null（所有人可见）
    for d in &defs {
        let admin = ["open", "limit", "ban", "unban", "slots", "revoke"]
            .contains(&d["name"].as_str().unwrap());
        assert_eq!(
            d["default_member_permissions"],
            if admin { Value::from(32) } else { Value::Null }
        );
        assert_eq!(d["type"], 1, "只注册斜杠命令，没有右键菜单命令");
    }
}

#[test]
fn intents_match_discordpy() {
    let g = golden();
    assert_eq!(NAIMEI_INTENTS.bits(), g["intents"].as_u64().unwrap());
    assert_eq!(WARDEN_INTENTS.bits(), g["warden_intents"].as_u64().unwrap());
    // 513 = GUILDS(1) + GUILD_MESSAGES(512)：没有任何特权 Intent
    assert_eq!(NAIMEI_INTENTS.bits(), 1 | 512);
}

#[test]
fn warden_presence_frame_equals_discordpy() {
    let expected = &golden()["warden_presence"]["payload"];
    assert_eq!(warden::presence_payload(), expected.to_string());
}

/// `PUT` 只发给 DISCORD_GUILD_ID 这一个公会，请求体就是上面那份定义；不碰全局命令。
#[tokio::test]
async fn sync_goes_to_the_one_guild_endpoint_only() {
    let server = MockServer::start().await;
    let api = DiscordApi::new("test-token", Some(server.discord_proxy()));
    server.reset(&Value::Null, &Value::Null);
    api.sync_guild_commands(APP, GUILD.parse().unwrap(), &commands::definitions())
        .await
        .unwrap();
    let calls = server.discord_log();
    assert_eq!(calls.len(), 1);
    let expected = &golden()["sync_request"];
    assert_eq!(calls[0]["method"], expected["method"]);
    assert_eq!(calls[0]["path"], expected["path"]);
    assert_eq!(calls[0]["json"].to_string(), expected["json"].to_string());
    // 机器人 Token 走 Authorization，且带 Bot 前缀
    assert_eq!(calls[0]["authorization"], "Bot test-token");
}
