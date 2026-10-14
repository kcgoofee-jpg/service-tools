//! 奶妹的斜杠命令定义：逐字节等于 discord.py 的 `tree.sync(guild=...)` 发给 Discord 的 JSON。
//!
//! 为什么手写 JSON 而不用 twilight 的 `Command` 结构：
//! - discord.py 会显式发 `"contexts": null`、`"integration_types": null`、`"dm_permission": true`，
//!   typed 结构会把 `None` 省略，`PUT` 整体覆盖时「省略」与「null」对 Discord 是否等价没有文档保证，
//!   而封号（680009）之后我们不想靠猜；
//! - 没写 `describe` 的参数 discord.py 会填占位描述 `…`（U+2026），这也要照发；
//! - 快照测试（`tests/commands_snapshot.rs`）直接拿 `tools/parity/golden/discord_commands.py`
//!   在旧代码上导出的 `to_dict()` 结果逐字符比较。
//!
//! 命令只注册到 `DISCORD_GUILD_ID` 这一个服务器（公会级），不注册全局命令。

use serde_json::{Value, json};
use twilight_model::gateway::Intents;

/// 管理员命令的 `default_member_permissions`：`manage_guild` = 1 << 5。
/// discord.py 发的是整数 32（不是字符串），照发。
pub const MANAGE_GUILD: u64 = 32;

/// 没写 `describe` 的参数描述（discord.py 的默认占位）。
pub const PLACEHOLDER_DESCRIPTION: &str = "…";

/// 奶妹的网关 Intents：只有 GUILDS（斜杠命令 / 服务器信息）与 GUILD_MESSAGES（只用来认「@奶妹」）。
/// 没有 MESSAGE_CONTENT / GUILD_MEMBERS / GUILD_PRESENCES 等任何特权 Intent。数值 513。
pub const NAIMEI_INTENTS: Intents = Intents::GUILDS.union(Intents::GUILD_MESSAGES);

/// 燕双鹰：零 Intent。
pub const WARDEN_INTENTS: Intents = Intents::empty();

/// 奶妹的全部斜杠命令（按注册顺序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Register,
    Quota,
    Feedback,
    ResetKey,
    Help,
    Open,
    Limit,
    Ban,
    Unban,
    Slots,
    Revoke,
}

impl Command {
    /// 与 discord_bot.py 里 `@tree.command` 的定义顺序一致。
    pub const ALL: [Command; 11] = [
        Command::Register,
        Command::Quota,
        Command::Feedback,
        Command::ResetKey,
        Command::Help,
        Command::Open,
        Command::Limit,
        Command::Ban,
        Command::Unban,
        Command::Slots,
        Command::Revoke,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Command::Register => "register",
            Command::Quota => "quota",
            Command::Feedback => "反馈",
            Command::ResetKey => "resetkey",
            Command::Help => "help",
            Command::Open => "open",
            Command::Limit => "limit",
            Command::Ban => "ban",
            Command::Unban => "unban",
            Command::Slots => "slots",
            Command::Revoke => "revoke",
        }
    }

    pub fn from_name(name: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// 一条命令的外壳：键顺序与 discord.py `Command.to_dict()` 一致。
fn command(name: &str, description: &str, options: Vec<Value>, admin: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "type": 1,
        "options": options,
        "nsfw": false,
        "dm_permission": true,
        "default_member_permissions": if admin { json!(MANAGE_GUILD) } else { Value::Null },
        "contexts": null,
        "integration_types": null,
    })
}

fn member_option() -> Value {
    json!({"type": 6, "name": "member", "description": PLACEHOLDER_DESCRIPTION, "required": true})
}

/// 发给 `PUT /applications/{app}/guilds/{guild}/commands` 的整个数组。
pub fn definitions() -> Vec<Value> {
    vec![
        command(
            "register",
            "领取你的猫头鹰公益站 API Key",
            vec![json!({
                "type": 3,
                "name": "key",
                "description": "收到站长发的邀请 Key 时粘贴在这里，绑定到你的账号（可选）",
                "required": false,
            })],
            false,
        ),
        command("quota", "查看今日额度、Key 有效期和服务状态", vec![], false),
        command(
            "反馈",
            "给站长提意见：5 道选择题，30 秒填完（只有你能看到，不会私信你）",
            vec![],
            false,
        ),
        command(
            "resetkey",
            "Key 丢了或泄露了？换一把新的（旧的立即失效）",
            vec![],
            false,
        ),
        command("help", "怎么使用猫头鹰公益站", vec![], false),
        command(
            "open",
            "（管理员）开放或关闭领 Key",
            vec![json!({
                "type": 3,
                "name": "state",
                "description": PLACEHOLDER_DESCRIPTION,
                "required": true,
                "choices": [
                    {"name": "开放", "value": "on"},
                    {"name": "关闭", "value": "off"},
                ],
            })],
            true,
        ),
        command(
            "limit",
            "（管理员）设置名额上限，0 为不限",
            vec![json!({
                "type": 4,
                "name": "count",
                "description": PLACEHOLDER_DESCRIPTION,
                "required": true,
                "min_value": 0,
                "max_value": 1000,
            })],
            true,
        ),
        command(
            "ban",
            "（管理员）永久禁止某位成员领取 Key，并撤销其现有 Key",
            vec![member_option()],
            true,
        ),
        command(
            "unban",
            "（管理员）解除某位成员的领取禁令",
            vec![member_option()],
            true,
        ),
        command("slots", "（管理员）查看名额占用", vec![], true),
        command(
            "revoke",
            "（管理员）撤销某位成员的 Key，释放名额",
            vec![member_option()],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip_and_follow_definition_order() {
        let defs = definitions();
        assert_eq!(defs.len(), Command::ALL.len());
        for (cmd, def) in Command::ALL.iter().zip(&defs) {
            assert_eq!(def["name"], cmd.name());
            assert_eq!(Command::from_name(cmd.name()), Some(*cmd));
        }
        assert_eq!(Command::from_name("nope"), None);
    }

    #[test]
    fn only_admin_commands_require_manage_guild() {
        for def in definitions() {
            let name = def["name"].as_str().unwrap();
            let admin = ["open", "limit", "ban", "unban", "slots", "revoke"].contains(&name);
            if admin {
                assert_eq!(def["default_member_permissions"], 32, "{name}");
            } else {
                assert!(def["default_member_permissions"].is_null(), "{name}");
            }
        }
    }

    #[test]
    fn intents_are_guilds_and_guild_messages_only() {
        assert_eq!(NAIMEI_INTENTS.bits(), 513);
        assert!(!NAIMEI_INTENTS.contains(Intents::MESSAGE_CONTENT));
        assert_eq!(WARDEN_INTENTS.bits(), 0);
    }
}
