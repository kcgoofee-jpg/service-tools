//! 后台里属于定时任务的接口：动态额度参数、定时任务的运行记录。
//!
//! | 接口 | 说明 |
//! |---|---|
//! | `GET /admin/api/quota-algo` | 当前结果、参数、最近 30 天的每日微调记录、「V5 每 1% ≈ 张」 |
//! | `PUT /admin/api/quota-algo` | 修改初始值 / 步长 / 范围，保存后立即重算 |
//! | `GET /admin/api/jobs` | 每个定时任务：名称、周期、上次运行的时间 / 结果 / 摘要 / 耗时 |

use crate::body::read_json_body;
use crate::error::{ApiError, HttpError, InternalError, ok_json};
use crate::jobs::JOBS;
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;
use gate_core::{jsondump, pyc, pyval, quota};
use serde_json::{Map, Value, json};

const BODY_LIMIT: usize = 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/api/quota-algo",
            get(quota_algo_get).put(quota_algo_put),
        )
        .route("/admin/api/jobs", get(jobs_get))
}

/// `json.loads(await db.get_setting(key, default) or default)`；解析失败是 500（旧版没有捕获）。
async fn setting_json(state: &AppState, key: &str, default: &str) -> Result<Value, ApiError> {
    let raw = state.db.get_setting(key).await?;
    let text = raw.as_deref().filter(|s| !s.is_empty()).unwrap_or(default);
    jsondump::loads(text)
        .ok_or_else(|| InternalError(format!("JSONDecodeError: {key} 不是合法 JSON")).into())
}

/// 动态额度：当前结果、参数、最近 30 天的每日微调记录，以及推算用的「V5 每 1% ≈ 张」。
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
    Ok(ok_json(&json!({
        "last": last,
        "history": history,
        "params": params,
        "images_per_percent": pyval::float(quota::V5_IMAGES_PER_PERCENT),
    })))
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

/// 修改初始值 / 步长 / 范围；保存后立即重算（也记进定时任务「动态额度」的运行记录）。
async fn quota_algo_put(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    // 极简模式（2026-10-11）：每人一个数 N。V5 每人上限由公式算出、没有可调的范围：传了也忽略
    const BOUNDS: [(&str, i64, i64); 5] = [
        ("quota_auto_enabled", 0, 1),
        ("quota_target_avg", 10, 2000),
        ("quota_ceiling_max", 10, 5000),
        ("quota_base_min_fair", 1, 2000),
        ("quota_step", 1, 500),
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
    let result = crate::jobs::quota::run_now(&state).await?;
    Ok(ok_json(&json!({"ok": true, "last": result})))
}

/// 一个任务的上次运行：只放约定的四个字段；没跑过（或库里的记录读不懂）是 `null`。
fn last_run(v: Option<&Value>) -> Value {
    let Some(Value::Object(m)) = v else {
        return Value::Null;
    };
    let pick = |k: &str| m.get(k).cloned().unwrap_or(Value::Null);
    json!({"at": pick("at"), "ok": pick("ok"), "ms": pick("ms"), "summary": pick("summary")})
}

/// 定时任务：按登记顺序，每个任务的名称、周期和上次运行的结果。
async fn jobs_get(State(state): State<AppState>, _s: AdminSession) -> Result<Response, ApiError> {
    let last = state.jobs.last().await;
    let jobs: Vec<Value> = JOBS
        .iter()
        .map(|j| {
            json!({
                "name": j.name,
                "title": j.title,
                "every": j.every,
                "every_secs": j.every_secs as i64,
                "last": last_run(last.get(j.name)),
            })
        })
        .collect();
    Ok(ok_json(&json!({"jobs": jobs})))
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

    #[test]
    fn last_run_keeps_only_the_contract_fields() {
        assert_eq!(last_run(None), Value::Null);
        assert_eq!(last_run(Some(&json!("坏数据"))), Value::Null);
        assert_eq!(
            last_run(Some(
                &json!({"at": 1.5, "ok": true, "ms": 3, "summary": "s", "x": 1})
            )),
            json!({"at": 1.5, "ok": true, "ms": 3, "summary": "s"})
        );
    }
}
