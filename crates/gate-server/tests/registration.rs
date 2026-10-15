//! Discord 自助领 Key：机器人桥接 `/self-register/*`、成员网页登录与个人状态、捐赠、原图打包。
//!
//! 真实的 AppState + 路由 + 数据库；Discord REST 换成本机的假服务（`DISCORD_API_BASE`），它记录每个请求，
//! 所以也能断言「哪些请求发出去了、哪些没有」。期望值来自旧 Python（`registration_routes.py` 在同样的场景下
//! 的真实输出），合成数据：Discord ID 一律 9000000000000000xx。

mod common;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use common::{Call, Env, setup};
use gate_server::registration::{DiscordUser, Gift, Issued, RegErr};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const GUILD: &str = "900000000000000001";
const MEMBER: &str = "900000000000000005";
const OTHER: &str = "900000000000000006";
const SECRET: &str = "contract-secret-contract-secret-0001";

// ------------------------------------------------------------------ 假 Discord

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    auth: String,
    reason: String,
    ctype: String,
    body: Bytes,
}

impl Seen {
    fn form(&self) -> Vec<(String, String)> {
        String::from_utf8_lossy(&self.body)
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

struct Scripted {
    method: &'static str,
    path: String,
    status: u16,
    body: String,
    once: bool,
}

#[derive(Default)]
struct DState {
    seen: Mutex<Vec<Seen>>,
    scripts: Mutex<Vec<Scripted>>,
}

struct Discord {
    addr: SocketAddr,
    state: Arc<DState>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

fn reply(status: u16, body: &str) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

async fn discord_handler(State(st): State<Arc<DState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let h = |n: &str| {
        parts
            .headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (method, path) = (parts.method.to_string(), parts.uri.path().to_string());
    st.seen.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        auth: h("authorization"),
        reason: h("x-audit-log-reason"),
        ctype: h("content-type"),
        body: bytes,
    });
    {
        let mut scripts = st.scripts.lock().unwrap();
        if let Some(i) = scripts
            .iter()
            .position(|s| s.method == method && s.path == path)
        {
            let (status, body) = (scripts[i].status, scripts[i].body.clone());
            if scripts[i].once {
                scripts.remove(i);
            }
            return reply(status, &body);
        }
    }
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segs.as_slice()) {
        ("GET", ["guilds", _, "members", _]) => reply(200, r#"{"roles": []}"#),
        ("PUT" | "DELETE", ["guilds", _, "members", _, "roles", _]) => reply(204, ""),
        ("POST", ["users", "@me", "channels"]) => reply(200, r#"{"id": "dm-1"}"#),
        ("POST", ["channels", _, "messages"]) => reply(200, "{}"),
        ("POST", ["oauth2", "token"]) => reply(200, r#"{"access_token": "tok"}"#),
        ("GET", ["users", "@me"]) => reply(
            200,
            &json!({"id": MEMBER, "username": "alice", "global_name": "Alice", "avatar": "a1"})
                .to_string(),
        ),
        ("GET", ["users", id]) => reply(
            200,
            &json!({"id": id, "username": "profile-user", "global_name": "Profile", "avatar": "av1"})
                .to_string(),
        ),
        _ => reply(404, "{}"),
    }
}

impl Discord {
    async fn start() -> Self {
        let state = Arc::new(DState::default());
        let app = axum::Router::new()
            .fallback(discord_handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            addr,
            state,
            _shutdown: tx,
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn script(&self, method: &'static str, path: &str, status: u16, body: Value) {
        self.push(method, path, status, body, false);
    }
    fn push(&self, method: &'static str, path: &str, status: u16, body: Value, once: bool) {
        self.state.scripts.lock().unwrap().push(Scripted {
            method,
            path: path.to_string(),
            status,
            body: body.to_string(),
            once,
        });
    }
    fn clear_scripts(&self) {
        self.state.scripts.lock().unwrap().clear();
    }
    fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }
    fn calls(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect()
    }
}

// ------------------------------------------------------------------ 公共小件

async fn setup_reg(extra: &[(&str, &str)]) -> (Env, Discord) {
    setup_reg_with(extra, true).await
}

/// `legacy_expiry`：旧版夹具都是在 `REGISTER_EXPIRES_DAYS` 默认 30 天下录的；3.0 默认改成 0（不过期，
/// docs/v3-deviations.md #70），对拍这些用例时显式给回 30。
async fn setup_reg_with(extra: &[(&str, &str)], legacy_expiry: bool) -> (Env, Discord) {
    let d = Discord::start().await;
    let base = d.url();
    let mut vars: Vec<(&str, &str)> = vec![
        ("DISCORD_CLIENT_ID", "client-id"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DISCORD_BOT_TOKEN", "fake-bot-token"),
        ("REGISTRATION_BRIDGE_SECRET", SECRET),
        ("DISCORD_GUILD_ID", GUILD),
        ("SITE_URL", "https://gate.test"),
        ("DISCORD_API_BASE", &base),
    ];
    if legacy_expiry {
        vars.push(("REGISTER_EXPIRES_DAYS", "30"));
    }
    vars.extend_from_slice(extra);
    let e = setup(&vars).await;
    (e, d)
}

fn who(id: &str, actor: &str) -> Value {
    json!({"discord_id": id, "guild_id": GUILD, "actor_id": actor})
}

fn merge(mut a: Value, b: Value) -> Value {
    a.as_object_mut()
        .unwrap()
        .extend(b.as_object().unwrap().clone());
    a
}

type Resp = (u16, HeaderMap, Value);

fn parse(r: (StatusCode, HeaderMap, Vec<u8>)) -> Resp {
    let v = serde_json::from_slice(&r.2).unwrap_or(Value::Null);
    (r.0.as_u16(), r.1, v)
}

async fn bridge(e: &Env, path: &str, body: Value) -> Resp {
    parse(
        Call::new("POST", path)
            .bearer(SECRET)
            .json(body)
            .send(&e.state)
            .await,
    )
}

fn detail(r: &Resp) -> &str {
    r.2["detail"].as_str().unwrap_or("")
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

async fn open_registration(e: &Env) {
    e.state.db.set_setting("register_open", "1").await.unwrap();
}

async fn issue(e: &Env, id: &str) -> Resp {
    bridge(
        e,
        "/self-register/issue",
        json!({"discord_id": id, "guild_id": GUILD, "username": "alice", "global_name": "Alice",
               "avatar": "a1b2", "name": "alice"}),
    )
    .await
}

fn svc(e: &Env) -> &gate_server::registration::RegistrationService {
    e.state.registration.as_deref().expect("已配置")
}

fn user(id: &str) -> DiscordUser {
    DiscordUser {
        id: id.into(),
        username: "bob".into(),
        global_name: "Bob".into(),
        avatar: "bb".into(),
    }
}

async fn actions(e: &Env) -> Vec<Value> {
    let mut v = e.state.db.list_admin_actions(200, 0).await.unwrap();
    v.reverse();
    v
}

fn action_tuple(a: &Value) -> (String, String, String, String, i64) {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    (
        s("actor"),
        s("action"),
        s("target"),
        s("detail"),
        a["ok"].as_i64().unwrap_or(-1),
    )
}

// ------------------------------------------------------------------ 桥接：鉴权与请求体校验

#[tokio::test]
async fn bridge_is_503_when_not_configured() {
    let e = setup(&[]).await;
    assert!(e.state.registration.is_none());
    let r = bridge(&e, "/self-register/info", who(MEMBER, MEMBER)).await;
    assert_eq!(r.0, 503);
    assert_eq!(r.2, json!({"detail": "自助领 Key 尚未配置"}));
    // 网页登录同样没配置
    let (s, _, b) = Call::new("GET", "/login").send(&e.state).await;
    assert_eq!(s, 503);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap(),
        json!({"detail": "登录尚未配置"})
    );
}

#[tokio::test]
async fn bridge_checks_body_first_then_secret_then_guild() {
    let (e, d) = setup_reg(&[]).await;
    let good = who(MEMBER, MEMBER);
    // 密钥：没带 / 不对 → 401「未授权」
    let none = parse(
        Call::new("POST", "/self-register/info")
            .json(good.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!((none.0, none.2), (401, json!({"detail": "未授权"})));
    let wrong = parse(
        Call::new("POST", "/self-register/info")
            .bearer("nope")
            .json(good.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(wrong.0, 401);
    // 请求体错误先于鉴权（旧版 FastAPI 先校验请求体）：缺字段的 422 用 pydantic 的形状
    let partial = json!({"discord_id": MEMBER});
    let miss = parse(
        Call::new("POST", "/self-register/quota")
            .bearer("nope")
            .json(partial.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(miss.0, 422);
    assert_eq!(
        miss.2,
        json!({"detail": [{"type": "missing", "loc": ["body", "guild_id"], "msg": "Field required",
                           "input": partial}]})
    );
    let no_body = parse(
        Call::new("POST", "/self-register/quota")
            .bearer(SECRET)
            .send(&e.state)
            .await,
    );
    assert_eq!(
        no_body.2,
        json!({"detail": [{"type": "missing", "loc": ["body"], "msg": "Field required", "input": null}]})
    );
    let list = bridge(&e, "/self-register/quota", json!([1])).await;
    assert_eq!(
        list.2,
        json!({"detail": [{"type": "model_attributes_type", "loc": ["body"],
                           "msg": "Input should be a valid dictionary or object to extract fields from",
                           "input": [1]}]})
    );
    let num = bridge(
        &e,
        "/self-register/quota",
        json!({"discord_id": 5, "guild_id": GUILD}),
    )
    .await;
    assert_eq!(
        num.2,
        json!({"detail": [{"type": "string_type", "loc": ["body", "discord_id"],
                           "msg": "Input should be a valid string", "input": 5}]})
    );
    // 服务器不对 / ID 不是数字 → 403
    let wrong_guild = bridge(
        &e,
        "/self-register/quota",
        json!({"discord_id": MEMBER, "guild_id": "2"}),
    )
    .await;
    assert_eq!(
        (wrong_guild.0, wrong_guild.2),
        (403, json!({"detail": "请在指定服务器使用该命令。"}))
    );
    let bad_id = bridge(
        &e,
        "/self-register/quota",
        json!({"discord_id": "abc", "guild_id": GUILD}),
    )
    .await;
    assert_eq!(bad_id.0, 403);
    // 以上没有任何一个请求去过 Discord
    assert!(d.seen().is_empty());
}

#[tokio::test]
async fn wrong_bridge_secret_counts_toward_auth_block() {
    let (e, _d) = setup_reg(&[("AUTH_FAIL_MAX", "3")]).await;
    let ip: SocketAddr = "203.0.113.9:4000".parse().unwrap();
    for _ in 0..3 {
        let mut req = Request::builder()
            .method("POST")
            .uri("/self-register/info")
            .header("authorization", "Bearer nope")
            .header("content-type", "application/json")
            .body(Body::from(who(MEMBER, MEMBER).to_string()))
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(ip));
        let resp = tower::ServiceExt::oneshot(gate_server::app(e.state.clone()), req)
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
    assert!(e.state.auth_guard.blocked("203.0.113.9", e.state.now()) > 0);
    assert_eq!(e.state.auth_guard.blocked("203.0.113.10", e.state.now()), 0);
}

// ------------------------------------------------------------------ 桥接：info / 封禁

#[tokio::test]
async fn info_has_only_what_the_bot_reads() {
    let (e, _d) = setup_reg(&[("REGISTER_MAX_USERS", "10")]).await;
    let info = bridge(&e, "/self-register/info", who(MEMBER, MEMBER)).await;
    assert_eq!(info.0, 200);
    assert_eq!(info.1["cache-control"], "no-store");
    // 名额、默认额度、默认功能、上游状态机器人都不读，已删除（docs/v3-deviations.md #74）
    assert_eq!(
        info.2,
        json!({"open": false, "notice": "", "upstream": {"image_cooldown_seconds": 0}})
    );
    assert_eq!(keys(&info.2), ["open", "notice", "upstream"]);
    open_registration(&e).await;
    let open = bridge(&e, "/self-register/info", who(MEMBER, MEMBER)).await;
    assert_eq!(open.2["open"], true);
}

#[tokio::test]
async fn ban_revokes_the_key_and_blocks_reissue_until_unban() {
    // 机器人的管理员命令（/ban /unban）已删除（docs/v3-deviations.md #73）：封禁只剩后台和服务方法
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    svc(&e).ban(MEMBER, "", "").await.unwrap();
    assert!(
        e.state
            .db
            .key_row_for_discord(MEMBER)
            .await
            .unwrap()
            .is_none()
    );
    let bans = e.state.db.list_bans("").await.unwrap();
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0]["reason"], "站长手动封禁");
    assert_eq!(bans[0]["banned_by"], "站长");
    assert_eq!(bans[0]["username"], "alice");
    assert_eq!(bans[0]["display_name"], "Alice");
    let again = issue(&e, MEMBER).await;
    assert_eq!(
        (again.0, detail(&again)),
        (403, "这个 Discord 账号已被站长停用，无法领取 Key。")
    );
    assert!(svc(&e).unban(MEMBER).await.unwrap());
    assert!(!svc(&e).unban(MEMBER).await.unwrap());
    assert_eq!(issue(&e, MEMBER).await.0, 200);
}

// ------------------------------------------------------------------ 桥接：发 Key

const WELCOME: &str = "🦉 **欢迎来到猫头鹰公益站！** 这是你的 API Key（只发这一次，请先保存）：\n`<KEY>`\n\n**三步开始出图（以柏宝绘为例）**\n1. 打开柏宝绘 → 渠道 → 配置 → 新建接入点\n2. 接口地址填 `https://gate.test`（**不要**加 /v1）\n3. API Key 填上面 `nai-` 开头的整串，保存后随便生成一张图试试\n其他支持自定义 NovelAI 地址的客户端同样这样填。\n\n**不确定填对没有？** 在 Discord 输入 `/quota`，能看到额度就说明 Key 正常；也可以在 https://gate.test 用 Discord 登录查看自己的额度。\n\n**规则**\n• 每日额度：每人每天最多 V4.5 及以下 30 张，全站共用、先到先得；每天 7 点重置\n• 已开通功能：文生图\n• 连续 3 天没有成功出图会被自动回收，之后不能再领（不另行提醒）\n• 每把 Key 同时生成 1 张、排队 1 张，多发的会被退回，等前一张出完再发\n• 只提供免费出图：V4.5 及以下总像素 ≤1024×1024（宽高取 64 的倍数，超出自动等比缩小）；≤28 步、每次 1 张；图生图 / 局部重绘不开放；Vibe 在「已开通功能」里时可用，消耗每日 Anlas 额度\n• Key 只给本服务器成员：退出服务器后 Key 自动失效\n• 一人一把，请勿分享：共用或转售会被回收\n\n**常用命令**：`/quota` 看今日额度 · `/resetkey` Key 丢了或泄露时换新 · `/help` 简要说明\n遇到问题到 🛠️｜问题反馈 发截图（记得打码 Key）。";

/// 没配 `REGISTER_EXPIRES_DAYS`：新 Key 不过期，欢迎私信里也没有「有效期」那一行（docs/v3-deviations.md #70）
#[tokio::test]
async fn issued_key_never_expires_by_default() {
    let (e, _d) = setup_reg_with(&[], false).await;
    open_registration(&e).await;
    let r = issue(&e, MEMBER).await;
    assert_eq!(r.0, 200, "{:?}", r.2);
    assert!(!r.2["message"].as_str().unwrap().contains("有效期"));
    let row = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    assert!(row["expires_at"].is_null(), "{row:?}");
}

#[tokio::test]
async fn issue_mints_a_key_exactly_like_legacy() {
    let (e, d) = setup_reg(&[]).await;
    // 领 Key 默认关闭
    let closed = issue(&e, MEMBER).await;
    assert_eq!(
        (closed.0, closed.2),
        (
            403,
            json!({"detail": "领 Key 当前已关闭，开放时间看站长公告。"})
        )
    );
    open_registration(&e).await;
    let r = issue(&e, MEMBER).await;
    assert_eq!(r.0, 200, "{:?}", r.2);
    assert_eq!(r.1["cache-control"], "no-store");
    assert_eq!(keys(&r.2), ["key", "message"]);
    let key = r.2["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("nai-") && key.len() == 36, "{key}");
    assert_eq!(
        r.2["message"].as_str().unwrap(),
        WELCOME.replace("<KEY>", &key)
    );
    // 落库
    let row = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row["token"], key.as_str());
    assert_eq!(row["name"], "Alice (@alice)");
    assert_eq!(row["discord_id"], MEMBER);
    assert_eq!(row["enabled"], 1);
    assert_eq!(row["daily_images"], 30);
    assert_eq!(row["daily_v5"], 0);
    assert_eq!(row["rpm"], 5);
    assert_eq!(row["allow_anlas"], 0);
    assert_eq!(row["allow_img2img"], 0);
    assert_eq!(row["exclude_global_v5"], 0);
    assert_eq!(row["image_model_scope"], "legacy");
    assert_eq!(row["features"], "image");
    let left = row["expires_at"].as_f64().unwrap() - e.state.now();
    assert!((left - 30.0 * 86400.0).abs() < 60.0, "{left}");
    let p = e
        .state
        .db
        .discord_profile_row(MEMBER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.0.as_deref(), p.1.as_deref(), p.2.as_deref()),
        (Some("alice"), Some("Alice"), Some("a1b2"))
    );
    // 再领 / 别的服务器 / 密钥不对
    let again = issue(&e, MEMBER).await;
    assert_eq!(
        (again.0, detail(&again)),
        (
            403,
            "这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。"
        )
    );
    let wrong = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": MEMBER, "guild_id": "1"}),
    )
    .await;
    assert_eq!(
        (wrong.0, detail(&wrong)),
        (403, "请在指定服务器使用 /register。")
    );
    let bad_auth = parse(
        Call::new("POST", "/self-register/issue")
            .header("authorization", "x".to_string())
            .json(json!({"discord_id": MEMBER, "guild_id": GUILD}))
            .send(&e.state)
            .await,
    );
    assert_eq!((bad_auth.0, bad_auth.2), (401, json!({"detail": "未授权"})));
    // 日志：成功与失败各记一条（失败带原因）
    let log: Vec<_> = actions(&e).await.iter().map(action_tuple).collect();
    let me = format!("Discord:{MEMBER}");
    assert!(log.contains(&(
        me.clone(),
        "领取 Key 失败（/register）".into(),
        String::new(),
        "领 Key 当前已关闭，开放时间看站长公告。".into(),
        1
    )));
    assert!(log.contains(&(
        me.clone(),
        "领取 Key（/register 直发）".into(),
        String::new(),
        String::new(),
        1
    )));
    // 发 Key 不私信、不碰 Discord（没有身份组门槛 / 成员身份组）
    assert!(d.seen().is_empty(), "{:?}", d.calls());
}

#[tokio::test]
async fn issue_enforces_capacity_age_hourly_cap_and_expiry_rotation() {
    let (e, _d) = setup_reg(&[("REGISTER_MAX_USERS", "1")]).await;
    open_registration(&e).await;
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    // 名额满：先到先得
    let full = issue(&e, OTHER).await;
    assert_eq!(
        (full.0, detail(&full)),
        (
            403,
            "名额已满（上限 1 人），请过段时间再试。闲置的 Key 会被自动回收，空出的名额先到先得。"
        )
    );
    // 撤销释放名额
    assert!(svc(&e).revoke(MEMBER, true).await.unwrap());
    assert!(
        !svc(&e).revoke(MEMBER, true).await.unwrap(),
        "没有 Key 可撤销"
    );
    assert_eq!(issue(&e, OTHER).await.0, 200);
    // Key 过期：释放名额，成员可以重新领
    let row = e
        .state
        .db
        .key_row_for_discord(OTHER)
        .await
        .unwrap()
        .unwrap();
    let mut f = serde_json::Map::new();
    f.insert("expires_at".into(), json!(e.state.now() - 5.0));
    e.state
        .db
        .update_key(row["id"].as_i64().unwrap(), &f)
        .await
        .unwrap();
    assert_eq!(svc(&e).count_active().await.unwrap(), 0);
    let renewed = issue(&e, OTHER).await;
    assert_eq!(renewed.0, 200);
    assert!(
        actions(&e)
            .await
            .iter()
            .map(action_tuple)
            .any(|a| a.1 == "Key 到期释放名额" && a.4 == 1)
    );
    // 被站长停用的成员不会因为过期被放走
    let row = e
        .state
        .db
        .key_row_for_discord(OTHER)
        .await
        .unwrap()
        .unwrap();
    let mut f = serde_json::Map::new();
    f.insert("enabled".into(), json!(0));
    f.insert("expires_at".into(), json!(e.state.now() - 5.0));
    e.state
        .db
        .update_key(row["id"].as_i64().unwrap(), &f)
        .await
        .unwrap();
    let held = issue(&e, OTHER).await;
    assert_eq!(
        (held.0, detail(&held)),
        (
            403,
            "这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。"
        )
    );
    // 每小时领取上限
    let (e2, _d2) = setup_reg(&[("REGISTER_MAX_USERS", "0")]).await;
    open_registration(&e2).await;
    e2.state
        .db
        .set_setting("issue_hourly_cap", "2")
        .await
        .unwrap();
    assert_eq!(issue(&e2, "900000000000000101").await.0, 200);
    assert_eq!(issue(&e2, "900000000000000102").await.0, 200);
    let limited = issue(&e2, "900000000000000103").await;
    assert_eq!(
        (limited.0, detail(&limited)),
        (
            403,
            "本小时领取人数较多，为保护服务稳定已暂时限流，请过几分钟再用 /register 领取。"
        )
    );
    e2.state
        .db
        .set_setting("issue_hourly_cap", "0")
        .await
        .unwrap();
    assert_eq!(issue(&e2, "900000000000000103").await.0, 200);
}

#[tokio::test]
async fn young_discord_accounts_are_refused() {
    let (e, _d) = setup_reg(&[("REGISTER_MIN_ACCOUNT_DAYS", "7")]).await;
    open_registration(&e).await;
    let snowflake = |age_days: f64| {
        let ms = e.state.now() * 1000.0 - age_days * 86_400_000.0;
        (((ms as u64) - 1_420_070_400_000) << 22).to_string()
    };
    let young = issue(&e, &snowflake(1.0)).await;
    assert_eq!(
        (young.0, detail(&young)),
        (403, "Discord 账号创建满 7 天后才能领 Key，请稍后再来。")
    );
    assert_eq!(issue(&e, &snowflake(30.0)).await.0, 200);
}

#[tokio::test]
async fn role_gate_uses_bot_token_against_the_configured_server() {
    let (e, d) = setup_reg(&[("DISCORD_ROLE_ID", "900000000000000077")]).await;
    open_registration(&e).await;
    let path = format!("/guilds/{GUILD}/members/{MEMBER}");
    // 不在服务器
    d.script("GET", &path, 404, json!({"code": 10007}));
    let r = issue(&e, MEMBER).await;
    assert_eq!(
        (r.0, detail(&r)),
        (
            403,
            "目前只开放给「指定身份组」：请先加入对应的社区服务器后再用 /register。"
        )
    );
    // 在服务器但没有身份组
    d.clear_scripts();
    d.script("GET", &path, 200, json!({"roles": ["1"]}));
    let r = issue(&e, MEMBER).await;
    assert_eq!(
        (r.0, detail(&r)),
        (
            403,
            "目前只开放给「指定身份组」，没有检测到这个身份组，暂时不能领取 Key。"
        )
    );
    // 限流 / 故障：不能误报成「你不在服务器」
    d.clear_scripts();
    d.script("GET", &path, 429, json!({"retry_after": 1}));
    let r = issue(&e, MEMBER).await;
    assert_eq!(
        (r.0, detail(&r)),
        (
            403,
            "Discord 暂时无法核验身份组，请过几分钟再用 /register。"
        )
    );
    assert!(
        e.state
            .db
            .key_row_for_discord(MEMBER)
            .await
            .unwrap()
            .is_none()
    );
    d.clear_scripts();
    d.script("GET", &path, 200, json!({"roles": ["900000000000000077"]}));
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    let seen = d.seen();
    assert!(
        seen.iter()
            .all(|s| s.auth == "Bot fake-bot-token" && s.path == path)
    );
    assert_eq!(seen.len(), 4);
}

#[tokio::test]
async fn role_gate_can_target_another_community_via_settings() {
    let (e, d) = setup_reg(&[]).await;
    open_registration(&e).await;
    let other_guild = "900000000000000222";
    for (k, v) in [
        ("register_role_guild", other_guild),
        ("register_role_id", "900000000000000333"),
        ("register_role_note", "创作者"),
    ] {
        e.state.db.set_setting(k, v).await.unwrap();
    }
    let path = format!("/guilds/{other_guild}/members/{MEMBER}");
    d.script("GET", &path, 404, json!({}));
    let r = issue(&e, MEMBER).await;
    assert_eq!(
        (r.0, detail(&r)),
        (
            403,
            "目前只开放给「创作者」：请先加入对应的社区服务器后再用 /register。"
        )
    );
    d.clear_scripts();
    d.script("GET", &path, 200, json!({"roles": ["900000000000000333"]}));
    assert_eq!(issue(&e, MEMBER).await.0, 200);
}

#[tokio::test]
async fn unreachable_discord_never_mints_when_a_role_is_required() {
    // 基地址指向没人监听的端口：既证明请求只会发往 DISCORD_API_BASE，也验证网络错误的处理
    let (e, _d) = setup_reg(&[
        ("DISCORD_ROLE_ID", "900000000000000077"),
        ("DISCORD_API_BASE", "http://127.0.0.1:1"),
    ])
    .await;
    open_registration(&e).await;
    let r = issue(&e, MEMBER).await;
    assert_eq!(
        (r.0, detail(&r)),
        (403, "Discord 暂时连不上，请过几分钟再用 /register。")
    );
    assert!(
        e.state
            .db
            .key_row_for_discord(MEMBER)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn invite_key_binds_without_minting_and_ignores_the_closed_switch() {
    let (e, _d) = setup_reg(&[]).await;
    // 站长发的邀请 Key：带「邀请」标签、没绑定 Discord
    let invite: gate_db::repo::keys::NewKey = serde_json::from_value(json!({
        "name": "邀请-朋友", "token": "nai-invite-xyz", "daily_images": 1,
        "monthly_anlas": 0.0, "daily_text_tokens": 0, "rpm": 5,
    }))
    .unwrap();
    let row = e.state.db.create_key(&invite, e.state.now()).await.unwrap();
    let key_id = row["id"].as_i64().unwrap();
    e.state
        .db
        .write(move |tx| {
            tx.execute(
                "INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?, '邀请', '', '站长', 0)",
                [key_id],
            )
            .map(|_| ())
        })
        .await
        .unwrap();
    assert!(svc(&e).is_unbound_invite(key_id).await.unwrap());
    // 写错的 Key
    let wrong = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": MEMBER, "guild_id": GUILD, "username": "bob", "invite_key": "nai-wrong"}),
    )
    .await;
    assert_eq!(
        (wrong.0, detail(&wrong)),
        (
            403,
            "这不是一把待绑定的邀请 Key。请检查是否复制完整；普通领取请不填这一项。"
        )
    );
    // 领 Key 关着也能绑；回复里没有新 Key
    let ok = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": MEMBER, "guild_id": GUILD, "username": "bob", "global_name": "Bob",
               "invite_key": "nai-invite-xyz"}),
    )
    .await;
    assert_eq!(ok.0, 200);
    assert_eq!(
        ok.2,
        json!({"message": "✅ 邀请 Key 已绑定到你的 Discord 账号，Key 不变，客户端里不用改。用 /quota 查看额度，Key 泄露可用 /resetkey 重置。"})
    );
    assert_eq!(ok.1["cache-control"], "no-store");
    let bound = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound["id"], key_id);
    assert_eq!(bound["name"], "Bob (@bob)");
    assert_eq!(bound["discord_id"], MEMBER);
    assert!(!svc(&e).is_unbound_invite(key_id).await.unwrap());
    // 别人不能再绑同一把；已绑过的人不能再绑
    let thief = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": OTHER, "guild_id": GUILD, "invite_key": "nai-invite-xyz"}),
    )
    .await;
    assert_eq!(thief.0, 403);
    let twice = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": MEMBER, "guild_id": GUILD, "invite_key": "nai-invite-xyz"}),
    )
    .await;
    assert_eq!(
        (twice.0, detail(&twice)),
        (
            403,
            "这个 Discord 账号已经领取过 Key，可用 /quota 查看、/resetkey 重置。"
        )
    );
    assert!(e.state.db.get_key(key_id).await.unwrap().is_some());
    assert!(
        actions(&e)
            .await
            .iter()
            .map(action_tuple)
            .any(|a| a.1 == "绑定邀请 Key（/register）" && a.2 == format!("Key #{key_id}"))
    );
}

#[tokio::test]
async fn gift_issues_even_when_closed_and_is_idempotent() {
    let (e, _d) = setup_reg(&[]).await;
    let u = DiscordUser {
        id: "900000000000000150".into(),
        username: "sese".into(),
        global_name: "涩涩".into(),
        avatar: String::new(),
    };
    let first = svc(&e).gift(&u).await.unwrap();
    let Gift::New { key_id, key } = first else {
        panic!("应当新发");
    };
    assert!(key.starts_with("nai-"));
    assert_eq!(svc(&e).gift(&u).await.unwrap(), Gift::Existing { key_id });
    let bad = svc(&e)
        .gift(&DiscordUser {
            id: "abc".into(),
            ..u.clone()
        })
        .await;
    assert_eq!(bad, Err(RegErr::Denied("Discord ID 不对".into())));
    svc(&e).ban(&u.id, "", "").await.unwrap();
    let banned = svc(&e).gift(&u).await;
    assert_eq!(
        banned,
        Err(RegErr::Denied("这个 Discord 账号已被站长停用".into()))
    );
    // 直发的路径同样认封禁
    assert!(matches!(
        svc(&e).issue_direct(&u.id, GUILD, &u, "").await,
        Err(RegErr::Denied(_))
    ));
}

// ------------------------------------------------------------------ 桥接：quota / resetkey / revoke

#[tokio::test]
async fn quota_and_resetkey_match_legacy() {
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    let r = issue(&e, MEMBER).await;
    let old_key = r.2["key"].as_str().unwrap().to_string();
    let q = bridge(&e, "/self-register/quota", who(MEMBER, MEMBER)).await;
    assert_eq!(q.0, 200);
    assert_eq!(q.1["cache-control"], "no-store");
    assert_eq!(
        keys(&q.2),
        [
            "enabled",
            "expires_at",
            "daily_images",
            "images",
            "legacy",
            "anlas",
            "daily_anlas",
            "daily_v5",
            "v5",
            "image_model_scope",
            "reset_hour",
            "quiet_now",
            "quiet_window",
            "quiet_cap",
            "features"
        ]
    );
    let mut got = q.2.clone();
    assert!(got["expires_at"].as_f64().unwrap() > e.state.now());
    got["expires_at"] = json!("<ts>");
    assert_eq!(
        got,
        json!({"enabled": true, "expires_at": "<ts>", "daily_images": 30, "images": 0, "legacy": 0,
               "anlas": 0, "daily_anlas": 50, "daily_v5": 0, "v5": 0, "image_model_scope": "legacy",
               "reset_hour": 7, "quiet_now": false, "quiet_window": "", "quiet_cap": 20,
               "features": [
                   {"id": "image", "label": "文生图", "on": true},
                   {"id": "upscale", "label": "放大", "on": false},
                   {"id": "augment", "label": "导演工具（线稿、上色等）", "on": false},
                   {"id": "vibe", "label": "Vibe 编码", "on": false},
                   {"id": "tags", "label": "标签补全", "on": false}]})
    );
    // 全站关掉某功能后，即使这把 Key 授权了也显示关
    e.state
        .db
        .set_setting("features_global", r#"{"image": false}"#)
        .await
        .unwrap();
    let q2 = bridge(&e, "/self-register/quota", who(MEMBER, MEMBER)).await;
    assert_eq!(
        q2.2["features"][0],
        json!({"id": "image", "label": "文生图", "on": false})
    );
    // 没领 Key
    let none = bridge(&e, "/self-register/quota", who(OTHER, OTHER)).await;
    assert_eq!(
        (none.0, none.2),
        (
            404,
            json!({"detail": "你还没有领取 Key，请先使用 /register。"})
        )
    );
    // 重置：id 不变、token 换新、旧 token 查不到
    let id = e
        .state
        .db
        .get_key_by_token(&old_key)
        .await
        .unwrap()
        .unwrap()["id"]
        .clone();
    let reset = bridge(&e, "/self-register/resetkey", who(MEMBER, MEMBER)).await;
    assert_eq!(reset.0, 200);
    assert_eq!(reset.1["cache-control"], "no-store");
    let new_key = reset.2["key"].as_str().unwrap();
    assert_ne!(new_key, old_key);
    assert!(
        e.state
            .db
            .get_key_by_token(&old_key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        e.state.db.get_key_by_token(new_key).await.unwrap().unwrap()["id"],
        id
    );
    let none = bridge(&e, "/self-register/resetkey", who(OTHER, OTHER)).await;
    assert_eq!(none.0, 404);
    assert!(
        actions(&e)
            .await
            .iter()
            .map(action_tuple)
            .any(|a| a.1 == "成员重置 Key（/resetkey）" && a.2 == "Key #2 Alice (@alice)")
    );
}

#[tokio::test]
async fn quota_shows_quiet_window_and_member_default_anlas() {
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    issue(&e, MEMBER).await;
    // 安静时段覆盖全天：quiet_start=0, quiet_end=24 不合法，用 0–23 外的组合覆盖当前小时
    let hour = e.state.clock.local_hour(e.state.now());
    let (start, end) = (hour, (hour + 1) % 24);
    e.state
        .guard
        .save(
            json!({"quiet_start": start, "quiet_end": end})
                .as_object()
                .unwrap(),
        )
        .await
        .unwrap();
    let q = bridge(&e, "/self-register/quota", who(MEMBER, MEMBER)).await;
    assert_eq!(q.2["quiet_now"], true);
    assert_eq!(q.2["quiet_window"], format!("{start}:00–{end}:00"));
    // 安静时段上限：默认 20（放慢）；站长设成 0 就是完全暂停，机器人据此选说法（docs/v3-deviations.md #74）
    assert_eq!(q.2["quiet_cap"], 20);
    e.state
        .guard
        .save(json!({"quiet_hourly_cap": 0}).as_object().unwrap())
        .await
        .unwrap();
    let q = bridge(&e, "/self-register/quota", who(MEMBER, MEMBER)).await;
    assert_eq!(q.2["quiet_cap"], 0);
    // Key 手动设置的 Anlas 日额度优先于全站默认
    let row = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    let mut f = serde_json::Map::new();
    f.insert("daily_anlas".into(), json!(12.9));
    e.state
        .db
        .update_key(row["id"].as_i64().unwrap(), &f)
        .await
        .unwrap();
    let q = bridge(&e, "/self-register/quota", who(MEMBER, MEMBER)).await;
    assert_eq!(q.2["daily_anlas"], 12);
}

// ------------------------------------------------------------------ 桥接：反馈 / 机器人配置

#[tokio::test]
async fn feedback_takes_one_text() {
    let (e, _d) = setup_reg(&[]).await;
    // 选择题的「取题目」接口已经去掉（docs/v3-deviations.md #52）
    let gone = bridge(&e, "/self-register/feedback/questions", who(MEMBER, MEMBER)).await;
    assert_eq!(gone.0, 404);
    // 提交也要鉴权
    let anon = parse(
        Call::new("POST", "/self-register/feedback")
            .json(merge(who(MEMBER, MEMBER), json!({"text": "你好呀"})))
            .send(&e.state)
            .await,
    );
    assert_eq!((anon.0, anon.2), (401, json!({"detail": "未授权"})));

    // 没领 Key 也能提交；首尾空白去掉，回执是固定那句
    let ok = bridge(
        &e,
        "/self-register/feedback",
        merge(
            who(MEMBER, MEMBER),
            json!({"username": "alice", "text": "  排队有点久，\n希望快一点。 \n"}),
        ),
    )
    .await;
    assert_eq!(
        (ok.0, ok.2),
        (
            200,
            json!({"message": "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"})
        )
    );
    let rows = e.state.db.list_feedback(10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["discord_id"], MEMBER);
    assert_eq!(rows[0]["username"], "alice");
    assert!(rows[0]["key_id"].is_null(), "没领 Key 也能提交");
    // 存进 `feedback.answers` 的是 {"text": …}（带空格的分隔符，和 Python json.dumps(ensure_ascii=False) 一致）
    assert_eq!(
        rows[0]["answers"].as_str().unwrap(),
        r#"{"text": "排队有点久，\n希望快一点。"}"#
    );

    // 10 分钟内同一个人只收一份
    let again = bridge(
        &e,
        "/self-register/feedback",
        merge(
            who(MEMBER, MEMBER),
            json!({"username": "alice", "text": "再来一条"}),
        ),
    )
    .await;
    assert_eq!(
        (again.0, again.2),
        (
            400,
            json!({"detail": "刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。"})
        )
    );
    assert_eq!(e.state.db.list_feedback(10).await.unwrap().len(), 1);

    // 空的、只有空白、不到 2 个字、没带 text：都不收，也不占冷却
    for body in [
        json!({"username": "bob", "text": ""}),
        json!({"username": "bob", "text": " \n\t　 "}),
        json!({"username": "bob", "text": " 好 "}),
        json!({"username": "bob"}),
    ] {
        let r = bridge(
            &e,
            "/self-register/feedback",
            merge(who(OTHER, OTHER), body),
        )
        .await;
        assert_eq!((r.0, r.2), (400, json!({"detail": "写点什么再提交吧～"})));
    }
    assert_eq!(e.state.db.list_feedback(10).await.unwrap().len(), 1);
    // text 不是字符串：和其它字段一样回 422
    let typed = bridge(
        &e,
        "/self-register/feedback",
        merge(who(OTHER, OTHER), json!({"text": 5})),
    )
    .await;
    assert_eq!(
        typed.2,
        json!({"detail": [{"type": "string_type", "loc": ["body", "text"],
                           "msg": "Input should be a valid string", "input": 5}]})
    );

    // 领了 Key 的人：反馈记上 key_id；超过 1000 字（首尾空白不算）截成 1000 字
    open_registration(&e).await;
    issue(&e, OTHER).await;
    let long = "长".repeat(1500);
    let ok = bridge(
        &e,
        "/self-register/feedback",
        merge(
            who(OTHER, OTHER),
            json!({"username": "bob", "text": format!("  {long}  ")}),
        ),
    )
    .await;
    assert_eq!(ok.0, 200);
    let rows = e.state.db.list_feedback(10).await.unwrap();
    assert_eq!(rows[0]["discord_id"], OTHER);
    assert_eq!(rows[0]["key_id"], 2);
    let stored: Value = serde_json::from_str(rows[0]["answers"].as_str().unwrap()).unwrap();
    assert_eq!(stored["text"].as_str().unwrap().chars().count(), 1000);
    assert_eq!(stored["text"], "长".repeat(1000));
}

/// 冷却按传入的时间算：600 秒内拒绝，满 600 秒再收；被拒绝的不占冷却。
#[tokio::test]
async fn feedback_cooldown_is_ten_minutes_per_person() {
    use gate_server::feedback::{SubmitError, submit};
    let e = setup(&[]).await;
    let db = &e.state.db;
    let t0 = 1_760_000_000.0;
    assert!(
        submit(db, MEMBER, "alice", None, "第一条", t0)
            .await
            .is_ok()
    );
    for dt in [0.0, 1.0, 599.9] {
        let r = submit(db, MEMBER, "alice", None, "又一条", t0 + dt).await;
        assert!(
            matches!(&r, Err(SubmitError::Rejected(m)) if m == "刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。"),
            "dt={dt}: {r:?}"
        );
    }
    // 别人不受影响；太短的先被拦下，也不会写库
    assert!(
        submit(db, OTHER, "bob", None, "好的", t0 + 1.0)
            .await
            .is_ok()
    ); // 刚好 2 个字
    assert!(matches!(
        submit(db, MEMBER, "alice", None, "好", t0 + 700.0).await,
        Err(SubmitError::Rejected(m)) if m == "写点什么再提交吧～"
    ));
    assert!(
        submit(db, MEMBER, "alice", None, "第二条", t0 + 600.0)
            .await
            .is_ok()
    );
    assert_eq!(db.list_feedback(10).await.unwrap().len(), 3);
}

#[tokio::test]
async fn bot_report_keeps_only_the_heartbeat() {
    let (e, _d) = setup_reg(&[]).await;
    // 机器人拉「跑图分享设置」的接口已删除（docs/v3-deviations.md #54）
    let cfg = parse(
        Call::new("GET", "/self-register/bot/config")
            .bearer(SECRET)
            .send(&e.state)
            .await,
    );
    assert_eq!(cfg.0, 404);
    // 心跳：只留白名单字段，加上 seen_at；旧机器人带的 ai_ready / ai_today / 其它字段一律不存
    let report = bridge(
        &e,
        "/self-register/bot/report",
        json!({"status": {"user": "naimei", "guild": "测试服", "latency_ms": 42, "ready_at": 1.5,
                          "ai_ready": false, "ai_today": 0, "extra": 1}}),
    )
    .await;
    assert_eq!((report.0, report.2), (200, json!({"ok": true})));
    let stored: Value =
        serde_json::from_str(&e.state.db.get_setting("bot_status").await.unwrap().unwrap())
            .unwrap();
    assert_eq!(
        keys(&stored),
        ["user", "guild", "latency_ms", "ready_at", "seen_at"]
    );
    assert_eq!(stored["guild"], "测试服");
    // 没有 status 的上报同样 200，不动已存的心跳
    assert_eq!(
        bridge(&e, "/self-register/bot/report", json!({})).await.2,
        json!({"ok": true})
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            &e.state.db.get_setting("bot_status").await.unwrap().unwrap()
        )
        .unwrap(),
        stored
    );
    // 旧机器人还会带 event：忽略，不再写 bot_events
    let ev = bridge(
        &e,
        "/self-register/bot/report",
        json!({"event": {"kind": "like", "title": "## 标题"}}),
    )
    .await;
    assert_eq!((ev.0, ev.2), (200, json!({"ok": true})));
    assert_eq!(e.state.db.get_setting("bot_events").await.unwrap(), None);
    // 鉴权不变：没带密钥 401；status 不是对象 422
    let anon = parse(
        Call::new("POST", "/self-register/bot/report")
            .json(json!({"status": {}}))
            .send(&e.state)
            .await,
    );
    assert_eq!((anon.0, anon.2), (401, json!({"detail": "未授权"})));
    let bad = bridge(&e, "/self-register/bot/report", json!({"status": "x"})).await;
    assert_eq!(
        bad.2,
        json!({"detail": [{"type": "dict_type", "loc": ["body", "status"],
                           "msg": "Input should be a valid dictionary", "input": "x"}]})
    );
}

// ------------------------------------------------------------------ 身份组 / 退群回收 / 资料补全 / 清空

#[tokio::test]
async fn member_role_is_granted_removed_and_retried() {
    let (e, d) = setup_reg(&[("DISCORD_MEMBER_ROLE_ID", "900000000000000055")]).await;
    open_registration(&e).await;
    let role_path = format!("/guilds/{GUILD}/members/{MEMBER}/roles/900000000000000055");
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    let put = d.seen().into_iter().find(|s| s.method == "PUT").unwrap();
    assert_eq!(put.path, role_path);
    assert_eq!(put.auth, "Bot fake-bot-token");
    assert_eq!(put.reason, "NAI Gate key issued");
    let flag = |id: &'static str| {
        let db = e.state.db.clone();
        async move {
            db.read(move |c| {
                c.query_row(
                    "SELECT role_granted FROM discord_registrations WHERE discord_id=?",
                    [id],
                    |r| r.get::<_, i64>(0),
                )
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(flag(MEMBER).await, 1);
    // 过期：同步时摘身份组；再同步是空操作（标志已清）
    let row = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    let mut f = serde_json::Map::new();
    f.insert("expires_at".into(), json!(e.state.now() - 5.0));
    e.state
        .db
        .update_key(row["id"].as_i64().unwrap(), &f)
        .await
        .unwrap();
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 1);
    let del = d
        .seen()
        .into_iter()
        .rfind(|s| s.method == "DELETE")
        .unwrap();
    assert_eq!(
        (del.path.as_str(), del.reason.as_str()),
        (role_path.as_str(), "NAI Gate key ended")
    );
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 0);
    // 撤销时摘除失败 → 登记待办，下次同步重试
    e.state.db.set_setting("register_open", "1").await.unwrap();
    assert_eq!(issue(&e, OTHER).await.0, 200);
    d.script(
        "DELETE",
        &format!("/guilds/{GUILD}/members/{OTHER}/roles/900000000000000055"),
        429,
        json!({}),
    );
    assert!(svc(&e).revoke(OTHER, true).await.unwrap());
    assert_eq!(e.state.db.pending_role_removals().await.unwrap(), [OTHER]);
    d.clear_scripts();
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 1);
    assert!(e.state.db.pending_role_removals().await.unwrap().is_empty());
}

#[tokio::test]
async fn sync_roles_removes_at_most_five_per_run() {
    // 待摘身份组一次堆起很多时（比如全站重置之后），每轮最多摘 5 个，剩下的留到下一轮（docs/v3-deviations.md #78）
    let (e, d) = setup_reg(&[("DISCORD_MEMBER_ROLE_ID", "900000000000000055")]).await;
    let ids: Vec<String> = (0..7).map(|i| format!("90000000000000040{i}")).collect();
    for id in &ids {
        e.state
            .db
            .queue_role_removal(id, e.state.now())
            .await
            .unwrap();
    }
    let deletes = |d: &Discord| {
        d.calls()
            .iter()
            .filter(|c| c.starts_with("DELETE "))
            .count()
    };
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 5);
    assert_eq!(deletes(&d), 5);
    assert_eq!(e.state.db.pending_role_removals().await.unwrap().len(), 2);
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 2);
    assert_eq!(deletes(&d), 7);
    assert!(e.state.db.pending_role_removals().await.unwrap().is_empty());
    assert_eq!(svc(&e).sync_roles().await.unwrap(), 0);
}

#[tokio::test]
async fn failed_role_grant_does_not_block_registration() {
    let (e, d) = setup_reg(&[("DISCORD_MEMBER_ROLE_ID", "900000000000000055")]).await;
    open_registration(&e).await;
    d.script(
        "PUT",
        &format!("/guilds/{GUILD}/members/{MEMBER}/roles/900000000000000055"),
        403,
        json!({}),
    );
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    // 标志先打上，之后过期同步一定会去摘
    let granted: i64 = e
        .state
        .db
        .read(|c| {
            c.query_row("SELECT role_granted FROM discord_registrations", (), |r| {
                r.get::<_, i64>(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(granted, 1);
}

#[tokio::test]
async fn departed_sweep_only_revokes_confirmed_departures() {
    let (e, d) = setup_reg(&[("REGISTER_MAX_USERS", "0")]).await;
    open_registration(&e).await;
    let ids = [
        "900000000000000201",
        "900000000000000202",
        "900000000000000203",
        "900000000000000204",
    ];
    for id in ids {
        assert_eq!(issue(&e, id).await.0, 200);
    }
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{}", ids[0]),
        404,
        json!({"code": 10007, "message": "Unknown Member"}),
    );
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{}", ids[1]),
        404,
        json!({"code": 10004, "message": "Unknown Guild"}),
    );
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{}", ids[2]),
        429,
        json!({"retry_after": 1}),
    );
    let now = e.state.now();
    let mut removed = 0;
    for _ in 0..4 {
        removed += svc(&e).sweep_departed(now, 1).await.unwrap();
    }
    assert_eq!(removed, 1);
    assert!(
        e.state
            .db
            .key_row_for_discord(ids[0])
            .await
            .unwrap()
            .is_none()
    );
    for id in &ids[1..] {
        assert!(e.state.db.key_row_for_discord(id).await.unwrap().is_some());
    }
    let checked: Vec<(String, f64)> = e
        .state
        .db
        .read(|c| {
            let mut s = c.prepare("SELECT discord_id, member_checked_at FROM discord_registrations ORDER BY discord_id")?;
            s.query_map((), |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })
        .await
        .unwrap();
    let at = |id: &str| checked.iter().find(|(i, _)| i == id).unwrap().1;
    assert_eq!(at(ids[3]), now, "在服务器：今天不再查");
    assert_eq!(at(ids[1]), now - 86400.0 + 3600.0, "没查清：1 小时后重查");
    assert_eq!(at(ids[2]), now - 86400.0 + 3600.0);
    let log: Vec<_> = actions(&e).await.iter().map(action_tuple).collect();
    assert!(log.iter().any(|a| a.0 == "系统"
        && a.1 == "退群回收 Key"
        && a.2 == format!("Discord:{} Alice", ids[0])
        && a.3 == "已不在服务器，Key 已删除、名额释放"));
    // 没有私信，也没有别的请求
    assert!(d.calls().iter().all(|c| c.starts_with("GET /guilds/")));
    // 总开关
    d.clear_scripts();
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{}", ids[3]),
        404,
        json!({"code": 10007}),
    );
    e.state.db.set_setting("member_sweep", "0").await.unwrap();
    assert_eq!(
        svc(&e)
            .sweep_departed(now + 3.0 * 86400.0, 5)
            .await
            .unwrap(),
        0
    );
    assert!(
        e.state
            .db
            .key_row_for_discord(ids[3])
            .await
            .unwrap()
            .is_some()
    );
}

/// 邀请 Key 只由站长手动删：持有人退群时不回收，只留一条操作日志提醒（docs/v3-deviations.md #57）。
#[tokio::test]
async fn departed_sweep_keeps_invite_keys_and_logs_a_reminder() {
    let (e, d) = setup_reg(&[("REGISTER_MAX_USERS", "0")]).await;
    open_registration(&e).await;
    let invited = "900000000000000211";
    let regular = "900000000000000212";
    for id in [invited, regular] {
        assert_eq!(issue(&e, id).await.0, 200);
        d.script(
            "GET",
            &format!("/guilds/{GUILD}/members/{id}"),
            404,
            json!({"code": 10007, "message": "Unknown Member"}),
        );
    }
    let key = e
        .state
        .db
        .key_row_for_discord(invited)
        .await
        .unwrap()
        .unwrap();
    let (key_id, key_name) = (
        key["id"].as_i64().unwrap(),
        key["name"].as_str().unwrap().to_string(),
    );
    e.state
        .db
        .run_script(vec![format!(
            "INSERT INTO key_tags (key_id, tag, note, by, ts) VALUES ({key_id}, '邀请', '', '站长', 1.0)"
        )])
        .await
        .unwrap();
    let now = e.state.now();
    let mut removed = 0;
    for _ in 0..3 {
        removed += svc(&e).sweep_departed(now, 5).await.unwrap();
    }
    assert_eq!(removed, 1, "只回收普通成员");
    assert!(
        e.state
            .db
            .key_row_for_discord(regular)
            .await
            .unwrap()
            .is_none()
    );
    let kept = e
        .state
        .db
        .key_row_for_discord(invited)
        .await
        .unwrap()
        .expect("邀请 Key 和领取记录都留着");
    assert_eq!(kept["id"].as_i64(), Some(key_id));
    assert_eq!(kept["enabled"].as_i64(), Some(1));
    assert!(e.state.db.list_bans("").await.unwrap().is_empty());
    // 提醒只有一条（核对时间已记下，同一天的后几轮不再查它）
    let log: Vec<_> = actions(&e).await.iter().map(action_tuple).collect();
    let reminders: Vec<_> = log
        .iter()
        .filter(|a| a.1 == "退群（邀请 Key 保留）")
        .collect();
    assert_eq!(
        reminders,
        vec![&(
            "系统".to_string(),
            "退群（邀请 Key 保留）".to_string(),
            format!("Key #{key_id} {key_name}"),
            format!("Discord:{invited} Alice 已不在服务器；邀请 Key 只由站长手动删除，未回收"),
            1
        )]
    );
    assert!(
        log.iter()
            .any(|a| a.1 == "退群回收 Key" && a.2 == format!("Discord:{regular} Alice"))
    );
    assert!(
        !log.iter()
            .any(|a| a.1 == "退群回收 Key" && a.2.contains(invited))
    );
    let checked: f64 = e
        .state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT member_checked_at FROM discord_registrations WHERE discord_id=?",
                [invited],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        checked,
        now + 30.0 * 86400.0,
        "下次核对推到 30 天后，每月最多提醒一条"
    );
    let gets = d
        .calls()
        .iter()
        .filter(|c| *c == &format!("GET /guilds/{GUILD}/members/{invited}"))
        .count();
    assert_eq!(gets, 1, "同一天只查一次");
}

#[tokio::test]
async fn sweep_treats_unreadable_404_as_unknown() {
    let (e, d) = setup_reg(&[]).await;
    let u = user("900000000000000250");
    svc(&e).gift(&u).await.unwrap();
    let path = format!("/guilds/{GUILD}/members/{}", u.id);
    d.state.scripts.lock().unwrap().push(Scripted {
        method: "GET",
        path,
        status: 404,
        body: "<html>not json</html>".into(),
        once: false,
    });
    assert_eq!(svc(&e).sweep_departed(e.state.now(), 1).await.unwrap(), 0);
    assert!(
        e.state
            .db
            .key_row_for_discord(&u.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn profile_backfill_names_default_keys() {
    let (e, d) = setup_reg(&[]).await;
    let bare = DiscordUser {
        id: "900000000000000260".into(),
        ..Default::default()
    };
    svc(&e).gift(&bare).await.unwrap();
    let before = e
        .state
        .db
        .key_row_for_discord(&bare.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before["name"], "Discord:900000000000000260");
    assert_eq!(svc(&e).backfill_profiles(5).await.unwrap(), 1);
    let after = e
        .state
        .db
        .key_row_for_discord(&bare.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after["name"], "Profile (@profile-user)");
    let p = e
        .state
        .db
        .discord_profile_row(&bare.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.0.as_deref(), Some("profile-user"));
    assert_eq!(
        svc(&e).backfill_profiles(5).await.unwrap(),
        0,
        "已补全的不再查"
    );
    assert_eq!(d.calls(), ["GET /users/900000000000000260"]);
}

#[tokio::test]
async fn background_jobs_do_not_panic_and_report_through_the_service() {
    let (e, d) = setup_reg(&[("DISCORD_MEMBER_ROLE_ID", "900000000000000055")]).await;
    open_registration(&e).await;
    let bare = DiscordUser {
        id: "900000000000000270".into(),
        ..Default::default()
    };
    svc(&e).gift(&bare).await.unwrap();
    gate_server::registration::registrar_jobs(&e.state).await;
    // 资料补全了；退群核对也做了（读成员，在服务器）
    let calls = d.calls();
    assert!(
        calls.contains(&"GET /users/900000000000000270".to_string()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!("GET /guilds/{GUILD}/members/900000000000000270")),
        "{calls:?}"
    );
}

// ------------------------------------------------------------------ 机器人不私信成员

#[tokio::test]
async fn the_gateway_never_opens_a_dm_channel() {
    // 领 Key / 换 Key / 奖励 / 封禁 / 撤销全部走一遍：假 Discord 上不能出现私信相关的请求
    // （docs/v3-deviations.md #76：Discord 应用曾被标记，680009，成员私信出口整个删除）
    let (e, d) = setup_reg(&[]).await;
    open_registration(&e).await;
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    assert_eq!(
        bridge(&e, "/self-register/resetkey", who(MEMBER, MEMBER))
            .await
            .0,
        200
    );
    let u = DiscordUser {
        id: "900000000000000150".into(),
        ..Default::default()
    };
    svc(&e).gift(&u).await.unwrap();
    svc(&e).ban(&u.id, "", "").await.unwrap();
    assert!(svc(&e).revoke(MEMBER, true).await.unwrap());
    let seen = d.seen();
    assert!(
        seen.iter()
            .all(|s| s.path != "/users/@me/channels" && !s.path.ends_with("/messages")),
        "{seen:?}"
    );
}

// ------------------------------------------------------------------ 网页登录

fn set_cookies(h: &HeaderMap) -> Vec<String> {
    h.get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

fn cookie_value(h: &HeaderMap, name: &str) -> String {
    set_cookies(h)
        .iter()
        .find_map(|c| c.strip_prefix(&format!("{name}=")))
        .map(|rest| rest.split(';').next().unwrap().to_string())
        .unwrap_or_default()
}

async fn get_raw(e: &Env, path: &str, cookie: Option<&str>) -> (u16, HeaderMap, Vec<u8>) {
    let mut c = Call::new("GET", path);
    if let Some(v) = cookie {
        c = c.header("cookie", v.to_string());
    }
    let r = c.send(&e.state).await;
    (r.0.as_u16(), r.1, r.2)
}

#[tokio::test]
async fn web_login_is_always_on() {
    let (e, _d) = setup_reg(&[]).await;
    // 暂停开关 web_login_paused 已删除（docs/v3-deviations.md #92）：库里残留的值不再拦登录
    set_db(&e, "web_login_paused", "1").await;
    let (s, h, _) = get_raw(&e, "/login", None).await;
    assert_eq!(s, 307);
    assert!(
        h["location"]
            .to_str()
            .unwrap()
            .starts_with("https://discord.com/oauth2/authorize?")
    );
    assert!(d.seen().is_empty());
}

#[tokio::test]
async fn web_login_flow_matches_legacy() {
    let (e, d) = setup_reg(&[]).await;
    // 起点：跳去 Discord 授权，带 state，并种下 state Cookie
    let (s, h, _) = get_raw(&e, "/login", None).await;
    assert_eq!(s, 307);
    let state = cookie_value(&h, "owl_login_state");
    assert_eq!(state.len(), 32);
    let loc = h["location"].to_str().unwrap().to_string();
    assert_eq!(
        loc,
        format!(
            "https://discord.com/oauth2/authorize?client_id=client-id&redirect_uri=https%3A%2F%2Fgate.test%2Flogin%2Fcallback&response_type=code&scope=identify&state={state}"
        )
    );
    assert_eq!(
        set_cookies(&h),
        [format!(
            "owl_login_state={state}; HttpOnly; Max-Age=600; Path=/; SameSite=lax; Secure"
        )]
    );
    // 回调：缺参数 / state 不对 → 失败
    for path in [
        "/login/callback".to_string(),
        "/login/callback?code=c&state=zzz".to_string(),
        format!("/login/callback?state={state}"),
        "/login/callback?error=access_denied".to_string(),
    ] {
        let (s, h, _) = get_raw(&e, &path, Some(&format!("owl_login_state={state}"))).await;
        assert_eq!(
            (s, h["location"].to_str().unwrap()),
            (307, "/?login=failed"),
            "{path}"
        );
    }
    assert!(d.seen().is_empty(), "校验失败不去换授权码");
    // 成功：换授权码、查身份，种会话 Cookie、清 state Cookie
    let (s, h, _) = get_raw(
        &e,
        &format!("/login/callback?code=abc&state={state}"),
        Some(&format!("owl_login_state={state}")),
    )
    .await;
    assert_eq!((s, h["location"].to_str().unwrap()), (307, "/"));
    let cookies = set_cookies(&h);
    assert_eq!(cookies.len(), 2);
    assert!(
        cookies[0].starts_with("owl_member=")
            && cookies[0].ends_with("; HttpOnly; Max-Age=2592000; Path=/; SameSite=lax; Secure")
    );
    assert_eq!(
        cookies[1],
        "owl_login_state=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=lax"
    );
    let seen = d.seen();
    assert_eq!(d.calls(), ["POST /oauth2/token", "GET /users/@me"]);
    assert!(
        seen[0]
            .ctype
            .starts_with("application/x-www-form-urlencoded")
    );
    let form = seen[0].form();
    for (k, v) in [
        ("client_id", "client-id"),
        ("client_secret", "client-secret"),
        ("grant_type", "authorization_code"),
        ("code", "abc"),
        ("redirect_uri", "https%3A%2F%2Fgate.test%2Flogin%2Fcallback"),
    ] {
        assert!(form.contains(&(k.to_string(), v.to_string())), "{k}");
    }
    assert_eq!(seen[1].auth, "Bearer tok");
    assert!(actions(&e).await.iter().map(action_tuple).any(|a| a
        == (
            format!("Discord:{MEMBER}"),
            "网页登录".into(),
            String::new(),
            String::new(),
            1
        )));
    // 会话 Cookie 能让 /public/me 认出人（没有 Key 时指纹是 "-"）
    let member_cookie = format!("owl_member={}", cookie_value(&h, "owl_member"));
    let me = parse(
        Call::new("GET", "/public/me")
            .header("cookie", member_cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(
        me.2,
        json!({"logged_in": true, "discord": {"id": MEMBER}, "has_key": false})
    );
    assert_eq!(me.1["cache-control"], "no-store");
    // 退出
    let out = Call::new("POST", "/logout").send(&e.state).await;
    assert_eq!(out.0, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&out.2).unwrap(),
        json!({"ok": true})
    );
    assert_eq!(
        set_cookies(&out.1),
        ["owl_member=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=lax"]
    );
}

#[tokio::test]
async fn web_login_rejections() {
    let (e, d) = setup_reg(&[]).await;
    require_role(&e, "900000000000000077").await;
    let ck = "owl_login_state=st";
    // 授权码被 Discord 拒绝
    d.script(
        "POST",
        "/oauth2/token",
        400,
        json!({"error": "invalid_grant"}),
    );
    let (s, h, _) = get_raw(&e, "/login/callback?code=bad&state=st", Some(ck)).await;
    assert_eq!(
        (s, h["location"].to_str().unwrap()),
        (307, "/?login=failed")
    );
    d.clear_scripts();
    // 配了身份组门槛、但账号不在那个服务器 → 不发会话
    d.script(
        "GET",
        &format!("/users/@me/guilds/{GUILD}/member"),
        404,
        json!({"code": 10007}),
    );
    let (s, h, _) = get_raw(&e, "/login/callback?code=ok&state=st", Some(ck)).await;
    assert_eq!(
        (s, h["location"].to_str().unwrap()),
        (307, "/?login=failed")
    );
    assert!(set_cookies(&h).is_empty());
    // 在服务器（不要求带身份组）→ 发会话
    d.clear_scripts();
    d.script(
        "GET",
        &format!("/users/@me/guilds/{GUILD}/member"),
        200,
        json!({"roles": []}),
    );
    let (s, h, _) = get_raw(&e, "/login/callback?code=ok&state=st", Some(ck)).await;
    assert_eq!((s, h["location"].to_str().unwrap()), (307, "/"));
    assert_eq!(set_cookies(&h).len(), 2);
    let me_call = d
        .seen()
        .into_iter()
        .rfind(|s| s.path.ends_with("/member"))
        .unwrap();
    assert_eq!(me_call.auth, "Bearer tok");
}

#[tokio::test]
async fn member_session_dies_when_the_key_is_reset() {
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    issue(&e, MEMBER).await;
    let (_, h, _) = get_raw(
        &e,
        "/login/callback?code=ok&state=st",
        Some("owl_login_state=st"),
    )
    .await;
    let cookie = format!("owl_member={}", cookie_value(&h, "owl_member"));
    let me = |c: &str| {
        let c = c.to_string();
        let st = e.state.clone();
        async move {
            parse(
                Call::new("GET", "/public/me")
                    .header("cookie", c)
                    .send(&st)
                    .await,
            )
        }
    };
    let before = me(&cookie).await;
    assert_eq!(before.2["logged_in"], true);
    assert_eq!(before.2["has_key"], true);
    // /resetkey 换了 Key，旧会话立刻失效
    assert_eq!(
        bridge(&e, "/self-register/resetkey", who(MEMBER, MEMBER))
            .await
            .0,
        200
    );
    assert_eq!(me(&cookie).await.2, json!({"logged_in": false}));
    // 篡改过的 Cookie 也不行
    assert_eq!(
        me("owl_member=900000000000000005:9999999999:-.deadbeef")
            .await
            .2,
        json!({"logged_in": false})
    );
}

// ------------------------------------------------------------------ 个人状态 / 重置券

async fn login_member(e: &Env, id: &str) -> String {
    let exp = e.state.now() as i64 + 3600;
    let key = e
        .state
        .db
        .key_row_for_discord(id)
        .await
        .unwrap()
        .map(gate_server::key::Key);
    let tag = gate_server::session::key_tag(key.as_ref());
    format!(
        "owl_member={}",
        gate_server::session::sign_member(&e.state, id, exp, &tag)
    )
}

#[tokio::test]
async fn public_me_matches_legacy_and_coupons_work() {
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    let anon = parse(Call::new("GET", "/public/me").send(&e.state).await);
    assert_eq!((anon.0, anon.2), (200, json!({"logged_in": false})));
    assert_eq!(anon.1["cache-control"], "no-store");
    issue(&e, MEMBER).await;
    let cookie = login_member(&e, MEMBER).await;
    let me = parse(
        Call::new("GET", "/public/me")
            .header("cookie", cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(me.0, 200);
    assert_eq!(
        keys(&me.2),
        [
            "logged_in",
            "discord",
            "has_key",
            "key",
            "name",
            "expires_at",
            "image_scope",
            "today",
            "queue",
            "coupons",
            "timing"
        ]
    );
    let key = e
        .state
        .db
        .key_row_for_discord(MEMBER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(me.2["key"], key["token"]);
    assert_eq!(me.2["name"], "Alice (@alice)");
    assert_eq!(
        me.2["discord"],
        json!({"id": MEMBER, "username": "alice", "display_name": "Alice", "avatar": "a1b2"})
    );
    assert_eq!(
        me.2["today"],
        json!({"images": 0, "v5": 0, "legacy": 0, "daily_images": 30, "daily_v5": 0, "anlas": 0,
               "daily_anlas": 50, "reset_hour": 7})
    );
    assert_eq!(me.2["queue"], json!([]));
    assert_eq!(me.2["coupons"], json!([]));
    assert_eq!(
        me.2["timing"],
        json!({"count": 0, "avg_dur_s": null, "avg_wait_s": null, "site_avg_dur_s": 9.0})
    );
    // 被停用的 Key：只说没有 Key
    // 券：别人的 / 过期的 / 用过的都不行
    let key_id = key["id"].as_i64().unwrap();
    let c = e
        .state
        .db
        .add_coupon(key_id, "reset", 7.0, "唱得真好听", "奶妹", e.state.now())
        .await
        .unwrap();
    let me = parse(
        Call::new("GET", "/public/me")
            .header("cookie", cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(me.2["coupons"][0]["note"], "唱得真好听");
    assert_eq!(me.2["coupons"][0]["by"], "奶妹");
    let use_path = format!("/public/me/coupons/{}/use", c.id);
    // 先用掉一些额度，确认用券会重置
    e.state
        .db
        .bump_counters(
            key_id,
            &e.state.day(),
            &gate_db::repo::counters::CounterDelta {
                v5: 3,
                anlas: 9.0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let anon = parse(Call::new("POST", &use_path).send(&e.state).await);
    assert_eq!((anon.0, anon.2), (401, json!({"detail": "请先登录"})));
    let used = parse(
        Call::new("POST", &use_path)
            .header("cookie", cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!((used.0, used.2), (200, json!({"ok": true})));
    assert_eq!(used.1["cache-control"], "no-store");
    let counter = e
        .state
        .db
        .get_counter(key_id, &e.state.day())
        .await
        .unwrap();
    assert_eq!(
        (counter.v5, counter.anlas),
        (0, 0.0),
        "用券重置今天的 V5 与 Anlas"
    );
    let again = parse(
        Call::new("POST", &use_path)
            .header("cookie", cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(
        (again.0, again.2),
        (409, json!({"detail": "这张券已经用过或过期了"}))
    );
    let bad = parse(
        Call::new("POST", "/public/me/coupons/abc/use")
            .header("cookie", cookie.clone())
            .send(&e.state)
            .await,
    );
    assert_eq!(
        bad.2,
        json!({"detail": [{"type": "int_parsing", "loc": ["path", "coupon_id"],
                           "msg": "Input should be a valid integer, unable to parse string as an integer",
                           "input": "abc"}]})
    );
    let log: Vec<_> = actions(&e).await.iter().map(action_tuple).collect();
    assert!(log.contains(&(
        format!("Discord:{MEMBER}"),
        "成员使用重置券".into(),
        "Alice (@alice)".into(),
        "券 #1（唱得真好听）".into(),
        1
    )));
    // 没有 Key 的会话不能用券
    let nokey = login_member(&e, OTHER).await;
    let r = parse(
        Call::new("POST", &use_path)
            .header("cookie", nokey)
            .send(&e.state)
            .await,
    );
    assert_eq!((r.0, r.2), (404, json!({"detail": "你还没有可用的 Key"})));
    // Key 被停用后个人页只显示没有 Key
    let mut f = serde_json::Map::new();
    f.insert("enabled".into(), json!(0));
    e.state.db.update_key(key_id, &f).await.unwrap();
    let off = parse(
        Call::new("GET", "/public/me")
            .header("cookie", cookie)
            .send(&e.state)
            .await,
    );
    assert_eq!(
        off.2,
        json!({"logged_in": true, "discord": {"id": MEMBER, "username": "alice",
        "display_name": "Alice", "avatar": "a1b2"}, "has_key": false})
    );
}

// ------------------------------------------------------------------ 捐赠

const DONATE_TOKEN: &str = "pst-synthetic-donation-token-0000000";

#[tokio::test]
async fn donate_validates_limits_and_never_logs_the_token() {
    let (e, _d) = setup_reg(&[]).await;
    let post = |body: Value, origin: Option<&'static str>| {
        let st = e.state.clone();
        async move {
            let mut c = Call::new("POST", "/public/donate").header("host", "gate.test".to_string());
            if let Some(o) = origin {
                c = c.header("origin", o.to_string());
            }
            parse(c.json(body).send(&st).await)
        }
    };
    let ok = post(
        json!({"token": DONATE_TOKEN, "message": "只用 4.5", "contact": "dc:abc"}),
        Some("https://gate.test"),
    )
    .await;
    assert_eq!((ok.0, ok.2.clone()), (200, json!({"ok": true})));
    assert_eq!(ok.1["cache-control"], "no-store");
    let saved = e.state.db.list_donations().await.unwrap();
    assert_eq!(saved.unhandled.len(), 1);
    assert_eq!(saved.unhandled[0]["token"], DONATE_TOKEN);
    assert_eq!(saved.unhandled[0]["message"], "只用 4.5");
    assert_eq!(saved.unhandled[0]["contact"], "dc:abc");
    let log: Vec<_> = actions(&e).await.iter().map(action_tuple).collect();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].1, "收到捐赠 Key");
    assert_eq!(log[0].2, format!("{} 字符", DONATE_TOKEN.chars().count()));
    assert_eq!(log[0].3, "要求：只用 4.5");
    assert!(!log.iter().any(|a| a.0.contains(DONATE_TOKEN)
        || a.2.contains(DONATE_TOKEN)
        || a.3.contains(DONATE_TOKEN)));
    // 校验
    let short = post(json!({"token": "too-short", "message": "x"}), None).await;
    assert_eq!(
        (short.0, short.2),
        (
            422,
            json!({"detail": "Token 看起来不完整：请从 NovelAI 官网把整段 Token 复制过来"})
        )
    );
    let blank = post(json!({"token": DONATE_TOKEN, "message": "  "}), None).await;
    assert_eq!(
        blank.2,
        json!({"detail": "请写一句留言 / 使用要求（500 字以内），站长好接进合适的池子"})
    );
    let long_contact = post(
        json!({"token": DONATE_TOKEN, "message": "x", "contact": "c".repeat(101)}),
        None,
    )
    .await;
    assert_eq!(
        long_contact.2,
        json!({"detail": "联系方式请控制在 100 字以内"})
    );
    let missing = post(json!({"token": DONATE_TOKEN}), None).await;
    assert_eq!(
        missing.2,
        json!({"detail": [{"type": "missing", "loc": ["body", "message"], "msg": "Field required",
                           "input": {"token": DONATE_TOKEN}}]})
    );
    let evil = post(
        json!({"token": DONATE_TOKEN, "message": "x"}),
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(
        (evil.0, evil.2),
        (403, json!({"detail": "只接受本站页面的提交"}))
    );
    // 失败的请求不占次数：已成功 1 次 + 再成功 4 次 = 5 次/小时，第 6 次 429
    for _ in 0..4 {
        assert_eq!(
            post(json!({"token": DONATE_TOKEN, "message": "x"}), None)
                .await
                .0,
            200
        );
    }
    let flood = post(json!({"token": DONATE_TOKEN, "message": "x"}), None).await;
    assert_eq!(
        (flood.0, flood.2),
        (429, json!({"detail": "提交太频繁，请一小时后再试"}))
    );
    assert_eq!(
        e.state.db.list_donations().await.unwrap().unhandled.len(),
        5
    );
}

#[tokio::test]
async fn donate_limit_is_per_source_ip() {
    let (e, _d) = setup_reg(&[]).await;
    let send = |ip: &'static str| {
        let st = e.state.clone();
        async move {
            let mut req = Request::builder()
                .method("POST")
                .uri("/public/donate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"token": DONATE_TOKEN, "message": "x"}).to_string(),
                ))
                .unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(
                format!("{ip}:1").parse::<SocketAddr>().unwrap(),
            ));
            tower::ServiceExt::oneshot(gate_server::app(st), req)
                .await
                .unwrap()
                .status()
        }
    };
    for _ in 0..5 {
        assert_eq!(send("203.0.113.1").await, 200);
    }
    assert_eq!(send("203.0.113.1").await, 429);
    assert_eq!(send("203.0.113.2").await, 200, "换一个来源地址不受影响");
}

// ------------------------------------------------------------------ 没有请求会发到真实 Discord

#[tokio::test]
async fn discord_base_comes_from_the_environment() {
    // 默认值是官方 v10；测试环境一律通过 DISCORD_API_BASE 换成本机假服务
    let plain =
        gate_server::env::EnvConfig::load(&std::collections::HashMap::<String, String>::new());
    assert_eq!(plain.discord_api_base, "https://discord.com/api/v10");
    // 跑一遍会碰 Discord 的流程：每个请求都落在假服务上
    let (e, d) = setup_reg(&[
        ("DISCORD_ROLE_ID", "900000000000000077"),
        ("DISCORD_MEMBER_ROLE_ID", "900000000000000055"),
    ])
    .await;
    open_registration(&e).await;
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{MEMBER}"),
        200,
        json!({"roles": ["900000000000000077"]}),
    );
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    assert!(svc(&e).revoke(MEMBER, true).await.unwrap());
    let calls = d.calls();
    assert!(calls.len() >= 3, "{calls:?}");
    assert!(d.seen().iter().all(|s| s.auth == "Bot fake-bot-token"));
}

#[tokio::test]
async fn issue_result_enum_is_provisioned_for_plain_requests() {
    let (e, _d) = setup_reg(&[]).await;
    open_registration(&e).await;
    let r = svc(&e)
        .issue_direct(MEMBER, GUILD, &user(MEMBER), "")
        .await
        .unwrap();
    assert!(matches!(r, Issued::Provisioned(_)));
    // 非数字 ID 直接拒绝，不查库
    let bad = svc(&e).issue_direct("12a", GUILD, &user("12a"), "").await;
    assert_eq!(
        bad,
        Err(RegErr::Denied("请在指定服务器使用 /register。".into()))
    );
}

// ------------------------------------------------------------------ 成员名 / 打包：对拍旧版 golden

/// 旧 Python 生成的夹具（脚本见标签 py-final）：成员名、zip / epub 的条目与文本内容（欢迎文案不再对拍，见 docs/v3-deviations.md #75）。
fn golden() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/registration_text.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn member_labels_match_legacy_golden() {
    // 欢迎语不再和旧版对拍（规则改了好几处，docs/v3-deviations.md #75）；成员名的写法没变，仍然对拍
    let g = golden();
    for c in g["labels"].as_array().unwrap() {
        let u = &c["user"];
        let s = |k: &str| u.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let user = DiscordUser {
            id: s("id"),
            username: s("username"),
            global_name: s("global_name"),
            avatar: String::new(),
        };
        assert_eq!(
            gate_server::registration::member_label(&user),
            c["out"].as_str().unwrap(),
            "{u}"
        );
    }
}

fn archive(bytes: Vec<u8>) -> (Vec<String>, serde_json::Map<String, Value>) {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let (mut names, mut files) = (Vec::new(), serde_json::Map::new());
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let name = f.name().unwrap().into_owned();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut data).unwrap();
        let v = if [".png", ".jpg", ".webp"].iter().any(|e| name.ends_with(e)) {
            json!(data.len())
        } else {
            json!(String::from_utf8(data).unwrap())
        };
        names.push(name.clone());
        files.insert(name, v);
    }
    (names, files)
}

#[test]
fn export_archives_match_legacy_golden() {
    let g = golden();
    let ex = &g["export"];
    let rows: Vec<gate_db::repo::audit::AuditImage> = ex["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| gate_db::repo::audit::AuditImage {
            id: r["id"].as_i64().unwrap(),
            ts: r["ts"].as_f64().unwrap(),
            image_type: r["type"].as_str().unwrap().into(),
            prompt: r["prompt"].as_str().unwrap().into(),
            negative: r["negative"].as_str().unwrap().into(),
            extra: r["extra"].as_str().unwrap().into(),
            image: r["image"].as_str().unwrap().as_bytes().to_vec(),
        })
        .collect();
    let clock = gate_core::clock::Clock::new("Asia/Shanghai", 7);
    let (now, who) = (ex["now"].as_f64().unwrap(), ex["who"].as_str().unwrap());
    // 夹具里还有旧版的 epub 条目：成员自助导出（EPUB）3.0 删了（docs/v3-deviations.md #49），只对拍后台导出用的 zip
    let (names, files) = archive(gate_server::exporter::build_zip(&rows, who, &clock, now));
    let want = &ex["zip"];
    let want_names: Vec<String> = want["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, want_names, "zip 条目顺序");
    for (name, content) in want["files"].as_object().unwrap() {
        assert_eq!(&files[name], content, "zip: {name}");
    }
}

// ------------------------------------------------------------------ 设置：名额 × V5 提示、校验、欢迎文案变体、记录开关（对拍旧版 ops.py）

fn obj(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test]
async fn registration_settings_ops_match_legacy() {
    let (e, _d) = setup_reg(&[]).await;
    e.state.db.set_setting("register_open", "1").await.unwrap();
    e.state
        .db
        .set_setting("register_image_scope", "all")
        .await
        .unwrap();
    e.state
        .db
        .set_setting("register_daily_v5", "3")
        .await
        .unwrap();
    // 名额 × 每人 V5 超过全站 V5 日限（150）：不再提醒（`v5_capacity` 已删除，docs/v3-deviations.md #69；
    // 机器人的 /limit 命令也已删除，#73），名额照常保存
    for max_users in [60, 50] {
        gate_server::ops::set_registration(
            &e.state,
            &obj(json!({"max_users": max_users, "notify": false})),
        )
        .await
        .unwrap();
        assert_eq!(
            e.state
                .db
                .get_setting("register_max_users")
                .await
                .unwrap()
                .as_deref(),
            Some(max_users.to_string().as_str())
        );
    }
    // 校验（旧版抛 ValueError，消息原样）
    let set = |b: Value| {
        let st = e.state.clone();
        async move { gate_server::ops::set_registration(&st, &obj(b)).await }
    };
    let invalid = |r: Result<(), gate_server::ops::OpsError>| match r {
        Err(gate_server::ops::OpsError::Invalid(m)) => m,
        other => panic!("应当是校验失败：{other:?}"),
    };
    assert_eq!(
        invalid(set(json!({"max_users": -1})).await),
        "max_users 超出范围 0～1000"
    );
    assert_eq!(
        invalid(set(json!({"role_id": "abc"})).await),
        "role_id 需要是 Discord 的数字 ID"
    );
    assert_eq!(
        invalid(set(json!({"role_guild": "900000000000000222"})).await),
        "限定身份组的服务器 ID 与身份组 ID 必须同时填写，或全部留空"
    );
    assert_eq!(
        invalid(set(json!({"expires_days": 99999})).await),
        "expires_days 超出范围 0～3650"
    );
    // 动态额度开着（默认）：新 Key 的张数 / V5 / 模型范围归算法管，不能手改（docs/v3-deviations.md #70）
    match set(json!({"image_scope": "all", "daily_images": 5})).await {
        Err(gate_server::ops::OpsError::Managed(m)) => assert_eq!(
            m,
            "daily_images、image_scope 由动态额度管理：关闭动态额度后才能手动修改"
        ),
        other => panic!("应当是动态额度管着：{other:?}"),
    }
    e.state
        .db
        .set_setting("quota_auto_enabled", "0")
        .await
        .unwrap();
    set(json!({"image_scope": "x"})).await.unwrap();
    assert_eq!(
        e.state
            .db
            .get_setting("register_image_scope")
            .await
            .unwrap()
            .as_deref(),
        Some("legacy"),
        "不认识的范围一律存成 legacy"
    );
    // 全部已开放功能 + 不过期 + 100 张
    set(json!({"features": null, "daily_images": 100, "expires_days": 0}))
        .await
        .unwrap();
    e.state
        .db
        .set_setting("register_image_scope", "legacy")
        .await
        .unwrap();
    e.state
        .db
        .set_setting("register_daily_v5", "0")
        .await
        .unwrap();
    // 记录开关：提示词记录 5 天
    let audit = gate_server::ops::set_audit(
        &e.state,
        &obj(json!({"prompts": true, "thumbs": false, "retention_days": 5})),
    )
    .await
    .unwrap();
    assert_eq!(
        audit,
        json!({"prompts": true, "thumbs": false, "retention_days": 5,
               "image_cap_mb": 8192, "image_keep_hours": 24})
    );
    // 欢迎文案：每人最多 100 张、全部功能、没有有效期这一行、带记录声明
    let r = bridge(
        &e,
        "/self-register/issue",
        json!({"discord_id": "900000000000000401", "guild_id": GUILD, "username": "neo"}),
    )
    .await;
    assert_eq!(r.0, 200, "{:?}", r.2);
    let key = r.2["key"].as_str().unwrap();
    let message = r.2["message"].as_str().unwrap();
    assert!(message.contains(
        "• 每日额度：每人每天最多 V4.5 及以下 100 张，全站共用、先到先得；每天 7 点重置\n"
    ));
    assert!(message.contains("• 已开通功能：全部已开放功能\n"));
    assert!(message.contains(&format!("`{key}`")));
    assert!(
        !message.contains("有效期") && !message.contains("保底"),
        "{message}"
    );
    assert!(
        message.ends_with(
            "\n为防止滥用和优化调度，本站会保留你的图片提示词（5 天后自动删除），仅站长可见。"
        ),
        "{message}"
    );
    let row = e
        .state
        .db
        .key_row_for_discord("900000000000000401")
        .await
        .unwrap()
        .unwrap();
    assert!(row["features"].is_null(), "全部已开放功能 = NULL");
    assert!(row["expires_at"].is_null(), "expires_days=0 = 不过期");
    // 生效的设置
    assert_eq!(
        Value::Object(svc(&e).settings().await.unwrap().to_map()),
        json!({"configured": true, "open": true, "max_users": 50, "features": null, "daily_images": 100,
               "daily_v5": 0, "image_scope": "legacy", "expires_days": 0, "role_guild": "",
               "role_id": "", "role_note": ""})
    );
    // 数值被夹在范围里
    let audit = gate_server::ops::set_audit(
        &e.state,
        &obj(json!({"retention_days": "9999", "image_cap_mb": 7, "image_keep_hours": 99999})),
    )
    .await
    .unwrap();
    assert_eq!(
        audit,
        json!({"prompts": true, "thumbs": false, "retention_days": 3650,
               "image_cap_mb": 7, "image_keep_hours": 8760})
    );
}

#[tokio::test]
async fn trailing_slash_in_api_base_is_tolerated() {
    let d = Discord::start().await;
    let base = format!("{}/", d.url());
    let e = setup(&[
        ("DISCORD_CLIENT_ID", "client-id"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DISCORD_BOT_TOKEN", "fake-bot-token"),
        ("REGISTRATION_BRIDGE_SECRET", SECRET),
        ("DISCORD_GUILD_ID", GUILD),
        ("SITE_URL", "https://gate.test"),
        ("DISCORD_API_BASE", &base),
        ("DISCORD_ROLE_ID", "900000000000000077"),
    ])
    .await;
    open_registration(&e).await;
    d.script(
        "GET",
        &format!("/guilds/{GUILD}/members/{MEMBER}"),
        200,
        json!({"roles": ["900000000000000077"]}),
    );
    assert_eq!(issue(&e, MEMBER).await.0, 200);
    assert_eq!(d.calls(), [format!("GET /guilds/{GUILD}/members/{MEMBER}")]);
}
