//! 生图参数：校验与整理、免费档钳制、费用预估（旧版 `_generate_image` 里到 `quota_image_check` 为止的部分）。

use crate::accounting::Entry;
use crate::admission::{self, Est};
use crate::auth::require_feature;
use crate::error::{ApiError, GateError, InternalError};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use gate_core::policy::{self, REFERENCE_FIELDS};
use gate_core::pyc::take_chars;
use gate_core::pyval::{py_str, truthy};
use serde_json::{Map, Value, json};

/// 生图请求经过校验、整理后的结果。
pub(super) struct Prepared {
    pub(super) key: Key,
    pub(super) body: Value,
    pub(super) model: String,
    pub(super) image_count: i64,
    pub(super) est: Est,
    pub(super) legacy_free_images: i64,
    pub(super) notes: Vec<String>,
    pub(super) detail: String,
}

/// 参数校验与整理（旧版 `_generate_image` 里到 `quota_image_check` 为止的部分）。
pub(super) async fn prepare(
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
        if !key.allow_img2img() {
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
    // 免费档钳制：只对没有 Anlas 权限的 Key 生效；有 Anlas 的 Key 可以用超规格参数，按 Anlas 扣。
    // 上限写死（与 policy 的免费条件、SYSTEM-FACTS 的文案同一口径），不再读环境变量（docs/v3-deviations.md #105）
    let mut notes: Vec<String> = Vec::new();
    if !key.is_admin() && !key.allow_anlas() {
        let (b, n, problem) = policy::clamp_image_params(
            &body,
            policy::FREE_MAX_PIXELS,
            policy::FREE_MAX_STEPS,
            true,
        )
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
    // 2.x 这里还有「V5 续杯」：`anlas_auto` 的 Key 当天个人 V5 用完后改用 Anlas 生成同规格 V5。
    // Anlas 自动分配早已下线、没有 Key 再是 anlas_auto，3.0 删除（docs/v3-deviations.md #66；列保留）
    let mut est = Est::from_cost(
        policy::estimate_image_cost(&body, true, true)
            .map_err(|e| pyerr_400(&e, "图片参数无效，无法估算费用"))?,
    );
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

fn pyerr_400(e: &gate_core::pyext::PyErr, msg: impl Into<String>) -> ApiError {
    use gate_core::pyext::PyErr;
    match e {
        PyErr::Type(_) | PyErr::Value(_) | PyErr::Overflow(_) => GateError::new(400, msg).into(),
        other => InternalError(other.to_string()).into(),
    }
}
