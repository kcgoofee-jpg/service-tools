//! 后台接口测试的共用部分：登录、带会话 Cookie 的请求、本机假 Discord（真实的注册服务把请求发到它）。
#![allow(dead_code)]

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request};
use gate_server::AppState;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const ADMIN_PASSWORD: &str = "dev-admin-password-123";

pub struct Resp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("响应不是 JSON（{e}）：{}", self.text()))
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// 状态码 + `{"detail": 文案}`。
    #[track_caller]
    pub fn assert_detail(&self, status: u16, detail: &str) {
        assert_eq!(self.status, status, "{}", self.text());
        assert_eq!(self.json(), json!({"detail": detail}), "状态码 {status}");
    }
}

/// 已登录的后台会话。
pub struct Admin {
    pub state: AppState,
    pub cookie: String,
}

/// 发一个请求（来源 = 本站，对端 127.0.0.1）。`cookie` 为空表示未登录；`headers` 覆盖默认头。
pub async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Resp {
    let mut hdrs: Vec<(String, String)> = vec![
        ("host".into(), "owl.example".into()),
        ("origin".into(), "https://owl.example".into()),
    ];
    if let Some(c) = cookie {
        hdrs.push(("cookie".into(), format!("nai_gate_admin={c}")));
    }
    for (k, v) in headers {
        hdrs.retain(|(x, _)| x != k);
        hdrs.push(((*k).into(), (*v).into()));
    }
    let bytes = body.map(|b| serde_json::to_vec(&b).unwrap());
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in &hdrs {
        b = b.header(k.as_str(), v.as_str());
    }
    if bytes.is_some() {
        b = b.header("content-type", "application/json");
    }
    if method != "GET" {
        b = b.header(
            "content-length",
            bytes.as_ref().map_or(0, Vec::len).to_string(),
        );
    }
    let mut req = b.body(Body::from(bytes.unwrap_or_default())).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 123))));
    let resp = gate_server::app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

impl Admin {
    pub async fn login(state: &AppState) -> Self {
        let r = send(
            state,
            "POST",
            "/admin/api/login",
            None,
            Some(json!({"password": ADMIN_PASSWORD})),
            &[],
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let set = r.headers["set-cookie"].to_str().unwrap();
        let cookie = set
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("nai_gate_admin=")
            .to_string();
        Self {
            state: state.clone(),
            cookie,
        }
    }
    pub async fn req(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        send(&self.state, method, path, Some(&self.cookie), body, &[]).await
    }
    pub async fn get(&self, path: &str) -> Resp {
        self.req("GET", path, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.req("POST", path, Some(body)).await
    }
    pub async fn put(&self, path: &str, body: Value) -> Resp {
        self.req("PUT", path, Some(body)).await
    }
    pub async fn patch(&self, path: &str, body: Value) -> Resp {
        self.req("PATCH", path, Some(body)).await
    }
    pub async fn delete(&self, path: &str) -> Resp {
        self.req("DELETE", path, None).await
    }
    /// 操作日志（新→旧）里 (操作名, 对象, 详情, 是否成功)。写日志是后台任务，先等一下。
    pub async fn actions(&self) -> Vec<(String, String, String, bool)> {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        crate::common::admin_actions(&self.state.db, 200)
            .await
            .iter()
            .map(|r| {
                (
                    r["action"].as_str().unwrap().to_string(),
                    r["target"].as_str().unwrap().to_string(),
                    r["detail"].as_str().unwrap().to_string(),
                    r["ok"].as_i64() == Some(1),
                )
            })
            .collect()
    }
    /// 执行一串 SQL（造测试数据）。
    pub async fn sql(&self, sql: &str) {
        let sql = sql.to_string();
        self.state
            .db
            .write(move |tx| tx.execute_batch(&sql))
            .await
            .unwrap();
    }
}

// ------------------------------------------------------------------ 本机假 Discord

#[derive(Default)]
struct DState {
    seen: Mutex<Vec<Value>>,
    closed: AtomicBool,
}

/// 记录每个请求的假 Discord REST 服务。`DISCORD_API_BASE` 指向它，真实的 `RegistrationService` 照常发请求。
/// `set_closed(true)` = 对方关了私信：创建私信频道回 403。
pub struct MockDiscord {
    addr: SocketAddr,
    state: Arc<DState>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn discord_handler(
    axum::extract::State(st): axum::extract::State<Arc<DState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (method, path) = (parts.method.to_string(), parts.uri.path().to_string());
    st.seen.lock().unwrap().push(json!([method, path, body]));
    let (status, text) = match (method.as_str(), path.as_str()) {
        ("POST", "/users/@me/channels") if st.closed.load(Ordering::SeqCst) => (403, "{}"),
        ("POST", "/users/@me/channels") => (200, r#"{"id": "dm-1"}"#),
        _ => (200, "{}"),
    };
    let mut r = axum::response::Response::new(Body::from(text));
    *r.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

impl MockDiscord {
    pub async fn start() -> Self {
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
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    /// 到目前为止收到的请求：`[方法, 路径, JSON 体]`。
    pub fn calls(&self) -> Vec<Value> {
        self.state.seen.lock().unwrap().clone()
    }
    pub fn clear(&self) {
        self.state.seen.lock().unwrap().clear();
    }
    pub fn set_closed(&self, closed: bool) {
        self.state.closed.store(closed, Ordering::SeqCst);
    }
}

/// 配齐自助领取（真实注册服务）的环境变量；`base` = 假 Discord 的地址。
pub fn registration_env(base: &str) -> Vec<(String, String)> {
    [
        ("DISCORD_CLIENT_ID", "client-id"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DISCORD_BOT_TOKEN", "fake-bot-token"),
        (
            "REGISTRATION_BRIDGE_SECRET",
            "contract-secret-contract-secret-0001",
        ),
        ("DISCORD_GUILD_ID", "900000000000000001"),
        ("SITE_URL", "https://gate.test"),
        ("DISCORD_API_BASE", base),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}
