//! 标签补全：同一把 Key 只保留最新的一次查询，排队有上限，超时记一条失败。

use super::binary;
use crate::accounting::Entry;
use crate::admission::acquire_slots;
use crate::auth::{authenticate, check_image_cooldown, ctx_of, require_feature};
use crate::body::read_json;
use crate::error::{ApiError, GateError, upstream_transport};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use gate_core::pyval::{py_str, truthy};
use gate_core::upstream_errors::upstream_error_message;
use gate_upstream::client::{Callbacks, RequestOpts};
use serde_json::json;
use std::time::Duration;

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

pub(super) async fn tags(state: AppState, req: Request) -> Result<Response, ApiError> {
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
            .wait(key.id(), state.env.global_concurrency)
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
}
