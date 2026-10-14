//! 网关事件的宽容解析：只取机器人用得到的字段。
//!
//! 为什么不用 `twilight_model` 的 `Event`：它会把整个 GUILD_CREATE / MESSAGE_CREATE / INTERACTION_CREATE
//! 严格反序列化（含频道、角色、组件树……）。Discord 新增一种频道或组件类型时，严格模型会整条事件解析失败，
//! 机器人就悄悄丢掉用户的命令（用户只看到「应用程序没有响应」）。discord.py 是动态取字段的，天然不怕这个，
//! 所以这里也只声明需要的字段、其余一律忽略。网关连接 / 心跳 / 重连仍全部交给 twilight-gateway。
//!
//! ID 一律保留 Discord 原样的十进制字符串，原样转给网关桥接（对应 Python 的 `str(interaction.user.id)`）。

use serde::Deserialize;
use serde_json::Value;

/// 事件里的用户对象。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
    /// 头像哈希（`a_` 开头表示动图）；没有头像是 null。
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    /// discord.py `str(user)`：新式用户名（discriminator 为 "0"）只显示 name，旧式带 `#1234`。
    pub fn display_tag(&self) -> String {
        match self.discriminator.as_deref() {
            None | Some("0") | Some("") => self.username.clone(),
            Some(d) => format!("{}#{d}", self.username),
        }
    }
}

/// 服务器成员（交互里附带的 `member`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub nick: Option<String>,
}

/// 斜杠命令选项。
#[derive(Debug, Clone, Deserialize)]
pub struct CommandOption {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub value: Option<Value>,
}

/// `INTERACTION_CREATE.data`（命令与弹窗提交共用，用不到的字段留空）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InteractionData {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<u8>,
    #[serde(default)]
    pub options: Vec<CommandOption>,
    #[serde(default)]
    pub custom_id: Option<String>,
    /// 弹窗提交的组件树，保持原始 JSON，由 `modal_values` 取值。
    #[serde(default)]
    pub components: Vec<Value>,
}

/// 交互类型：2 = 斜杠命令，5 = 弹窗提交。
pub const INTERACTION_APPLICATION_COMMAND: u8 = 2;
pub const INTERACTION_MODAL_SUBMIT: u8 = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct Interaction {
    pub id: String,
    #[serde(default)]
    pub application_id: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub member: Option<Member>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub data: Option<InteractionData>,
}

impl Interaction {
    /// 发起者：服务器里是 `member.user`，私聊里是 `user`。
    pub fn actor(&self) -> Option<&User> {
        self.member
            .as_ref()
            .and_then(|m| m.user.as_ref())
            .or(self.user.as_ref())
    }

    /// 服务器昵称（discord.py `Member.nick`）；私聊没有。
    pub fn nick(&self) -> Option<&str> {
        self.member.as_ref().and_then(|m| m.nick.as_deref())
    }

    /// `str(interaction.guild_id)`：没有服务器时 Python 得到字符串 "None"。
    pub fn guild_id_str(&self) -> String {
        self.guild_id.clone().unwrap_or_else(|| "None".into())
    }

    pub fn option(&self, name: &str) -> Option<&CommandOption> {
        self.data
            .as_ref()
            .and_then(|d| d.options.iter().find(|o| o.name == name))
    }
}

/// 弹窗提交里各组件的取值：`custom_id` → 选中的值列表。
/// 组件树是 `Label(18){ component: Select(3){ custom_id, values } }`，旧式 `ActionRow(1){ components: [...] }` 也认。
pub fn modal_values(components: &[Value]) -> Vec<(String, Vec<String>)> {
    fn walk(v: &Value, out: &mut Vec<(String, Vec<String>)>) {
        if let Some(inner) = v.get("component") {
            walk(inner, out);
        }
        if let Some(list) = v.get("components").and_then(Value::as_array) {
            for c in list {
                walk(c, out);
            }
        }
        if let Some(id) = v.get("custom_id").and_then(Value::as_str) {
            let values = v
                .get("values")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            out.push((id.to_owned(), values));
        }
    }
    let mut out = Vec::new();
    for c in components {
        walk(c, &mut out);
    }
    out
}

/// READY 里我们关心的部分。
#[derive(Debug, Clone)]
pub struct ReadyInfo {
    pub user: User,
    pub application_id: String,
}

/// GUILD_CREATE / GUILD_UPDATE 里我们关心的部分（`/help` 之外只用来取服务器名与主人 ID）。
#[derive(Debug, Clone, Deserialize)]
pub struct GuildInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    /// 掉线的服务器只有 id 和 `unavailable: true`。
    #[serde(default)]
    pub unavailable: bool,
}

/// MESSAGE_CREATE 里我们关心的部分。没有 MESSAGE_CONTENT Intent 时，`content` 只有在机器人被 @ 时才有值。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageCreate {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    pub author: User,
    #[serde(default)]
    pub content: String,
}

/// 一条网关消息分类后的结果。
#[derive(Debug, Clone)]
pub enum Gateway {
    Ready(ReadyInfo),
    GuildCreate(GuildInfo),
    GuildUpdate(GuildInfo),
    GuildDelete(String),
    MessageCreate(MessageCreate),
    Interaction(Box<Interaction>),
    /// 其他事件（含 HELLO / HEARTBEAT_ACK 等控制帧）：机器人不关心。
    Other,
}

#[derive(Deserialize)]
struct Head<'a> {
    op: u8,
    #[serde(borrow, default)]
    t: Option<&'a str>,
}

#[derive(Deserialize)]
struct Frame<T> {
    d: T,
}

#[derive(Deserialize)]
struct ReadyRaw {
    user: User,
    #[serde(default)]
    application: Option<AppRaw>,
}

#[derive(Deserialize)]
struct AppRaw {
    id: String,
}

#[derive(Deserialize)]
struct GuildGone {
    id: String,
    #[serde(default)]
    unavailable: bool,
}

/// 解析一条网关文本消息。JSON 本身坏掉才返回 Err；不认识的事件返回 `Gateway::Other`。
pub fn parse(raw: &str) -> Result<Gateway, serde_json::Error> {
    let head: Head<'_> = serde_json::from_str(raw)?;
    if head.op != 0 {
        return Ok(Gateway::Other);
    }
    Ok(match head.t {
        Some("READY") => {
            let f: Frame<ReadyRaw> = serde_json::from_str(raw)?;
            Gateway::Ready(ReadyInfo {
                user: f.d.user,
                application_id: f.d.application.map(|a| a.id).unwrap_or_default(),
            })
        }
        Some("GUILD_CREATE") => {
            Gateway::GuildCreate(serde_json::from_str::<Frame<GuildInfo>>(raw)?.d)
        }
        Some("GUILD_UPDATE") => {
            Gateway::GuildUpdate(serde_json::from_str::<Frame<GuildInfo>>(raw)?.d)
        }
        Some("GUILD_DELETE") => {
            // `unavailable: true` 只是 Discord 那边故障，discord.py 会保留缓存；真正被踢出 / 退出才删
            let gone = serde_json::from_str::<Frame<GuildGone>>(raw)?.d;
            if gone.unavailable {
                Gateway::Other
            } else {
                Gateway::GuildDelete(gone.id)
            }
        }
        Some("MESSAGE_CREATE") => {
            Gateway::MessageCreate(serde_json::from_str::<Frame<MessageCreate>>(raw)?.d)
        }
        Some("INTERACTION_CREATE") => {
            Gateway::Interaction(Box::new(serde_json::from_str::<Frame<Interaction>>(raw)?.d))
        }
        _ => Gateway::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(t: &str, d: Value) -> String {
        json!({"op": 0, "s": 1, "t": t, "d": d}).to_string()
    }

    #[test]
    fn parses_ready() {
        let raw = frame(
            "READY",
            json!({"v": 10, "user": {"id": "900000000000000099", "username": "naimei", "discriminator": "0"},
                   "guilds": [{"id": "900000000000000001", "unavailable": true}],
                   "application": {"id": "900000000000000003", "flags": 0}, "session_id": "s"}),
        );
        let Gateway::Ready(r) = parse(&raw).unwrap() else {
            panic!("not ready")
        };
        assert_eq!(r.application_id, "900000000000000003");
        assert_eq!(r.user.display_tag(), "naimei");
    }

    #[test]
    fn legacy_discriminator_tag() {
        let u = User {
            id: "900000000000000001".into(),
            username: "old".into(),
            discriminator: Some("1234".into()),
            ..User::default()
        };
        assert_eq!(u.display_tag(), "old#1234");
    }

    #[test]
    fn guild_create_ignores_unknown_shapes() {
        // 频道类型 / 组件类型不认识也不能影响取 name / owner_id
        let raw = frame(
            "GUILD_CREATE",
            json!({"id": "900000000000000001", "name": "测试服", "owner_id": "900000000000000002",
                   "channels": [{"id": "1", "type": 9999}], "threads": "whatever"}),
        );
        let Gateway::GuildCreate(g) = parse(&raw).unwrap() else {
            panic!("not guild create")
        };
        assert_eq!(g.name.as_deref(), Some("测试服"));
        assert_eq!(g.owner_id.as_deref(), Some("900000000000000002"));
    }

    #[test]
    fn interaction_actor_prefers_member_user() {
        let raw = frame(
            "INTERACTION_CREATE",
            json!({"id": "900000000000000010", "application_id": "900000000000000003", "type": 2,
                   "token": "tok", "guild_id": "900000000000000001",
                   "member": {"user": {"id": "900000000000000005", "username": "alice", "global_name": "Alice",
                                       "avatar": "abc"}, "nick": "A"},
                   "data": {"id": "1", "name": "quota", "type": 1,
                            "options": [{"name": "key", "type": 3, "value": "k"}]}}),
        );
        let Gateway::Interaction(it) = parse(&raw).unwrap() else {
            panic!("not interaction")
        };
        let u = it.actor().unwrap();
        assert_eq!(
            (u.id.as_str(), u.username.as_str()),
            ("900000000000000005", "alice")
        );
        assert_eq!(it.nick(), Some("A"));
        assert_eq!(it.option("key").unwrap().value, Some(json!("k")));
        assert_eq!(it.guild_id_str(), "900000000000000001");
    }

    #[test]
    fn dm_interaction_has_no_guild() {
        let raw = frame(
            "INTERACTION_CREATE",
            json!({"id": "900000000000000010", "application_id": "900000000000000003", "type": 2, "token": "t",
                   "user": {"id": "900000000000000005", "username": "alice"}, "data": {"name": "help"}}),
        );
        let Gateway::Interaction(it) = parse(&raw).unwrap() else {
            panic!("not interaction")
        };
        assert_eq!(it.guild_id_str(), "None");
        assert_eq!(it.actor().unwrap().id, "900000000000000005");
    }

    #[test]
    fn control_frames_and_unknown_events_are_other() {
        assert!(matches!(
            parse(r#"{"op":11,"d":null}"#).unwrap(),
            Gateway::Other
        ));
        assert!(matches!(
            parse(&frame("TYPING_START", json!({"x": 1}))).unwrap(),
            Gateway::Other
        ));
        assert!(parse("not json").is_err());
    }

    #[test]
    fn modal_values_walk_label_tree() {
        let comps = vec![
            json!({"type": 18, "id": 1, "component": {"type": 3, "id": 2, "custom_id": "sel-a", "values": ["满意"]}}),
            json!({"type": 18, "id": 3, "component": {"type": 3, "id": 4, "custom_id": "sel-b", "values": []}}),
            json!({"type": 1, "components": [{"type": 3, "custom_id": "sel-c", "values": ["x", "y"]}]}),
        ];
        assert_eq!(
            modal_values(&comps),
            vec![
                ("sel-a".to_string(), vec!["满意".to_string()]),
                ("sel-b".to_string(), vec![]),
                ("sel-c".to_string(), vec!["x".to_string(), "y".to_string()]),
            ]
        );
    }
}
