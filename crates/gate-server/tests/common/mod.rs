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
        ("KEY_IMAGE_MIN_INTERVAL", "0"),
        ("IMAGE_MIN_INTERVAL", "0"),
        ("POST_REQUEST_JITTER_MIN", "0"),
        ("POST_REQUEST_JITTER_MAX", "0"),
        ("QUEUE_TIMEOUT", "5"),
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
    let env = EnvConfig::load(&vars);
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

/// 领 Key 欢迎语在 3.0 改正了几句和现状不符的话（docs/v3-deviations.md #48）：冻结的旧版原文按同样的规则换掉再比。
#[allow(dead_code)] // 只有部分测试文件用到
pub fn v3_welcome(legacy: &str) -> String {
    legacy
        .replace(
            "天没有使用会被自动回收（回收前 1 天私信提醒）",
            "天没有成功出图会被自动回收，之后不能再领（不另行提醒）",
        )
        .replace(
            "图生图、Vibe 等会消耗 Anlas 的功能不开放",
            "图生图 / 局部重绘不开放；Vibe 在「已开通功能」里时可用，消耗每日 Anlas 额度",
        )
        .replace(
            "一人一把，请勿分享（本站记录打码后的来源网段防分享，不存完整 IP，7 天后删除）",
            "一人一把，请勿分享：共用或转售会被回收",
        )
        .replace(
            "网页登录开放后还能在 https://gate.test/ 用 Discord 一键登录、打包下载自己的作品。",
            "也可以在 https://gate.test/ 用 Discord 登录查看自己的额度。",
        )
}
