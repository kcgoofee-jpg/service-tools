//! 出图请求通路的端到端测试：真实的 AppState + 路由 + 记账，上游换成本地假服务。

mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TOKEN, setup};
use gate_db::repo::usage::{LogFilter, Success};
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

/// 每人只有每日上限：2.x 的每人注水（底仓 30 张、每小时补 4 张，一天最多约 126 张）已删除
/// （docs/v3-deviations.md #62）。上限 200、今天已用 150 张的算法管理成员照样能出图，不会被 `paced` 拦下；
/// 用满上限才是「今日 V4.5 免费图额度已用完」。
#[tokio::test]
async fn no_per_member_pacing_below_the_daily_cap() {
    let e = setup(&[]).await;
    e.state
        .db
        .write(|tx| {
            tx.execute(
                "UPDATE api_keys SET daily_images=200, quota_auto=1 WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let used = |n: i64| Success {
        key_id: 1,
        kind: "image".into(),
        model: "nai-diffusion-4-5-full".into(),
        day: e.state.day(),
        images: n,
        legacy_free_images: n,
        ..Default::default()
    };
    e.state
        .db
        .record_success(&used(150), e.state.now())
        .await
        .unwrap();
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    // 用满 200 张：个人额度用完（402），不是「出图速度过快」
    e.state
        .db
        .record_success(&used(49), e.state.now())
        .await
        .unwrap();
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 402, "{}", String::from_utf8_lossy(&body));
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("今日 V4.5 免费图额度已用完（200 张/天）"),
        "{text}"
    );
    assert!(!text.contains("每小时自动恢复"), "{text}");
}

/// 每把 Key 的限速只剩排队与 RPM 两层（docs/v3-deviations.md #64）：
/// - 同一把 Key 同时只生成 1 张：第二张排队、等第一张出完才发往上游（即使还有空着的上游账号）——
///   这是排队本身做的，旧版靠每把 Key 一个的信号量；
/// - 再多的并发请求是「上一张还没出完」（`key_busy`），不再有「同时进行中的请求过多」（`key_rate`）；
/// - 前一张出完，下一张马上能发，没有 15 秒的每 Key 出图间隔。
#[tokio::test]
async fn one_image_per_key_at_a_time_without_per_key_interval() {
    let e = setup(&[("NAI_TOKENS", "pst-synthetic-one,pst-synthetic-two")]).await;
    // 账号级的随机抖动（0～5 秒）与这里无关，关掉免得计时不稳
    e.state
        .guard
        .with(|g| g.values.insert("interval_jitter".into(), 0));
    e.mock.push(
        "/ai/generate-image",
        Reply::Chunks {
            status: 200,
            content_type: "binary/octet-stream".into(),
            chunks: vec![(
                std::time::Duration::from_millis(600),
                bytes::Bytes::from(gate_testkit::zip_of_pngs(1)),
            )],
        },
    );
    let spawn = |state: AppState| {
        tokio::spawn(async move {
            call(
                &state,
                "POST",
                "/ai/generate-image",
                Some(TOKEN),
                Some(image_body("nai-diffusion-4-5-full", 1)),
            )
            .await
        })
    };
    let first = spawn(e.state.clone());
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let second = spawn(e.state.clone());
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // 第一张在上游，第二张在排队：第二个上游账号空着也不发
    assert_eq!(e.mock.requests_to("/ai/generate-image").len(), 1);
    let view = e.state.guard.with(|g| g.queue_view(Some(1)));
    assert_eq!(
        (view["running"].as_i64(), view["waiting"].as_i64()),
        (Some(1), Some(1))
    );
    // 第三、四张：排队满了（同时生成 1 张 + 排队 1 张）
    for _ in 0..2 {
        let (status, _, body) = call(
            &e.state,
            "POST",
            "/ai/generate-image",
            Some(TOKEN),
            Some(image_body("nai-diffusion-4-5-full", 1)),
        )
        .await;
        let text = String::from_utf8_lossy(&body).to_string();
        assert_eq!(status, 429, "{text}");
        assert!(text.contains("上一张图还没出完"), "{text}");
    }
    let (s1, _, _) = first.await.unwrap();
    let (s2, _, _) = second.await.unwrap();
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(e.mock.requests_to("/ai/generate-image").len(), 2);
    // 前一张出完，下一张立刻能发（没有每 Key 15 秒间隔）
    let t0 = std::time::Instant::now();
    let (status, _, _) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-4-5-full", 1)),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        t0.elapsed()
    );
    let rows = logs(&e.state).await;
    let reasons: Vec<&str> = rows
        .iter()
        .filter(|r| r["status"] == "rejected")
        .map(|r| r["reason"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(reasons, ["key_busy", "key_busy"], "{rows:?}");
}

/// 「V5 续杯」已删除（docs/v3-deviations.md #66）：旧版 `anlas_auto` 的 Key 当天个人 V5 用完、
/// 账号 V5 也见底时会改用 Anlas 出同规格 V5；现在个人 V5 用完就是 402，不会悄悄花 Anlas。
#[tokio::test]
async fn no_v5_refill_with_anlas() {
    let e = setup(&[]).await;
    e.state
        .db
        .write(|tx| {
            tx.execute(
                "UPDATE api_keys SET anlas_auto=1, allow_anlas=1, daily_v5=1, image_model_scope='all' WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    e.state
        .db
        .set_setting("quota_algo_last", r#"{"v5": {"percent": 0}}"#)
        .await
        .unwrap();
    e.state
        .db
        .record_success(
            &Success {
                key_id: 1,
                kind: "image".into(),
                model: "nai-diffusion-5-full".into(),
                day: e.state.day(),
                images: 1,
                v5: 1,
                ..Default::default()
            },
            e.state.now(),
        )
        .await
        .unwrap();
    let (status, _, body) = call(
        &e.state,
        "POST",
        "/ai/generate-image",
        Some(TOKEN),
        Some(image_body("nai-diffusion-5-full", 1)),
    )
    .await;
    let text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(status, 402, "{text}");
    assert!(text.contains("今日 V5 额度已用完（1 张/天）"), "{text}");
    assert!(e.mock.requests_to("/ai/generate-image").is_empty());
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
    let stored = common::query(&e.state.db, "SELECT client FROM usage_log").await;
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
    // 记录开关只来自后台设置（环境变量 AUDIT_PROMPTS / AUDIT_THUMBS 已删除，docs/v3-deviations.md #93）
    let e = setup(&[]).await;
    e.state.db.set_setting("audit_prompts", "1").await.unwrap();
    e.state.db.set_setting("audit_thumbs", "1").await.unwrap();
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
    let rows = common::query(
        &e.state.db,
        "SELECT COUNT(*) AS n FROM generation_audit WHERE key_id=1",
    )
    .await;
    assert_eq!(rows[0]["n"], 1);
    assert!(e.state.db.audit_image_bytes().await.unwrap() > 0);
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

/// 记账恒等式（原「观测」「容量」模块在线上核对的，现在由测试守住）：每个成功的出图请求，
/// 今日计数器、用量日志、生成记录三处的张数一致；内存里的每小时计数等于本小时日志里的张数，且不超过上限。
#[tokio::test]
async fn accounting_identity_across_counters_logs_and_audit() {
    let e = setup(&[]).await;
    e.state.db.set_setting("audit_prompts", "1").await.unwrap();
    e.state.db.set_setting("audit_thumbs", "1").await.unwrap();
    for n in [1, 2] {
        let (status, _, body) = call(
            &e.state,
            "POST",
            "/ai/generate-image",
            Some(TOKEN),
            Some(image_body("nai-diffusion-4-5-full", n)),
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let counter = e
        .state
        .db
        .get_counter(1, &e.state.day())
        .await
        .unwrap()
        .images;
    let logged = common::query(
        &e.state.db,
        "SELECT COALESCE(SUM(images), 0) AS n FROM usage_log WHERE status='ok' AND kind LIKE 'image%'",
    )
    .await[0]["n"]
        .as_i64()
        .unwrap();
    let audited = common::query(
        &e.state.db,
        "SELECT COUNT(*) AS n FROM generation_audit WHERE status='ok'",
    )
    .await[0]["n"]
        .as_i64()
        .unwrap();
    // 每张成功出图一条生成记录（一次请求多张时网关按实际出的张数记）
    assert!(logged >= 2, "{logged}");
    assert_eq!((counter, audited), (logged, logged));
    let now = e.state.now();
    let tokens = e.state.nai.tokens().await;
    let in_memory: i64 = tokens
        .iter()
        .map(|t| e.state.guard.with(|g| g.hour_count(&t.token_id, now)))
        .sum();
    assert_eq!(in_memory, logged, "内存每小时计数 = 本小时日志张数");
    let cap = e.state.guard.with(|g| g.hourly_cap(now));
    assert!(in_memory <= cap * tokens.len() as i64);
}
