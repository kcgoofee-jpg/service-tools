//! 桥接契约：一个迷你 axum 假网关，按网关 `/self-register/*` 的真实行为实现
//! （鉴权方式、必填字段、错误码与 `detail` 文案、响应 JSON 形状），用来验证机器人发出的请求
//! 合乎网关的预期、并且能正确解读网关的每一种响应。网关侧的实现（crates/gate-server/src/routes/registration.rs）
//! 照这份契约对接；管理员接口 `ops` / `slots` / `revoke` 已删除（docs/v3-deviations.md #73）。

// 假网关里用 Response 当 Err 方便早退，体积大无所谓
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use gate_discord::bridge::{Bridge, who_payload};
use gate_discord::events::Interaction;
use gate_discord::handlers::{self, Backend};
use serde_json::{Value, json};

const GUILD: &str = "900000000000000001";
const MEMBER: &str = "900000000000000005";
const NO_KEY: &str = "900000000000000006";
const SECRET: &str = "contract-secret";

#[derive(Default)]
struct Seen {
    /// 每个请求收到的 (方法, 路径, 请求体)
    requests: Vec<(String, String, Value)>,
}

type Shared = Arc<Mutex<Seen>>;

fn detail(status: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"detail": msg})),
    )
        .into_response()
}

/// `_service()`：Bearer 密钥不对 → 401「未授权」。
fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {SECRET}"))
}

/// `Who` 的 discord_id / guild_id 必填且为字符串，缺了是 422。
fn who_fields(body: &Value) -> Result<(String, String), Response> {
    let get = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    match (get("discord_id"), get("guild_id")) {
        (Some(d), Some(g)) => Ok((d, g)),
        _ => Err(detail(422, "field required")),
    }
}

/// `checked()`：guild_id 必须是本服务器、discord_id 必须是数字。
fn checked(body: &Value) -> Result<String, Response> {
    let (discord_id, guild_id) = who_fields(body)?;
    if guild_id != GUILD || discord_id.is_empty() || !discord_id.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(detail(403, "请在指定服务器使用该命令。"));
    }
    Ok(discord_id)
}

macro_rules! gate {
    ($shared:expr, $method:expr, $path:expr, $headers:expr, $body:expr) => {{
        $shared.lock().unwrap().requests.push((
            $method.to_owned(),
            $path.to_owned(),
            $body.clone(),
        ));
        if !authorized(&$headers) {
            return detail(401, "未授权");
        }
    }};
}

async fn quota(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/quota", h, body);
    let id = match checked(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if id == NO_KEY {
        return detail(404, "你还没有领取 Key，请先使用 /register。");
    }
    Json(json!({
        "enabled": true, "expires_at": null, "daily_images": 20, "images": 5, "legacy": 4, "anlas": 0,
        "daily_anlas": 0, "daily_v5": 3, "v5": 1, "image_model_scope": "all", "reset_hour": 7,
        "quiet_now": false, "quiet_window": "", "quiet_cap": 20,
        "features": [{"id": "image", "label": "文生图", "on": true}, {"id": "upscale", "label": "放大", "on": false}],
    }))
    .into_response()
}

async fn info(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/info", h, body);
    if let Err(r) = checked(&body) {
        return r;
    }
    Json(json!({
        "open": true, "notice": "测试公告",
        "upstream": {"image_cooldown_seconds": 0},
    }))
    .into_response()
}

async fn resetkey(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/resetkey", h, body);
    let id = match checked(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if id == NO_KEY {
        return detail(404, "你还没有领取 Key，请先使用 /register。");
    }
    Json(json!({"key": "nai-contract-new-key"})).into_response()
}

async fn issue(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/issue", h, body);
    // IssueRequest：discord_id / guild_id 必填，其余字符串字段缺省为空串
    let Some(guild) = body.get("guild_id").and_then(Value::as_str) else {
        return detail(422, "field required");
    };
    if body.get("discord_id").and_then(Value::as_str).is_none() {
        return detail(422, "field required");
    }
    if guild != GUILD {
        return detail(403, "请在指定服务器使用该命令。");
    }
    if body
        .get("invite_key")
        .and_then(Value::as_str)
        .is_some_and(|k| !k.is_empty())
    {
        return Json(json!({"message": "已绑定邀请 Key。"})).into_response();
    }
    Json(json!({"message": "领取成功！你的 Key：nai-contract", "key_id": 7})).into_response()
}

/// `/self-register/feedback`（docs/v3-deviations.md #52）：一段文字，去掉首尾空白后不到 2 个字回 400「写点什么再提交吧～」。
async fn feedback(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/feedback", h, body);
    if let Err(r) = checked(&body) {
        return r;
    }
    let text = body.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().chars().count() < 2 {
        return detail(400, "写点什么再提交吧～");
    }
    Json(json!({"message": "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"})).into_response()
}

/// 心跳。3.0 起只有 `status`（`event` 和 `/self-register/bot/config` 随跑图分享设置一起删了：docs/v3-deviations.md #54）。
async fn bot_report(State(s): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    gate!(s, "POST", "/self-register/bot/report", h, body);
    // BotReport：status 可选
    Json(json!({"ok": true})).into_response()
}

async fn gateway() -> (Bridge, Shared, String) {
    let seen: Shared = Arc::default();
    let app = Router::new()
        .route("/self-register/quota", post(quota))
        .route("/self-register/info", post(info))
        .route("/self-register/resetkey", post(resetkey))
        .route("/self-register/issue", post(issue))
        .route("/self-register/feedback", post(feedback))
        .route("/self-register/bot/report", post(bot_report))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (Bridge::new(url.clone(), SECRET), seen, url)
}

fn interaction(user: &str, guild: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": "900000000000000020", "application_id": "900000000000000003", "type": 2, "token": "tok",
        "guild_id": guild, "member": {"user": {"id": user, "username": "someone"}}, "data": {"name": "x"},
    }))
    .unwrap()
}

async fn call(b: &Bridge, path: &str, user: &str, extra: &[(&str, Value)]) -> Backend {
    let payload = who_payload(&interaction(user, GUILD), extra).unwrap();
    b.command(path, &payload).await.unwrap()
}

#[tokio::test]
async fn member_commands_work_end_to_end() {
    let (bridge, seen, _) = gateway().await;

    let q = call(&bridge, "/self-register/quota", MEMBER, &[]).await;
    assert_eq!(q.status, 200);
    let i = call(&bridge, "/self-register/info", MEMBER, &[]).await;
    let text =
        handlers::quota_text(&q.data, i.is_ok().then_some(&i.data), 1_760_000_000.0).unwrap();
    assert_eq!(
        text,
        "今日 V4.5：4 / 20（每天 7 点重置）\n今日 V5：1 / 3\n可用模型：含 V5\n已开通功能：文生图\n未开通：放大"
    );

    let help = handlers::help_text("https://site.test", i.is_ok().then_some(&i.data)).unwrap();
    assert!(help.ends_with("📢 测试公告"), "{help}");

    let r = call(&bridge, "/self-register/resetkey", MEMBER, &[]).await;
    assert!(
        handlers::resetkey_text(&r.data)
            .unwrap()
            .contains("`nai-contract-new-key`")
    );

    // 没领 Key：网关回 404 + detail，机器人原样转述
    let none = call(&bridge, "/self-register/quota", NO_KEY, &[]).await;
    assert_eq!(
        (none.status, none.text().as_str()),
        (404, "你还没有领取 Key，请先使用 /register。")
    );

    // 每个请求都是 POST + 鉴权 + 两个身份字段（没有管理员相关的 actor_id）
    let seen = seen.lock().unwrap();
    assert!(seen.requests.iter().all(|(m, _, _)| m == "POST"));
    for (_, path, body) in seen.requests.iter() {
        assert_eq!(
            body["discord_id"].as_str().map(|s| !s.is_empty()),
            Some(true),
            "{path}"
        );
        assert_eq!(body["guild_id"], GUILD, "{path}");
        assert!(body.get("actor_id").is_none(), "{path}");
    }
}

#[tokio::test]
async fn wrong_guild_and_bad_secret_are_reported_verbatim() {
    let (bridge, _, url) = gateway().await;
    let payload = who_payload(&interaction(MEMBER, "900000000000000099"), &[]).unwrap();
    let r = bridge
        .command("/self-register/quota", &payload)
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.text().as_str()),
        (403, "请在指定服务器使用该命令。")
    );

    // 密钥不对：401「未授权」（真实网关还会把它计入无效请求限流）
    let wrong = Bridge::new(url, "not-the-secret");
    let payload = who_payload(&interaction(MEMBER, GUILD), &[]).unwrap();
    let r = wrong
        .command("/self-register/quota", &payload)
        .await
        .unwrap();
    assert_eq!((r.status, r.text().as_str()), (401, "未授权"));
}

#[tokio::test]
async fn register_issue_contract() {
    let (bridge, seen, _) = gateway().await;
    let payload = json!({"discord_id": MEMBER, "guild_id": GUILD, "name": "alice", "username": "alice",
                         "global_name": "Alice", "avatar": "a1b2", "invite_key": ""});
    let (status, body) = bridge.issue(&payload).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        handlers::register_message(status, &body).unwrap(),
        "领取成功！你的 Key：nai-contract"
    );

    let with_invite = json!({"discord_id": MEMBER, "guild_id": GUILD, "invite_key": "nai-inv"});
    let (status, body) = bridge.issue(&with_invite).await.unwrap();
    assert_eq!(
        handlers::register_message(status, &body).unwrap(),
        "已绑定邀请 Key。"
    );

    let other_guild = json!({"discord_id": MEMBER, "guild_id": "900000000000000099"});
    let (status, body) = bridge.issue(&other_guild).await.unwrap();
    assert_eq!(status, 403);
    assert_eq!(
        handlers::register_message(status, &body).unwrap(),
        "请在指定服务器使用该命令。"
    );

    // 鉴权头和路径
    let seen = seen.lock().unwrap();
    assert!(
        seen.requests
            .iter()
            .all(|(m, p, _)| m == "POST" && p == "/self-register/issue")
    );
}

#[tokio::test]
async fn feedback_contract() {
    let (bridge, seen, _) = gateway().await;
    // 太短 → 网关 400 + 中文原因，机器人原样转述
    let short = call(
        &bridge,
        "/self-register/feedback",
        MEMBER,
        &[("text", json!(" 好 ")), ("username", json!("alice"))],
    )
    .await;
    assert_eq!(handlers::feedback_receipt(&short), "写点什么再提交吧～");
    let full = call(
        &bridge,
        "/self-register/feedback",
        MEMBER,
        &[
            ("text", json!("排队有点久，希望快一点")),
            ("username", json!("alice")),
        ],
    )
    .await;
    assert_eq!(
        handlers::feedback_receipt(&full),
        "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"
    );

    let seen = seen.lock().unwrap();
    // 不再向网关要题目
    assert!(
        seen.requests
            .iter()
            .all(|(_, p, _)| p == "/self-register/feedback")
    );
    let (_, _, body) = seen
        .requests
        .iter()
        .rfind(|(_, p, _)| p == "/self-register/feedback")
        .unwrap();
    assert_eq!(body["username"], "alice");
    assert_eq!(body["text"], "排队有点久，希望快一点");
    assert!(body.get("answers").is_none());
}

#[tokio::test]
async fn bot_endpoints_contract() {
    let (bridge, seen, _) = gateway().await;
    let status =
        json!({"status": {"user": "naimei", "guild": "测试服", "latency_ms": 42, "ready_at": 1.5}});
    let ack = bridge.bot("/report", &status).await.unwrap();
    assert_eq!(ack, json!({"ok": true}));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 1, "机器人只报心跳，不再拉配置");
    assert_eq!(
        seen.requests[0],
        ("POST".into(), "/self-register/bot/report".into(), status)
    );
}

#[tokio::test]
async fn unreachable_or_hanging_gateway_degrades_to_unavailable() {
    // 连不上（端口没人听）→ (0, 服务暂时不可用)
    let down = Bridge::new("http://127.0.0.1:9", SECRET);
    let payload = who_payload(&interaction(MEMBER, GUILD), &[]).unwrap();
    assert_eq!(
        down.command("/self-register/quota", &payload)
            .await
            .unwrap(),
        Backend::unavailable()
    );
    assert_eq!(down.issue(&json!({})).await, None);
    assert_eq!(down.bot("/report", &json!({})).await, None);

    // 接了连接却一直不回 → 超时后同样降级
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock); // 不读不写，保持打开
        }
    });
    let slow = Bridge::new(url, SECRET)
        .with_timeouts(Duration::from_millis(200), Duration::from_millis(200));
    assert_eq!(
        slow.command("/self-register/quota", &payload)
            .await
            .unwrap(),
        Backend::unavailable()
    );
    assert_eq!(slow.issue(&json!({})).await, None);
    assert_eq!(slow.bot("/report", &json!({})).await, None);
}
