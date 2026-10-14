//! 占位：完整实现（Launcher multipart 图片请求体）由并行任务提供，合入时整体替换本文件。

use crate::error::HttpError;
use axum::body::Body;
use axum::http::HeaderMap;
use bytes::Bytes;
use http_body_util::BodyExt;

pub async fn read_bounded_body(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<Bytes, HttpError> {
    if let Some(v) = headers.get("content-length") {
        let s = v.to_str().unwrap_or("x");
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HttpError::new(400, "Content-Length 无效"));
        }
        let digits = s.trim_start_matches('0');
        if digits.len() > limit.to_string().len() || digits.parse::<usize>().unwrap_or(0) > limit {
            return Err(HttpError::new(413, "请求体过大"));
        }
    }
    let limited = http_body_util::Limited::new(body, limit);
    match limited.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            Err(HttpError::new(413, "请求体过大"))
        }
        Err(_) => Err(HttpError::new(400, "请求体传输中断")),
    }
}

pub fn parse_image_multipart(
    _content_type: &str,
    _body: Bytes,
    _limit: usize,
) -> Result<serde_json::Map<String, serde_json::Value>, HttpError> {
    Err(HttpError::new(400, "multipart 请求格式无效或过大"))
}
