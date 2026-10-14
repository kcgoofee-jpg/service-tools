//! 网页：Vite 构建产物（`web/dist`）打进二进制；页面 HTML 由网关返回并注入首屏数据。
//!
//! - 静态资源都在 `/_owl/*`（不能用 `/assets/*`：Caddy 把它路由给 workbench 容器）。带哈希的文件长期缓存。
//! - `STATIC_DIR` 指向磁盘目录时优先读磁盘（线上热修前端用，不用重建镜像）。
//! - CSP 收紧为 `script-src 'self'`：页面里没有任何内联脚本，首屏数据用
//!   `<script type="application/json" id="owl-boot">` 注入（不会被执行，不受 script-src 限制）。

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Dist;

/// 首页 / 问卷的安全头（比旧版更严：去掉 'unsafe-inline' 与 Discord CDN 头像源）。
pub const PUBLIC_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
    img-src 'self' data:; frame-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
/// 后台的安全头。
pub const ADMIN_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
    connect-src 'self'; frame-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Landing,
    Admin,
    Survey,
}

impl Page {
    fn file(self) -> &'static str {
        match self {
            Page::Landing => "landing.html",
            Page::Admin => "admin.html",
            Page::Survey => "survey.html",
        }
    }
}

#[derive(Clone, Default)]
pub struct Assets {
    /// 磁盘覆盖目录（`STATIC_DIR`）。
    disk: Option<PathBuf>,
}

impl Assets {
    pub fn new(disk: Option<PathBuf>) -> Self {
        Self {
            disk: disk.filter(|p| p.is_dir()),
        }
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        if path.contains("..") {
            return None;
        }
        if let Some(dir) = &self.disk
            && let Ok(b) = std::fs::read(dir.join(path))
        {
            return Some(b);
        }
        Dist::get(path).map(|f| f.data.into_owned())
    }

    /// `/_owl/<path>`：哈希文件长期缓存，其余（theme-init.js、favicon）短缓存。
    pub fn asset(&self, path: &str) -> Response {
        let Some(bytes) = self.read(path) else {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        };
        let mime = mime_for(path);
        let hashed = path
            .rsplit_once('-')
            .is_some_and(|(_, tail)| tail.len() >= 8 && tail.contains('.'));
        let cache = if hashed {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=3600"
        };
        let mut resp = Response::new(Body::from(bytes));
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        h.insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        resp
    }

    /// 页面 HTML，可选注入首屏数据。
    pub fn page(&self, page: Page, boot: Option<&Value>) -> Response {
        let Some(bytes) = self.read(page.file()) else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "前端未构建（web/dist 缺失）",
            )
                .into_response();
        };
        let mut html = String::from_utf8_lossy(&bytes).into_owned();
        let tag = match boot {
            Some(v) => format!(
                r#"<script type="application/json" id="owl-boot">{}</script>"#,
                json_for_html(v)
            ),
            None => String::new(),
        };
        html = html.replacen("<!--owl-boot-->", &tag, 1);
        let mut resp = Response::new(Body::from(html));
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        apply_page_headers(h, page);
        resp
    }
}

pub fn apply_page_headers(h: &mut HeaderMap, page: Page) {
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    match page {
        Page::Admin => {
            h.insert(
                "content-security-policy",
                HeaderValue::from_static(ADMIN_CSP),
            );
            h.insert("x-frame-options", HeaderValue::from_static("DENY"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        Page::Landing | Page::Survey => {
            h.insert(
                "content-security-policy",
                HeaderValue::from_static(PUBLIC_CSP),
            );
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            // 首屏数据按登录态不同：共享缓存不能把成员的版本发给别人
            h.insert(header::VARY, HeaderValue::from_static("Cookie"));
        }
    }
}

/// 放进 `<script type="application/json">` 的 JSON：把 `<` `>` `&` 和 U+2028/2029 转义，杜绝提前闭合标签。
fn json_for_html(v: &Value) -> String {
    let raw = String::from_utf8(gate_core::pyjson::wire(v)).unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn boot_json_cannot_break_out() {
        let s =
            json_for_html(&json!({"x": "</script><script>alert(1)</script>", "y": "a&b\u{2028}"}));
        assert!(!s.contains('<') && !s.contains('>') && !s.contains('&'));
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["x"], "</script><script>alert(1)</script>");
    }

    #[test]
    fn csp_has_no_unsafe_inline() {
        assert!(!PUBLIC_CSP.contains("unsafe-inline") && !ADMIN_CSP.contains("unsafe-inline"));
        // 旧版测试锁定的三条（test_landing.py）
        for need in [
            "default-src 'none'",
            "frame-ancestors 'none'",
            "connect-src 'self'",
        ] {
            assert!(PUBLIC_CSP.contains(need));
        }
    }
}
