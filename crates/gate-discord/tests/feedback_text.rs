//! `/反馈` 的流程（docs/v3-deviations.md #52）：点命令立刻弹出一个只有一段文字的弹窗（不先问网关），
//! 提交后把文字交给网关，回执只有本人可见。Discord 请求逐键比较，网关请求逐字段比较。
//!
//! 弹窗提交的负载形状（Label 里包着 TextInput，取值字段是 `value`）依据 Discord 文档的 Modal Submit 结构。

mod common;

use std::sync::Arc;

use common::{APP, BOT_ID, GUILD, MockServer, NOW, SECRET, SITE};
use gate_discord::config::NaimeiConfig;
use gate_discord::events::{GuildInfo, Interaction, User};
use gate_discord::naimei::Naimei;
use serde_json::{Value, json};

const MEMBER: &str = "900000000000000005";

fn naimei(server: &MockServer) -> Arc<Naimei> {
    let cfg = NaimeiConfig {
        token: "test-token".into(),
        guild_id: GUILD.parse().unwrap(),
        backend: server.backend_url(),
        site: SITE.into(),
        secret: SECRET.into(),
    };
    let app = Naimei::with_clock(cfg, Some(server.discord_proxy()), Box::new(|| NOW));
    app.set_bot_user(User {
        id: BOT_ID.into(),
        username: "naimei".into(),
        discriminator: Some("0".into()),
        ..User::default()
    });
    app.upsert_guild(GuildInfo {
        id: GUILD.into(),
        name: Some("测试服".into()),
        unavailable: false,
    });
    app
}

fn member() -> Value {
    json!({"user": {"id": MEMBER, "username": "alice", "global_name": "Alice", "avatar": "a1b2c3",
                    "discriminator": "0"}, "nick": null, "roles": []})
}

/// `/反馈` 斜杠命令。
fn command(n: u32) -> Interaction {
    serde_json::from_value(json!({
        "id": format!("9000000000000002{n:02}"), "application_id": APP, "type": 2, "token": format!("tok-{n}"),
        "guild_id": GUILD, "channel_id": "900000000000000004", "member": member(),
        "data": {"id": "900000000000000050", "name": "反馈", "type": 1, "guild_id": GUILD},
    }))
    .unwrap()
}

/// 弹窗提交：`components` 是 Discord 发来的组件树。
fn submit(n: u32, custom_id: &str, components: Value) -> Interaction {
    serde_json::from_value(json!({
        "id": format!("9000000000000002{n:02}"), "application_id": APP, "type": 5, "token": format!("tok-{n}"),
        "guild_id": GUILD, "channel_id": "900000000000000004", "member": member(),
        "data": {"custom_id": custom_id, "components": components},
    }))
    .unwrap()
}

/// 文字框在 Label 里：`Label(18){ component: TextInput(4){ custom_id, value } }`。
fn text_components(value: &str) -> Value {
    json!([{"type": 18, "id": 1, "component": {"type": 4, "id": 2, "custom_id": "text", "value": value}}])
}

const RECEIPT: &str = "收到啦，谢谢你的反馈～奶妹会转给站长 ✨";

fn modal_expected() -> Value {
    json!({
        "custom_id": "owl-feedback",
        "title": "猫头鹰公益站 · 反馈",
        "components": [{
            "type": 18,
            "label": "想对站长说的话",
            "component": {
                "type": 4,
                "custom_id": "text",
                "style": 2,
                "min_length": 2,
                "max_length": 1000,
                "required": true,
                "placeholder": "遇到的问题、建议都可以写。只有站长能看到，回执只有你自己可见。",
            },
        }],
    })
}

fn bridge_paths(server: &MockServer) -> Vec<String> {
    server
        .bridge_log()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn command_opens_the_modal_at_once_without_asking_the_gateway() {
    let server = MockServer::start().await;
    let app = naimei(&server);
    // 网关什么也不配：要是还去取题目，会收到 500（见 MockServer）并走错误分支
    server.reset(&Value::Null, &Value::Null);
    app.handle_interaction(command(1)).await;

    assert!(bridge_paths(&server).is_empty(), "不该向网关发任何请求");
    let calls = server.discord_log();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["method"], "POST");
    assert_eq!(
        calls[0]["path"],
        "/interactions/900000000000000201/tok-1/callback"
    );
    assert_eq!(calls[0]["query"], json!({"with_response": "true"}));
    // 回调类型 9 = 弹窗；内容逐键等于 handlers::feedback_modal()
    assert_eq!(
        calls[0]["json"],
        json!({"type": 9, "data": modal_expected()})
    );
    assert_eq!(
        calls[0]["json"].to_string(),
        json!({"type": 9, "data": modal_expected()}).to_string(),
        "键顺序也一致"
    );
    assert_eq!(
        gate_discord::handlers::feedback_modal().to_string(),
        modal_expected().to_string()
    );
}

#[tokio::test]
async fn modal_rejected_by_discord_tells_the_member_something_went_wrong() {
    let server = MockServer::start().await;
    let app = naimei(&server);
    server.reset(
        &Value::Null,
        &json!({"callback": {"status": 400, "json": {"message": "Invalid Form Body", "code": 50035}}}),
    );
    app.handle_interaction(command(2)).await;
    let calls = server.discord_log();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0]["json"]["type"], 9);
    assert_eq!(calls[1]["json"]["type"], 4);
    assert_eq!(calls[1]["json"]["data"]["flags"], 64);
    assert_eq!(
        calls[1]["json"]["data"]["content"],
        "出了点问题，已自动记录，请稍后再试；一直不行请在频道里告诉站长。"
    );
    assert!(bridge_paths(&server).is_empty());
}

#[tokio::test]
async fn submit_reads_the_text_and_sends_it_to_the_gateway_with_an_ephemeral_receipt() {
    let server = MockServer::start().await;
    // 全新的实例：弹窗不靠机器人内存里的登记，重启后提交照样认得
    let app = naimei(&server);
    server.reset(
        &json!({"/self-register/feedback": {"status": 200, "json": {"message": RECEIPT}}}),
        &Value::Null,
    );
    let text = "排队有点久，\n希望快一点。";
    app.handle_interaction(submit(3, "owl-feedback", text_components(text)))
        .await;

    let bridge = server.bridge_log();
    assert_eq!(bridge.len(), 1, "{bridge:?}");
    assert_eq!(bridge[0]["method"], "POST");
    assert_eq!(bridge[0]["path"], "/self-register/feedback");
    assert_eq!(bridge[0]["authorization"], format!("Bearer {SECRET}"));
    assert_eq!(bridge[0]["content_type"], "application/json");
    assert_eq!(
        bridge[0]["json"],
        json!({"discord_id": MEMBER, "guild_id": GUILD, "text": text, "username": "alice"})
    );
    assert!(bridge[0]["json"].get("answers").is_none());

    let calls = server.discord_log();
    assert_eq!(calls.len(), 2, "{calls:?}");
    // 先确认（3 秒内），再用只有本人可见的跟进消息回话；不发私信
    assert_eq!(
        calls[0]["path"],
        "/interactions/900000000000000203/tok-3/callback"
    );
    assert_eq!(calls[0]["json"], json!({"type": 6}));
    assert_eq!(calls[1]["path"], format!("/webhooks/{APP}/tok-3"));
    assert_eq!(
        calls[1]["json"],
        json!({"content": RECEIPT, "tts": false, "flags": 64})
    );
}

#[tokio::test]
async fn submit_relays_the_gateways_rejection_verbatim() {
    let server = MockServer::start().await;
    let app = naimei(&server);
    for (n, detail) in [
        (4, "写点什么再提交吧～"),
        (5, "刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。"),
    ] {
        server.reset(
            &json!({"/self-register/feedback": {"status": 400, "json": {"detail": detail}}}),
            &Value::Null,
        );
        app.handle_interaction(submit(n, "owl-feedback", text_components(" 好 ")))
            .await;
        let calls = server.discord_log();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(
            calls[1]["json"],
            json!({"content": detail, "tts": false, "flags": 64})
        );
    }
}

#[tokio::test]
async fn submit_without_a_text_box_sends_an_empty_text_and_lets_the_gateway_decide() {
    let server = MockServer::start().await;
    let app = naimei(&server);
    server.reset(
        &json!({"/self-register/feedback": {"status": 400, "json": {"detail": "写点什么再提交吧～"}}}),
        &Value::Null,
    );
    // 组件树里没有 custom_id = text 的输入框（理论上不会发生）
    app.handle_interaction(submit(6, "owl-feedback", json!([])))
        .await;
    let bridge = server.bridge_log();
    assert_eq!(bridge.len(), 1);
    assert_eq!(bridge[0]["json"]["text"], "");
    assert_eq!(
        server.discord_log()[1]["json"]["content"],
        "写点什么再提交吧～"
    );
}

#[tokio::test]
async fn gateway_down_gives_the_usual_unavailable_receipt() {
    let server = MockServer::start().await;
    let app = naimei(&server);
    server.reset(
        &json!({"/self-register/feedback": {"error": "connect"}}),
        &Value::Null,
    );
    app.handle_interaction(submit(7, "owl-feedback", text_components("你好呀")))
        .await;
    let calls = server.discord_log();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1]["json"],
        json!({"content": "服务暂时不可用，请稍后再试。", "tts": false, "flags": 64})
    );
}

#[tokio::test]
async fn every_modal_submit_is_treated_as_the_feedback_form() {
    // 机器人只有 /反馈 这一个弹窗，不再区分 custom_id：升级前发出去的旧版选择题弹窗提交上来，
    // 取不到文字框的内容，交给网关当空内容拒绝，成员会看到「写点什么再提交吧～」，而不是没有回应
    let server = MockServer::start().await;
    let app = naimei(&server);
    server.reset(
        &json!({"/self-register/feedback": {"status": 400, "json": {"detail": "写点什么再提交吧～"}}}),
        &Value::Null,
    );
    app.handle_interaction(submit(
        8,
        "0123456789abcdef0123456789abcdef",
        json!([{"type": 18, "id": 1, "component": {"type": 3, "id": 2, "custom_id": "sel-a", "values": ["满意"]}}]),
    ))
    .await;
    let bridge = server.bridge_log();
    assert_eq!(bridge.len(), 1, "{bridge:?}");
    assert_eq!(bridge[0]["json"]["text"], "");
    let calls = server.discord_log();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1]["json"],
        json!({"content": "写点什么再提交吧～", "tts": false, "flags": 64})
    );
}
