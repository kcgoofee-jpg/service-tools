//! 文本生成 / 流式 / 语音 / OpenAI 兼容聊天路由的端到端测试：真实的 AppState + 路由 + 记账，上游换成本地假服务。
//!
//! 期望值来自旧 Python 网关：同样的请求跑过 `app.main`（假上游是同样会吐 `word0 word1 …` 的脚本化 mock），
//! 状态码、响应头、响应体字节、写进库的用量行都逐条抄下来。假上游的 JSON 是紧凑写法（`{"token":" word0 "}`），
//! 旧 mock 是 `json.dumps` 的带空格写法——网关对原生接口原样转发，所以这里期望的是紧凑写法。

mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use common::{Call, Env, TOKEN, settle, setup};
use gate_db::repo::keys::NewKey;
use gate_db::repo::usage::LogFilter;
use gate_testkit::Reply;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

const ADMIN: &str = "nai-test-admin-key-0000000000000000000";
const LIMITED: &str = "nai-test-limited-key-00000000000000000";

const EXTRA_FEATURES: &str = "站长暂未开放此功能：文本生成 / OpenAI 兼容聊天";

type Reply3 = (StatusCode, axum::http::HeaderMap, Vec<u8>);

async fn call_post(e: &Env, path: &str, token: &str, body: Value) -> Reply3 {
    Call::new("POST", path)
        .bearer(token)
        .json(body)
        .send(&e.state)
        .await
}

async fn call_post_anon(e: &Env, path: &str, body: Value) -> Reply3 {
    Call::new("POST", path).json(body).send(&e.state).await
}

async fn call_get(e: &Env, path: &str, token: &str) -> Reply3 {
    Call::new("GET", path).bearer(token).send(&e.state).await
}

async fn call_get_anon(e: &Env, path: &str) -> Reply3 {
    Call::new("GET", path).send(&e.state).await
}

async fn add_key(e: &Env, name: &str, token: &str, extra: Value) {
    let mut v = json!({
        "name": name, "token": token, "daily_images": 100, "daily_v5": 10, "monthly_anlas": 0.0,
        "daily_text_tokens": 1000, "rpm": 1000,
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    let key: NewKey = serde_json::from_value(v).unwrap();
    e.state.db.create_key(&key, e.state.now()).await.unwrap();
}

/// 站长 Key（不限额、可用语音、任意模型）。
async fn add_admin(e: &Env) {
    add_key(e, "站长", ADMIN, json!({"is_admin": true})).await;
}

async fn add_limited(e: &Env, token: &str, name: &str, daily_text_tokens: i64) {
    add_key(
        e,
        name,
        token,
        json!({"daily_text_tokens": daily_text_tokens}),
    )
    .await;
}

fn native(max_length: i64) -> Value {
    json!({"model": "llama-3-erato-v1", "input": "once upon", "parameters": {"max_length": max_length}})
}

fn native_with(input: &str) -> Value {
    json!({"model": "llama-3-erato-v1", "input": input, "parameters": {"max_length": 5}})
}

fn chat_body(extra: Value) -> Value {
    let mut v = json!({"messages": [{"role": "user", "content": "你好"}]});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn chat_with(content: &str, extra: Value) -> Value {
    let mut v = chat_body(extra);
    v["messages"][0]["content"] = json!(content);
    v
}

fn text(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).unwrap()
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| panic!("不是 JSON：{}", text(body)))
}

fn ct(headers: &axum::http::HeaderMap) -> &str {
    headers["content-type"].to_str().unwrap()
}

/// 用量日志，按时间从旧到新，只留断言用得到的列：(kind, model, status, tokens, detail, reason, up_status)。
type Row = (String, String, String, i64, String, String, i64);

async fn rows(e: &Env) -> Vec<Row> {
    settle().await;
    let mut all = e
        .state
        .db
        .list_logs(500, 0, &LogFilter::default())
        .await
        .unwrap();
    all.reverse();
    all.iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_string(),
                r["model"].as_str().unwrap().to_string(),
                r["status"].as_str().unwrap().to_string(),
                r["tokens"].as_i64().unwrap(),
                r["detail"].as_str().unwrap().to_string(),
                r["reason"].as_str().unwrap().to_string(),
                r["up_status"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn row(
    kind: &str,
    model: &str,
    status: &str,
    tokens: i64,
    detail: &str,
    reason: &str,
    up: i64,
) -> Row {
    (
        kind.into(),
        model.into(),
        status.into(),
        tokens,
        detail.into(),
        reason.into(),
        up,
    )
}

/// 旧版 `GateError` 的响应形态：顶层 statusCode / message 与 NovelAI 官方一致，message 带站点前缀。
fn assert_gate_error(status: StatusCode, body: &[u8], code: u16, message: &str) {
    assert_eq!(status.as_u16(), code, "{}", text(body));
    let v = json_of(body);
    let full = format!("猫头鹰公益站提醒：{message}");
    assert_eq!(v["statusCode"], code);
    assert_eq!(v["message"], full);
    assert_eq!(v["error"]["message"], full);
    assert_eq!(v["error"]["status"], code);
    assert_eq!(v["error"]["request_id"].as_str().unwrap().len(), 8);
}

fn assert_internal_error(status: StatusCode, body: &[u8]) {
    assert_eq!(status, 500, "{}", text(body));
    let v = json_of(body);
    assert_eq!(v["error"]["status"], 500);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("猫头鹰公益站提醒：服务器内部错误，已自动记录。反馈时请附上错误编号 ")
    );
}

/// 把 `"created": <数字>` / `"created":<数字>` 的数字抹成 0（时间戳每次不同）。
fn zero_created(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("\"created\":") {
        let (head, tail) = rest.split_at(i + "\"created\":".len());
        out.push_str(head);
        let tail_trim = tail.trim_start_matches(' ');
        out.push_str(&tail[..tail.len() - tail_trim.len()]);
        let digits = tail_trim.chars().take_while(char::is_ascii_digit).count();
        out.push('0');
        rest = &tail_trim[digits..];
    }
    out.push_str(rest);
    out
}

const SSE_HEADERS: [(&str, &str); 3] = [
    ("content-type", "text/event-stream; charset=utf-8"),
    ("cache-control", "no-cache"),
    ("x-accel-buffering", "no"),
];

fn assert_sse_headers(headers: &axum::http::HeaderMap) {
    for (k, v) in SSE_HEADERS {
        assert_eq!(headers[k], v, "{k}");
    }
    assert!(headers.get("x-request-id").is_some());
}

/// 一次性回完的 SSE 脚本。
fn sse_script(wire: &[u8]) -> Reply {
    Reply::Chunks {
        status: 200,
        content_type: "text/event-stream".into(),
        chunks: vec![(Duration::ZERO, Bytes::copy_from_slice(wire))],
    }
}

/// 逐字节回的 SSE 脚本（行结束符 / 事件被切在任何位置都得对）。
fn sse_bytewise(wire: &[u8]) -> Reply {
    Reply::Chunks {
        status: 200,
        content_type: "text/event-stream".into(),
        chunks: wire
            .iter()
            .map(|b| (Duration::ZERO, Bytes::copy_from_slice(&[*b])))
            .collect(),
    }
}

fn words(n: usize) -> String {
    (0..n)
        .map(|i| format!("data: {{\"token\":\" word{i} \"}}\n\n"))
        .collect::<String>()
}

// ------------------------------------------------------------------ /v1/models、/v1/me

const MODELS_JSON: &str = r#"{"object":"list","data":[{"id":"llama-3-erato-v1","object":"model","owned_by":"novelai"},{"id":"kayra-v1","object":"model","owned_by":"novelai"}]}"#;

#[tokio::test]
async fn models_listing_needs_no_auth() {
    let e = setup(&[]).await;
    for path in ["/v1/models", "/v1"] {
        let (status, headers, body) = call_get_anon(&e, path).await;
        assert_eq!(status, 200);
        assert_eq!(ct(&headers), "application/json");
        assert_eq!(text(&body), MODELS_JSON);
    }
    assert!(e.mock.requests().is_empty());
    assert!(rows(&e).await.is_empty());
}

const FEATURES_ALL_ON_VOICE_OFF: &str = r#"[{"id":"image","label":"文生图","on":true},{"id":"upscale","label":"放大","on":true},{"id":"augment","label":"导演工具（线稿、上色等）","on":true},{"id":"vibe","label":"Vibe 编码","on":true},{"id":"tags","label":"标签补全","on":true},{"id":"text","label":"文本生成 / OpenAI 兼容聊天","on":true},{"id":"voice","label":"语音合成","on":false}]"#;

fn me_json(
    name: &str,
    is_admin: bool,
    tokens: i64,
    requests: i64,
    daily_text: i64,
    delete_days: i64,
    features: &str,
) -> String {
    format!(
        "{{\"name\":\"{name}\",\"is_admin\":{is_admin},\"generated_images_total\":0,\"today\":{{\"images\":0,\"daily_images\":100,\"legacy_free_images_today\":0,\"anlas_today\":0.0,\"daily_anlas\":0.0,\"anlas_auto\":false,\"v5_today\":0,\"daily_v5\":10,\"image_model_scope\":\"legacy\",\"anlas_month\":0.0,\"monthly_anlas\":0.0,\"text_tokens\":{tokens},\"daily_text_tokens\":{daily_text},\"requests\":{requests}}},\"expires_at\":null,\"inactivity_delete_days\":{delete_days},\"features\":{features}}}"
    )
}

#[tokio::test]
async fn me_matches_legacy_bytes() {
    let e = setup(&[]).await;
    add_admin(&e).await;
    let (status, headers, body) = call_get(&e, "/v1/me", TOKEN).await;
    assert_eq!(status, 200, "{}", text(&body));
    assert_eq!(ct(&headers), "application/json");
    assert_eq!(
        text(&body),
        me_json("测试成员", false, 0, 0, 1000, 3, FEATURES_ALL_ON_VOICE_OFF)
    );
    // 站长：没有闲置回收，语音可用
    let (_, _, body) = call_get(&e, "/v1/me", ADMIN).await;
    let admin_features = FEATURES_ALL_ON_VOICE_OFF.replace(
        r#"{"id":"voice","label":"语音合成","on":false}"#,
        r#"{"id":"voice","label":"语音合成","on":true}"#,
    );
    assert_eq!(
        text(&body),
        me_json("站长", true, 0, 0, 1000, 0, &admin_features)
    );
}

#[tokio::test]
async fn me_counts_usage_and_respects_feature_switches() {
    let e = setup(&[]).await;
    add_limited(&e, LIMITED, "限额成员", 3).await;
    add_key(
        &e,
        "仅出图",
        "nai-test-feat-key-00000000000000000000",
        json!({"features": "image"}),
    )
    .await;
    let (status, _, _) = call_post(&e, "/ai/generate-stream", LIMITED, native(10)).await;
    assert_eq!(status, 200);
    settle().await;
    let (_, _, body) = call_get(&e, "/v1/me", LIMITED).await;
    assert_eq!(
        text(&body),
        me_json("限额成员", false, 3, 1, 3, 3, FEATURES_ALL_ON_VOICE_OFF)
    );
    // Key 自己的功能列表：只开了出图（Vibe 因为全站 Anlas 池开放，仍是可用）
    let (_, _, body) = call_get(&e, "/v1/me", "nai-test-feat-key-00000000000000000000").await;
    let v = json_of(&body);
    let on: Vec<(&str, bool)> = v["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), f["on"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        on,
        [
            ("image", true),
            ("upscale", false),
            ("augment", false),
            ("vibe", true),
            ("tags", false),
            ("text", false),
            ("voice", false)
        ]
    );
    // 站长全局关掉文本 / 语音：普通成员看到 off，站长不受影响
    e.state
        .db
        .set_setting("features_global", r#"{"text": false, "voice": false}"#)
        .await
        .unwrap();
    let (_, _, body) = call_get(&e, "/v1/me", TOKEN).await;
    let v = json_of(&body);
    assert_eq!(
        v["features"][5],
        json!({"id": "text", "label": "文本生成 / OpenAI 兼容聊天", "on": false})
    );
}

#[tokio::test]
async fn me_auth_errors() {
    let e = setup(&[]).await;
    let (status, _, body) = call_get_anon(&e, "/v1/me").await;
    assert_gate_error(status, &body, 401, "缺少 API Key");
    let (status, _, body) = call_get(&e, "/v1/me", "nai-wrong").await;
    assert_gate_error(status, &body, 401, "无效的 API Key");
}

// ------------------------------------------------------------------ 原生流式 /ai/generate-stream

#[tokio::test]
async fn native_stream_forwards_upstream_bytes_and_records_tokens() {
    let e = setup(&[]).await;
    for path in [
        "/ai/generate-stream",
        "/nai/ai/generate-stream",
        "/v1/ai/generate-stream",
    ] {
        let (status, headers, body) = call_post(&e, path, TOKEN, native(5)).await;
        assert_eq!(status, 200, "{path}: {}", text(&body));
        assert_sse_headers(&headers);
        assert_eq!(
            text(&body),
            format!("{}data: [DONE]\n\n", words(5)),
            "{path}"
        );
    }
    // 发给上游的请求体原样（钳制后没变化），Accept 是 text/event-stream
    let sent = e.mock.requests_to("/ai/generate-stream");
    assert_eq!(sent.len(), 3);
    assert_eq!(
        text(&sent[0].body),
        r#"{"model":"llama-3-erato-v1","input":"once upon","parameters":{"max_length":5}}"#
    );
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    // 没有 detail、没有上游状态码（流式只在非 200 时记状态码）
    let model = "llama-3-erato-v1";
    assert_eq!(
        rows(&e).await,
        vec![
            row("text", model, "ok", 5, "", "", 0),
            row("text", model, "ok", 5, "", "", 0),
            row("text", model, "ok", 5, "", "", 0)
        ]
    );
    let c = e.state.db.get_counter(1, &e.state.day()).await.unwrap();
    assert_eq!((c.text_tokens, c.requests), (15, 3));
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn native_stream_clamps_parameters_like_legacy() {
    let e = setup(&[]).await;
    let mut body = native(400);
    body["parameters"]["min_length"] = json!(999);
    let (status, _, out) = call_post(&e, "/ai/generate-stream", TOKEN, body).await;
    assert_eq!(status, 200);
    assert_eq!(text(&out), format!("{}data: [DONE]\n\n", words(20)));
    let sent = e.mock.requests_to("/ai/generate-stream");
    assert_eq!(
        text(&sent[0].body),
        r#"{"model":"llama-3-erato-v1","input":"once upon","parameters":{"max_length":250,"min_length":250}}"#
    );
    assert_eq!(
        rows(&e).await,
        vec![row(
            "text",
            "llama-3-erato-v1",
            "ok",
            20,
            "max_length 已钳制到 250",
            "",
            0
        )]
    );
}

#[tokio::test]
async fn native_stream_truncates_overlong_input_in_the_middle() {
    let e = setup(&[]).await;
    let body =
        json!({"model": "kayra-v1", "input": "猫".repeat(9000), "parameters": {"max_length": 2}});
    let (status, _, _) = call_post(&e, "/ai/generate-stream", TOKEN, body).await;
    assert_eq!(status, 200);
    assert_eq!(
        rows(&e).await,
        vec![row(
            "text",
            "kayra-v1",
            "ok",
            2,
            "输入超出 8192 token 上下文，已省略中间 2566 字符（保留开头与最近的结尾）",
            "",
            0
        )]
    );
    let sent = e.mock.requests_to("/ai/generate-stream");
    assert!(
        sent[0].json()["input"]
            .as_str()
            .unwrap()
            .contains("\n…（中间省略）…\n")
    );
}

#[tokio::test]
async fn native_requests_validate_model_whitelist_and_retired_names() {
    let e = setup(&[]).await;
    add_admin(&e).await;
    let post = |token: &'static str, body: Value| {
        let state = e.state.clone();
        async move {
            Call::new("POST", "/ai/generate-stream")
                .bearer(token)
                .json(body)
                .send(&state)
                .await
        }
    };
    let (status, headers, out) = post(TOKEN, native_with_model("nope-model")).await;
    assert_gate_error(status, &out, 400, "不支持的文本模型");
    assert_eq!(ct(&headers), "application/json");
    let clio = "clio-v1：上游已没有这个模型（NovelAI 老模型 clio 已下线）；可用：llama-3-erato-v1、kayra-v1";
    let (status, _, out) = post(TOKEN, native_with_model("clio-v1")).await;
    assert_gate_error(status, &out, 400, clio);
    // 已下线的名字对站长也一样（上游对它回 403，会被误当成账号被拒）
    let (status, _, out) = post(ADMIN, native_with_model("glm-4-6")).await;
    assert_gate_error(
        status,
        &out,
        400,
        "glm-4-6：GLM-4.6 只能走 NovelAI 的 OpenAI 兼容接口，本站暂未开放；可用：llama-3-erato-v1、kayra-v1",
    );
    // 没写 model：按 "?" 记；model 不是字符串：按 str() 记
    let (status, _, out) = post(TOKEN, json!({"input": "x"})).await;
    assert_gate_error(status, &out, 400, "不支持的文本模型");
    let (status, _, _) = post(TOKEN, json!({"model": 5, "input": "x"})).await;
    assert_eq!(status, 400);
    let (status, _, _) = post(TOKEN, json!({"model": null, "input": "x"})).await;
    assert_eq!(status, 400);
    // 站长可以用白名单以外的名字（打到上游）
    let (status, _, out) = post(ADMIN, native_with_model("kayra-v1")).await;
    assert_eq!(status, 200, "{}", text(&out));
    assert_eq!(e.mock.requests_to("/ai/generate-stream").len(), 1);
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "text",
                "nope-model",
                "rejected",
                0,
                "model not allowed",
                "",
                0
            ),
            row(
                "text",
                "clio-v1",
                "rejected",
                0,
                "model retired upstream",
                "",
                0
            ),
            row(
                "text",
                "glm-4-6",
                "rejected",
                0,
                "model retired upstream",
                "",
                0
            ),
            row("text", "?", "rejected", 0, "model not allowed", "", 0),
            row("text", "5", "rejected", 0, "model not allowed", "", 0),
            row("text", "None", "rejected", 0, "model not allowed", "", 0),
            row("text", "kayra-v1", "ok", 5, "", "", 0),
        ]
    );
}

fn native_with_model(model: &str) -> Value {
    json!({"model": model, "input": "once upon", "parameters": {"max_length": 5}})
}

#[tokio::test]
async fn native_requests_reject_bad_bodies() {
    let e = setup(&[]).await;
    let send = |body: Body| {
        let state = e.state.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri("/ai/generate-stream")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(body)
                .unwrap();
            let resp = gate_server::app(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec();
            (status, bytes)
        }
    };
    let (status, out) = send(Body::from("{not json")).await;
    assert_gate_error(status, &out, 400, "请求体不是合法 JSON");
    let (status, out) = send(Body::from("")).await;
    assert_gate_error(status, &out, 400, "请求体不是合法 JSON");
    let (status, out) = send(Body::from("[1]")).await;
    assert_gate_error(status, &out, 400, "请求体必须是 JSON 对象");
    // 文本请求体上限 1 MiB
    let big = json!({"model": "kayra-v1", "input": "x".repeat(1024 * 1024 + 10)});
    let (status, out) = send(Body::from(serde_json::to_vec(&big).unwrap())).await;
    assert_gate_error(status, &out, 413, "请求体过大");
    // 输入过长（按字符数）
    let long = json!({"model": "kayra-v1", "input": "x".repeat(24001)});
    let (status, out) = send(Body::from(serde_json::to_vec(&long).unwrap())).await;
    assert_gate_error(status, &out, 400, "输入过长（24001 字符，上限 24000）");
    // 参数里的类型错误：旧版是未捕获的 ValueError → 500
    let bad = json!({"model": "kayra-v1", "input": "x", "parameters": {"max_length": "abc"}});
    let (status, out) = send(Body::from(serde_json::to_vec(&bad).unwrap())).await;
    assert_internal_error(status, &out);
    assert!(e.mock.requests().is_empty());
    let rows = rows(&e).await;
    assert_eq!(
        rows[..5].to_vec(),
        vec![
            row("text", "", "rejected", 0, "400 请求体不是合法 JSON", "", 0),
            row("text", "", "rejected", 0, "400 请求体不是合法 JSON", "", 0),
            row(
                "text",
                "",
                "rejected",
                0,
                "400 请求体必须是 JSON 对象",
                "",
                0
            ),
            row("text", "", "rejected", 0, "413 请求体过大", "", 0),
            row(
                "text",
                "kayra-v1",
                "rejected",
                0,
                "输入过长（24001 字符，上限 24000）",
                "",
                0
            ),
        ]
    );
    assert_eq!(rows[5].2, "error");
}

#[tokio::test]
async fn native_stream_handles_parameters_that_are_not_a_dict() {
    let e = setup(&[]).await;
    let body = json!({"model": "kayra-v1", "input": 12345, "parameters": 5});
    let (status, _, out) = call_post(&e, "/ai/generate-stream", TOKEN, body).await;
    assert_eq!(status, 200);
    assert_eq!(text(&out), format!("{}data: [DONE]\n\n", words(20)));
    let sent = e.mock.requests_to("/ai/generate-stream");
    assert_eq!(
        text(&sent[0].body),
        r#"{"model":"kayra-v1","input":12345,"parameters":{"max_length":150}}"#
    );
}

#[tokio::test]
async fn native_requests_enforce_auth_rpm_and_feature_switches() {
    let e = setup(&[]).await;
    add_admin(&e).await;
    add_key(
        &e,
        "限频",
        "nai-test-rpm1-key-00000000000000000000",
        json!({"rpm": 1}),
    )
    .await;
    add_key(
        &e,
        "仅出图",
        "nai-test-feat-key-00000000000000000000",
        json!({"features": "image"}),
    )
    .await;
    let feat = "nai-test-feat-key-00000000000000000000";
    let rpm = "nai-test-rpm1-key-00000000000000000000";
    for path in [
        "/ai/generate-stream",
        "/ai/generate",
        "/v1/chat/completions",
        "/ai/generate-voice",
    ] {
        let (status, _, out) = call_post_anon(&e, path, native(1)).await;
        assert_gate_error(status, &out, 401, "缺少 API Key");
        let (status, _, out) = call_post(&e, path, "nai-wrong", native(1)).await;
        assert_gate_error(status, &out, 401, "无效的 API Key");
    }
    // 限频：第二次起 429（语音也先限频再查管理员）
    let (status, _, _) = call_post(&e, "/ai/generate-stream", rpm, native(1)).await;
    assert_eq!(status, 200);
    for path in [
        "/ai/generate-stream",
        "/v1/chat/completions",
        "/ai/generate-voice",
    ] {
        let (status, _, out) = call_post(&e, path, rpm, native(1)).await;
        assert_gate_error(
            status,
            &out,
            429,
            "请求过于频繁（上限 1 次/分钟），请稍后再试",
        );
    }
    // 这把 Key 的功能列表里没有 text / voice
    let denied = "你的 Key 暂无权使用：文本生成 / OpenAI 兼容聊天（可向站长申请）";
    for path in [
        "/ai/generate-stream",
        "/ai/generate",
        "/v1/chat/completions",
    ] {
        let (status, _, out) = call_post(&e, path, feat, native(1)).await;
        assert_gate_error(status, &out, 403, denied);
    }
    let (status, _, out) = call_post(&e, "/ai/generate-voice", feat, json!({"text": "hi"})).await;
    assert_gate_error(
        status,
        &out,
        403,
        "你的 Key 暂无权使用：语音合成（可向站长申请）",
    );
    // 站长全局关闭：普通成员被拒，站长不受影响（连语音也能用）
    e.state
        .db
        .set_setting("features_global", r#"{"text": false, "voice": false}"#)
        .await
        .unwrap();
    for path in [
        "/ai/generate-stream",
        "/ai/generate",
        "/v1/chat/completions",
    ] {
        let (status, _, out) = call_post(&e, path, TOKEN, native(1)).await;
        assert_gate_error(status, &out, 403, EXTRA_FEATURES);
    }
    e.mock.push("/ai/generate-voice", voice_ok());
    let (status, _, _) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_eq!(status, 200);
    let (status, _, _) = call_post(&e, "/ai/generate-stream", ADMIN, native(1)).await;
    assert_eq!(status, 200);
    let rows = rows(&e).await;
    let rejected: Vec<_> = rows.iter().filter(|r| r.2 == "rejected").collect();
    assert_eq!(rejected.len(), 3 + 4 + 3, "{rows:?}");
    // 被拒的请求记在对应 kind 下，原因码只有限频带（key_rate）
    assert!(rows.contains(&row(
        "text",
        "",
        "rejected",
        0,
        "429 请求过于频繁（上限 1 次/分钟），请稍后再试",
        "key_rate",
        0
    )));
    assert!(rows.contains(&row(
        "chat",
        "",
        "rejected",
        0,
        "429 请求过于频繁（上限 1 次/分钟），请稍后再试",
        "key_rate",
        0
    )));
    assert!(rows.contains(&row(
        "voice",
        "",
        "rejected",
        0,
        "429 请求过于频繁（上限 1 次/分钟），请稍后再试",
        "key_rate",
        0
    )));
    assert!(rows.contains(&row(
        "voice",
        "",
        "rejected",
        0,
        "403 你的 Key 暂无权使用：语音合成（可向站长申请）",
        "",
        0
    )));
    assert!(rows.contains(&row(
        "chat",
        "",
        "rejected",
        0,
        &format!("403 {EXTRA_FEATURES}"),
        "",
        0
    )));
}

#[tokio::test]
async fn daily_text_quota_cuts_the_stream_and_blocks_the_next_request() {
    let e = setup(&[]).await;
    add_limited(&e, LIMITED, "限额成员", 3).await;
    // 每日 3 个 token：第 3 个 token 撞上限，不转发，补一个 [DONE]；那 3 个都算数
    let (status, _, body) = call_post(&e, "/ai/generate-stream", LIMITED, native(10)).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), format!("{}data: [DONE]\n\n", words(2)));
    let (status, _, body) = call_post(&e, "/ai/generate-stream", LIMITED, native(10)).await;
    let quota = "已达今日文本额度（3 tokens/天），明日再来吧";
    assert_gate_error(status, &body, 402, quota);
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", LIMITED, chat_body(json!({}))).await;
    assert_gate_error(status, &body, 402, quota);
    let (status, _, body) = call_post(&e, "/ai/generate", LIMITED, native(10)).await;
    assert_gate_error(status, &body, 402, quota);
    assert_eq!(e.mock.requests_to("/ai/generate-stream").len(), 1);
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "text",
                "llama-3-erato-v1",
                "ok",
                3,
                "达到每日上限被截断",
                "",
                0
            ),
            row("text", "", "rejected", 0, &format!("402 {quota}"), "", 0),
            row("chat", "", "rejected", 0, &format!("402 {quota}"), "", 0),
            row("text", "", "rejected", 0, &format!("402 {quota}"), "", 0),
        ]
    );
    // 站长不受每日额度限制，也不会被截断
    add_admin(&e).await;
    let (status, _, body) = call_post(&e, "/ai/generate-stream", ADMIN, native(30)).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), format!("{}data: [DONE]\n\n", words(20)));
}

#[tokio::test]
async fn native_stream_event_framing_matches_legacy() {
    let e = setup(&[]).await;
    // 逐字节到达、CRLF、带 event 名：按事件重新编码（行结束符统一成 \n）
    let wire = b"event: newToken\ndata: {\"token\": \"a\"}\n\nevent: newToken\r\ndata: {\"token\": \"b\"}\r\n\r\ndata: [DONE]\n\n";
    e.mock.push("/ai/generate-stream", sse_bytewise(wire));
    let (status, _, body) =
        call_post(&e, "/ai/generate-stream", TOKEN, native_with("EVENTNAMES")).await;
    assert_eq!(status, 200);
    assert_eq!(
        text(&body),
        "event: newToken\ndata: {\"token\": \"a\"}\n\nevent: newToken\ndata: {\"token\": \"b\"}\n\ndata: [DONE]\n\n"
    );
    // 没有 token 的事件不转发；空 token 转发并计数
    e.mock.push(
        "/ai/generate-stream",
        sse_script(b"data: {\"foo\": 1}\n\ndata: {\"token\": \"\"}\n\ndata: {\"token\": \"a\"}\n\ndata: [DONE]\n\n"),
    );
    let (_, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native_with("EMPTYTOKEN")).await;
    assert_eq!(
        text(&body),
        "data: {\"token\": \"\"}\n\ndata: {\"token\": \"a\"}\n\ndata: [DONE]\n\n"
    );
    // 上游没发 [DONE] 就结束：我们也不补
    e.mock.push(
        "/ai/generate-stream",
        sse_script(b"data: {\"token\": \"a\"}\n\ndata: {\"token\": \"b\"}\n\n"),
    );
    let (_, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native_with("NOTDONE")).await;
    assert_eq!(
        text(&body),
        "data: {\"token\": \"a\"}\n\ndata: {\"token\": \"b\"}\n\n"
    );
    let m = "llama-3-erato-v1";
    assert_eq!(
        rows(&e).await,
        vec![
            row("text", m, "ok", 2, "", "", 0),
            row("text", m, "ok", 2, "", "", 0),
            row("text", m, "ok", 2, "", "", 0)
        ]
    );
}

#[tokio::test]
async fn native_stream_upstream_failures_after_headers_become_error_events() {
    let e = setup(&[]).await;
    let failed = "上游文本生成失败，请稍后重试";
    let bad_format = "上游文本事件流格式无效";
    let err_event = |msg: &str| {
        format!(
            "event: error\ndata: {{\"error\": \"{msg}\", \"message\": \"{msg}\", \"status_code\": 502}}\n\n"
        )
    };
    let hello_then = |tail: String| format!("data: {{\"token\": \"hello\"}}\n\n{tail}");
    let cases: Vec<(&str, Vec<u8>, String, bool)> = vec![
        // (描述, 上游原文, 期望响应体, 之前是否有 token)
        (
            "error 事件",
            b"data: {\"token\": \"hello\"}\n\nevent: error\ndata: {\"message\":\"private\"}\n\n"
                .to_vec(),
            hello_then(err_event(failed)),
            true,
        ),
        (
            "error 字段",
            b"data: {\"token\": \"hello\"}\n\ndata: {\"error\":\"private\"}\n\n".to_vec(),
            hello_then(err_event(failed)),
            true,
        ),
        (
            "非 JSON",
            b"data: {\"token\": \"hello\"}\n\ndata: <html>private</html>\n\n".to_vec(),
            hello_then(err_event(bad_format)),
            true,
        ),
        (
            "token 不是字符串",
            b"data: {\"token\": 5}\n\n".to_vec(),
            err_event(bad_format),
            false,
        ),
        (
            "带 message 字段",
            b"data: {\"token\": \"a\", \"message\": \"x\"}\n\n".to_vec(),
            err_event(failed),
            false,
        ),
    ];
    let mut expected_rows = Vec::new();
    for (name, wire, expect, had_token) in cases {
        // 逐字节送达，哪里切都一样
        e.mock.push("/ai/generate-stream", sse_bytewise(&wire));
        let (status, headers, body) =
            call_post(&e, "/ai/generate-stream", TOKEN, native_with(name)).await;
        // 响应头已经发出：状态码是 200，错误编码在事件流里
        assert_eq!(status, 200, "{name}");
        assert_sse_headers(&headers);
        assert_eq!(text(&body), expect, "{name}");
        assert!(!text(&body).contains("private"), "{name}");
        let msg = if expect.contains(bad_format) {
            bad_format
        } else {
            failed
        };
        expected_rows.push(row("text", "llama-3-erato-v1", "error", 0, msg, "", 0));
        if had_token {
            expected_rows.push(row("text", "llama-3-erato-v1", "ok", 1, "", "", 0));
        }
    }
    // 先记 error 再记 ok（按创建顺序落库）
    assert_eq!(rows(&e).await, expected_rows);
}

#[tokio::test]
async fn native_stream_upstream_rejections_are_plain_json_errors() {
    let e = setup(&[]).await;
    // 上游状态 → (给成员的状态码, 上游状态记在日志里)
    for (up, out) in [
        (400, 400),
        (422, 422),
        (404, 502),
        (500, 502),
        (502, 502),
        (503, 503),
    ] {
        e.mock.push(
            "/ai/generate-stream",
            Reply::json(
                up,
                &json!({"statusCode": up, "message": "private upstream detail"}),
            ),
        );
        let (status, headers, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
        assert_gate_error(
            status,
            &body,
            out,
            &format!("上游文本请求失败（HTTP {up}）"),
        );
        assert_eq!(ct(&headers), "application/json");
        assert!(!text(&body).contains("private"));
    }
    // 403「模型不存在」：成员填错模型，不是账号被拒
    e.mock.push("/ai/generate-stream", Reply::model_403("x"));
    let (status, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
    let unknown = "上游不认识这个文本模型；本站可用的文本模型：llama-3-erato-v1、kayra-v1";
    assert_gate_error(status, &body, 400, unknown);
    let m = "llama-3-erato-v1";
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 400）",
                "",
                400
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 422）",
                "",
                422
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 404）",
                "",
                404
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 500）",
                "",
                500
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 502）",
                "",
                502
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 503）",
                "",
                503
            ),
            row("text", m, "error", 0, unknown, "", 403),
        ]
    );
    // 并发槽都归还了
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn native_stream_upstream_429_pauses_only_the_text_channel() {
    let e = setup(&[]).await;
    e.mock
        .push("/ai/generate-stream", Reply::rate_limited("30"));
    let (status, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
    assert_gate_error(
        status,
        &body,
        429,
        "上游限流(429)，文本 / 语音通道暂停约 30 秒，请稍后再试",
    );
    // 文本通道冷却中：不再打上游
    let (status, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
    assert_eq!(status, 503, "{}", text(&body));
    let msg = json_of(&body)["message"].as_str().unwrap().to_string();
    assert!(
        msg.starts_with("猫头鹰公益站提醒：上游文本通道限流冷却中，约 "),
        "{msg}"
    );
    assert!(msg.ends_with(" 秒后恢复（生图不受影响）"), "{msg}");
    assert_eq!(e.mock.requests_to("/ai/generate-stream").len(), 1);
    // 非流式补全走另一条通道，不受这个冷却影响
    let (status, _, _) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
    assert_eq!(status, 200);
    let rows = rows(&e).await;
    assert_eq!(
        rows[0],
        row(
            "text",
            "llama-3-erato-v1",
            "error",
            0,
            "上游限流(429)，文本 / 语音通道暂停约 30 秒，请稍后再试",
            "",
            429
        )
    );
    assert_eq!(rows[1].2, "error");
    assert_eq!(rows[1].6, 0);
    assert_eq!(
        rows[2],
        row("text", "llama-3-erato-v1", "ok", 16, "", "", 200)
    );
}

#[tokio::test]
async fn native_stream_upstream_account_errors_are_masked() {
    let e = setup(&[]).await;
    e.mock.push("/ai/generate-stream", Reply::account_403());
    let (status, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
    assert_gate_error(
        status,
        &body,
        502,
        "上游拒绝了请求（HTTP 403），该账号已临时隔离保护",
    );
    e.mock.push(
        "/ai/generate-stream",
        Reply::json(401, &json!({"statusCode": 401, "message": "Unauthorized"})),
    );
    let (status, _, body) = call_post(&e, "/ai/generate-stream", TOKEN, native(5)).await;
    assert_gate_error(
        status,
        &body,
        502,
        "上游令牌已失效（401），请站长更换 NovelAI Token",
    );
    // 401 之后这个上游账号被停用：后续文本请求拿到「暂时不可用」
    let (status, _, body) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
    assert_gate_error(
        status,
        &body,
        503,
        "上游账号暂时不可用（不是额度用完），请稍后再试",
    );
    let m = "llama-3-erato-v1";
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "text",
                m,
                "error",
                0,
                "上游拒绝了请求（HTTP 403），该账号已临时隔离保护",
                "",
                403
            ),
            row(
                "text",
                m,
                "error",
                0,
                "上游令牌已失效（401），请站长更换 NovelAI Token",
                "",
                401
            ),
        ]
    );
}

#[tokio::test]
async fn native_stream_client_disconnect_still_records_the_tokens_it_read() {
    let e = setup(&[]).await;
    // 每个 token 间隔 100 毫秒；读到第一帧就断开
    let chunks: Vec<(Duration, Bytes)> = (0..10)
        .map(|i| {
            (
                Duration::from_millis(100),
                Bytes::from(format!("data: {{\"token\":\"t{i}\"}}\n\n")),
            )
        })
        .chain([(Duration::ZERO, Bytes::from_static(b"data: [DONE]\n\n"))])
        .collect();
    e.mock.push(
        "/ai/generate-stream",
        Reply::Chunks {
            status: 200,
            content_type: "text/event-stream".into(),
            chunks,
        },
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai/generate-stream")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&native(10)).unwrap()))
        .unwrap();
    let resp = gate_server::app(e.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(first.starts_with(b"data: {\"token\":\"t0\"}"));
    drop(body);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let rows = rows(&e).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].2.as_str()), ("text", "ok"));
    assert!(
        (1..=4).contains(&rows[0].3),
        "断开时只读到了前几个 token：{rows:?}"
    );
    // 并发槽 / 在途名额 / 上游连接都已归还
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let auth = format!("Bearer {TOKEN}");
    let permits: Vec<_> = (0..4)
        .map(|_| e.state.inflight.acquire(&auth).unwrap())
        .collect();
    assert_eq!(permits.len(), 4);
}

#[tokio::test]
async fn too_many_inflight_requests_are_refused_before_reading_the_body() {
    let e = setup(&[]).await;
    for _ in 0..4 {
        e.mock.push("/ai/generate", Reply::Hang);
    }
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let state = e.state.clone();
        tasks.push(tokio::spawn(async move {
            Call::new("POST", "/ai/generate")
                .bearer(TOKEN)
                .json(native(5))
                .send(&state)
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let limit = "同时进行中的请求过多（上限 4 个），请等前面的请求完成";
    for path in [
        "/ai/generate",
        "/ai/generate-stream",
        "/v1/chat/completions",
    ] {
        let (status, _, body) = call_post(&e, path, TOKEN, native(5)).await;
        assert_gate_error(status, &body, 429, limit);
    }
    // 语音接口旧版没套这个限制：走到后面的「仅限管理员」
    let (status, _, body) = call_post(&e, "/ai/generate-voice", TOKEN, json!({"text": "hi"})).await;
    assert_gate_error(
        status,
        &body,
        403,
        "语音合成目前仅限管理员 Key（它按 Anlas 计费，暂未接入额度统计）",
    );
    // /v1/me 也没有
    let (status, _, _) = call_get(&e, "/v1/me", TOKEN).await;
    assert_eq!(status, 200);
    for t in tasks {
        t.abort();
    }
}

// ------------------------------------------------------------------ 非流式 /ai/generate

#[tokio::test]
async fn native_text_returns_upstream_json_and_records_estimated_tokens() {
    let e = setup(&[]).await;
    for path in ["/ai/generate", "/nai/ai/generate", "/v1/ai/generate"] {
        let (status, headers, body) = call_post(&e, path, TOKEN, native(5)).await;
        assert_eq!(status, 200, "{path}: {}", text(&body));
        assert_eq!(ct(&headers), "application/json");
        assert_eq!(
            text(&body),
            r#"{"output":" Once upon a time  Once upon a time  Once upon a time "}"#
        );
    }
    let sent = e.mock.requests_to("/ai/generate");
    assert_eq!(sent[0].header("accept"), Some("application/json"));
    assert_eq!(
        text(&sent[0].body),
        r#"{"model":"llama-3-erato-v1","input":"once upon","parameters":{"max_length":5}}"#
    );
    let m = "llama-3-erato-v1";
    assert_eq!(
        rows(&e).await,
        vec![
            row("text", m, "ok", 16, "", "", 200),
            row("text", m, "ok", 16, "", "", 200),
            row("text", m, "ok", 16, "", "", 200)
        ]
    );
    // 钳制说明进 detail
    let (status, _, _) = call_post(&e, "/ai/generate", TOKEN, native(400)).await;
    assert_eq!(status, 200);
    assert_eq!(
        rows(&e).await[3],
        row("text", m, "ok", 16, "max_length 已钳制到 250", "", 200)
    );
    let (status, _, body) =
        call_post(&e, "/ai/generate", TOKEN, native_with_model("nope-model")).await;
    assert_gate_error(status, &body, 400, "不支持的文本模型");
}

#[tokio::test]
async fn native_text_odd_upstream_outputs_follow_python_semantics() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    // 列表 / 字典按元素个数估（旧版 estimate_tokens 直接 len()）；空输出、缺字段、非 JSON 都记 0 个 token
    for (reply, body, tokens) in [
        (
            Reply::json(
                200,
                &json!({"output": ["a", "b", "c", "d", "e", "f", "g", "h"]}),
            ),
            r#"{"output":["a","b","c","d","e","f","g","h"]}"#,
            3,
        ),
        (
            Reply::json(200, &json!({"output": ""})),
            r#"{"output":""}"#,
            0,
        ),
        (
            Reply::json(200, &json!({"nothing": 1})),
            r#"{"nothing":1}"#,
            0,
        ),
        (Reply::status(200, "<html>"), "<html>", 0),
    ] {
        e.mock.push("/ai/generate", reply);
        let (status, headers, out) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
        assert_eq!((status, text(&out)), (StatusCode::OK, body.to_string()));
        // 响应头固定 application/json，不跟上游走
        assert_eq!(ct(&headers), "application/json");
        let last = rows(&e).await.pop().unwrap();
        assert_eq!(last, row("text", m, "ok", tokens, "", "", 200), "{body}");
    }
    // output 是数字：旧版 len(5) 抛 TypeError → 500，不记账
    e.mock
        .push("/ai/generate", Reply::json(200, &json!({"output": 5})));
    let before = rows(&e).await.len();
    let (status, _, out) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
    assert_internal_error(status, &out);
    let after = rows(&e).await;
    assert_eq!(after.len(), before + 1);
    assert_eq!(after.last().unwrap().2, "error");
}

#[tokio::test]
async fn native_text_upstream_statuses_use_the_status_message_table() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    let table = [
        (400, "上游未接受请求参数，请检查输入"),
        (403, "上游服务拒绝了请求，请稍后重试"),
        (404, "上游服务接口或模型不可用，请稍后重试"),
        (500, "上游服务暂时不可用，请稍后重试"),
        (502, "上游服务暂时不可用，请稍后重试"),
        (503, "上游服务暂时不可用，请稍后重试"),
        (201, "上游请求失败，请稍后重试"),
    ];
    let mut expected = Vec::new();
    for (up, why) in table {
        e.mock.push(
            "/ai/generate",
            Reply::json(up, &json!({"statusCode": up, "message": "private"})),
        );
        let (status, _, body) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
        // 状态码就是上游给的（200 以外一律不当成功；旧版连 201 也原样当错误状态回）
        assert_gate_error(status, &body, up, &format!("{why}（上游状态 {up}）"));
        assert!(!text(&body).contains("private"));
        expected.push(row(
            "text",
            m,
            "error",
            0,
            &format!("upstream {up}"),
            "",
            up as i64,
        ));
    }
    // 上游 403「模型不存在」在非流式里不特殊处理（和旧版一样）
    e.mock.push("/ai/generate", Reply::model_403("x"));
    let (status, _, body) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
    assert_gate_error(
        status,
        &body,
        403,
        "上游服务拒绝了请求，请稍后重试（上游状态 403）",
    );
    expected.push(row("text", m, "error", 0, "upstream 403", "", 403));
    assert_eq!(rows(&e).await, expected);
}

#[tokio::test]
async fn native_text_upstream_429_retries_once_then_reports_cooldown() {
    let e = setup(&[]).await;
    e.mock.push("/ai/generate", Reply::rate_limited("30"));
    let (status, _, body) = call_post(&e, "/ai/generate", TOKEN, native(5)).await;
    assert_eq!(status, 503, "{}", text(&body));
    let msg = json_of(&body)["message"].as_str().unwrap().to_string();
    assert!(
        msg.starts_with("猫头鹰公益站提醒：上游账号暂时限流冷却中，约 "),
        "{msg}"
    );
    assert!(
        msg.ends_with(" 秒后恢复（不是额度用完），请稍后再试"),
        "{msg}"
    );
    assert_eq!(e.mock.requests_to("/ai/generate").len(), 1);
    // 没有用量记录（5xx 的网关错误不记「拒绝」）
    assert!(rows(&e).await.is_empty());
}

// ------------------------------------------------------------------ 语音

fn voice_ok() -> Reply {
    Reply::Status {
        status: 200,
        headers: vec![("content-type".into(), "audio/mpeg".into())],
        body: Bytes::from_static(b"ID3fake-audio"),
    }
}

#[tokio::test]
async fn voice_is_admin_only_and_forwards_the_audio() {
    let e = setup(&[]).await;
    add_admin(&e).await;
    let (status, _, body) = call_post(
        &e,
        "/ai/generate-voice",
        TOKEN,
        json!({"text": "hi", "voice": "Aini"}),
    )
    .await;
    assert_gate_error(
        status,
        &body,
        403,
        "语音合成目前仅限管理员 Key（它按 Anlas 计费，暂未接入额度统计）",
    );
    assert!(e.mock.requests().is_empty());
    for (path, voice) in [
        ("/ai/generate-voice", json!("Aini")),
        ("/nai/ai/generate-voice", json!(7)),
    ] {
        e.mock.push("/ai/generate-voice", voice_ok());
        let (status, headers, body) =
            call_post(&e, path, ADMIN, json!({"text": "hi", "voice": voice})).await;
        assert_eq!(status, 200, "{}", text(&body));
        assert_eq!(ct(&headers), "audio/mpeg");
        assert_eq!(body, b"ID3fake-audio");
    }
    // 上游没给 content-type：audio/mpeg；text/* 补字符集
    e.mock.push("/ai/generate-voice", Reply::status(200, "mp3"));
    let (_, headers, _) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_eq!(ct(&headers), "audio/mpeg");
    e.mock.push(
        "/ai/generate-voice",
        Reply::Status {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: Bytes::from_static(b"hello"),
        },
    );
    let (_, headers, _) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_eq!(ct(&headers), "text/plain; charset=utf-8");
    let sent = e.mock.requests_to("/ai/generate-voice");
    assert_eq!(text(&sent[0].body), r#"{"text":"hi","voice":"Aini"}"#);
    assert_eq!(text(&sent[1].body), r#"{"text":"hi","voice":7}"#);
    assert_eq!(sent[0].header("accept"), Some("*/*"));
    assert_eq!(
        rows(&e).await,
        vec![
            row("voice", "", "rejected", 0, "voice is admin-only", "", 0),
            row("voice", "Aini", "ok", 0, "", "", 200),
            row("voice", "7", "ok", 0, "", "", 200),
            row("voice", "", "ok", 0, "", "", 200),
            row("voice", "", "ok", 0, "", "", 200),
        ]
    );
    // 请求体上限 1 MiB
    let big = json!({"text": "x".repeat(1024 * 1024 + 10)});
    let (status, _, body) = call_post(&e, "/ai/generate-voice", ADMIN, big).await;
    assert_gate_error(status, &body, 413, "请求体过大");
}

#[tokio::test]
async fn voice_upstream_failures() {
    let e = setup(&[]).await;
    add_admin(&e).await;
    e.mock.push(
        "/ai/generate-voice",
        Reply::json(500, &json!({"message": "private"})),
    );
    let (status, _, body) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_gate_error(
        status,
        &body,
        500,
        "上游服务暂时不可用，请稍后重试（上游状态 500）",
    );
    // 4xx 由外层统一记「拒绝」；5xx 不记用量行
    e.mock.push("/ai/generate-voice", Reply::account_403());
    let (status, _, body) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_gate_error(
        status,
        &body,
        403,
        "上游服务拒绝了请求，请稍后重试（上游状态 403）",
    );
    // 默认假上游没有语音端点：404
    let (status, _, body) = call_post(&e, "/ai/generate-voice", ADMIN, json!({"text": "hi"})).await;
    assert_gate_error(
        status,
        &body,
        404,
        "上游服务接口或模型不可用，请稍后重试（上游状态 404）",
    );
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "voice",
                "",
                "rejected",
                0,
                "403 上游服务拒绝了请求，请稍后重试（上游状态 403）",
                "",
                403
            ),
            row(
                "voice",
                "",
                "rejected",
                0,
                "404 上游服务接口或模型不可用，请稍后重试（上游状态 404）",
                "",
                404
            ),
        ]
    );
}

// ------------------------------------------------------------------ OpenAI 兼容聊天

const CHAT_NON_STREAM_4: &str = r#"{"id":"chatcmpl-naigate","object":"chat.completion","created":0,"model":"llama-3-erato-v1","choices":[{"index":0,"message":{"role":"assistant","content":" word0  word1  word2  word3 "},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":4,"total_tokens":5}}"#;

fn chunk(model: &str, delta: &str, finish: &str) -> String {
    format!(
        "data: {{\"id\": \"chatcmpl-naigate\", \"object\": \"chat.completion.chunk\", \"created\": 0, \"model\": \"{model}\", \"choices\": [{{\"index\": 0, \"delta\": {delta}, \"finish_reason\": {finish}}}]}}\n\n"
    )
}

fn content(model: &str, s: &str) -> String {
    chunk(model, &format!("{{\"content\": \"{s}\"}}"), "null")
}

#[tokio::test]
async fn chat_non_stream_maps_to_the_upstream_text_api() {
    let e = setup(&[]).await;
    let (status, headers, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 4})),
    )
    .await;
    assert_eq!(status, 200, "{}", text(&body));
    assert_eq!(ct(&headers), "application/json");
    let v = json_of(&body);
    let created = v["created"].as_i64().unwrap();
    assert!((created - e.state.now() as i64).abs() <= 5);
    assert_eq!(zero_created(&text(&body)), CHAT_NON_STREAM_4);
    // 请求映射：明文 input + use_string，参数顺序与浮点写法和旧版一致
    let sent = e.mock.requests_to("/ai/generate-stream");
    assert_eq!(
        text(&sent[0].body),
        r#"{"input":"你好","model":"llama-3-erato-v1","parameters":{"max_length":4,"min_length":1,"temperature":0.9,"top_p":0.9,"top_k":40,"use_string":true}}"#
    );
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    assert_eq!(
        rows(&e).await,
        vec![row("chat", "llama-3-erato-v1", "ok", 4, "", "", 0)]
    );
    let c = e.state.db.get_counter(1, &e.state.day()).await.unwrap();
    assert_eq!((c.text_tokens, c.requests), (4, 1));
}

#[tokio::test]
async fn chat_stream_emits_openai_chunks_with_python_json_spacing() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    let (status, headers, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 3, "stream": true})),
    )
    .await;
    assert_eq!(status, 200, "{}", text(&body));
    assert_sse_headers(&headers);
    let expect = format!(
        "{}{}{}{}{}data: [DONE]\n\n",
        chunk(m, "{}", "null"),
        content(m, " word0 "),
        content(m, " word1 "),
        content(m, " word2 "),
        chunk(m, "{}", "\"stop\"")
    );
    assert_eq!(zero_created(&text(&body)), expect);
    assert_eq!(rows(&e).await, vec![row("chat", m, "ok", 3, "", "", 0)]);
    // stream 写成任何真值都按流式；False 是非流式
    let (_, headers, _) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 1, "stream": "yes"})),
    )
    .await;
    assert_eq!(headers["content-type"], "text/event-stream; charset=utf-8");
    let (_, headers, _) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 1, "stream": false})),
    )
    .await;
    assert_eq!(headers["content-type"], "application/json");
}

#[tokio::test]
async fn chat_request_and_model_mapping_follow_python_coercions() {
    let e = setup(&[]).await;
    let sent_params = |e: &Env| {
        let all = e.mock.requests_to("/ai/generate-stream");
        let last = all.last().unwrap().clone();
        (text(&last.body), last.json())
    };
    // 不认识的模型 → 默认模型；kayra 保留
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"model": "gpt-4o", "max_tokens": 2})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json_of(&body)["model"], "llama-3-erato-v1");
    let (_, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"model": "kayra-v1", "max_tokens": 1})),
    )
    .await;
    assert_eq!(json_of(&body)["model"], "kayra-v1");
    // max_tokens：字符串 "2" → 2；0 / 缺省 → 150；超大 → 钳到 250
    for (mt, expect) in [
        (json!("2"), 2),
        (json!(0), 150),
        (json!(100000), 250),
        (json!(null), 150),
        (json!(true), 1),
        (json!(3.9), 3),
    ] {
        let (status, _, _) = call_post(
            &e,
            "/v1/chat/completions",
            TOKEN,
            chat_body(json!({"max_tokens": mt})),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            sent_params(&e).1["parameters"]["max_length"],
            expect,
            "{mt}"
        );
    }
    // 温度：整数 1 → 1.0；字符串 "0.5" → 0.5；0 → 0.9
    let (_, _, _) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 1, "temperature": 1, "top_p": "0.5"})),
    )
    .await;
    assert_eq!(
        sent_params(&e).0,
        r#"{"input":"你好","model":"llama-3-erato-v1","parameters":{"max_length":1,"min_length":1,"temperature":1.0,"top_p":0.5,"top_k":40,"use_string":true}}"#
    );
    let (_, _, _) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"max_tokens": 1, "temperature": 0, "top_p": 0})),
    )
    .await;
    assert!(
        sent_params(&e)
            .0
            .contains(r#""temperature":0.9,"top_p":0.9"#)
    );
    // 旧版的未捕获异常：500
    for bad in [
        json!({"max_tokens": "abc"}),
        json!({"temperature": "hot"}),
        json!({"top_p": [1]}),
        json!({"max_tokens": [1]}),
        // float("nan") 能解析，但 httpx 发 JSON 时拒绝 NaN / Infinity（allow_nan=False）→ 旧版 500
        json!({"temperature": "nan"}),
        json!({"top_p": "-inf"}),
    ] {
        let before = e.mock.requests_to("/ai/generate-stream").len();
        let (status, _, body) =
            call_post(&e, "/v1/chat/completions", TOKEN, chat_body(bad.clone())).await;
        assert_internal_error(status, &body);
        assert_eq!(
            e.mock.requests_to("/ai/generate-stream").len(),
            before,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn chat_builds_the_prompt_from_message_contents_like_python() {
    let e = setup(&[]).await;
    let usage = |body: &[u8]| json_of(body)["usage"]["prompt_tokens"].as_i64().unwrap();
    let last_input = |e: &Env| {
        e.mock
            .requests_to("/ai/generate-stream")
            .last()
            .unwrap()
            .json()["input"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // content 是列表：按 Python str() 展开；非 dict 的消息跳过；缺 content 当空串
    let body = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}, "skip", {"role": "assistant"}], "max_tokens": 1});
    let (status, _, out) = call_post(&e, "/v1/chat/completions", TOKEN, body).await;
    assert_eq!(status, 200);
    assert_eq!(last_input(&e), "[{'type': 'text', 'text': 'hi'}]");
    assert_eq!(usage(&out), 10);
    // 多条消息用换行拼起来，整体 strip
    let body = json!({"messages": [{"content": "  a"}, {"content": null}, {"content": 1.5}, {"content": "b  "}], "max_tokens": 1});
    let (_, _, _) = call_post(&e, "/v1/chat/completions", TOKEN, body).await;
    assert_eq!(last_input(&e), "a\nNone\n1.5\nb");
    // 没有 messages / messages 是字符串或字典：空输入
    for body in [
        json!({"max_tokens": 1}),
        json!({"messages": "hello", "max_tokens": 1}),
        json!({"messages": {"a": 1}, "max_tokens": 1}),
        json!({"messages": [], "max_tokens": 1}),
    ] {
        let (status, _, out) = call_post(&e, "/v1/chat/completions", TOKEN, body).await;
        assert_eq!(status, 200);
        assert_eq!(last_input(&e), "");
        assert_eq!(usage(&out), 0);
    }
    // messages 是数字：不能迭代，旧版 TypeError → 500
    let (status, _, out) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        json!({"messages": 5, "max_tokens": 1}),
    )
    .await;
    assert_internal_error(status, &out);
}

#[tokio::test]
async fn chat_rejections_before_the_upstream() {
    let e = setup(&[]).await;
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_body(json!({"model": "clio-v1"})),
    )
    .await;
    assert_gate_error(
        status,
        &body,
        400,
        "clio-v1：上游已没有这个模型（NovelAI 老模型 clio 已下线）；可用：llama-3-erato-v1、kayra-v1",
    );
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_with(&"x".repeat(24001), json!({})),
    )
    .await;
    assert_gate_error(status, &body, 400, "输入过长（24001 字符，上限 24000）");
    assert!(e.mock.requests().is_empty());
    // 聊天接口这两种拒绝旧版没自己记日志：由外层统一记，model 为空
    assert_eq!(
        rows(&e).await,
        vec![
            row(
                "chat",
                "",
                "rejected",
                0,
                "400 clio-v1：上游已没有这个模型（NovelAI 老模型 clio 已下线）；可用：llama-3-erato-v1、kayra-v1",
                "",
                0
            ),
            row(
                "chat",
                "",
                "rejected",
                0,
                "400 输入过长（24001 字符，上限 24000）",
                "",
                0
            ),
        ]
    );
}

#[tokio::test]
async fn chat_quota_cut_still_finishes_the_reply_normally() {
    let e = setup(&[]).await;
    add_limited(&e, "nai-test-cut2-key-00000000000000000000", "截断一", 2).await;
    add_limited(&e, "nai-test-cut3-key-00000000000000000000", "截断二", 2).await;
    let m = "llama-3-erato-v1";
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        "nai-test-cut2-key-00000000000000000000",
        chat_body(json!({"stream": true, "max_tokens": 5})),
    )
    .await;
    assert_eq!(status, 200);
    // 第 2 个 token 撞上限：不转发，但照常 stop + [DONE]
    let expect = format!(
        "{}{}{}data: [DONE]\n\n",
        chunk(m, "{}", "null"),
        content(m, " word0 "),
        chunk(m, "{}", "\"stop\"")
    );
    assert_eq!(zero_created(&text(&body)), expect);
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        "nai-test-cut3-key-00000000000000000000",
        chat_body(json!({"max_tokens": 5})),
    )
    .await;
    assert_eq!(status, 200);
    // 被截断的那个 token 计入 completion_tokens，但不在内容里
    assert_eq!(
        zero_created(&text(&body)),
        r#"{"id":"chatcmpl-naigate","object":"chat.completion","created":0,"model":"llama-3-erato-v1","choices":[{"index":0,"message":{"role":"assistant","content":" word0 "},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#
    );
    assert_eq!(
        rows(&e).await,
        vec![
            row("chat", m, "ok", 2, "", "", 0),
            row("chat", m, "ok", 2, "", "", 0)
        ]
    );
}

#[tokio::test]
async fn chat_stream_failures_after_headers_are_openai_error_chunks() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    let hello = "data: {\"token\": \"hello\"}\n\n";
    let error_frame = |msg: &str| {
        format!(
            "data: {{\"error\": {{\"message\": \"{msg}\", \"status\": 502}}}}\n\ndata: [DONE]\n\n"
        )
    };
    let failed = "上游文本生成失败，请稍后重试";
    let bad = "上游文本事件流格式无效";
    // 没有 stop 分块：流是中途坏掉的
    let with_hello = |msg: &str| {
        format!(
            "{}{}{}",
            chunk(m, "{}", "null"),
            content(m, "hello"),
            error_frame(msg)
        )
    };
    let cases: Vec<(String, String, String, bool)> = vec![
        (
            format!("{hello}event: error\ndata: {{\"message\":\"private\"}}\n\n"),
            with_hello(failed),
            failed.into(),
            true,
        ),
        (
            format!("{hello}data: <html>private</html>\n\n"),
            with_hello(bad),
            bad.into(),
            true,
        ),
        (
            "data: {\"token\": 5}\n\n".into(),
            format!("{}{}", chunk(m, "{}", "null"), error_frame(bad)),
            bad.into(),
            false,
        ),
    ];
    let mut expected = Vec::new();
    for (wire, expect, why, had_token) in cases {
        e.mock
            .push("/ai/generate-stream", sse_bytewise(wire.as_bytes()));
        let (status, headers, body) = call_post(
            &e,
            "/v1/chat/completions",
            TOKEN,
            chat_with("x", json!({"stream": true})),
        )
        .await;
        assert_eq!(status, 200);
        assert_sse_headers(&headers);
        assert_eq!(zero_created(&text(&body)), expect);
        assert!(!text(&body).contains("private"));
        expected.push(row("chat", m, "error", 0, &why, "", 0));
        if had_token {
            expected.push(row("chat", m, "ok", 1, "", "", 0));
        }
    }
    assert_eq!(rows(&e).await, expected);
    // 空 token 不发也不数；没有 [DONE] 就结束照样收尾
    e.mock.push(
        "/ai/generate-stream",
        sse_script(
            b"data: {\"token\": \"\"}\n\ndata: {\"token\": \"a\"}\n\ndata: {\"token\": \"b\"}\n\n",
        ),
    );
    let (_, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_with("x", json!({"stream": true})),
    )
    .await;
    let expect = format!(
        "{}{}{}{}data: [DONE]\n\n",
        chunk(m, "{}", "null"),
        content(m, "a"),
        content(m, "b"),
        chunk(m, "{}", "\"stop\"")
    );
    assert_eq!(zero_created(&text(&body)), expect);
}

#[tokio::test]
async fn chat_non_stream_failures_become_http_errors() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    // 中途坏掉：整个请求 502（此前读到的 token 仍然记账）
    e.mock.push(
        "/ai/generate-stream",
        sse_script(
            b"data: {\"token\": \"hello\"}\n\nevent: error\ndata: {\"message\":\"private\"}\n\n",
        ),
    );
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
    assert_gate_error(status, &body, 502, "上游文本生成失败，请稍后重试");
    e.mock.push(
        "/ai/generate-stream",
        sse_script(b"data: {\"error\":\"private\"}\n\n"),
    );
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
    assert_gate_error(status, &body, 502, "上游文本生成失败，请稍后重试");
    // 空 token 跳过；没有 [DONE] 也算读完
    e.mock.push(
        "/ai/generate-stream",
        sse_script(b"data: {\"token\": \"a\"}\n\ndata: {\"token\": \"b\"}\n\n"),
    );
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
    assert_eq!(status, 200);
    assert_eq!(json_of(&body)["choices"][0]["message"]["content"], "ab");
    // 上游在开流之前就拒绝
    for (up, out) in [(400, 400), (422, 422), (500, 502), (503, 503)] {
        e.mock.push(
            "/ai/generate-stream",
            Reply::json(up, &json!({"message": "private"})),
        );
        let (status, _, body) =
            call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
        assert_gate_error(
            status,
            &body,
            out,
            &format!("上游文本请求失败（HTTP {up}）"),
        );
    }
    e.mock.push("/ai/generate-stream", Reply::model_403("x"));
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
    assert_gate_error(
        status,
        &body,
        400,
        "上游不认识这个文本模型；本站可用的文本模型：llama-3-erato-v1、kayra-v1",
    );
    assert_eq!(
        rows(&e).await,
        vec![
            row("chat", m, "error", 0, "上游文本生成失败，请稍后重试", "", 0),
            row("chat", m, "ok", 1, "", "", 0),
            row("chat", m, "error", 0, "上游文本生成失败，请稍后重试", "", 0),
            row("chat", m, "ok", 2, "", "", 0),
            row(
                "chat",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 400）",
                "",
                400
            ),
            row(
                "chat",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 422）",
                "",
                422
            ),
            row(
                "chat",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 500）",
                "",
                500
            ),
            row(
                "chat",
                m,
                "error",
                0,
                "上游文本请求失败（HTTP 503）",
                "",
                503
            ),
            row(
                "chat",
                m,
                "error",
                0,
                "上游不认识这个文本模型；本站可用的文本模型：llama-3-erato-v1、kayra-v1",
                "",
                403
            ),
        ]
    );
}

#[tokio::test]
async fn chat_stream_rejections_are_plain_json_errors_too() {
    let e = setup(&[]).await;
    e.mock.push(
        "/ai/generate-stream",
        Reply::json(503, &json!({"message": "private"})),
    );
    let (status, headers, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_with("x", json!({"stream": true})),
    )
    .await;
    assert_gate_error(status, &body, 503, "上游文本请求失败（HTTP 503）");
    assert_eq!(ct(&headers), "application/json");
    e.mock
        .push("/ai/generate-stream", Reply::rate_limited("30"));
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_with("x", json!({}))).await;
    assert_gate_error(
        status,
        &body,
        429,
        "上游限流(429)，文本 / 语音通道暂停约 30 秒，请稍后再试",
    );
    let (status, _, body) = call_post(
        &e,
        "/v1/chat/completions",
        TOKEN,
        chat_with("x", json!({"stream": true})),
    )
    .await;
    assert_eq!(status, 503, "{}", text(&body));
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn chat_client_disconnect_mid_stream_records_and_releases() {
    let e = setup(&[]).await;
    let chunks: Vec<(Duration, Bytes)> = (0..10)
        .map(|i| {
            (
                Duration::from_millis(100),
                Bytes::from(format!("data: {{\"token\":\"t{i}\"}}\n\n")),
            )
        })
        .collect();
    e.mock.push(
        "/ai/generate-stream",
        Reply::Chunks {
            status: 200,
            content_type: "text/event-stream".into(),
            chunks,
        },
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&chat_body(json!({"stream": true, "max_tokens": 10}))).unwrap(),
        ))
        .unwrap();
    let resp = gate_server::app(e.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let mut body = resp.into_body();
    // 起始分块之后再等一个 token
    let _ = body.frame().await.unwrap().unwrap();
    let _ = body.frame().await.unwrap().unwrap();
    drop(body);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let rows = rows(&e).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].2.as_str()), ("chat", "ok"));
    assert!((1..=4).contains(&rows[0].3), "{rows:?}");
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

fn slow_script(content_type: &str, pieces: Vec<(u64, String)>) -> Reply {
    Reply::Chunks {
        status: 200,
        content_type: content_type.into(),
        chunks: pieces
            .into_iter()
            .map(|(ms, s)| (Duration::from_millis(ms), Bytes::from(s)))
            .collect(),
    }
}

#[tokio::test]
async fn non_stream_requests_finish_and_record_even_if_the_client_gives_up() {
    let e = setup(&[]).await;
    let m = "llama-3-erato-v1";
    // 补全：上游 300 毫秒后才回，客户端 80 毫秒就走了
    e.mock.push(
        "/ai/generate",
        slow_script(
            "application/json",
            vec![(300, r#"{"output":"abcdefg"}"#.into())],
        ),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai/generate")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&native(5)).unwrap()))
        .unwrap();
    let gave_up = tokio::time::timeout(
        Duration::from_millis(80),
        gate_server::app(e.state.clone()).oneshot(req),
    )
    .await;
    assert!(gave_up.is_err());
    // 聊天（非流式）：读到一半走掉，后面的 token 照样读完、记账
    let pieces = (0..6)
        .map(|i| (100, format!("data: {{\"token\":\"t{i}\"}}\n\n")))
        .collect();
    e.mock.push(
        "/ai/generate-stream",
        slow_script("text/event-stream", pieces),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&chat_body(json!({"max_tokens": 6}))).unwrap(),
        ))
        .unwrap();
    let gave_up = tokio::time::timeout(
        Duration::from_millis(250),
        gate_server::app(e.state.clone()).oneshot(req),
    )
    .await;
    assert!(gave_up.is_err());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        rows(&e).await,
        vec![
            row("text", m, "ok", 2, "", "", 200),
            row("chat", m, "ok", 6, "", "", 0)
        ]
    );
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn an_open_stream_keeps_the_key_slot_until_it_is_done() {
    // 每把 Key 同时只跑一个请求（默认 KEY_CONCURRENCY=1）：第二个排队，排不上就 429
    let e = setup(&[("QUEUE_TIMEOUT", "1")]).await;
    let pieces = (0..5)
        .map(|i| (300, format!("data: {{\"token\":\"t{i}\"}}\n\n")))
        .collect();
    e.mock.push(
        "/ai/generate-stream",
        slow_script("text/event-stream", pieces),
    );
    let state = e.state.clone();
    let first = tokio::spawn(async move {
        Call::new("POST", "/ai/generate-stream")
            .bearer(TOKEN)
            .json(native(5))
            .send(&state)
            .await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, _, body) =
        call_post(&e, "/v1/chat/completions", TOKEN, chat_body(json!({}))).await;
    assert_gate_error(status, &body, 429, "排队等待超时，当前站点繁忙，请稍后再试");
    let (status, _, _) = first.await.unwrap();
    assert_eq!(status, 200);
    // 流结束、结算完之后槽归还：下一个请求正常
    let (status, _, _) = call_post(&e, "/ai/generate-stream", TOKEN, native(2)).await;
    assert_eq!(status, 200);
    let all = rows(&e).await;
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(
        all[0],
        row(
            "chat",
            "",
            "rejected",
            0,
            "429 排队等待超时，当前站点繁忙，请稍后再试",
            "queue_timeout",
            0
        )
    );
}

/// 真实 TCP 连接：上游正好沉默的时候成员断开，网关也要马上发现、记账、归还名额
/// （旧版靠 ASGI 的 http.disconnect；这里靠响应体被丢弃）。
#[tokio::test]
async fn disconnect_is_noticed_while_the_upstream_is_silent() {
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let e = setup(&[]).await;
    e.mock.push(
        "/ai/generate-stream",
        Reply::Chunks {
            status: 200,
            content_type: "text/event-stream".into(),
            chunks: vec![
                (
                    Duration::ZERO,
                    Bytes::from_static(b"data: {\"token\":\"a\"}\n\n"),
                ),
                (
                    Duration::from_secs(30),
                    Bytes::from_static(b"data: {\"token\":\"b\"}\n\n"),
                ),
            ],
        },
    );
    let app = gate_server::app(e.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
                SocketAddr,
            >(app),
        )
        .await
        .unwrap();
    });
    let body = serde_json::to_vec(&native(5)).unwrap();
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /ai/generate-stream HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    conn.write_all(&body).await.unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 4096];
    while !text(&got).contains("\"a\"") {
        let n = conn.read(&mut buf).await.unwrap();
        assert!(n > 0, "连接提前关闭：{}", text(&got));
        got.extend_from_slice(&buf[..n]);
    }
    assert!(text(&got).starts_with("HTTP/1.1 200"));
    let gone = Instant::now();
    drop(conn);
    let mut recorded = Vec::new();
    while gone.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        recorded = rows(&e).await;
        if !recorded.is_empty() {
            break;
        }
    }
    assert_eq!(
        recorded,
        vec![row("text", "llama-3-erato-v1", "ok", 1, "", "", 0)]
    );
    assert!(gone.elapsed() < Duration::from_secs(3));
    assert_eq!(
        e.state
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    server.abort();
}
