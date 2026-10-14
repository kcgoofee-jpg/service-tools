//! 错误响应：三种格式，与旧版逐字节一致。
//!
//! 1. [`GateError`]（成员接口）：顶层 `statusCode` / `message` 与 NovelAI 官方错误格式一致，
//!    客户端直接显示这句话；消息统一带「猫头鹰公益站提醒：」前缀，免得成员以为是 NovelAI 官方出错。
//!    `{"statusCode": s, "message": m, "error": {"message": m, "status": s, "request_id": rid}}`
//! 2. [`HttpError`]（后台 / 桥接 / 成员页接口，对应 FastAPI 的 `HTTPException`）：`{"detail": m}`。
//! 3. 未预料的内部错误（500）：`{"error": {"message": "...错误编号 rid", "status": 500, "request_id": rid}}`，
//!    并带 `X-Request-Id`。
//!
//! 请求编号 rid 来自 [`crate::reqctx`]：错误在请求处理过程中产生时一定能拿到。

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

pub const NOTICE_PREFIX: &str = "猫头鹰公益站提醒：";

/// 成员接口的拒绝 / 失败。`code` 是拒绝原因码（gate_core::reasons），用于用量日志统计。
#[derive(Debug, Clone)]
pub struct GateError {
    pub status: u16,
    pub message: String,
    pub code: String,
    pub billing_uncertain: bool,
}

impl GateError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: String::new(),
            billing_uncertain: false,
        }
    }

    pub fn with_code(status: u16, message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: code.into(),
            billing_uncertain: false,
        }
    }

    pub fn uncertain(mut self, yes: bool) -> Self {
        self.billing_uncertain = yes;
        self
    }

    /// 带前缀的成员可见文案。
    pub fn display_message(&self) -> String {
        if self.message.starts_with(NOTICE_PREFIX) {
            self.message.clone()
        } else {
            format!("{NOTICE_PREFIX}{}", self.message)
        }
    }
}

/// 附在响应扩展里，让外层中间件（记拒绝日志 / Bug 追踪）知道这是哪种错误。
#[derive(Debug, Clone)]
pub struct GateErrorInfo(pub GateError);

impl IntoResponse for GateError {
    fn into_response(self) -> Response {
        let rid = crate::reqctx::current_rid();
        let message = self.display_message();
        let body = json!({
            "statusCode": self.status,
            "message": message,
            "error": {"message": message, "status": self.status, "request_id": rid},
        });
        let mut resp = json_response(self.status, &body);
        resp.extensions_mut().insert(GateErrorInfo(self));
        resp
    }
}

/// FastAPI `HTTPException(status, detail)` 的等价物。
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub detail: Value,
}

impl HttpError {
    pub fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: Value::String(detail.into()),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        json_response(self.status, &json!({"detail": self.detail}))
    }
}

/// 未预料的内部错误：记 Bug，回给成员一个可以报给站长的请求编号。
#[derive(Debug)]
pub struct InternalError(pub String);

impl<E: std::fmt::Display> From<E> for InternalError {
    fn from(e: E) -> Self {
        InternalError(e.to_string())
    }
}

/// 附在 500 响应上，交给外层中间件记 Bug。
#[derive(Debug, Clone)]
pub struct InternalErrorInfo(pub String);

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        let rid = crate::reqctx::current_rid();
        let body = json!({"error": {
            "message": format!("{NOTICE_PREFIX}服务器内部错误，已自动记录。反馈时请附上错误编号 {rid}"),
            "status": 500,
            "request_id": rid,
        }});
        let mut resp = json_response(500, &body);
        if !rid.is_empty()
            && let Ok(v) = HeaderValue::from_str(&rid)
        {
            resp.headers_mut().insert("x-request-id", v);
        }
        resp.extensions_mut().insert(InternalErrorInfo(self.0));
        resp
    }
}

/// 与 Starlette `JSONResponse` 相同的序列化（紧凑、UTF-8、Python 浮点格式）。
pub fn json_response(status: u16, body: &Value) -> Response {
    let mut resp = Response::new(axum::body::Body::from(gate_core::pyjson::wire(body)));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    resp
}

/// 普通 200 JSON 响应。
pub fn ok_json(body: &Value) -> Response {
    json_response(200, body)
}

/// 路由处理函数的统一返回类型。
pub type ApiResult = Result<Response, ApiError>;

/// 三种错误的合集，方便 `?`。
#[derive(Debug)]
pub enum ApiError {
    Gate(GateError),
    Http(HttpError),
    Internal(InternalError),
}

impl From<GateError> for ApiError {
    fn from(e: GateError) -> Self {
        ApiError::Gate(e)
    }
}
impl From<HttpError> for ApiError {
    fn from(e: HttpError) -> Self {
        ApiError::Http(e)
    }
}
impl From<InternalError> for ApiError {
    fn from(e: InternalError) -> Self {
        ApiError::Internal(e)
    }
}
impl From<gate_db::DbError> for ApiError {
    fn from(e: gate_db::DbError) -> Self {
        ApiError::Internal(InternalError(e.to_string()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Gate(e) => e.into_response(),
            ApiError::Http(e) => e.into_response(),
            ApiError::Internal(e) => e.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn gate_error_shape_matches_python() {
        let resp =
            GateError::new(429, "请求过于频繁（上限 5 次/分钟），请稍后再试").into_response();
        assert_eq!(resp.status(), 429);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"statusCode":429,"message":"猫头鹰公益站提醒：请求过于频繁（上限 5 次/分钟），请稍后再试","error":{"message":"猫头鹰公益站提醒：请求过于频繁（上限 5 次/分钟），请稍后再试","status":429,"request_id":""}}"#
        );
    }

    #[tokio::test]
    async fn http_error_is_detail() {
        let resp = HttpError::new(401, "未登录或会话已过期").into_response();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"detail":"未登录或会话已过期"}"#
        );
    }
}
