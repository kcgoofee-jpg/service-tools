//! 成员接口的鉴权与准入前检查（旧版 `main.authenticate` / `require_feature` / `check_rpm` /
//! `check_image_cooldown` / `limit_inflight`）。

use crate::error::{ApiError, GateError, InternalError};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use axum::extract::ConnectInfo;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use gate_core::reasons;
use gate_core::site_flags::{self, Spec};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};

/// 请求头按 latin-1 解码（Starlette 的 `request.headers.get` 行为）；没有这个头为空串。
pub fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|v| v.as_bytes().iter().map(|&b| b as char).collect())
        .unwrap_or_default()
}

/// 对端地址是否在信任列表里（uvicorn `FORWARDED_ALLOW_IPS`：精确地址、CIDR 或 `*`）。
fn trusted(host: &str, list: &[String]) -> bool {
    list.iter().any(|t| {
        if t == "*" || t == host {
            return true;
        }
        let (Some((net, bits)), Ok(ip)) = (t.split_once('/'), host.parse::<IpAddr>()) else {
            return false;
        };
        let (Ok(net), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u32>()) else {
            return false;
        };
        match (net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) if bits <= 32 => {
                let mask = if bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits)
                };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) if bits <= 128 => {
                let mask = if bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - bits)
                };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    })
}

/// 成员的 IP（旧版 `request.client.host`，经 uvicorn ProxyHeadersMiddleware 处理）：
/// 直连对端受信任时取 X-Forwarded-For 从右往左第一个不受信任的地址；全部受信任取最左边那个。
pub fn client_ip(parts: &Parts, trusted_list: &[String]) -> String {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let Some(peer) = peer else {
        return "unknown".into();
    };
    let xff = header_str(&parts.headers, "x-forwarded-for");
    if xff.is_empty() || !trusted(&peer, trusted_list) {
        return peer;
    }
    let hosts: Vec<&str> = xff.split(',').map(str::trim).collect();
    if trusted_list.iter().any(|t| t == "*") {
        return hosts[0].to_string();
    }
    hosts
        .iter()
        .rev()
        .find(|h| !trusted(h, trusted_list))
        .unwrap_or(&hosts[0])
        .to_string()
}

/// 请求上下文（中间件一定会放；单测里直接调路由时补一个）。
pub fn ctx_of(parts: &Parts) -> ReqCtx {
    parts
        .extensions
        .get::<ReqCtx>()
        .cloned()
        .unwrap_or_else(|| ReqCtx::new(String::new()))
}

/// `Authorization: Bearer <key>` 里的 Key（大小写不敏感；没有为空串）。
pub fn bearer(headers: &HeaderMap) -> String {
    let auth = header_str(headers, "authorization");
    if auth
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
    {
        gate_core::pyc::strip(&auth[7..]).to_string()
    } else {
        String::new()
    }
}

/// 校验虚拟 Key。`passive`：首页每秒查排队位置，只读，不算使用，不参与暂停闸门。
pub async fn authenticate(state: &AppState, parts: &Parts, passive: bool) -> Result<Key, ApiError> {
    let ctx = ctx_of(parts);
    let ip = client_ip(parts, &state.env.forwarded_allow_ips);
    let now = state.now();
    let wait = state.auth_guard.blocked(&ip, now);
    let token = bearer(&parts.headers);
    let too_many = || {
        GateError::new(
            429,
            format!("无效请求过多，请 {} 分钟后再试", wait / 60 + 1),
        )
    };
    if token.is_empty() {
        if wait > 0 {
            return Err(too_many().into());
        }
        return Err(GateError::new(401, "缺少 API Key").into());
    }
    let Some(row) = state.db.get_key_by_token(&token).await? else {
        // 被拦截的 IP 只拦「无效 Key」；持有有效 Key 的成员（如同一出口 IP 的其他人）不受影响。
        if wait > 0 {
            return Err(too_many().into());
        }
        record_auth_failure(state, &ip, now);
        return Err(GateError::new(401, "无效的 API Key").into());
    };
    let key = Key(row);
    ctx.set_key(key.id(), key.name());
    if !key.enabled() {
        let why = key.get("disabled_reason").as_str();
        return Err(GateError::new(403, gate_core::key_status::disabled_message(why)).into());
    }
    if key.expired(now) {
        return Err(GateError::new(403, "该 Key 已过期").into());
    }
    if passive {
        return Ok(key);
    }
    if let Some((until, reason)) = state.pauses.paused(key.id(), now) {
        let until = state.clock.format(until, "%m-%d %H:%M");
        let msg = if reason.is_empty() {
            format!("这把 Key 已被临时暂停，{until} 自动恢复。")
        } else {
            format!("{reason}，{until} 自动恢复。")
        };
        return Err(GateError::with_code(403, msg, reasons::KEY_PAUSED).into());
    }
    // 只要 Key 实际通过鉴权即视为使用。记「最后使用时间」只是记账：写失败不能让成员的请求 500。
    if let Err(e) = state.db.touch_key(key.id(), now).await {
        state.bug("touch_key", Some(("DbError", &e.to_string())), &ctx);
    }
    Ok(key)
}

/// 记一次无效 Key；达到阈值时拦截该 IP 并告警。
pub fn record_auth_failure(state: &AppState, ip: &str, now: f64) {
    let e = &state.env;
    if state.auth_guard.record_failure(
        ip,
        e.auth_fail_max,
        e.auth_fail_window,
        e.auth_block_seconds,
        now,
    ) {
        state.alerter.notify(
            "auth_flood",
            &format!(
                "有 IP 在 {} 秒内用无效 Key 反复请求 {} 次，已临时拦截 {} 分钟（IP 后两段：{}）。",
                e.auth_fail_window.max(1),
                e.auth_fail_max,
                e.auth_block_seconds.div_euclid(60),
                crate::runtime::mask_ip(ip)
            ),
            1800.0,
        );
    }
}

/// `site_flags.get(db, spec, settings)`。
pub async fn site_flag(state: &AppState, spec: &Spec) -> Result<Value, gate_db::DbError> {
    let raw = state.db.get_setting(spec.key).await?;
    Ok(site_flags::get_from_raw(
        spec,
        raw.as_deref(),
        Some(&state.env.settings_view()),
    ))
}

/// 数值型站点设置（旧版直接拿来和数字比较）。
pub async fn site_flag_f64(state: &AppState, spec: &Spec) -> Result<f64, gate_db::DbError> {
    Ok(gate_core::pyval::number_like(&site_flag(state, spec).await?).unwrap_or(0.0))
}

/// 功能开关（站长全局开关 + 这把 Key 的功能列表）。
pub async fn require_feature(state: &AppState, key: &Key, name: &str) -> Result<(), ApiError> {
    use gate_core::features::{GLOBAL_KEY, VibeAnlas, check_decision, needs_vibe_site_flags};
    let global = state.db.get_setting(GLOBAL_KEY).await?;
    let vibe = if needs_vibe_site_flags(global.as_deref(), &key.0, name) {
        Some(VibeAnlas {
            until: site_flag_f64(state, &site_flags::GLOBAL_DAILY_ANLAS_UNTIL).await?,
            daily_cap: site_flag_f64(state, &site_flags::GLOBAL_DAILY_ANLAS).await? as i64,
        })
    } else {
        None
    };
    match check_decision(global.as_deref(), &key.0, name, state.now(), vibe) {
        Ok(None) => Ok(()),
        Ok(Some(reason)) => Err(GateError::new(403, reason).into()),
        Err(e) => Err(InternalError(e.to_string()).into()),
    }
}

/// 每分钟请求数。
pub fn check_rpm(state: &AppState, key: &Key) -> Result<(), GateError> {
    if key.is_admin() {
        return Ok(());
    }
    let limit = key.rpm().max(1) as usize;
    if state
        .rpm
        .check_then_push(&key.id(), limit, 60.0, state.now())
    {
        return Ok(());
    }
    Err(GateError::with_code(
        429,
        format!("请求过于频繁（上限 {} 次/分钟），请稍后再试", key.rpm()),
        reasons::KEY_RATE,
    ))
}

/// 上游 429 后的全站图片冷却。
pub fn check_image_cooldown(state: &AppState) -> Result<(), GateError> {
    let remaining = state.cooldown.remaining(state.now());
    if remaining > 0 {
        return Err(GateError::with_code(
            429,
            format!("上游图片服务限流保护中，所有图片生成暂停约 {remaining} 秒"),
            reasons::COOLDOWN,
        ));
    }
    Ok(())
}

/// 读请求体之前按 Authorization 限制同时进行中的请求数（单人并发堆积能拖垮 2 GB 机器）。
pub fn limit_inflight(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<crate::runtime::InflightPermit>, GateError> {
    state
        .inflight
        .acquire(&header_str(headers, "authorization"))
        .map_err(|_| {
            GateError::with_code(
                429,
                format!(
                    "同时进行中的请求过多（上限 {} 个），请等前面的请求完成",
                    crate::runtime::MAX_INFLIGHT_PER_KEY
                ),
                reasons::KEY_RATE,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(peer: &str, xff: Option<&str>) -> Parts {
        let mut b = axum::http::Request::builder().uri("/");
        if let Some(x) = xff {
            b = b.header("x-forwarded-for", x);
        }
        let (mut p, _) = b.body(()).unwrap().into_parts();
        p.extensions.insert(ConnectInfo(
            format!("{peer}:1234").parse::<SocketAddr>().unwrap(),
        ));
        p
    }

    #[test]
    fn forwarded_for_trust() {
        let t = vec!["127.0.0.1".to_string(), "172.30.0.1".to_string()];
        assert_eq!(
            client_ip(&parts("172.30.0.1", Some("203.0.113.9")), &t),
            "203.0.113.9"
        );
        // 不受信任的对端：XFF 被忽略
        assert_eq!(
            client_ip(&parts("198.51.100.7", Some("203.0.113.9")), &t),
            "198.51.100.7"
        );
        // 多跳：从右往左第一个不受信任的
        assert_eq!(
            client_ip(
                &parts("127.0.0.1", Some("10.9.9.9, 203.0.113.9, 172.30.0.1")),
                &t
            ),
            "203.0.113.9"
        );
        assert_eq!(client_ip(&parts("127.0.0.1", None), &t), "127.0.0.1");
        let cidr = vec!["172.30.0.0/16".to_string()];
        assert_eq!(
            client_ip(&parts("172.30.5.5", Some("203.0.113.9")), &cidr),
            "203.0.113.9"
        );
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "bEaReR   nai-abc  ".parse().unwrap());
        assert_eq!(bearer(&h), "nai-abc");
        h.insert("authorization", "Basic x".parse().unwrap());
        assert_eq!(bearer(&h), "");
    }
}
