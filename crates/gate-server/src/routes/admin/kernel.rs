//! 后台里属于自适应内核的接口（旧版 `app/admin.py`）：动态额度、自动驾驶、模块。
//!
//! | 接口 | 说明 |
//! |---|---|
//! | `GET /admin/api/quota-algo` | 当前结果、参数、最近 30 天的每日微调记录 |
//! | `PUT /admin/api/quota-algo` | 修改初始值 / 步长 / 范围，保存后立即重算 |
//! | `GET /admin/api/autopilot` | 每条规则的最新判断和最近 7 天的每小时快照 |
//! | `GET /admin/api/modules` | 模块：问题、开关、运行记录、交叉校验、关键参数当前值 |
//! | `PUT /admin/api/modules/{name}` | 启用 / 关闭一个模块，然后立即跑一轮 |
//! | `POST /admin/api/modules/run` | 手动立即执行一轮模块任务与交叉校验 |

use crate::body::read_json_body;
use crate::error::{ApiError, HttpError, InternalError, ok_json};
use crate::kernel::{Kernel, run_quota_algo};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use axum::routing::{get, post, put};
use gate_core::{jsondump, pyc, quota};
use serde_json::{Map, Value, json};

const BODY_LIMIT: usize = 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/api/quota-algo",
            get(quota_algo_get).put(quota_algo_put),
        )
        .route("/admin/api/autopilot", get(autopilot_get))
        .route("/admin/api/modules", get(modules_get))
        // 旧版 FastAPI：`PUT /modules/run` 会落到 `PUT /modules/{name}`（name="run"）→ 404「没有这个模块」
        .route(
            "/admin/api/modules/run",
            post(modules_run_all).put(modules_put_run),
        )
        .route("/admin/api/modules/{name}", put(modules_put))
}

/// `json.loads(await db.get_setting(key, default) or default)`；解析失败是 500（旧版没有捕获）。
async fn setting_json(state: &AppState, key: &str, default: &str) -> Result<Value, ApiError> {
    let raw = state.db.get_setting(key).await?;
    let text = raw.as_deref().filter(|s| !s.is_empty()).unwrap_or(default);
    jsondump::loads(text)
        .ok_or_else(|| InternalError(format!("JSONDecodeError: {key} 不是合法 JSON")).into())
}

/// 动态额度：当前结果、参数、最近 30 天的每日微调记录。
async fn quota_algo_get(
    State(state): State<AppState>,
    _s: AdminSession,
) -> Result<Response, ApiError> {
    // `{k: await db.get_setting(k, v)}`：库里存过就是字符串，没存过就是代码里的起点值（整数）
    let mut params = Map::new();
    for (k, v) in quota::DEFAULTS {
        let value = match state.db.get_setting(k).await? {
            Some(s) => Value::String(s),
            None => Value::from(v),
        };
        params.insert(k.to_string(), value);
    }
    let last = setting_json(&state, quota::STATE_KEY, "{}").await?;
    let history = setting_json(&state, quota::HISTORY_KEY, "[]").await?;
    Ok(ok_json(
        &json!({"last": last, "history": history, "params": params}),
    ))
}

/// 自动驾驶：每条规则的最新判断（观察模式下只记录不执行）和最近 7 天的每小时快照。
async fn autopilot_get(
    State(state): State<AppState>,
    _s: AdminSession,
) -> Result<Response, ApiError> {
    let last = setting_json(&state, gate_core::autopilot::STATE_KEY, "{}").await?;
    let history = setting_json(&state, gate_core::autopilot::HISTORY_KEY, "[]").await?;
    Ok(ok_json(&json!({"last": last, "history": history})))
}

/// `admin._num(value, int, lo, hi, name)`：严格解析后台数值——空值、小数、越界一律 422。
/// 这些字段里 0 代表「不限」，绝不能把空值或非法值静默变成 0，否则一次误操作就会拆掉额度保护。
fn num_int(value: &Value, lo: i64, hi: i64, name: &str) -> Result<i64, HttpError> {
    let empty = || HttpError::new(422, format!("{name} 不能为空"));
    let invalid = || HttpError::new(422, format!("{name} 必须是有效数字"));
    let v = match value {
        Value::Null | Value::Bool(_) => return Err(empty()),
        Value::String(s) if pyc::strip(s).is_empty() => return Err(empty()),
        Value::String(s) => pyc::parse_float(s).ok_or_else(invalid)?,
        Value::Number(n) => n.as_f64().ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    if !v.is_finite() {
        return Err(invalid());
    }
    if v.fract() != 0.0 {
        return Err(HttpError::new(422, format!("{name} 必须是整数")));
    }
    if !((lo as f64) <= v && v <= (hi as f64)) {
        return Err(HttpError::new(
            422,
            format!("{name} 必须在 {lo}～{hi} 之间"),
        ));
    }
    Ok(v as i64)
}

/// 修改初始值 / 步长 / 范围；保存后立即重算。
async fn quota_algo_put(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    // 极简模式（2026-10-11）：每人一个数 N；保底一层 3.0 已删除（docs/v3-deviations.md #61）
    const BOUNDS: [(&str, i64, i64); 7] = [
        ("quota_auto_enabled", 0, 1),
        ("quota_target_avg", 10, 2000),
        ("quota_ceiling_max", 10, 5000),
        ("quota_base_min_fair", 1, 2000),
        ("quota_step", 1, 500),
        ("quota_v5_min", 0, 500),
        ("quota_v5_max", 1, 500),
    ];
    let mut updates: Vec<(String, String)> = Vec::new();
    for (k, v) in &body {
        if let Some((_, lo, hi)) = BOUNDS.iter().find(|(name, _, _)| name == k) {
            updates.push((k.clone(), num_int(v, *lo, *hi, k)?.to_string()));
        }
    }
    if !updates.is_empty() {
        state.db.set_settings_bulk(&updates).await?;
        // 改了初始值时，同步当前值（否则要等明天的微调）：站长手动定今天的 N
        if let Some((_, n)) = updates.iter().find(|(k, _)| k == "quota_target_avg") {
            state
                .db
                .set_settings_bulk(&[("quota_ceiling".to_string(), n.clone())])
                .await?;
        }
    }
    let result = run_quota_algo(&state).await?;
    Ok(ok_json(&json!({"ok": true, "last": result})))
}

/// 模块：问题、开关、运行记录、交叉校验、关键参数当前值。
async fn modules_get(
    State(state): State<AppState>,
    _s: AdminSession,
) -> Result<Response, ApiError> {
    Ok(ok_json(
        &json!({"modules": state.kernel.snapshot(&state).await?}),
    ))
}

/// 启用 / 关闭一个模块；关闭只停止自动调整，保护类的硬限制不受影响。
async fn modules_put(
    State(state): State<AppState>,
    _s: AdminSession,
    Path(name): Path<String>,
    req: Request,
) -> Result<Response, ApiError> {
    if !Kernel::has_module(&name) {
        return Err(HttpError::new(404, "没有这个模块").into());
    }
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let Some(Value::Bool(on)) = body.get("enabled") else {
        return Err(HttpError::new(422, "enabled 必须是 true / false").into());
    };
    state.kernel.set_enabled(&state, &name, *on).await?;
    state.kernel.tick_all(&state).await?;
    Ok(ok_json(
        &json!({"ok": true, "modules": state.kernel.snapshot(&state).await?}),
    ))
}

async fn modules_put_run(_s: AdminSession) -> Result<Response, ApiError> {
    Err(HttpError::new(404, "没有这个模块").into())
}

/// 手动立即执行一轮模块任务与交叉校验。
async fn modules_run_all(
    State(state): State<AppState>,
    _s: AdminSession,
) -> Result<Response, ApiError> {
    state.kernel.tick_all(&state).await?;
    Ok(ok_json(
        &json!({"ok": true, "modules": state.kernel.snapshot(&state).await?}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(r: Result<i64, HttpError>) -> String {
        gate_core::pyval::py_str(&r.unwrap_err().detail)
    }

    #[test]
    fn strict_numbers_like_admin_num() {
        assert_eq!(num_int(&json!(25), 1, 500, "x").unwrap(), 25);
        assert_eq!(num_int(&json!(25.0), 1, 500, "x").unwrap(), 25);
        assert_eq!(num_int(&json!(" 25 "), 1, 500, "x").unwrap(), 25);
        assert_eq!(num_int(&json!("1e1"), 1, 500, "x").unwrap(), 10);
        assert_eq!(err(num_int(&json!(null), 1, 500, "x")), "x 不能为空");
        assert_eq!(err(num_int(&json!(true), 1, 500, "x")), "x 不能为空");
        assert_eq!(err(num_int(&json!("  "), 1, 500, "x")), "x 不能为空");
        assert_eq!(err(num_int(&json!("abc"), 1, 500, "x")), "x 必须是有效数字");
        assert_eq!(err(num_int(&json!("nan"), 1, 500, "x")), "x 必须是有效数字");
        assert_eq!(err(num_int(&json!([1]), 1, 500, "x")), "x 必须是有效数字");
        assert_eq!(err(num_int(&json!(2.5), 1, 500, "x")), "x 必须是整数");
        assert_eq!(err(num_int(&json!(0), 1, 500, "x")), "x 必须在 1～500 之间");
        assert_eq!(
            err(num_int(&json!(501), 1, 500, "x")),
            "x 必须在 1～500 之间"
        );
    }
}
