//! 网关集成测试的公共部分：临时数据目录 + 假上游 + 一把成员 Key。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use gate_db::repo::keys::NewKey;
use gate_server::AppState;
use gate_server::env::EnvConfig;
use gate_server::web::Assets;
use gate_testkit::MockUpstream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

pub const TOKEN: &str = "nai-test-member-key-000000000000000000";
pub const ADMIN_PASSWORD: &str = "dev-admin-password-123";

pub struct Env {
    pub state: AppState,
    pub mock: MockUpstream,
    _dir: tempfile::TempDir,
}

pub async fn setup(extra: &[(&str, &str)]) -> Env {
    let mock = MockUpstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut vars: HashMap<String, String> = [
        ("DATA_DIR", dir.path().to_str().unwrap()),
        ("NAI_TOKENS", "pst-synthetic-upstream-token"),
        ("NAI_IMAGE_HOST", &mock.url()),
        ("POST_REQUEST_JITTER_MIN", "0"),
        ("POST_REQUEST_JITTER_MAX", "0"),
        ("SECRET_KEY", "test-secret-key-test-secret-key-0123456789"),
        ("ADMIN_PASSWORD", ADMIN_PASSWORD),
        ("UPSTREAM_HTTP2", "0"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    let mut env = EnvConfig::load(&vars);
    // 这两项是代码常量（不读环境变量），测试里直接改：没有出图间隔，排队 5 秒就算超时
    env.image_min_interval = 0.0;
    env.queue_timeout = 5;
    let state = AppState::build(env, Assets::new(None)).await.unwrap();
    let key: NewKey = serde_json::from_value(json!({
        "name": "测试成员", "token": TOKEN, "daily_images": 100, "daily_v5": 10,
        "monthly_anlas": 0.0, "daily_text_tokens": 1000, "rpm": 60,
    }))
    .unwrap();
    state.db.create_key(&key, state.now()).await.unwrap();
    Env {
        state,
        mock,
        _dir: dir,
    }
}

pub struct Call<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: Option<Value>,
}

impl<'a> Call<'a> {
    pub fn new(method: &'a str, path: &'a str) -> Self {
        Self {
            method,
            path,
            headers: vec![],
            body: None,
        }
    }
    pub fn bearer(mut self, t: &str) -> Self {
        self.headers.push(("authorization", format!("Bearer {t}")));
        self
    }
    pub fn header(mut self, k: &'a str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    pub fn json(mut self, v: Value) -> Self {
        self.body = Some(v);
        self
    }
    pub async fn send(self, state: &AppState) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut b = Request::builder().method(self.method).uri(self.path);
        for (k, v) in &self.headers {
            b = b.header(*k, v);
        }
        let req = match self.body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&v).unwrap()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = gate_server::app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }
}

/// 后台任务（记账 / 操作日志）写完。
pub async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}
