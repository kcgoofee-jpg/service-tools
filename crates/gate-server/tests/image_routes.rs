//! 出图请求通路的端到端测试：真实的 AppState + 路由 + 记账，上游换成本地假服务。

mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TOKEN, setup};
use gate_db::repo::usage::LogFilter;
use gate_server::AppState;
use gate_testkit::Reply;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn image_body(model: &str, n: i64) -> Value {
    json!({
        "input": "1girl, solo", "model": model, "action": "generate",
        "parameters": {"width": 832, "height": 1216, "steps": 28, "scale": 5, "sampler": "k_euler_ancestral",
                       "n_samples": n, "seed": 1234, "negative_prompt": "lowres"},
    })
}

async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(a) = auth {
        b = b.header("authorization", format!("Bearer {a}"));
    }
    let req = match body {
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

async fn logs(state: &AppState) -> Vec<Value> {
    // 记账在后台任务里：等它们写完
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    state
        .db
        .list_logs(50, 0, &LogFilter::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn generate_image_ok_records_usage() {
    let e = setup(&[]).await;
    let (status, headers, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(body.starts_with(b"PK"));
    assert!(headers.get("x-request-id").is_some());
    let sent = e.mock.requests_to("/ai/generate-image");
    assert_eq!(sent.len(), 1);
    let rows = logs(&e.state).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "ok");
    assert_eq!(rows[0]["kind"], "image");
    assert_eq!(rows[0]["images"], 1);
    let detail = rows[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with("832x1216/28step 免费"), "{detail}");
    let c = e.state.db.get_counter(1, &e.state.day()).await.unwrap();
    assert_eq!((c.images, c.legacy_free_images), (1, 1));
}

#[tokio::test]
async fn auth_errors_have_novelai_shape() {
    let e = setup(&[]).await;
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        None,
        Some(image_body("x", 1)),
    )
    .await;
    assert_eq!(status, 401);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["message"], "猫头鹰公益站提醒：缺少 API Key");
    assert_eq!(v["statusCode"], 401);
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some("nai-wrong"),
        Some(image_body("x", 1)),
    )
    .await;
    assert_eq!(status, 401);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["message"], "猫头鹰公益站提醒：无效的 API Key");
    assert!(e.mock.requests().is_empty());
}

#[tokio::test]
async fn unknown_model_is_rejected_and_logged() {
    let e = setup(&[]).await;
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("not-a-model", 1)),
    )
    .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let rows = logs(&e.state).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["status"], "rejected");
    assert!(e.mock.requests_to("/ai/generate-image").is_empty());
}

#[tokio::test]
async fn upstream_429_pauses_all_image_generation() {
    let e = setup(&[]).await;
    e.mock.push("/ai/generate-image", Reply::rate_limited("30"));
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 429, "{}", String::from_utf8_lossy(&body));
    assert!(e.state.cooldown.remaining(e.state.now()) >= 59);
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 429);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("所有图片生成暂停约"),
        "{v}"
    );
    let rows = logs(&e.state).await;
    let reasons: Vec<&str> = rows
        .iter()
        .map(|r| r["reason"].as_str().unwrap_or(""))
        .collect();
    assert!(reasons.contains(&"cooldown"), "{rows:?}");
    assert_eq!(e.mock.requests_to("/ai/generate-image").len(), 1);
}

#[tokio::test]
async fn stream_sse_ok() {
    let e = setup(&[]).await;
    let (status, headers, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image-stream",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["content-type"], "text/event-stream");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("final"), "{text}");
    let rows = logs(&e.state).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], "image_stream");
    assert_eq!(rows[0]["status"], "ok");
    assert_eq!(rows[0]["images"], 1);
}

#[tokio::test]
async fn stream_bad_wire_format_is_400_json() {
    let e = setup(&[]).await;
    let mut body = image_body("nai-diffusion-4-5-full", 1);
    body["parameters"]["stream"] = json!("xml");
    let (status, _, out) = call(
        &e.state,
        "POST",
        "/ai/generate-image-stream",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(status, 400);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["message"],
        "猫头鹰公益站提醒：stream 仅支持 sse 或 msgpack"
    );
}

#[tokio::test]
async fn stream_upstream_failure_before_start_is_plain_json() {
    let e = setup(&[]).await;
    e.mock.push(
        "/ai/generate-image-stream",
        Reply::json(500, &json!({"message": "boom"})),
    );
    let (status, headers, out) = call(
        &e.state,
        "POST",
        "/ai/generate-image-stream",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert!(status.is_server_error(), "{status}");
    assert_eq!(headers["content-type"], "application/json");
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("{\"error\": "), "{text}");
    let rows = logs(&e.state).await;
    assert!(rows.iter().any(|r| r["status"] == "error"), "{rows:?}");
}

#[tokio::test]
async fn suggest_tags_get() {
    let e = setup(&[]).await;
    let (status, _, out) = call(
        &e.state,
        "GET",
        "/ai/generate-image/suggest-tags?model=nai-diffusion-4-5-full&prompt=1g",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&out));
    let sent = e.mock.requests_to("/ai/generate-image/suggest-tags");
    assert_eq!(sent[0].query, "prompt=1g&model=nai-diffusion-4-5-full");
    let rows = logs(&e.state).await;
    assert_eq!(rows[0]["kind"], "tags");
    assert_eq!(rows[0]["status"], "ok");
}

#[tokio::test]
async fn v1_alias_and_root_post_route_to_generate() {
    let e = setup(&[]).await;
    let (status, _, _) = call(
        &e.state,
        "POST",
        "/v1/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = call(
        &e.state,
        "POST",
        "/",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn generation_audit_records_prompt_and_image() {
    let e = setup(&[("AUDIT_PROMPTS", "1"), ("AUDIT_THUMBS", "1")]).await;
    let (status, _, _) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(e.state.db.audit_image_count(1, 0.0).await.unwrap(), 1);
    assert!(e.state.db.audit_image_bytes().await.unwrap() > 0);
}

#[tokio::test]
async fn single_key_upstream_500s_do_not_page_owner() {
    let e = setup(&[]).await;
    assert!(!e.state.note_upstream_5xx(1));
    assert!(!e.state.note_upstream_5xx(1));
    assert!(e.state.note_upstream_5xx(2));
}
