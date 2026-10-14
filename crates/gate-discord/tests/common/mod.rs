//! 测试用的假服务器：同一个端口同时扮演「网关桥接」（`/self-register/*`）与「Discord REST」（`/api/v10/*`），
//! 按场景脚本回话，并逐条记录收到的请求。测试里不连任何真实的 Discord。
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::{Map, Value, json};

/// 合成数据（与 tools/parity/golden/discord_commands.py 保持一致）。
pub const GUILD: &str = "900000000000000001";
pub const OWNER: &str = "900000000000000002";
pub const APP: &str = "900000000000000003";
pub const BOT_ID: &str = "900000000000000099";
pub const SECRET: &str = "test-secret";
pub const SITE: &str = "https://site.test";
/// 夹具里固定的 `time.time()`。
pub const NOW: f64 = 1_760_000_000.0;

#[derive(Default)]
struct Script {
    /// 路径 → 响应规格：`{"status", "json"}` / `{"status", "text"}` / `{"error": ...}`；`"*"` 为兜底。
    bridge: Map<String, Value>,
    /// `callback` / `followup` / `message` → `{"status", "json"}`：让对应的 Discord 请求失败。
    discord: Map<String, Value>,
}

#[derive(Default)]
struct Log {
    bridge: Vec<Value>,
    discord: Vec<Value>,
}

#[derive(Default)]
struct Shared {
    script: Mutex<Script>,
    log: Mutex<Log>,
}

pub struct MockServer {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

impl MockServer {
    pub async fn start() -> MockServer {
        let shared = Arc::new(Shared::default());
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        MockServer { addr, shared }
    }

    /// 网关地址（`REGISTRATION_BACKEND_URL`）。
    pub fn backend_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 传给 `Naimei::new` 的 Discord REST 代理地址。
    pub fn discord_proxy(&self) -> String {
        self.addr.to_string()
    }

    /// 换一套脚本并清空记录。
    pub fn reset(&self, bridge: &Value, discord: &Value) {
        *self.shared.script.lock().unwrap() = Script {
            bridge: bridge.as_object().cloned().unwrap_or_default(),
            discord: discord.as_object().cloned().unwrap_or_default(),
        };
        *self.shared.log.lock().unwrap() = Log::default();
    }

    pub fn bridge_log(&self) -> Vec<Value> {
        self.shared.log.lock().unwrap().bridge.clone()
    }

    pub fn discord_log(&self) -> Vec<Value> {
        self.shared.log.lock().unwrap().discord.clone()
    }
}

fn query_map(query: Option<&str>) -> Value {
    let mut out = Map::new();
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    Value::Object(out)
}

fn json_response(status: u16, body: &Value) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

/// 传输层失败：先发状态行与响应头，再让响应体报错（客户端 `bytes()` / `json()` 失败）。
fn broken_response() -> Response {
    let stream = futures::stream::iter([Err::<Bytes, std::io::Error>(std::io::Error::other(
        "synthetic",
    ))]);
    let mut r = Response::new(Body::from_stream(stream));
    r.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("100"));
    r
}

fn scripted(spec: &Value) -> Response {
    if spec.get("error").is_some() {
        return broken_response();
    }
    let status = spec["status"].as_u64().unwrap_or(200) as u16;
    if let Some(body) = spec.get("json") {
        json_response(status, body)
    } else {
        let mut r = Response::new(Body::from(spec["text"].as_str().unwrap_or("").to_owned()));
        *r.status_mut() = StatusCode::from_u16(status).unwrap();
        r
    }
}

async fn handle(State(shared): State<Arc<Shared>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let body_json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    let path = parts.uri.path().to_owned();
    let method = parts.method.as_str().to_owned();

    if path.starts_with("/self-register/") {
        let auth = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map_or(Value::Null, |s| Value::String(s.to_owned()));
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map_or(Value::Null, |s| Value::String(s.to_owned()));
        let mut log = shared.log.lock().unwrap();
        log.bridge.push(json!({
            "method": method, "path": path, "authorization": auth, "content_type": content_type, "json": body_json,
        }));
        drop(log);
        let script = shared.script.lock().unwrap();
        return match script.bridge.get(&path).or_else(|| script.bridge.get("*")) {
            Some(spec) => scripted(spec),
            None => json_response(500, &json!({"detail": "mock: 场景没有给这个路径配置响应"})),
        };
    }

    if let Some(rest) = path.strip_prefix("/api/v10") {
        shared.log.lock().unwrap().discord.push(json!({
            "method": method, "path": rest, "query": query_map(parts.uri.query()), "json": body_json,
            "authorization": parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()),
        }));
        let kind = if rest.ends_with("/callback") {
            "callback"
        } else if rest.starts_with("/webhooks/") {
            "followup"
        } else if rest.starts_with("/channels/") {
            "message"
        } else {
            ""
        };
        let script = shared.script.lock().unwrap();
        if let Some(spec) = script.discord.get(kind) {
            return scripted(spec);
        }
        return json_response(
            200,
            &if method == "PUT" {
                json!([])
            } else {
                json!({})
            },
        );
    }

    json_response(404, &json!({"detail": "mock: unknown path"}))
}

/// 把一组 JSON 里的字符串按映射表整体替换（弹窗 custom_id ↔ 占位符）。
pub fn deep_replace(v: &mut Value, map: &HashMap<String, String>) {
    match v {
        Value::String(s) => {
            if let Some(r) = map.get(s.as_str()) {
                *s = r.clone();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| deep_replace(x, map)),
        Value::Object(o) => o.values_mut().for_each(|x| deep_replace(x, map)),
        _ => {}
    }
}

/// 从一批 Discord 请求里找出弹窗（回调类型 9），返回「真实 custom_id → 占位符」
/// （弹窗本身是 `<modal>`，第 i 个下拉框是 `<sel{i}>`）。
pub fn modal_placeholders(calls: &[Value]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for call in calls {
        let body = &call["json"];
        if body["type"] == 9 {
            let modal = &body["data"];
            out.insert(
                modal["custom_id"].as_str().unwrap().to_owned(),
                "<modal>".to_owned(),
            );
            for (i, c) in modal["components"].as_array().unwrap().iter().enumerate() {
                out.insert(
                    c["component"]["custom_id"].as_str().unwrap().to_owned(),
                    format!("<sel{i}>"),
                );
            }
        }
    }
    out
}
