//! 成员网页接口（旧版 `app/registration_routes.py` 的下半部分）：用 Discord 登录、个人状态、重置券、
//! 捐赠 Key、原图打包。会话 Cookie 的签发 / 校验见 `crate::session`。

use super::EXPORT_PER_MEMBER_SECS;
use super::registration::{no_store, read_model};
use crate::action_log::log_action;
use crate::audit::audit_image_days;
use crate::auth::{client_ip, header_str, site_flag};
use crate::error::{ApiError, ApiResult, HttpError, InternalError, ok_json};
use crate::registration::RegErr;
use crate::session::{
    MEMBER_COOKIE, MEMBER_SESSION_DAYS, STATE_COOKIE, cookie, delete_cookie_header, key_tag,
    member_state, set_cookie_header, sign_member,
};
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use gate_core::pyc::{len_chars, strip};
use gate_core::pyval;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use subtle::ConstantTimeEq;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_start))
        .route("/login/callback", get(login_callback))
        .route("/logout", post(logout))
        .route("/public/donate", post(public_donate))
        .route("/public/me", get(public_me))
        .route("/public/me/coupons/{coupon_id}/use", post(use_my_coupon))
        .route("/public/my-export", get(my_export))
}

/// 每 IP 每小时最多提交几次捐赠。
const DONATE_PER_HOUR: usize = 5;

/// `RedirectResponse(url)`：307，没有响应体。
fn redirect(location: &str) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    if let Ok(v) = HeaderValue::from_str(location) {
        resp.headers_mut().insert("location", v);
    }
    resp.headers_mut()
        .insert("content-length", HeaderValue::from_static("0"));
    resp
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    InternalError(e.to_string()).into()
}

/// 网页 Discord 登录是否暂停（默认暂停，设置 web_login_paused=0 才开放）：Discord 应用审核期间不发起任何 OAuth。
async fn web_login_paused(state: &AppState) -> Result<bool, ApiError> {
    Ok(pyval::truthy(
        &site_flag(state, &gate_core::site_flags::WEB_LOGIN_PAUSED).await?,
    ))
}

async fn log_member(state: &AppState, who: &str, action: &str, target: &str, detail: &str) {
    log_action(
        &state.db,
        &format!("Discord:{who}"),
        action,
        target,
        detail,
        true,
        state.now(),
    )
    .await;
}

// ------------------------------------------------------------------ 用 Discord 登录

async fn login_start(State(state): State<AppState>) -> ApiResult {
    let Some(svc) = state.registration.as_deref() else {
        return Err(HttpError::new(503, "登录尚未配置").into());
    };
    // 首页按钮已隐藏；书签 / 旧链接 / 爬虫直接访问也不能进入 OAuth
    if web_login_paused(&state).await? {
        return Ok(redirect("/?login=paused#live"));
    }
    use base64::Engine;
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 24]>());
    let mut resp = redirect(&svc.web_login_url(&nonce));
    resp.headers_mut().append(
        "set-cookie",
        set_cookie_header(STATE_COOKIE, &nonce, 600, true, "lax"),
    );
    Ok(resp)
}

async fn login_callback(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult {
    let Some(svc) = state.registration.as_deref() else {
        return Err(HttpError::new(503, "登录尚未配置").into());
    };
    if web_login_paused(&state).await? {
        return Ok(redirect("/?login=paused#live"));
    }
    let code = q.get("code").map_or("", String::as_str);
    let given = q.get("state").map_or("", String::as_str);
    let expected = cookie(&headers, STATE_COOKIE).unwrap_or_default();
    let same =
        given.len() == expected.len() && bool::from(given.as_bytes().ct_eq(expected.as_bytes()));
    if code.is_empty() || given.is_empty() || !same {
        // error=access_denied（成员点了取消）/ 缺 state cookie（跨浏览器打开授权页）都会走到这里
        tracing::info!(
            "[web-login] callback rejected: code={} state_ok=n err={}",
            if code.is_empty() { 'n' } else { 'y' },
            gate_core::pyc::take_chars(q.get("error").map_or("", String::as_str), 40)
        );
        return Ok(redirect("/?login=failed"));
    }
    let who = match svc.web_identify(code).await {
        Ok(w) => w,
        Err(RegErr::Denied(_)) => return Ok(redirect("/?login=failed")),
        Err(RegErr::Internal(m)) => return Err(internal(m)),
    };
    if !who.in_server {
        // 不在社区服务器的 Discord 账号不发会话
        tracing::info!("[web-login] not in server / role gate");
        return Ok(redirect("/?login=failed"));
    }
    let mut resp = redirect("/");
    let exp = state.now() as i64 + MEMBER_SESSION_DAYS * 86400;
    let tag = key_tag(svc.key_row_for(&who.id).await?.as_ref());
    resp.headers_mut().append(
        "set-cookie",
        set_cookie_header(
            MEMBER_COOKIE,
            &sign_member(&state, &who.id, exp, &tag),
            MEMBER_SESSION_DAYS * 86400,
            true,
            "lax",
        ),
    );
    resp.headers_mut().append(
        "set-cookie",
        delete_cookie_header(STATE_COOKIE, false, "lax", false),
    );
    log_member(&state, &who.id, "网页登录", "", "").await;
    Ok(resp)
}

async fn logout() -> Response {
    let mut resp = ok_json(&json!({"ok": true}));
    resp.headers_mut().append(
        "set-cookie",
        delete_cookie_header(MEMBER_COOKIE, false, "lax", false),
    );
    resp
}

// ------------------------------------------------------------------ 捐赠 Key

/// 捐赠提交的限流与校验（同步完成，限流计数的「查 → 记」在一把锁里，并发请求不会一起溜过去）。
/// 次序与旧版一致：先看每 IP 每小时次数，再校验字段；字段不合格的请求不占次数。
fn admit_donation(
    state: &AppState,
    ip: &str,
    now: f64,
    token: &str,
    message: &str,
    contact: &str,
) -> Result<(String, String, String), HttpError> {
    let mut map = state.member_web.donate_hits.lock().expect("donate");
    if map.len() > 5000 {
        // 定期清掉过期来源，避免换 IP 喷射时内存只增不减
        map.retain(|_, v| v.last().is_some_and(|t| now - t < 3600.0));
    }
    let mut hits: Vec<f64> = map
        .get(ip)
        .map(|v| v.iter().copied().filter(|t| now - t < 3600.0).collect())
        .unwrap_or_default();
    if hits.len() >= DONATE_PER_HOUR {
        return Err(HttpError::new(429, "提交太频繁，请一小时后再试"));
    }
    let (token, message, contact) = (
        strip(token).to_string(),
        strip(message).to_string(),
        strip(contact).to_string(),
    );
    if !(20..=4000).contains(&len_chars(&token)) {
        return Err(HttpError::new(
            422,
            "Token 看起来不完整：请从 NovelAI 官网把整段 Token 复制过来",
        ));
    }
    if message.is_empty() || len_chars(&message) > 500 {
        return Err(HttpError::new(
            422,
            "请写一句留言 / 使用要求（500 字以内），站长好接进合适的池子",
        ));
    }
    if len_chars(&contact) > 100 {
        return Err(HttpError::new(422, "联系方式请控制在 100 字以内"));
    }
    hits.push(now);
    map.insert(ip.to_string(), hits);
    Ok((token, message, contact))
}

/// 首页「捐赠 Key」：访客把 NovelAI Token 交给站长并附使用要求。只入库，人工验证后才接入，绝不写日志。
async fn public_donate(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let token = m.string("token", None);
    let message = m.string("message", None);
    let contact = m.string("contact", Some(""));
    m.finish()?;
    let origin = header_str(&parts.headers, "origin");
    if !origin.is_empty() {
        let host = origin.split_once("://").map_or(origin.as_str(), |(_, h)| h);
        if host != header_str(&parts.headers, "host") {
            return Err(HttpError::new(403, "只接受本站页面的提交").into());
        }
    }
    let ip = client_ip(&parts, &state.env.forwarded_allow_ips);
    let now = state.now();
    let (token, message, contact) = admit_donation(&state, &ip, now, &token, &message, &contact)?;
    state
        .db
        .save_donation(&token, &message, &contact, now)
        .await?;
    // 日志只记动作，绝不记 Token 原文
    log_action(
        &state.db,
        &format!("网页:{ip}"),
        "收到捐赠 Key",
        &format!("{} 字符", len_chars(&token)),
        &format!("要求：{}", gate_core::pyc::take_chars(&message, 120)),
        true,
        now,
    )
    .await;
    Ok(no_store(ok_json(&json!({"ok": true}))))
}

// ------------------------------------------------------------------ 个人状态 / 重置券

/// 登录后的个人状态：不用粘贴 Key 就能看额度、排队、到期；并提供自己的 Key 供一键复制。
/// 内容由首页首屏用的同一份 [`super::public::me_payload`] 生成（旧版 `public_me`），不另写一份。
async fn public_me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let Some((discord_id, key)) = member_state(&state, &headers).await? else {
        return Ok(no_store(ok_json(&json!({"logged_in": false}))));
    };
    let me = super::public::me_payload(&state, &discord_id, key.as_ref()).await?;
    Ok(no_store(ok_json(&me)))
}

/// FastAPI 的路径参数 `coupon_id: int` 解析失败。
fn int_path_error(raw: &str) -> HttpError {
    HttpError {
        status: 422,
        detail: json!([{
            "type": "int_parsing", "loc": ["path", "coupon_id"],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": raw,
        }]),
    }
}

/// 成员在首页用掉一张重置券：重置今天的 V5 与 Anlas 额度（和站长后台「重置今日额度」一样）。
async fn use_my_coupon(
    State(state): State<AppState>,
    Path(coupon_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let coupon_id: i64 = coupon_id
        .trim()
        .parse()
        .map_err(|_| int_path_error(&coupon_id))?;
    let Some((discord_id, key)) = member_state(&state, &headers).await? else {
        return Err(HttpError::new(401, "请先登录").into());
    };
    let Some(key) = key.filter(|k| k.enabled()) else {
        return Err(HttpError::new(404, "你还没有可用的 Key").into());
    };
    let now = state.now();
    let Some(c) = state.db.use_coupon(coupon_id, key.id(), now).await? else {
        return Err(HttpError::new(409, "这张券已经用过或过期了").into());
    };
    state
        .db
        .reset_daily_image_quota(key.id(), &state.day())
        .await?;
    let note = c
        .get("note")
        .filter(|n| pyval::truthy(n))
        .map(gate_core::pyext::py_str);
    log_member(
        &state,
        &discord_id,
        "成员使用重置券",
        key.name(),
        &format!(
            "券 #{coupon_id}{}",
            note.map(|n| format!("（{n}）")).unwrap_or_default()
        ),
    )
    .await;
    Ok(no_store(ok_json(&json!({"ok": true}))))
}

// ------------------------------------------------------------------ 原图打包

#[derive(serde::Deserialize, Default)]
struct ExportQuery {
    format: Option<String>,
}

/// 成员自助打包自己的全部原图：format=epub（电子书）或 zip（原图）。单次一种。
///
/// 上限（线上 bug 修复：2.x 里这三个全局量被误删，接口必定 500）：单次最多打包最新的 200MB 原图；
/// 全站同一时刻只打包一份；每人每 60 秒一次（失败的请求不占用这个间隔）。
async fn my_export(
    State(state): State<AppState>,
    Query(q): Query<ExportQuery>,
    headers: HeaderMap,
) -> ApiResult {
    let format = q.format.unwrap_or_else(|| "zip".into());
    let Some((discord_id, key)) = member_state(&state, &headers).await? else {
        return Err(HttpError::new(401, "请先用 Discord 登录").into());
    };
    if format != "zip" && format != "epub" {
        return Err(HttpError::new(400, "format 只能是 zip 或 epub").into());
    }
    let now = state.now();
    {
        let mut at = state.member_web.export_at.lock().expect("export");
        at.retain(|_, t| now - *t < EXPORT_PER_MEMBER_SECS);
        if at
            .get(&discord_id)
            .is_some_and(|t| now - *t < EXPORT_PER_MEMBER_SECS)
        {
            return Err(HttpError::new(429, "打包有点重，请 1 分钟后再试").into());
        }
    }
    let Some(key) = key.filter(|k| k.enabled()) else {
        return Err(HttpError::new(404, "你还没有可用的 Key").into());
    };
    // 全站同时只打包一份
    let Ok(_guard) = state.member_web.export_lock.try_lock() else {
        return Err(HttpError::new(429, "有人正在打包，请过半分钟再试").into());
    };
    let days = audit_image_days(&state.db).await?;
    if days <= 0 {
        return Err(HttpError::new(403, "原图下载暂未开放").into());
    }
    let rows = state
        .db
        .audit_images_for(
            key.id(),
            state.member_web.export_max_bytes.load(Ordering::Relaxed),
            now - (days * 86400) as f64,
        )
        .await?;
    if rows.is_empty() {
        return Err(HttpError::new(
            404,
            "最近 2 天没有可打包的原图（打包只用来找回最近误删的图）",
        )
        .into());
    }
    state
        .member_web
        .export_at
        .lock()
        .expect("export")
        .insert(discord_id, now);
    let who = if key.name().is_empty() {
        "我的作品".to_string()
    } else {
        key.name().to_string()
    };
    let count = rows.len();
    let clock = state.clock.clone();
    let as_epub = format == "epub";
    let blob = tokio::task::spawn_blocking(move || {
        if as_epub {
            crate::exporter::build_epub(&rows, &who, &clock, now)
        } else {
            crate::exporter::build_zip(&rows, &who, &clock, now)
        }
    })
    .await
    .map_err(internal)?;
    let (media, ext) = if as_epub {
        ("application/epub+zip", "epub")
    } else {
        ("application/zip", "zip")
    };
    let mut resp = Response::new(Body::from(blob));
    let h = resp.headers_mut();
    h.insert("content-type", HeaderValue::from_static(media));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"owl-{count}.{ext}\"")) {
        h.insert("content-disposition", v);
    }
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(resp)
}
