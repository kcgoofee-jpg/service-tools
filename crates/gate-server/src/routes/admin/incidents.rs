//! 状态页事件的后台接口（旧版 `status_page.admin_router`）：发布事件、追加更新、列出全部事件。
//!
//! 事件按 Statuspage 标准生命周期发布：调查中 → 已确定 → 监控中 → 已解决（中途可发「更新」）。
//! 正文留空 = 用该状态的标准句式；写法约定（不写内部原因 / 数据量 / 成员）见 `routes::status` 顶部。
//! 与其它后台接口一样经过操作日志中间件，写请求自动校验来源。

use crate::body::{JsonObject, read_json_body};
use crate::error::{ApiError, HttpError, ok_json};
use crate::routes::status::{COMPONENTS, STATUS_LABEL, incident_json, standard_body};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::post;
use gate_core::pyc::{strip, take_chars};
use gate_core::pyval::py_str;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/incidents", post(create).get(list))
        .route(
            "/admin/api/incidents/{incident_id}/updates",
            post(add_update),
        )
}

const BODY_LIMIT: usize = 1024 * 1024;
/// 事件标题最长 200 字，更新正文最长 2000 字（按字符，与旧版一致）。
const TITLE_MAX: usize = 200;
const BODY_MAX: usize = 2000;
const IMPACTS: [&str; 4] = ["none", "minor", "major", "critical"];
/// `at` 允许的范围：400 天前到明天（坏值入库会让公开 /status 出错）。
const AT_PAST_SECS: f64 = 400.0 * 86400.0;
const AT_FUTURE_SECS: f64 = 86400.0;

fn unprocessable(msg: &str) -> HttpError {
    HttpError::new(422, msg)
}

/// 路径里的 `incident_id: int`。FastAPI 在进入处理函数之前就校验路径参数，所以它排在登录校验前面：
/// 不是整数一律 422（pydantic v2 的错误格式），不管有没有登录。
struct IncidentId(i64);

impl FromRequestParts<AppState> for IncidentId {
    type Rejection = HttpError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, HttpError> {
        let raw = Path::<String>::from_request_parts(parts, state)
            .await
            .map(|Path(s)| s)
            .unwrap_or_default();
        let digits = raw.trim().trim_start_matches(['+', '-']);
        match raw.trim().parse::<i64>() {
            Ok(n) => Ok(Self(n)),
            // 位数超出 64 位的整数是合法的 int，只是必然不存在这个事件
            Err(_) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                Ok(Self(i64::MAX))
            }
            Err(_) => Err(HttpError {
                status: 422,
                detail: json!([{
                    "type": "int_parsing",
                    "loc": ["path", "incident_id"],
                    "msg": "Input should be a valid integer, unable to parse string as an integer",
                    "input": raw,
                }]),
            }),
        }
    }
}

/// `_check`：返回 (状态, 正文)。正文去首尾空白，留空用标准句式，最长 2000 字。
fn check(body: &JsonObject, need_title: bool) -> Result<(String, String), HttpError> {
    let status = body.get("status").map(py_str).unwrap_or_default();
    if !STATUS_LABEL.iter().any(|(s, _)| *s == status) {
        return Err(unprocessable(
            "status 必须是 investigating / identified / update / monitoring / resolved",
        ));
    }
    let given = body.get("body").map(py_str).unwrap_or_default();
    let given = strip(&given);
    let text = if given.is_empty() {
        standard_body(&status).unwrap_or_default()
    } else {
        given
    };
    if need_title && strip(&body.get("title").map(py_str).unwrap_or_default()).is_empty() {
        return Err(unprocessable("需要 title"));
    }
    Ok((status, take_chars(text, BODY_MAX).to_string()))
}

/// `_at`：可选的事件时间（秒）。超出 [400 天前, 明天] 返回 422（要秒，不是毫秒）。
fn parse_at(body: &JsonObject, now: f64) -> Result<Option<f64>, HttpError> {
    let at = match body.get("at") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()),
        Some(_) => None,
    };
    let Some(at) = at else {
        return Err(unprocessable("at 必须是秒级时间戳"));
    };
    if !(now - AT_PAST_SECS..=now + AT_FUTURE_SECS).contains(&at) {
        return Err(unprocessable("at 超出范围（要秒，不是毫秒）"));
    }
    Ok(Some(at))
}

async fn create(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let (status, text) = check(&body, true)?;
    let impact = match body.get("impact") {
        None => "minor",
        Some(Value::String(s)) if IMPACTS.contains(&s.as_str()) => s.as_str(),
        Some(_) => {
            return Err(unprocessable("impact 必须是 none / minor / major / critical").into());
        }
    };
    let mut comps: Vec<String> = match body.get("components") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .filter(|c| COMPONENTS.iter().any(|(k, _)| k == c))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if comps.is_empty() {
        comps.push("api".into());
    }
    let now = state.now();
    let at = parse_at(&body, now)?;
    let title = body.get("title").map(py_str).unwrap_or_default();
    let title = take_chars(strip(&title), TITLE_MAX);
    let id = state
        .db
        .create_incident(title, impact, &comps, &status, &text, at.unwrap_or(now))
        .await?;
    Ok(ok_json(&json!({"id": id})))
}

async fn add_update(
    State(state): State<AppState>,
    IncidentId(id): IncidentId,
    _s: AdminSession,
    req: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let (status, text) = check(&body, false)?;
    let missing = || HttpError::new(404, "事件不存在");
    if !state.db.incident_exists(id).await? {
        return Err(missing().into());
    }
    let now = state.now();
    let at = parse_at(&body, now)?;
    if !state
        .db
        .add_incident_update(id, &status, &text, at.unwrap_or(now))
        .await?
    {
        return Err(missing().into());
    }
    Ok(ok_json(&json!({"ok": true})))
}

/// 全部事件（含只影响官网 / 机器人、不上公开页的），开始时间新的在前。
async fn list(State(state): State<AppState>, _s: AdminSession) -> Result<Response, ApiError> {
    let incidents = state.db.list_incidents(0.0).await?;
    Ok(ok_json(&json!({
        "incidents": incidents.iter().map(incident_json).collect::<Vec<_>>(),
    })))
}
