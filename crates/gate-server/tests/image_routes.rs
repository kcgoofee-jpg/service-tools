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

/// 不识别客户端、不记设备（docs/v3-deviations.md #50）：不管 User-Agent、启动器请求头还是请求体长什么样，
/// 用量日志的 client 列都是空串，后台日志接口也不返回这个字段。
#[tokio::test]
async fn usage_log_never_records_a_client() {
    let e = setup(&[]).await;
    let mut body = image_body("nai-diffusion-4-5-full", 1);
    body["parameters"]["ucPresetId"] = json!(1);
    let req = Request::builder()
        .method("POST")
        .uri("/ai/generate-image")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("user-agent", "DSH-Tavern-Image/1.2 (Windows NT 10.0)")
        .header("x-correlation-id", "synthetic")
        .header("x-initiated-at", "1")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = gate_server::app(e.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let rows = logs(&e.state).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("client").is_none(), "{}", rows[0]);
    let stored = e
        .state
        .db
        .execute_fetchall_compat("SELECT client FROM usage_log", &[])
        .await
        .unwrap();
    assert_eq!(stored, [json!({"client": ""})]);
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

/// 停用的 Key：按停用原因给出说明（docs/v3-deviations.md #47）。全站重置停掉的要告诉成员去重新领。
#[tokio::test]
async fn disabled_key_message_depends_on_reason() {
    let e = setup(&[]).await;
    let msg = |body: &[u8]| -> String {
        let v: Value = serde_json::from_slice(body).unwrap();
        v["error"]["message"]
            .as_str()
            .or(v["message"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let reason = |state: gate_server::AppState| async move {
        state
            .db
            .get_key(1)
            .await
            .unwrap()
            .unwrap()
            .get("disabled_reason")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let request = || {
        call(
            &e.state,
            "POST",
            "/ai/generate-image",
            Some(TOKEN),
            Some(image_body("x", 1)),
        )
    };

    // 全站重置
    e.state.db.reset_all_keys().await.unwrap();
    assert_eq!(reason(e.state.clone()).await, "site_reset");
    let (status, _, body) = request().await;
    assert_eq!(status, 403);
    assert!(
        msg(&body).contains("全站重置") && msg(&body).contains("/register"),
        "{}",
        msg(&body)
    );

    // 重新启用：原因清掉；站长手动停用：另一种说明
    let on = json!({"enabled": true});
    e.state
        .db
        .update_key(1, on.as_object().unwrap())
        .await
        .unwrap();
    assert_eq!(reason(e.state.clone()).await, Value::Null);
    let off = json!({"enabled": false});
    e.state
        .db
        .update_key(1, off.as_object().unwrap())
        .await
        .unwrap();
    assert_eq!(reason(e.state.clone()).await, "admin");
    let (status, _, body) = request().await;
    assert_eq!(status, 403);
    assert!(
        msg(&body).ends_with("该 Key 已被站长停用，有疑问请联系站长。"),
        "{}",
        msg(&body)
    );
    assert!(e.mock.requests().is_empty());
}

/// 被临时暂停的 Key：没写原因时只说「已被临时暂停，到点自动恢复」（防分享体系已删，不再提「异常多地使用」）；
/// 写了原因（自动驾驶的限流）就用原因。
#[tokio::test]
async fn paused_key_message_without_reason_is_neutral() {
    let e = setup(&[]).await;
    let msg = |body: &[u8]| -> String {
        let v: Value = serde_json::from_slice(body).unwrap();
        v["error"]["message"]
            .as_str()
            .or(v["message"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let request = || {
        call(
            &e.state,
            "POST",
            "/ai/generate-image",
            Some(TOKEN),
            Some(image_body("x", 1)),
        )
    };
    let until = e.state.now() + 600.0;

    e.state.pauses.set(1, until, String::new());
    let (status, _, body) = request().await;
    assert_eq!(status, 403);
    let m = msg(&body);
    assert!(
        m.contains("这把 Key 已被临时暂停，") && m.ends_with(" 自动恢复。"),
        "{m}"
    );
    assert!(!m.contains("多地") && !m.contains("仅限本人"), "{m}");

    e.state.pauses.set(
        1,
        until,
        "你的 Key 短时间内被大量拒绝，已暂停 1 小时".into(),
    );
    let (status, _, body) = request().await;
    assert_eq!(status, 403);
    let m = msg(&body);
    assert!(
        m.contains("你的 Key 短时间内被大量拒绝，已暂停 1 小时，") && m.ends_with(" 自动恢复。"),
        "{m}"
    );
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
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-accel-buffering"], "no");
    let rows = logs(&e.state).await;
    assert!(rows.iter().any(|r| r["status"] == "error"), "{rows:?}");
    // 客户端拿到了 JSON 错误，不是「断开」：不能记 disconnect Bug（会私信站长）
    let bugs = e.state.bugs.recent();
    assert!(!bugs.iter().any(|b| b.source == "disconnect"), "{bugs:?}");
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
    let (_, total) = e.state.db.list_audit(10, 0, Some(1)).await.unwrap();
    assert_eq!(total, 1);
    assert!(e.state.db.audit_image_bytes().await.unwrap() > 0);
}

#[tokio::test]
async fn single_key_upstream_500s_do_not_page_owner() {
    let e = setup(&[]).await;
    assert!(!e.state.note_upstream_5xx(1));
    assert!(!e.state.note_upstream_5xx(1));
    assert!(e.state.note_upstream_5xx(2));
}

#[tokio::test]
async fn cors_preflight_allows_authorization_and_fallbacks_are_json() {
    let e = setup(&[]).await;
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/ai/generate-image")
        .header("origin", "https://client.example")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(Body::empty())
        .unwrap();
    let resp = gate_server::app(e.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    assert_eq!(h["access-control-allow-origin"], "*");
    assert!(
        h["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("authorization")
    );
    let (status, _, body) = call(&e.state, "GET", "/no-such-path", None, None).await;
    assert_eq!(
        (status.as_u16(), String::from_utf8_lossy(&body).to_string()),
        (404, r#"{"detail":"Not Found"}"#.to_string())
    );
    let (status, _, body) = call(&e.state, "GET", "/ai/generate-image", Some(TOKEN), None).await;
    assert_eq!(
        (status.as_u16(), String::from_utf8_lossy(&body).to_string()),
        (405, r#"{"detail":"Method Not Allowed"}"#.to_string())
    );
}
