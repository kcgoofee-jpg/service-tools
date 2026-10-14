//! 后台接口的请求解析（成员 / 记录两组接口共用）。
//!
//! FastAPI 的行为要照搬：路径 / 查询参数先按声明的类型校验，失败回 422 + `{"detail": [{type, loc, msg, input}]}`，
//! 而且**发生在后台鉴权之前**（没登录的人拿到的也是 422，不是 401）；校验失败不进操作日志。
//! 查询串按 Starlette 的规则解析：`+` 是空格、同名参数以最后一个为准、`?all` 这种没有等号的值为空串。

use super::{ActionLogged, percent_decode};
use crate::body::{JsonObject, read_json_body};
use crate::error::{ApiError, HttpError, InternalError, json_response};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{FromRequestParts, RawPathParams, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// 处理函数的错误：普通错误，或 FastAPI 的参数校验失败。
pub enum Reject {
    Api(ApiError),
    Validation(Vec<Value>),
}

impl From<ApiError> for Reject {
    fn from(e: ApiError) -> Self {
        Reject::Api(e)
    }
}
impl From<HttpError> for Reject {
    fn from(e: HttpError) -> Self {
        Reject::Api(e.into())
    }
}
impl From<InternalError> for Reject {
    fn from(e: InternalError) -> Self {
        Reject::Api(e.into())
    }
}
impl From<gate_db::DbError> for Reject {
    fn from(e: gate_db::DbError) -> Self {
        Reject::Api(e.into())
    }
}

impl IntoResponse for Reject {
    fn into_response(self) -> Response {
        match self {
            Reject::Api(e) => e.into_response(),
            Reject::Validation(items) => {
                let mut resp = json_response(422, &json!({"detail": items}));
                // 旧版里 RequestValidationError 不经过操作日志的包装：不记
                resp.extensions_mut().insert(ActionLogged);
                resp
            }
        }
    }
}

pub type R = Result<Response, Reject>;

/// pydantic v2 宽松模式下 `str → int`：首尾空白、一个正负号、数字间可夹单个下划线、`2.0` 这种小数部分全为 0 的写法；
/// 不认 `2.`、`.5`、`2e0`、Unicode 数字。超出 i128 的值饱和（只用来比大小 / 夹取，不会参与计算）。
pub fn parse_py_int(raw: &str) -> Option<i128> {
    let t = raw.trim();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let (int_part, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    if let Some(f) = frac
        && (f.is_empty() || !f.bytes().all(|b| b == b'0'))
    {
        return None;
    }
    let b = int_part.as_bytes();
    if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit() || *c == b'_') {
        return None;
    }
    for (i, c) in b.iter().enumerate() {
        if *c == b'_' {
            let prev = i > 0 && b[i - 1].is_ascii_digit();
            let next = b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if !(prev && next) {
                return None;
            }
        }
    }
    let mut v: i128 = 0;
    for c in b.iter().filter(|c| **c != b'_') {
        v = v.saturating_mul(10).saturating_add(i128::from(c - b'0'));
    }
    Some(if neg { -v } else { v })
}

/// pydantic v2 的 `str → bool`：不区分大小写的 0/1/true/false/t/f/yes/no/y/n/on/off，不接受空白。
pub fn parse_py_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Python `parse_qsl(keep_blank_values=True)`：`+` 变空格再百分号解码，空段跳过，没有 `=` 的值为空串。
fn parse_qsl(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let (k, v) = seg.split_once('=').unwrap_or((seg, ""));
            let dec = |s: &str| percent_decode(&s.replace('+', " "));
            (dec(k), dec(v))
        })
        .collect()
}

/// 一次后台请求：解析好的路径 / 查询参数，加上还没读的请求体。
pub struct Ctx {
    pub state: AppState,
    pub parts: Parts,
    body: Option<Body>,
    path: Vec<(String, String)>,
    query: Vec<(String, String)>,
    errors: Vec<Value>,
}

impl Ctx {
    pub async fn new(state: AppState, req: Request) -> Result<Self, Reject> {
        let (mut parts, body) = req.into_parts();
        let path = match RawPathParams::from_request_parts(&mut parts, &state).await {
            Ok(p) => p
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let query = parse_qsl(parts.uri.query().unwrap_or(""));
        Ok(Self {
            state,
            parts,
            body: Some(body),
            path,
            query,
            errors: Vec::new(),
        })
    }

    /// 路径参数原文（已百分号解码）。
    pub fn path(&self, name: &str) -> String {
        self.path
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// 查询参数原文；同名参数取最后一个（Starlette `QueryParams.get`）。
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn fail(&mut self, loc: &str, name: &str, kind: &str, msg: &str, input: &str) {
        self.errors.push(json!({
            "type": kind, "loc": [loc, name], "msg": msg, "input": input,
        }));
    }

    fn int_error(&mut self, loc: &str, name: &str, input: &str) {
        self.fail(
            loc,
            name,
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
            input,
        );
    }

    /// 声明为 `int` 的路径参数。
    pub fn path_int(&mut self, name: &str) -> i128 {
        let raw = self.path(name);
        parse_py_int(&raw).unwrap_or_else(|| {
            self.int_error("path", name, &raw);
            0
        })
    }

    /// 声明为 `int = default` 的查询参数。
    pub fn query_int(&mut self, name: &str, default: i128) -> i128 {
        let Some(raw) = self.query(name).map(str::to_string) else {
            return default;
        };
        parse_py_int(&raw).unwrap_or_else(|| {
            self.int_error("query", name, &raw);
            default
        })
    }

    /// 声明为 `Optional[int] = None` 的查询参数。
    pub fn query_opt_int(&mut self, name: &str) -> Option<i128> {
        let raw = self.query(name)?.to_string();
        match parse_py_int(&raw) {
            Some(v) => Some(v),
            None => {
                self.int_error("query", name, &raw);
                None
            }
        }
    }

    /// 声明为 `bool = default` 的查询参数。
    pub fn query_bool(&mut self, name: &str, default: bool) -> bool {
        let Some(raw) = self.query(name).map(str::to_string) else {
            return default;
        };
        parse_py_bool(&raw).unwrap_or_else(|| {
            self.fail(
                "query",
                name,
                "bool_parsing",
                "Input should be a valid boolean, unable to interpret input",
                &raw,
            );
            default
        })
    }

    /// 参数校验（FastAPI 在进入处理函数之前做；所有错误一起返回）。
    pub fn check(&mut self) -> Result<(), Reject> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(Reject::Validation(std::mem::take(&mut self.errors)))
        }
    }

    /// 旧版 `require_admin(request)` 之前的全部步骤：参数校验 → 来源校验（写请求）→ 会话校验。
    pub async fn admin(&mut self) -> Result<(), Reject> {
        self.check()?;
        AdminSession::from_request_parts(&mut self.parts, &self.state)
            .await
            .map_err(Reject::from)?;
        Ok(())
    }

    /// `read_json_body(request)`：请求体最大 1 MiB，必须是 JSON 对象。
    pub async fn json(&mut self) -> Result<JsonObject, Reject> {
        let body = self.body.take().unwrap_or_else(Body::empty);
        Ok(read_json_body(&self.parts.headers, body, 1024 * 1024).await?)
    }
}

// ----------------------------------------------------------------------------- 下载响应

/// `urllib.parse.quote(s)`（默认 safe="/"）：字母数字和 `_.-~/` 原样，其余按 UTF-8 百分号编码。
pub fn py_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 二进制下载 / 图片响应。
pub fn bytes_response(data: Vec<u8>, media_type: &str, headers: &[(&str, String)]) -> Response {
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(media_type) {
        h.insert("content-type", v);
    }
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(v),
        ) {
            h.insert(name, val);
        }
    }
    resp
}

/// 数据库 / 内部错误统一成 500（带请求编号）。
pub fn internal(msg: impl std::fmt::Display) -> Reject {
    Reject::Api(InternalError(msg.to_string()).into())
}

/// `py_int` 超出 SQLite 整数范围时旧版抛 OverflowError → 500。
pub fn sql_int(v: i128) -> Result<i64, Reject> {
    i64::try_from(v)
        .map_err(|_| internal("OverflowError: Python int too large to convert to SQLite INTEGER"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_rules_match_pydantic() {
        for (s, v) in [
            ("2", Some(2)),
            (" 2 ", Some(2)),
            ("-2", Some(-2)),
            ("+2", Some(2)),
            ("2.0", Some(2)),
            ("2.00", Some(2)),
            ("1_0", Some(10)),
            ("00012", Some(12)),
            ("-0", Some(0)),
            ("2.", None),
            (".5", None),
            ("2.5", None),
            ("2e0", None),
            ("0x10", None),
            ("1__0", None),
            ("_1", None),
            ("1_", None),
            ("٣", None),
            ("１２", None),
            ("", None),
            ("nan", None),
        ] {
            assert_eq!(parse_py_int(s), v, "{s:?}");
        }
        assert_eq!(
            parse_py_int("99999999999999999999"),
            Some(99999999999999999999)
        );
    }

    #[test]
    fn bool_rules_match_pydantic() {
        for s in ["1", "true", "TRUE", "Yes", "on", "T", "y"] {
            assert_eq!(parse_py_bool(s), Some(true), "{s}");
        }
        for s in ["0", "false", "No", "OFF", "f", "N"] {
            assert_eq!(parse_py_bool(s), Some(false), "{s}");
        }
        for s in ["", " 1", "1 ", "2", "-1", "enable", "none"] {
            assert_eq!(parse_py_bool(s), None, "{s:?}");
        }
    }

    #[test]
    fn qsl_matches_starlette() {
        let q = parse_qsl("a=1&a=2&b&c=x+y%20z&&d=%E4%B8%AD");
        assert_eq!(
            q,
            [
                ("a".into(), "1".into()),
                ("a".into(), "2".into()),
                ("b".into(), "".into()),
                ("c".into(), "x y z".into()),
                ("d".into(), "中".into()),
            ]
        );
    }

    #[test]
    fn quote_keeps_slash_and_tilde() {
        assert_eq!(
            py_quote("owl-月见草-3.zip"),
            "owl-%E6%9C%88%E8%A7%81%E8%8D%89-3.zip"
        );
        assert_eq!(py_quote("a b/c~d"), "a%20b/c~d");
    }
}
