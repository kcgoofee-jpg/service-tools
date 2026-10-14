//! 出图、流式出图、Vibe 编码、图片工具、标签补全（旧版 `app/routers/image_routes.py`）。
//!
//! 取消语义（与旧版一致）：
//! - 排队阶段（出图间隔、并发槽、额度核对）客户端断开 = 直接取消，不记账；
//! - 一旦开始上游操作，就在后台任务里跑完并记账（成员断开也不中断），额度预留 / 并发槽 / 在途名额
//!   随任务一起释放；流式请求在真正发往上游之前断开仍可取消（记一条「cancelled」）。

use crate::accounting::Entry;
use crate::admission::{
    self, Est, acquire_slots, image_admission, refund_local_reject, reserve_image_budget,
    wait_for_user_image_slot,
};
use crate::auth::{
    authenticate, check_image_cooldown, check_rpm, ctx_of, limit_inflight, require_feature,
};
use crate::body::{IMAGE_BODY_LIMIT, read_image_payload, read_json};
use crate::error::{ApiError, GateError, InternalError, json_response, upstream_transport};
use crate::key::Key;
use crate::media::{MAX_RESPONSE_BYTES, prepare_tool, validate_result};
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use futures::StreamExt;
use gate_core::image_events::{ImageEventTracker, WireFormat, encode_image_event};
use gate_core::policy::{self, REFERENCE_FIELDS, VIBE_ENCODING_ANLAS};
use gate_core::pyc::take_chars;
use gate_core::pyval::{py_str, truthy};
use gate_core::reasons;
use gate_core::upstream_errors::upstream_error_message;
use gate_upstream::client::{BoxFuture, Callbacks, RequestOpts, UpstreamError};
use serde_json::{Map, Value, json};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn router() -> Router<AppState> {
    let mut r = Router::new();
    for p in ["/ai/generate-image", "/nai/ai/generate-image", "/"] {
        r = r.route(p, post(generate_image));
    }
    for p in ["/ai/generate-image-stream", "/nai/ai/generate-image-stream"] {
        r = r.route(p, post(generate_image_stream));
    }
    for p in ["/ai/encode-vibe", "/nai/ai/encode-vibe"] {
        r = r.route(p, post(encode_vibe));
    }
    for p in ["/ai/upscale", "/nai/ai/upscale"] {
        r = r.route(p, post(upscale));
    }
    for p in ["/ai/augment-image", "/nai/ai/augment-image"] {
        r = r.route(p, post(augment));
    }
    for p in [
        "/ai/generate-image/suggest-tags",
        "/nai/ai/generate-image/suggest-tags",
    ] {
        r = r.route(p, post(suggest_tags).get(suggest_tags));
    }
    r
}

// ------------------------------------------------------------------ 公共小件

/// 上游错误 → 成员接口错误（旧版 `upstream_call` 的转换）。网关回调的拒绝原样穿过。
fn up_to_gate(e: UpstreamError) -> GateError {
    if e.local {
        return GateError::with_code(e.status, e.message, e.code);
    }
    let status = if matches!(e.status, 429 | 503) {
        e.status
    } else {
        502
    };
    GateError::with_code(status, e.message, e.code)
        .uncertain(e.billing_uncertain)
        .from_upstream(true)
}

fn local(e: ApiError) -> UpstreamError {
    match e {
        ApiError::Gate(g) => UpstreamError::local(g.status, g.message, g.code),
        ApiError::Http(h) => UpstreamError::local(h.status, py_str(&h.detail), ""),
        ApiError::Internal(i) => UpstreamError::local(500, i.0, ""),
    }
}

fn detail160(status: u16, message: &str) -> String {
    take_chars(&format!("{status} {message}"), 160).to_string()
}

/// 上游 429：暂停全站图片请求（两种响应都在收尾时记日志，回调只更新冷却）。
fn on_429(state: &AppState) -> Box<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync> {
    let s = state.clone();
    Box::new(move |retry_after| {
        let s = s.clone();
        Box::pin(async move {
            let secs = s.limits().image_429_cooldown_seconds.max(retry_after);
            s.block_image_generation(secs).await;
        })
    })
}

fn before_dispatch(state: &AppState) -> Box<dyn Fn() -> Result<(), UpstreamError> + Send + Sync> {
    let s = state.clone();
    Box::new(move || check_image_cooldown(&s).map_err(|g| local(g.into())))
}

/// 已经开始的上游操作：放进后台任务跑完（含记账），调用方断开也不中断。
/// `delivered`：结果已经完整交给客户端（之后断开不算「没拿到图」）。
async fn shielded<T: Send + 'static>(
    state: &AppState,
    ctx: &ReqCtx,
    fut: impl Future<Output = Result<T, ApiError>> + Send + 'static,
) -> Result<T, ApiError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let s = state.clone();
    let c = ctx.clone();
    state.tasks.spawn(ctx.clone().scope(async move {
        let r = fut.await;
        if tx.send(r).is_err() {
            s.bugs.capture(crate::bugs::Report {
                source: "disconnect",
                title: "客户端在图片返回前断开连接（图已生成并计数）",
                level: "warn",
                rid: &c.rid(),
                key_id: c.key().map(|(id, _)| id),
                ..Default::default()
            });
        }
    }));
    rx.await
        .unwrap_or_else(|_| Err(InternalError("后台任务异常结束".into()).into()))
}

fn binary(content: Bytes, media: &str, no_store: bool) -> Response {
    let mut r = Response::new(Body::from(content));
    r.headers_mut().insert(
        "content-type",
        HeaderValue::from_str(media)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if no_store {
        r.headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
    }
    r
}

/// Python `int(x)` 用于 `n_samples`（`p.get("n_samples", 1) or 1` 之后）。
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        Value::Number(n) => n.as_f64().and_then(gate_core::pyc::int_of),
        Value::String(s) => gate_core::pyc::parse_int(s),
        _ => None,
    }
}

/// 这次请求的形态摘要（只用我们自己发出的数据，绝不碰上游响应体）。上游 5xx 时记下，
/// 判断「这位成员的请求特征」还是「上游抽风」。
fn request_shape(body: &Value) -> String {
    let empty = Map::new();
    let p = body
        .get("parameters")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let g = |k: &str| py_str(p.get(k).unwrap_or(&Value::Null));
    let flag = |k: &str| u8::from(p.get(k).is_some_and(truthy));
    let refs = match p.get("reference_image_multiple") {
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    };
    let chars = p
        .get("v4_prompt")
        .and_then(|v| v.get("caption"))
        .and_then(|c| c.get("char_captions"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let text_len = |v: Option<&Value>| match v {
        None | Some(Value::Null) => 0,
        Some(v) if !truthy(v) => 0,
        Some(v) => gate_core::pyc::len_chars(&py_str(v)),
    };
    let mut keys: Vec<&String> = p.keys().collect();
    keys.sort();
    let keys = keys
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let vibe = p.get("director_reference_images").is_some_and(truthy)
        || p.get("director_reference_images_cached")
            .is_some_and(truthy);
    format!(
        "{} · {}x{} · {}步 · n={} · action={} · prompt={}字 uc={}字 角色={} · sampler={} v={} sm={}/{} qt={} ucPreset={} · 图={} 蒙版={} 参考={} vibe={} 原图={} · keys={}",
        py_str(body.get("model").unwrap_or(&Value::String("?".into()))),
        g("width"),
        g("height"),
        g("steps"),
        py_str(p.get("n_samples").unwrap_or(&json!(1))),
        py_str(
            body.get("action")
                .unwrap_or(&Value::String("generate".into()))
        ),
        text_len(body.get("input")),
        text_len(p.get("negative_prompt")),
        chars,
        g("sampler"),
        g("params_version"),
        flag("sm"),
        flag("sm_dyn"),
        flag("qualityToggle"),
        g("ucPreset"),
        flag("image"),
        flag("mask"),
        refs,
        u8::from(vibe),
        flag("add_original_image"),
        take_chars(&keys, 220),
    )
}

fn pyerr_400(e: &gate_core::pyext::PyErr, msg: impl Into<String>) -> ApiError {
    use gate_core::pyext::PyErr;
    match e {
        PyErr::Type(_) | PyErr::Value(_) | PyErr::Overflow(_) => GateError::new(400, msg).into(),
        other => InternalError(other.to_string()).into(),
    }
}

// ------------------------------------------------------------------ 出图

async fn generate_image(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match generate(state, req, false, permit).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn generate_image_stream(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match generate(state, req, true, permit).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// 生图请求经过校验、整理后的结果。
struct Prepared {
    key: Key,
    body: Value,
    model: String,
    image_count: i64,
    est: Est,
    legacy_free_images: i64,
    notes: Vec<String>,
    detail: String,
}

async fn generate(
    state: AppState,
    req: Request,
    streaming: bool,
    permit: Option<crate::runtime::InflightPermit>,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_image_cooldown(&state)?;
    check_rpm(&state, &key)?;
    require_feature(&state, &key, "image").await?;
    let body = read_image_payload(&parts.headers, body, IMAGE_BODY_LIMIT).await?;
    let prep = prepare(&state, &ctx, key, body).await?;
    if streaming {
        return stream(state, ctx, prep, permit).await;
    }
    let key = prep.key.clone();
    let r = async {
        wait_for_user_image_slot(&state, &key).await?;
        let admission = image_admission(&state, &ctx, &key).await?;
        let slots = acquire_slots(&state, &key, true).await?;
        // 排队 / 预算等待之后再查一次冷却和额度
        check_image_cooldown(&state)?;
        let reserved =
            reserve_image_budget(&state, &key, &prep.est, prep.legacy_free_images).await?;
        let (s, c) = (state.clone(), ctx.clone());
        shielded(&state, &ctx, async move {
            let _held = (admission, slots, permit);
            perform_generation(&s, &c, prep, Arc::new(reserved)).await
        })
        .await
    }
    .await;
    if let Err(ApiError::Gate(_)) = &r {
        refund_local_reject(&state, &ctx, &key).await;
    }
    r
}

/// 参数校验与整理（旧版 `_generate_image` 里到 `quota_image_check` 为止的部分）。
async fn prepare(
    state: &AppState,
    ctx: &ReqCtx,
    key: Key,
    body: Map<String, Value>,
) -> Result<Prepared, ApiError> {
    let mut body = Value::Object(body);
    let model_str = |b: &Value| match b.get("model") {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    };
    ctx.set_model(&model_str(&body));
    ctx.refine_client_from_payload(&body);
    let reject = |model: &str, detail: &str| {
        state.record(
            ctx,
            &key,
            Entry::new("image", model, "rejected").detail(detail),
        );
    };
    if !body.get("parameters").is_some_and(Value::is_object) {
        return Err(GateError::new(400, "生图参数缺失或格式无效").into());
    }
    match policy::normalize_image_request(&mut body, &mut || rand::random::<u32>()) {
        Ok(()) => {}
        Err(gate_core::pyext::PyErr::Value(m)) => {
            let model = py_str(body.get("model").unwrap_or(&Value::String("?".into())));
            reject(take_chars(&model, 80), &m);
            return Err(GateError::new(400, format!("图片参数无效：{m}")).into());
        }
        Err(e) => return Err(InternalError(e.to_string()).into()),
    }
    let mut model = py_str(body.get("model").unwrap_or(&Value::Null));
    policy::fix_name_digit_weights(&mut body); // bm94199:: → bm94199 ::（否则上游当成权重 → NaN）
    policy::sanitize_and_translate_prompts(&mut body); // WebUI 语法 (tag:1.2) → 1.2::tag::
    if let Some(problem) = policy::upstream_parameter_problem(&body) {
        reject(&model, take_chars(&problem, 120));
        return Err(GateError::new(400, problem).into());
    }
    let params_has = |b: &Value, names: &[&str]| {
        names.iter().any(|n| {
            b.get("parameters")
                .and_then(|p| p.get(*n))
                .is_some_and(truthy)
        })
    };
    if params_has(&body, &REFERENCE_FIELDS) {
        require_feature(state, &key, "vibe").await?; // 参考图 / Vibe 经由 generate-image 传入时同样受功能开关约束
    }
    let Some(tier) = policy::image_model_tier(&model) else {
        reject(&model, "未列入本站图片模型白名单");
        return Err(GateError::new(400, "不支持或尚未开放的图片模型").into());
    };
    if tier == "v5" && !key.is_admin() && key.image_model_scope() != "all" {
        reject(&model, "模型权限：仅允许 V4.5 及更低");
        return Err(GateError::new(403, "你的 Key 目前只能用 V4.5 及更低模型").into());
    }
    if body.get("image").is_some_and(truthy) || body.get("mask").is_some_and(truthy) {
        return Err(GateError::new(400, "生图请求的 image 和 mask 请放在 parameters 中").into());
    }
    if let Some(problem) = policy::validate_image_references(&body) {
        return Err(GateError::new(400, problem).into());
    }
    if params_has(&body, &["image", "mask"]) && !key.is_admin() {
        if !(key.allow_img2img() && state.env.allow_img2img) {
            reject(&model, "img2img 未开放");
            return Err(GateError::new(
                400,
                "本站不支持图生图（img2img）/ 局部重绘（会消耗 Anlas）。请在客户端里移除参考图（原图）后再生成",
            )
            .into());
        }
        if !key.allow_anlas() {
            reject(&model, "img2img 未开通 Anlas 权限");
            return Err(
                GateError::new(402, "该 Key 未开通 Anlas 权限，无法使用图生图 / 局部重绘").into(),
            );
        }
    }
    // 免费档钳制：只对没有 Anlas 权限的 Key 生效；有 Anlas 的 Key 可以用超规格参数，按 Anlas 扣
    let mut notes: Vec<String> = Vec::new();
    if state.env.safe_clamp && !key.is_admin() && !key.allow_anlas() {
        let (b, n, problem) =
            policy::clamp_image_params(&body, state.env.max_pixels, state.env.max_steps, true)
                .map_err(|e| pyerr_400(&e, "图片参数无效"))?;
        if let Some(problem) = problem {
            reject(&model, &problem);
            return Err(GateError::new(400, problem).into());
        }
        body = b;
        notes = n;
    }
    let (b, more) = policy::fill_v4_prompt(&body); // V4 / V4.5 缺结构化提示词时上游回 500：照官方网页端补上
    body = b;
    notes.extend(more);
    if tier == "v5" {
        if policy::is_v5_medium(&py_str(body.get("model").unwrap_or(&Value::Null))) {
            let (b, more) = policy::medium_normalize(&body);
            body = b;
            notes.extend(more);
        }
        model = py_str(body.get("model").unwrap_or(&Value::Null));
    }
    // 局部重绘缺蒙版 / 图生图带 SMEA → 上游只会回 500：放在费用估算之前，坏请求不占额度
    let (b, more, problem) = policy::guard_image_action(&body);
    if let Some(problem) = problem {
        reject(&model, &problem);
        return Err(GateError::new(400, problem).into());
    }
    body = b;
    notes.extend(more);
    let p = body.get("parameters").cloned().unwrap_or(Value::Null);
    let n = p
        .get("n_samples")
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or(json!(1));
    let image_count = py_int(&n).ok_or_else(|| GateError::new(400, "一次生成张数必须是正整数"))?;
    if image_count < 1 {
        return Err(GateError::new(400, "一次生成张数必须是正整数").into());
    }
    let mut est = Est::from_cost(
        policy::estimate_image_cost(&body, true, true)
            .map_err(|e| pyerr_400(&e, "图片参数无效，无法估算费用"))?,
    );
    // V5 续杯：自动分配了 Anlas 的成员当天个人 V5 用完后改用 Anlas 生成同规格 V5。
    // 只在账号 V5 额度真的用完（≤ 1%）时才走 Anlas，否则会悄悄多用全站共享的 V5 额度。
    if est.v5 > 0
        && gate_core::pyval::number_like(key.get("anlas_auto")) == Some(1.0)
        && key.allow_anlas()
        && key.daily_v5() > 0
    {
        let last = state.db.get_setting("quota_algo_last").await?;
        let v5_left = last
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| {
                v.get("v5")
                    .filter(|x| truthy(x))
                    .and_then(|x| x.get("percent"))
                    .cloned()
            })
            .and_then(|p| gate_core::pyval::number_like(&p));
        if v5_left.is_some_and(|l| l <= 1.0)
            && state.db.get_counter(key.id(), &state.day()).await?.v5 >= key.daily_v5()
        {
            est = Est::from_cost(
                policy::estimate_image_cost(&body, true, false)
                    .map_err(|e| pyerr_400(&e, "图片参数无效，无法估算费用"))?,
            );
            notes.push("今日 V5 已用完，使用自动分配的 Anlas 续杯".into());
        }
    }
    let legacy_free_images = if tier == "legacy"
        && policy::legacy_normal_free_eligible(&body).map_err(|e| InternalError(e.to_string()))?
    {
        1
    } else {
        0
    };
    if params_has(&body, &REFERENCE_FIELDS) {
        est.is_vibe = true;
    }
    if est.v5 == 0 {
        let snapshot = state.budget.lock().await.clone();
        admission::quota_image_check(state, &snapshot, &key, &est, legacy_free_images, None)
            .await?;
    }
    let cost: Vec<String> = [
        (est.anlas > 0.0).then(|| format!("est={}A", est.anlas as i64)),
        (est.v5 > 0).then(|| format!("V5额度+{}", est.v5)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let cost = if cost.is_empty() {
        "免费".to_string()
    } else {
        cost.join("; ")
    };
    // 尺寸 / 步数 / 费用始终在前：钳制说明只追加在后面，不能顶掉记账信息
    let size = |p: &Value| {
        let g = |k: &str| py_str(p.get(k).unwrap_or(&Value::Null));
        format!("{}x{}/{}step", g("width"), g("height"), g("steps"))
    };
    let detail = std::iter::once(format!("{} {cost}", size(&p)))
        .chain(notes.iter().cloned())
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Prepared {
        key,
        body,
        model,
        image_count,
        est,
        legacy_free_images,
        notes,
        detail,
    })
}

/// V5 额度确认回调用到的可变状态（旧版 `nonlocal est, detail`）。
struct Billing {
    est: Est,
    detail: String,
}

fn resolve_v5(
    state: &AppState,
    prep: &Prepared,
    billing: Arc<Mutex<Billing>>,
    reserved: Arc<crate::admission::Reserved>,
) -> gate_upstream::client::ResolveV5<'static> {
    let (body, key, legacy, notes) = (
        Arc::new(prep.body.clone()),
        prep.key.clone(),
        prep.legacy_free_images,
        prep.notes.clone(),
    );
    let _ = state;
    Box::new(move |exhausted: bool| {
        let (body, key, notes, billing, reserved) = (
            body.clone(),
            key.clone(),
            notes.clone(),
            billing.clone(),
            reserved.clone(),
        );
        Box::pin(async move {
            if exhausted && !key.allow_anlas() {
                // 账号的 V5 免费额度刚用完：直说原因，不要报成「你的 Key 没开通付费权限」
                return Err(UpstreamError::local(
                    402,
                    "上游账号今日 V5 免费额度已耗尽（非个人原因），请改用 V4.5 生成",
                    reasons::V5_EXHAUSTED,
                ));
            }
            let cost = policy::estimate_image_cost(&body, true, !exhausted)
                .map_err(|e| UpstreamError::local(400, e.message().to_string(), ""))?;
            let updated = Est::from_cost(cost);
            reserved.update(&updated, legacy).await.map_err(local)?;
            let mut b = billing.lock().expect("billing");
            b.est = updated;
            if exhausted {
                let p = body.get("parameters").cloned().unwrap_or(Value::Null);
                let g = |k: &str| py_str(p.get(k).unwrap_or(&Value::Null));
                b.detail = notes
                    .iter()
                    .cloned()
                    .chain([
                        format!(
                            "{}x{}/{}step est={}A",
                            g("width"),
                            g("height"),
                            g("steps"),
                            updated.anlas as i64
                        ),
                        "官方确认 V5 额度不可用，按 Anlas 估算记账".into(),
                    ])
                    .collect::<Vec<_>>()
                    .join("; ");
            }
            Ok(())
        })
    })
}

async fn perform_generation(
    state: &AppState,
    ctx: &ReqCtx,
    prep: Prepared,
    reserved: Arc<crate::admission::Reserved>,
) -> Result<Response, ApiError> {
    let billing = Arc::new(Mutex::new(Billing {
        est: prep.est,
        detail: prep.detail.clone(),
    }));
    let (key, model) = (prep.key.clone(), prep.model.clone());
    let est0 = prep.est;
    let cb = Callbacks {
        on_rate_limited: Some(on_429(state)),
        resolve_v5_cost: (est0.v5 > 0)
            .then(|| resolve_v5(state, &prep, billing.clone(), reserved.clone())),
        before_dispatch: Some(before_dispatch(state)),
        on_dispatch: None,
    };
    let url = format!("{}/ai/generate-image", state.nai.image_host());
    let mut opts = RequestOpts::post(url, &prep.body);
    opts.requires_anlas = est0.anlas > 0.0;
    opts.v5_free = est0.v5 > 0;
    opts.image_count = prep.image_count as usize;
    opts.image_lane = true;
    opts.max_response_bytes = Some(MAX_RESPONSE_BYTES);
    opts.queue_timeout = state.limits().queue_timeout;
    let allowance = state.allowance.with(&state.nai);
    let allow: Option<&dyn gate_upstream::client::Allowance> = (est0.v5 > 0).then_some(&allowance);
    let res = state.nai.request(opts, &cb, allow, ctx).await;
    let est = billing.lock().expect("billing").est;
    let resp = match res {
        Ok(r) => r,
        Err(e) if e.transport => return Err(upstream_transport(ctx, e.message)),
        Err(e) => {
            let exc = up_to_gate(e);
            if !ctx.was_sent() && !exc.billing_uncertain {
                // 还没发到上游就被本地拒绝：记「拒绝」+ 原因码，不算上游失败
                state
                    .settle_record(
                        ctx,
                        &key,
                        Entry::new("image", &model, "rejected")
                            .detail(detail160(exc.status, &exc.message))
                            .reason(exc.code.clone()),
                    )
                    .await;
                return Err(exc.into());
            }
            state
                .settle_record(
                    ctx,
                    &key,
                    Entry::new("image", &model, "error")
                        .detail(exc.message.clone())
                        .unconfirmed(if exc.billing_uncertain {
                            est.anlas
                        } else {
                            0.0
                        }),
                )
                .await;
            state
                .audit_generation(ctx, &key, "image", &model, "error", &prep.body, None)
                .await;
            state.record_upstream(false);
            return Err(exc.into());
        }
    };
    if !matches!(resp.status, 200 | 201) {
        let shape = if resp.status >= 500 {
            request_shape(&prep.body)
        } else {
            String::new()
        };
        if !shape.is_empty() {
            tracing::warn!(
                "[upstream-5xx] rid={} status={} {shape}",
                ctx.rid(),
                resp.status
            );
        }
        let detail = if shape.is_empty() {
            format!("upstream {}", resp.status)
        } else {
            format!("upstream {}（{}）", resp.status, take_chars(&shape, 110))
        };
        state
            .settle_record(
                ctx,
                &key,
                Entry::new("image", &model, "error")
                    .detail(detail)
                    .unconfirmed(if resp.status >= 500 { est.anlas } else { 0.0 }),
            )
            .await;
        state
            .audit_generation(ctx, &key, "image", &model, "error", &prep.body, None)
            .await;
        state.record_upstream(resp.status < 500);
        if resp.status >= 500 && state.note_upstream_5xx(key.id()) {
            state.alerter.notify(
                "upstream_5xx",
                &format!("NovelAI 图片接口返回 {}，上游可能故障。", resp.status),
                1800.0,
            );
        }
        return Err(
            GateError::new(resp.status, upstream_error_message(i64::from(resp.status)))
                .from_upstream(true)
                .into(),
        );
    }
    let detail = billing.lock().expect("billing").detail.clone();
    state
        .settle_record(
            ctx,
            &key,
            Entry {
                images: prep.image_count,
                anlas: est.anlas,
                v5: est.v5,
                legacy_free_images: prep.legacy_free_images,
                detail,
                ..Entry::new("image", &model, "ok")
            },
        )
        .await;
    state.schedule_audit(
        ctx,
        &key,
        "image",
        &model,
        "ok",
        prep.body.clone(),
        Some(resp.body.clone()),
    );
    state.record_upstream(true);
    let media = resp
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(binary(resp.body, &media, false))
}

// ------------------------------------------------------------------ 流式出图

/// 流式格式容错：返回 SSE 或 MessagePack；认不出的写法 400。
fn normalize_wire_format(raw: Option<&Value>) -> Result<WireFormat, GateError> {
    let token = match raw {
        None | Some(Value::Null) | Some(Value::Bool(_)) => return Ok(WireFormat::Sse),
        Some(v) => gate_core::pyc::strip(&py_str(v)).to_lowercase(),
    };
    match token.as_str() {
        "msgpack" | "messagepack" | "binary" => Ok(WireFormat::Msgpack),
        "" | "sse" | "text/event-stream" | "event-stream" | "stream" => Ok(WireFormat::Sse),
        _ => Err(GateError::new(400, "stream 仅支持 sse 或 msgpack")),
    }
}

/// 推流的一端（旧版 `ImageStreamResponse`）：响应头要等上游接受之后才发；之前出错就回一个普通 JSON 错误。
struct StreamSink {
    head: Option<tokio::sync::oneshot::Sender<Head>>,
    tx: tokio::sync::mpsc::Sender<Bytes>,
    wire: WireFormat,
    disconnected: bool,
    /// 已经发出 200 响应头（之后接收端关闭才算客户端断开；回了 JSON 错误时接收端本来就会关）。
    started: bool,
}

enum Head {
    Start,
    Error(u16, String),
}

impl StreamSink {
    async fn start(&mut self) {
        if let Some(h) = self.head.take() {
            self.started = true;
            let _ = h.send(Head::Start);
        }
    }

    /// 客户端在拿到全部图之前走了（只在已经开始推流时才有意义）。
    fn client_gone(&self) -> bool {
        self.started && (self.disconnected || self.tx.is_closed())
    }

    /// 慢 / 已断开的接收方不能拖住上游结算：每块最多等 10 秒。
    async fn chunk(&mut self, data: Vec<u8>) {
        if self.disconnected {
            return;
        }
        let sent =
            tokio::time::timeout(Duration::from_secs(10), self.tx.send(Bytes::from(data))).await;
        if !matches!(sent, Ok(Ok(()))) {
            self.disconnected = true;
        }
    }

    async fn error(&mut self, status: u16, message: &str) {
        if let Some(h) = self.head.take() {
            let _ = h.send(Head::Error(status, message.to_string()));
        } else {
            let ev = json!({"event_type": "error", "error": message, "message": message, "status_code": status});
            if let Ok(frame) = encode_image_event(&ev, self.wire) {
                self.chunk(frame).await;
            }
        }
    }
}

async fn stream(
    state: AppState,
    ctx: ReqCtx,
    mut prep: Prepared,
    permit: Option<crate::runtime::InflightPermit>,
) -> Result<Response, ApiError> {
    // 现有面板默认 SSE；Launcher 显式要 MessagePack
    let wire = normalize_wire_format(prep.body.get("parameters").and_then(|p| p.get("stream")))?;
    let wire_name = if wire == WireFormat::Msgpack {
        "msgpack"
    } else {
        "sse"
    };
    if let Some(p) = prep
        .body
        .get_mut("parameters")
        .and_then(Value::as_object_mut)
    {
        p.insert("stream".into(), Value::String(wire_name.into()));
    }
    let (head_tx, head_rx) = tokio::sync::oneshot::channel();
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(32);
    let disconnect = CancellationToken::new();
    let sink = StreamSink {
        head: Some(head_tx),
        tx,
        wire,
        disconnected: false,
        started: false,
    };
    let (s, c, d) = (state.clone(), ctx.clone(), disconnect.clone());
    state.tasks.spawn(ctx.clone().scope(async move {
        let _permit = permit; // 在途名额占到推流和结算结束
        run_stream(s, c, prep, sink, d).await;
    }));
    // 响应头发出之前客户端断开：通知后台任务（还没发往上游就取消）
    let guard = disconnect.clone().drop_guard();
    let head = head_rx.await;
    guard.disarm();
    match head {
        Ok(Head::Start) => {
            let body =
                Body::from_stream(tokio_stream_from(rx).map(Ok::<Bytes, std::convert::Infallible>));
            let mut r = Response::new(body);
            let h = r.headers_mut();
            h.insert("content-type", HeaderValue::from_static(wire.media_type()));
            h.insert("cache-control", HeaderValue::from_static("no-store"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            Ok(r)
        }
        Ok(Head::Error(status, message)) => {
            // json.dumps(..., ensure_ascii=False)：默认分隔符带空格
            let m = String::from_utf8(gate_core::pyjson::wire(&Value::String(message)))
                .unwrap_or_default();
            let text = format!("{{\"error\": {m}, \"message\": {m}}}");
            let mut r = Response::new(Body::from(text));
            *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let h = r.headers_mut();
            h.insert("content-type", HeaderValue::from_static("application/json"));
            h.insert("cache-control", HeaderValue::from_static("no-store"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            Ok(r)
        }
        Err(_) => Ok(json_response(500, &json!({"error": "stream worker ended"}))),
    }
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> impl futures::Stream<Item = Bytes> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// 旧版 `run_stream`：排队阶段可取消；上游阶段在真正派发之前也可取消。
async fn run_stream(
    state: AppState,
    ctx: ReqCtx,
    prep: Prepared,
    mut sink: StreamSink,
    disconnect: CancellationToken,
) {
    let key = prep.key.clone();
    let model = prep.model.clone();
    let queued = async {
        wait_for_user_image_slot(&state, &key).await?;
        let admission = image_admission(&state, &ctx, &key).await?;
        let slots = acquire_slots(&state, &key, true).await?;
        check_image_cooldown(&state)?;
        let reserved =
            reserve_image_budget(&state, &key, &prep.est, prep.legacy_free_images).await?;
        Ok::<_, ApiError>((admission, slots, reserved))
    };
    let held = tokio::select! {
        r = queued => r,
        _ = disconnect.cancelled() => return, // 排队时断开：直接取消，不记账
    };
    let res = match held {
        Ok((admission, slots, reserved)) => {
            let _held = (admission, slots);
            perform_stream(
                &state,
                &ctx,
                prep,
                Arc::new(reserved),
                &mut sink,
                &disconnect,
            )
            .await
        }
        Err(e) => Err(e),
    };
    match res {
        Ok(()) => {}
        Err(ApiError::Gate(exc)) => {
            // 和非流式一样记「拒绝」：自动驾驶、AIMD、每日微调都靠这些记录感知拥挤
            refund_local_reject(&state, &ctx, &key).await;
            state.record(
                &ctx,
                &key,
                Entry::new("image_stream", &model, "rejected")
                    .detail(detail160(exc.status, &exc.message))
                    .reason(exc.code.clone()),
            );
            sink.error(exc.status, &exc.message).await;
        }
        Err(other) => {
            let resp = other.into_response();
            sink.error(resp.status().as_u16(), "服务器内部错误，已自动记录")
                .await;
        }
    }
}

enum StreamFail {
    /// 上游 / 协议 / 超时：给成员的说明、状态码、是否可能已扣费
    Upstream(u16, String, bool),
}

async fn perform_stream(
    state: &AppState,
    ctx: &ReqCtx,
    prep: Prepared,
    reserved: Arc<crate::admission::Reserved>,
    sink: &mut StreamSink,
    disconnect: &CancellationToken,
) -> Result<(), ApiError> {
    let key = prep.key.clone();
    let model = prep.model.clone();
    let image_count = prep.image_count;
    let wire = sink.wire;
    let dispatched = Arc::new(AtomicBool::new(false));
    let mut delivered = false;
    let billing = Arc::new(Mutex::new(Billing {
        est: prep.est,
        detail: prep.detail.clone(),
    }));
    let est0 = prep.est;
    let mut tracker =
        ImageEventTracker::new(image_count, wire).map_err(|e| InternalError(e.to_string()))?;
    let mut failure: Option<String> = None;
    let mut billing_uncertain = false;
    let mut local_reject = false;
    let mut cancelled = false;

    let d = dispatched.clone();
    let cb = Callbacks {
        on_rate_limited: Some(on_429(state)),
        resolve_v5_cost: (est0.v5 > 0)
            .then(|| resolve_v5(state, &prep, billing.clone(), reserved.clone())),
        before_dispatch: Some(before_dispatch(state)),
        on_dispatch: Some(Box::new(move || d.store(true, Ordering::SeqCst))),
    };
    let allowance = state.allowance.with(&state.nai);
    let allow: Option<&dyn gate_upstream::client::Allowance> = (est0.v5 > 0).then_some(&allowance);
    let url = format!("{}/ai/generate-image-stream", state.nai.image_host());
    let outcome: Result<Result<(), StreamFail>, GateError> = async {
        // 总时长上限：上游一直发进度帧也不能无限占着
        let work = async {
            let open = state.nai.image_stream(
                &url,
                &prep.body,
                est0.anlas > 0.0,
                est0.v5 > 0,
                state.limits().queue_timeout,
                &cb,
                allow,
                ctx,
            );
            tokio::pin!(open);
            let mut seen_cancel = false;
            let opened = loop {
                tokio::select! {
                    r = &mut open => break r,
                    _ = disconnect.cancelled(), if !seen_cancel => {
                        seen_cancel = true;
                        if !dispatched.load(Ordering::SeqCst) {
                            return Ok(Err(None)); // 还没发往上游：取消
                        }
                    }
                }
            };
            let mut handle = match opened {
                Ok(h) => h,
                Err(e) if e.local => return Err(GateError::with_code(e.status, e.message, e.code)),
                Err(e) => {
                    return Ok(Err(Some(StreamFail::Upstream(
                        e.status,
                        e.message,
                        e.billing_uncertain,
                    ))));
                }
            };
            billing_uncertain = true;
            let media = handle
                .headers()
                .and_then(|h| h.get("content-type"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let mut allowed = vec![wire.media_type()];
            if wire == WireFormat::Msgpack {
                // 官方文档对这个端点仍写 SSE：即使是这个头也按二进制帧校验
                allowed.extend([
                    "application/msgpack",
                    "application/octet-stream",
                    "text/event-stream",
                ]);
            }
            if !allowed.contains(&media.as_str()) {
                handle.finish().await;
                return Ok(Err(Some(StreamFail::Upstream(
                    502,
                    "上游未返回有效的图片事件流".into(),
                    false,
                ))));
            }
            sink.start().await;
            let mut chunks = handle.take_body().expect("stream body");
            let fail = loop {
                let Some(chunk) = chunks.next().await else {
                    break None;
                };
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(_) => {
                        break Some(StreamFail::Upstream(
                            502,
                            "图片流连接中断或超时".into(),
                            false,
                        ));
                    }
                };
                // 错误事件在认出之前绝不转发一部分（即使跨网络分块）
                let out = tracker.frames(&chunk);
                for f in out.frames {
                    handle.completed_images = f.completed_after;
                    sink.chunk(f.bytes).await;
                }
                if let Some(e) = out.error {
                    break Some(StreamFail::Upstream(502, e.to_string(), false));
                }
                if tracker.failed() {
                    break Some(StreamFail::Upstream(502, "上游流式生成失败".into(), false));
                }
                if tracker.completed_images() as i64 == image_count {
                    delivered = true;
                    break None;
                }
            };
            let fail = match fail {
                Some(f) => Some(f),
                None => {
                    tracker.finish();
                    ((tracker.completed_images() as i64) < image_count).then(|| {
                        StreamFail::Upstream(
                            502,
                            "图片流提前结束，未收到全部最终图片".into(),
                            false,
                        )
                    })
                }
            };
            drop(chunks);
            handle.completed_images = tracker.completed_images();
            handle.finish().await;
            Ok(match fail {
                None => Ok(()),
                Some(f) => Err(Some(f)),
            })
        };
        match tokio::time::timeout(Duration::from_secs(300), work).await {
            Ok(Ok(Ok(()))) => Ok(Ok(())),
            Ok(Ok(Err(None))) => {
                cancelled = true;
                Ok(Ok(()))
            }
            Ok(Ok(Err(Some(f)))) => Ok(Err(f)),
            Ok(Err(g)) => Err(g),
            Err(_) => Ok(Err(StreamFail::Upstream(
                502,
                "图片流连接中断或超时".into(),
                dispatched.load(Ordering::SeqCst),
            ))),
        }
    }
    .await;

    let gate_err = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(StreamFail::Upstream(status, message, uncertain))) => {
            billing_uncertain |= uncertain;
            // 上游 error 事件的原话只进日志，不转发给客户端（可能含上游内部信息）
            failure = Some(if tracker.error_detail().is_empty() {
                message.clone()
            } else {
                format!("{message}（上游：{}）", tracker.error_detail())
            });
            sink.error(status, &message).await;
            None
        }
        Err(g) => {
            local_reject = !dispatched.load(Ordering::SeqCst);
            Some(g)
        }
    };

    // 收尾记账（旧版 finally）
    let completed = tracker.completed_images() as i64;
    let est = billing.lock().expect("billing").est;
    let detail = billing.lock().expect("billing").detail.clone();
    let was_dispatched = dispatched.load(Ordering::SeqCst);
    if !was_dispatched && completed == 0 && failure.is_none() && !local_reject {
        // 还在排队时客户端就断开了：没发到上游，不能记成「上游出错」，也不能拉低上游健康度
        state
            .settle_record(
                ctx,
                &key,
                Entry::new("image_stream", &model, "cancelled")
                    .detail("排队时客户端断开（未发到上游，未记费）"),
            )
            .await;
    } else if !(local_reject && completed == 0) {
        let mut settled_anlas = 0.0;
        if completed > 0 {
            // 按完整结果重算首张减免，沿用派发前确认的 V5 额度状态
            let mut completed_body = prep.body.clone();
            if let Some(p) = completed_body
                .get_mut("parameters")
                .and_then(Value::as_object_mut)
            {
                p.insert("n_samples".into(), json!(completed));
            }
            let settled = policy::estimate_image_cost(&completed_body, true, est.v5 > 0)
                .map(Est::from_cost)
                .unwrap_or(est);
            settled_anlas = settled.anlas;
            let suffix = if completed < image_count {
                format!("; 完成 {completed}/{image_count}")
            } else {
                String::new()
            };
            state
                .settle_record(
                    ctx,
                    &key,
                    Entry {
                        images: completed,
                        anlas: settled.anlas,
                        v5: settled.v5,
                        legacy_free_images: prep.legacy_free_images,
                        detail: format!("{detail}{suffix}"),
                        ..Entry::new("image_stream", &model, "ok")
                    },
                )
                .await;
        }
        let ok = completed > 0 && failure.is_none();
        state
            .audit_generation(
                ctx,
                &key,
                "image_stream",
                &model,
                if ok { "ok" } else { "error" },
                &prep.body,
                if completed > 0 {
                    tracker.first_image().map(Bytes::copy_from_slice)
                } else {
                    None
                },
            )
            .await;
        state.record_upstream(ok);
        if failure.is_some() || completed == 0 {
            // 未结算部分单独记为待核对费用
            state
                .settle_record(
                    ctx,
                    &key,
                    Entry::new("image_stream", &model, "error")
                        .detail(
                            failure
                                .clone()
                                .unwrap_or_else(|| "未收到最终图片，未记费".into()),
                        )
                        .unconfirmed(if billing_uncertain {
                            (est.anlas - settled_anlas).max(0.0)
                        } else {
                            0.0
                        }),
                )
                .await;
        }
    }
    if cancelled {
        return Ok(());
    }
    // 派发之后客户端断开、没拿到全部图：记一条 Bug（图已生成并计数）
    if (disconnect.is_cancelled() || sink.client_gone()) && !delivered && was_dispatched {
        state.bug("disconnect", None, ctx);
    }
    match gate_err {
        Some(g) => Err(g.into()),
        None => Ok(()),
    }
}

// ------------------------------------------------------------------ 图片工具 / Vibe 编码

async fn upscale(State(state): State<AppState>, req: Request) -> Response {
    image_tool(state, req, "upscale").await
}

async fn augment(State(state): State<AppState>, req: Request) -> Response {
    image_tool(state, req, "augment-image").await
}

async fn image_tool(state: AppState, req: Request, operation: &'static str) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match tool(state, req, operation, permit).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn tool(
    state: AppState,
    req: Request,
    operation: &'static str,
    permit: Option<crate::runtime::InflightPermit>,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_image_cooldown(&state)?;
    check_rpm(&state, &key)?;
    require_feature(
        &state,
        &key,
        if operation == "upscale" {
            "upscale"
        } else {
            "augment"
        },
    )
    .await?;
    if !key.is_admin() && !(key.allow_img2img() && state.env.allow_img2img) {
        return Err(GateError::new(403, "此 Key 或本站未开放图片处理权限（img2img）").into());
    }
    let body = read_image_payload(&parts.headers, body, IMAGE_BODY_LIMIT).await?;
    wait_for_user_image_slot(&state, &key).await?;
    let admission = image_admission(&state, &ctx, &key).await?;
    let slots = acquire_slots(&state, &key, true).await?;
    check_image_cooldown(&state)?;
    let body = Value::Object(body);
    let op = operation.to_string();
    let (payload, cost) = tokio::task::spawn_blocking(move || prepare_tool(&body, &op))
        .await
        .map_err(|e| InternalError(e.to_string()))?
        .map_err(|m| GateError::new(400, m))?;
    let model = payload
        .get("model")
        .or_else(|| payload.get("req_type"))
        .map(py_str)
        .unwrap_or_default();
    let est = Est {
        anlas: cost as f64,
        v5: 0,
        is_vibe: false,
    };
    let reserved = reserve_image_budget(&state, &key, &est, 0).await?;
    let (s, c) = (state.clone(), ctx.clone());
    shielded(&state, &ctx, async move {
        let _held = (admission, slots, reserved, permit);
        let state = s;
        let ctx = c;
        let mut billing_uncertain = false;
        let failed: Result<Response, (GateError, bool)> = async {
            let mut opts = RequestOpts::post(
                format!("{}/ai/{operation}", state.nai.image_host()),
                &payload,
            );
            opts.requires_anlas = cost > 0;
            opts.image_lane = true;
            opts.max_response_bytes = Some(MAX_RESPONSE_BYTES);
            opts.queue_timeout = state.limits().queue_timeout;
            let cb = Callbacks {
                on_rate_limited: Some(on_429(&state)),
                before_dispatch: Some(before_dispatch(&state)),
                ..Default::default()
            };
            let resp = match state.nai.request(opts, &cb, None, &ctx).await {
                Ok(r) => r,
                Err(e) if e.transport => return Err((GateError::new(502, ""), true)),
                Err(e) => return Err((up_to_gate(e), false)),
            };
            billing_uncertain = matches!(resp.status, 200 | 201) || resp.status >= 500;
            if !matches!(resp.status, 200 | 201) {
                let s = if (400..500).contains(&resp.status) {
                    resp.status
                } else {
                    502
                };
                return Err((
                    GateError::new(
                        s,
                        format!("图片工具请求失败（上游状态 {}），未记费", resp.status),
                    )
                    .from_upstream(true),
                    false,
                ));
            }
            let content = resp.body.clone();
            let req_type = py_str(
                payload
                    .get("req_type")
                    .unwrap_or(&Value::String(String::new())),
            );
            let checked = tokio::task::spawn_blocking(move || {
                validate_result(&content, &req_type, None).map(|(m, n)| (m.to_string(), n))
            })
            .await
            .map_err(|_| (GateError::new(502, ""), true))?;
            let (media, count) = checked.map_err(|_| (GateError::new(502, ""), true))?;
            state
                .settle_record(
                    &ctx,
                    &key,
                    Entry {
                        images: count as i64,
                        anlas: cost as f64,
                        detail: format!("图片工具 {cost} Anlas；返回 {count} 张"),
                        ..Entry::new(operation, &model, "ok")
                    },
                )
                .await;
            Ok(binary(resp.body, &media, true))
        }
        .await;
        match failed {
            Ok(r) => Ok(r),
            Err((exc, opaque)) => {
                billing_uncertain |= exc.billing_uncertain;
                let detail = if opaque {
                    "图片工具连接失败或返回无效结果，未记费".to_string()
                } else {
                    exc.message.clone()
                };
                state
                    .settle_record(
                        &ctx,
                        &key,
                        Entry::new(operation, &model, "error")
                            .detail(detail)
                            .unconfirmed(if billing_uncertain { cost as f64 } else { 0.0 }),
                    )
                    .await;
                if opaque {
                    return Err(GateError::new(
                        502,
                        "图片工具连接失败或返回无效结果，未记费；请勿自动重试",
                    )
                    .from_upstream(true)
                    .into());
                }
                Err(exc.into())
            }
        }
    })
    .await
}

async fn encode_vibe(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match vibe(state, req, permit).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// V4 / V4.5 参考图编码：只有成功返回二进制结果才扣 2 Anlas。
async fn vibe(
    state: AppState,
    req: Request,
    permit: Option<crate::runtime::InflightPermit>,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_image_cooldown(&state)?;
    check_rpm(&state, &key)?;
    require_feature(&state, &key, "vibe").await?;
    let mut body = read_image_payload(&parts.headers, body, IMAGE_BODY_LIMIT).await?;
    // 兼容 Launcher 的写法
    if let Some(value) = body.shift_remove("information_extracted") {
        if body
            .get("informationExtracted")
            .is_some_and(|v| *v != value)
        {
            return Err(GateError::new(400, "两种 informationExtracted 字段的值不一致").into());
        }
        body.insert("informationExtracted".into(), value);
    }
    let body = Value::Object(body);
    if let Some(problem) = policy::validate_vibe_encoding(&body) {
        return Err(GateError::new(400, problem).into());
    }
    let model = py_str(&body["model"]);
    let payload = json!({
        "image": body["image"],
        "model": body["model"],
        "information_extracted": body["informationExtracted"],
    });
    let anlas = VIBE_ENCODING_ANLAS as f64;
    let est = Est {
        anlas,
        v5: 0,
        is_vibe: true,
    };
    {
        let snapshot = state.budget.lock().await.clone();
        admission::quota_image_check(&state, &snapshot, &key, &est, 0, None).await?;
    }
    wait_for_user_image_slot(&state, &key).await?;
    let admission = image_admission(&state, &ctx, &key).await?;
    let slots = acquire_slots(&state, &key, true).await?;
    check_image_cooldown(&state)?;
    let reserved = reserve_image_budget(&state, &key, &est, 0).await?;
    let (s, c) = (state.clone(), ctx.clone());
    shielded(&state, &ctx, async move {
        let _held = (admission, slots, reserved, permit);
        let (state, ctx) = (s, c);
        let mut opts = RequestOpts::post(
            format!("{}/ai/encode-vibe", state.nai.image_host()),
            &payload,
        );
        opts.accept = "application/octet-stream";
        opts.requires_anlas = true;
        opts.image_lane = true;
        opts.max_response_bytes = Some(MAX_RESPONSE_BYTES);
        opts.queue_timeout = state.limits().queue_timeout;
        let cb = Callbacks {
            on_rate_limited: Some(on_429(&state)),
            before_dispatch: Some(before_dispatch(&state)),
            ..Default::default()
        };
        let err_entry = |detail: String, unconfirmed: f64| {
            Entry::new("vibe_encode", &model, "error")
                .detail(detail)
                .unconfirmed(unconfirmed)
        };
        let resp = match state.nai.request(opts, &cb, None, &ctx).await {
            Ok(r) => r,
            Err(e) if e.transport => {
                state
                    .settle_record(
                        &ctx,
                        &key,
                        err_entry("上游编码请求失败，未记费".into(), 0.0),
                    )
                    .await;
                return Err(
                    GateError::new(502, "Vibe 编码连接失败，未记费；请勿自动重试")
                        .from_upstream(true)
                        .into(),
                );
            }
            Err(e) => {
                let exc = up_to_gate(e);
                let unconfirmed = if exc.billing_uncertain { anlas } else { 0.0 };
                state
                    .settle_record(&ctx, &key, err_entry(exc.message.clone(), unconfirmed))
                    .await;
                return Err(exc.into());
            }
        };
        if !matches!(resp.status, 200 | 201) {
            let unconfirmed = if resp.status >= 500 { anlas } else { 0.0 };
            state
                .settle_record(
                    &ctx,
                    &key,
                    err_entry(format!("upstream {}", resp.status), unconfirmed),
                )
                .await;
            let s = if (400..500).contains(&resp.status) {
                resp.status
            } else {
                502
            };
            return Err(GateError::new(
                s,
                format!("Vibe 编码失败（上游状态 {}），未记费", resp.status),
            )
            .from_upstream(true)
            .into());
        }
        let ct = resp
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_lowercase();
        if resp.body.is_empty() || ct.contains("json") || ct.starts_with("text/") {
            state
                .settle_record(
                    &ctx,
                    &key,
                    err_entry("上游未返回有效二进制编码，未记费".into(), anlas),
                )
                .await;
            return Err(GateError::new(502, "Vibe 编码未返回有效数据，未记费")
                .from_upstream(true)
                .into());
        }
        state
            .settle_record(
                &ctx,
                &key,
                Entry {
                    anlas,
                    detail: format!("Vibe 编码 {VIBE_ENCODING_ANLAS} Anlas"),
                    ..Entry::new("vibe_encode", &model, "ok")
                },
            )
            .await;
        Ok(binary(resp.body, "application/octet-stream", true))
    })
    .await
}

// ------------------------------------------------------------------ 标签补全

async fn suggest_tags(State(state): State<AppState>, req: Request) -> Response {
    match tags(state, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// `urllib.parse.urlencode`（quote_plus：空格 → +，只保留字母数字和 `_.-~`）。
fn urlencode(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

async fn tags(state: AppState, req: Request) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_image_cooldown(&state)?;
    require_feature(&state, &key, "tags").await?;
    struct Finish<'a>(&'a AppState, i64, bool);
    impl Drop for Finish<'_> {
        fn drop(&mut self) {
            if self.2 {
                self.0.tags.finish(self.1);
            }
        }
    }
    let mut fin = Finish(&state, key.id(), false);
    let l = state.limits();
    let run = async {
        let admitted = state
            .tags
            .wait(
                key.id(),
                state.env.global_concurrency,
                l.key_image_min_interval,
            )
            .await;
        match admitted {
            // 已被同一 Key 更新的补全查询取代：回空结果，不记拒绝
            crate::runtime::TagAdmit::Superseded => {
                return Ok(crate::error::ok_json(&json!({"tags": []})));
            }
            crate::runtime::TagAdmit::Full => {
                return Err(GateError::new(429, "补全查询排队人数过多，请稍后再试").into());
            }
            crate::runtime::TagAdmit::Admitted => fin.2 = true,
        }
        suggest(&state, &ctx, &key, &parts, body).await
    };
    match tokio::time::timeout(Duration::from_secs_f64(l.queue_timeout.max(0.0)), run).await {
        Ok(r) => r,
        Err(_) => {
            state.record(
                &ctx,
                &key,
                Entry::new("tags", "", "error").detail("补全查询排队或请求超时"),
            );
            Err(GateError::new(429, "补全查询等待超时，请稍后再试").into())
        }
    }
}

async fn suggest(
    state: &AppState,
    ctx: &ReqCtx,
    key: &Key,
    parts: &axum::http::request::Parts,
    body: Body,
) -> Result<Response, ApiError> {
    let (prompt, model) = if parts.method == Method::GET {
        let q = parts.uri.query().unwrap_or("");
        let mut prompt = String::new();
        let mut model = String::new();
        for (k, v) in form_pairs(q) {
            match k.as_str() {
                "prompt" => prompt = v,
                "model" => model = v,
                _ => {}
            }
        }
        (prompt, model)
    } else {
        let b = read_json(&parts.headers, body, 64 * 1024).await?;
        let s = |k: &str| match b.get(k) {
            Some(v) if truthy(v) => py_str(v),
            _ => String::new(),
        };
        (s("prompt"), s("model"))
    };
    let _slots = acquire_slots(state, key, false).await?;
    check_image_cooldown(state)?;
    let query = urlencode(&[("prompt", &prompt), ("model", &model)]);
    let mut opts = RequestOpts::get(format!(
        "{}/ai/generate-image/suggest-tags?{query}",
        state.nai.image_host()
    ));
    opts.accept = "application/json";
    opts.image_lane = true;
    opts.wait_for_image_slot = false;
    let (s, c, k) = (state.clone(), ctx.clone(), key.clone());
    let cb = Callbacks {
        on_rate_limited: Some(Box::new(move |retry_after| {
            let (s, c, k) = (s.clone(), c.clone(), k.clone());
            Box::pin(async move {
                let secs = s.limits().image_429_cooldown_seconds.max(retry_after);
                let cooldown = s.block_image_generation(secs).await;
                s.record(
                    &c,
                    &k,
                    Entry::new("tags", "", "error").detail(format!(
                        "上游限流(429)：全站图片生成暂停约 {cooldown} 秒，保护上游 Token"
                    )),
                );
            })
        })),
        ..Default::default()
    };
    let resp = match state.nai.request(opts, &cb, None, ctx).await {
        Ok(r) => r,
        Err(e) if e.transport => return Err(upstream_transport(ctx, e.message)),
        Err(e) => {
            if e.status != 429 {
                // 真实上游 429 已由回调记录
                state.record(
                    ctx,
                    key,
                    Entry::new("tags", "", "error").detail(e.message.clone()),
                );
            }
            let status = if matches!(e.status, 429 | 503) {
                e.status
            } else {
                502
            };
            return Err(GateError::with_code(status, e.message, e.code)
                .from_upstream(!e.local)
                .into());
        }
    };
    if resp.status != 200 {
        state.record(
            ctx,
            key,
            Entry::new("tags", "", "error")
                .detail(format!("上游标签接口返回 HTTP {}", resp.status)),
        );
        return Err(
            GateError::new(resp.status, upstream_error_message(i64::from(resp.status)))
                .from_upstream(true)
                .into(),
        );
    }
    state.record(ctx, key, Entry::new("tags", "", "ok"));
    Ok(binary(resp.body, "application/json", false))
}

/// `application/x-www-form-urlencoded` 查询串解析（重复键以最后一个为准，与 Starlette 一致）。
fn form_pairs(q: &str) -> Vec<(String, String)> {
    let dec = |s: &str| {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%'
                && i + 2 < bytes.len()
                && let (Some(h), Some(l)) = (
                    bytes.get(i + 1).and_then(|c| (*c as char).to_digit(16)),
                    bytes.get(i + 2).and_then(|c| (*c as char).to_digit(16)),
                )
            {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (dec(k), dec(v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_aliases() {
        assert_eq!(normalize_wire_format(None).unwrap(), WireFormat::Sse);
        assert_eq!(
            normalize_wire_format(Some(&json!(true))).unwrap(),
            WireFormat::Sse
        );
        assert_eq!(
            normalize_wire_format(Some(&json!(" MessagePack "))).unwrap(),
            WireFormat::Msgpack
        );
        assert_eq!(
            normalize_wire_format(Some(&json!("text/event-stream"))).unwrap(),
            WireFormat::Sse
        );
        assert_eq!(
            normalize_wire_format(Some(&json!(1))).unwrap_err().status,
            400
        );
    }

    #[test]
    fn urlencode_matches_python() {
        // urllib.parse.urlencode({"prompt": "1girl, blue~eyes*", "model": "nai-diffusion-4-5-full"})
        assert_eq!(
            urlencode(&[
                ("prompt", "1girl, blue~eyes*"),
                ("model", "nai-diffusion-4-5-full")
            ]),
            "prompt=1girl%2C+blue~eyes%2A&model=nai-diffusion-4-5-full"
        );
        assert_eq!(urlencode(&[("prompt", "猫")]), "prompt=%E7%8C%AB");
    }

    #[test]
    fn query_pairs_last_wins() {
        let p = form_pairs("prompt=a+b&prompt=%E7%8C%AB&model=x");
        assert_eq!(p.last().unwrap(), &("model".to_string(), "x".to_string()));
        assert_eq!(p[1].1, "猫");
    }

    #[test]
    fn shape_summary() {
        let body = json!({"model": "nai-diffusion-4-5-full", "input": "猫猫", "parameters": {
            "width": 832, "height": 1216, "steps": 28, "sampler": "k_euler", "sm": true, "zeta": 1, "alpha": 2}});
        let s = request_shape(&body);
        assert!(s.starts_with("nai-diffusion-4-5-full · 832x1216 · 28步 · n=1 · action=generate · prompt=2字 uc=0字 角色=0"));
        assert!(s.ends_with("keys=alpha,height,sampler,sm,steps,width,zeta"));
        assert!(s.contains("sm=1/0 qt=0 ucPreset=None"));
    }
}
