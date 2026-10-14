//! 公开页面、实时看板、状态与客户端兼容视图（旧版 `app/routers/public_routes.py` + `app/live.py` 的路由部分）。
//!
//! - `/terms` `/privacy`：服务条款 / 隐私政策（Discord 开发者门户「APP 验证」要求配置这两个链接）；
//! - `POST /member/survey`：成员问卷；
//! - `/announcement`：站长公告（沙箱化嵌入首页）；
//! - `/public/live`（JSON）、`/public/live/stream`（SSE，3.0 新增）、`/v1/live/me`、`/public/status`、`/queue-status`；
//! - `POST /public/client-error`：网页脚本报错上报；
//! - `/user/*`、`/ai/user/*`：给原生 NAI 客户端的虚拟订阅 / 数据 / 优先级 / 密钥库 / 账户信息；
//! - `GET /admin/api/events`（3.0 新增）：后台 SSE（实时快照 + 角标数字）；
//! - 首页首屏数据（[`landing_boot`]，由 pages.rs 的 `/` 使用）。
//!
//! `/healthz`、`/`、`/survey`、`/admin*`、`/_owl/*`、图标在 pages.rs。

use crate::auth::{authenticate, client_ip, ctx_of, header_str, site_flag_f64};
use crate::clients::client_name;
use crate::error::{ApiError, ApiResult, HttpError, InternalError, json_response, ok_json};
use crate::key::Key;
use crate::live::{self, Snapshot, StreamKind, StreamSlot, wire_text};
use crate::session::{self, ADMIN_COOKIE, AdminSession};
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use gate_core::pyc::{strip, take_chars};
use gate_core::pyloads::{self, Options};
use gate_core::pyobj::{Obj, PyDict};
use gate_core::site_flags::GLOBAL_DAILY_V5;
use gate_db::repo::feedback::Survey;
use serde_json::{Map, Value, json};
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

pub fn router() -> Router<AppState> {
    let mut r = Router::new()
        .route("/terms", get(terms))
        .route("/privacy", get(privacy))
        .route("/member/survey", post(member_survey))
        .route("/announcement", get(announcement))
        .route("/public/live", get(public_live))
        .route("/public/live/stream", get(public_live_stream))
        .route("/public/client-error", post(client_error))
        .route("/v1/live/me", get(live_me))
        .route("/public/status", get(public_status))
        .route("/queue-status", get(queue_status))
        .route("/admin/api/events", get(admin_events));
    for p in ["/user/subscription", "/ai/user/subscription"] {
        r = r.route(p, get(user_subscription));
    }
    for p in ["/user/data", "/ai/user/data"] {
        r = r.route(p, get(user_data));
    }
    for p in ["/user/priority", "/ai/user/priority"] {
        r = r.route(p, get(user_priority));
    }
    for p in ["/user/keystore", "/ai/user/keystore"] {
        r = r.route(p, get(user_keystore));
    }
    for p in ["/user/information", "/ai/user/information"] {
        r = r.route(p, get(user_information));
    }
    r
}

// ------------------------------------------------------------------ 小件

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// 已经序列化好的 JSON 文本 → 响应（与 Starlette `JSONResponse` 一样的 content-type）。
fn json_text(text: String) -> Response {
    let mut r = Response::new(Body::from(text));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn html_response(body: String, headers: &[(&'static str, &'static str)]) -> Response {
    let mut r = Response::new(Body::from(body));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    for (k, v) in headers {
        h.insert(*k, HeaderValue::from_static(v));
    }
    r
}

/// 请求体的前 `n` 个字节（旧版 `(await request.body())[:4096]`；多余的不读）。
async fn head_bytes(body: Body, n: usize) -> Vec<u8> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < n {
        match stream.next().await {
            Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(n);
    buf
}

/// `json.loads(raw)`；读不懂返回 None。
fn loads(raw: &[u8]) -> Option<Obj> {
    pyloads::loads_bytes(raw, Options::LOADS)
        .ok()
        .map(|l| l.value)
}

/// `str(d.get(name) or "")`
fn text_of(d: &PyDict, name: &str) -> String {
    d.get(name)
        .filter(|o| o.truthy())
        .map(Obj::py_str)
        .unwrap_or_default()
}

// ------------------------------------------------------------------ 服务条款 / 隐私政策

const DOC_HEADERS: [(&str, &str); 4] = [
    (
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; sandbox; frame-ancestors 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "public, max-age=3600"),
];

/// 两份文档的「最近更新」日期。
const DOC_UPDATED: &str = "2026-10-15";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 旧版 `_doc_page`：自包含的静态文档页（内联样式，没有脚本）。`host` 是本站域名，没配 `SITE_URL` 时不写。
fn doc_page(
    title: &str,
    updated: &str,
    host: Option<&str>,
    sections: &[(&str, Vec<String>)],
) -> String {
    let rows: String = sections
        .iter()
        .map(|(h, ps)| {
            format!(
                "<h2>{}</h2>{}",
                esc(h),
                ps.iter()
                    .map(|p| format!("<p>{}</p>", esc(p)))
                    .collect::<String>()
            )
        })
        .collect();
    let site = match host {
        Some(h) => format!("猫头鹰公益站（{h}）"),
        None => "猫头鹰公益站".to_string(),
    };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{t} · 猫头鹰公益站</title><style>\
body{{margin:0;background:#f6f5f1;color:#22211d;font:15px/1.75 -apple-system,\"PingFang SC\",\
\"Microsoft YaHei\",sans-serif;padding:2.5rem 1rem}}\
main{{max-width:46rem;margin:0 auto;background:#fff;border:1px solid #dedcd1;border-radius:12px;\
padding:2rem 2.2rem}}\
h1{{font-size:1.4rem;margin:.2rem 0}}h2{{font-size:1.05rem;margin:1.6rem 0 .4rem}}\
.upd{{color:#87867f;font-size:.85rem;margin:0 0 1rem}}p{{margin:.4rem 0}}\
</style></head><body><main><h1>{t}</h1><p class=\"upd\">{site} \
· 最近更新：{u}</p>{rows}</main></body></html>",
        t = esc(title),
        u = esc(updated),
    )
}

/// `SITE_URL` 里的域名（`https://gate.example.top/` → `gate.example.top`）；没配为 None。
fn site_host(state: &AppState) -> Option<String> {
    let url = state.env.site_url.trim();
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| (*x).to_string()).collect()
}

/// 服务条款（Discord APP 验证要求）。内容与首页「使用须知」同一口径。
async fn terms(State(state): State<AppState>) -> Response {
    let host = site_host(&state);
    let who = match &host {
        Some(h) => format!("「猫头鹰公益站」（{h}，下称「本站」）"),
        None => "「猫头鹰公益站」（下称「本站」）".to_string(),
    };
    let sections: Vec<(&str, Vec<String>)> = vec![
        (
            "服务性质",
            vec![
                format!(
                    "{who}是由个人运营的非商业公益服务，为 Discord 社区「猫头鹰公益站」的成员提供 NovelAI 生图请求的中转（代理）。"
                ),
                "本站不保证服务可用性或出图质量，可能因上游（NovelAI）状况、账号额度、维护或风控随时暂停、限流、调整规则或终止服务，且无需事先通知。".into(),
            ],
        ),
        (
            "资格与 Key",
            s(&[
                "服务面向本站 Discord 社区成员。成员通过社区内 /register 命令领取专属 API Key，一人一把。",
                // 3.0：登录方式只有 Discord
                "网页登录只支持 Discord 账号（OAuth），不提供其他登录方式。",
                // 3.0：去掉「系统记录」的说法
                "Key 仅限本人使用。严禁转售、转借、多人共用或用于商业用途。多人共用或转售会被回收 Key。",
            ]),
        ),
        (
            "使用规范",
            s(&[
                "严禁生成违反法律法规或极端有害的内容（包括但不限于违法、危害未成年人、仇恨与骚扰内容），违规账号将被永久封禁且不再恢复。",
                "请遵守本站公布的额度与并发规则（如一次一张、勿狂重试）。退出 Discord 服务器或 Key 长期闲置后，Key 会被自动回收。",
            ]),
        ),
        (
            "额度与公平",
            s(&["本站为免费公益服务，出图额度有限。站方可根据上游状况随时调整每人额度、全站上限、限流与排队策略，并在状态页与社区公告中说明。"]),
        ),
        (
            "责任限制",
            s(&[
                "在法律允许的最大范围内，本站按「现状」提供服务，不对任何直接或间接损失承担责任。",
                "使用本站即表示你已阅读并同意本条款。条款更新后在本页公布即生效，重大变更会在社区公告。",
            ]),
        ),
        ("联系", s(&["如有疑问，请在 Discord 社区「猫头鹰公益站」内联系站长。"])),
    ];
    html_response(
        doc_page("服务条款", DOC_UPDATED, host.as_deref(), &sections),
        &DOC_HEADERS,
    )
}

/// 隐私政策（Discord APP 验证要求）。与实际数据流一致：提示词 / 用量 / 图片仅站长可见。
async fn privacy(State(state): State<AppState>) -> Response {
    let host = site_host(&state);
    let sections: Vec<(&str, Vec<String>)> = vec![
        (
            "我们收集什么",
            s(&[
                "Discord 账号信息：成员领取或登录时收集 Discord 用户 ID、用户名、显示名与头像，用于识别身份与展示。",
                "使用记录：Key 的请求时间、模型、尺寸、等待与拒绝原因等用量日志；你输入的生图提示词；生成的图片。",
                "如你使用「捐赠 Key」功能：你提交的 NovelAI Token、留言与联系方式。",
            ]),
        ),
        (
            "用途",
            s(&[
                "发放与管理 Key、展示个人额度；防滥用与防多人共用（行为统计）；公平调度与容量规划；社区公告与私信通知。",
            ]),
        ),
        (
            "保存期限",
            s(&[
                "用量日志与提示词长期保存（用于审计与防滥用）；生成的图片自动轮删（服务器空间满时删除最旧的，默认保留数天）；Discord 资料与领取记录在 Key 存续期间保存，Key 回收后随之清理或匿名化。",
            ]),
        ),
        (
            "谁能看到",
            s(&["仅站长本人可查看上述数据。本站不会出售、出租或与第三方共享你的个人数据。"]),
        ),
        (
            "Cookie 与第三方",
            s(&[
                "本站仅在浏览器写入一个登录态会话 Cookie（HttpOnly，约 30 天），不用于跨站跟踪。",
                // 3.0：登录方式只有 Discord
                "网页登录只有 Discord 一种方式，本站不收集你的 Discord 密码。",
                "网页「用 Discord 登录」通过 Discord OAuth 完成，只申请 identify（身份识别）权限，不读取你的消息、好友或服务器列表；生图请求仅转发给 NovelAI 官方接口。",
            ]),
        ),
        (
            "你的选择",
            s(&[
                "退出 Discord 服务器即可使 Key 被回收；如需删除登记资料或用量数据，请在社区内联系站长。",
            ]),
        ),
        (
            "联系",
            s(&["隐私相关问题请在 Discord 社区「猫头鹰公益站」内联系站长。"]),
        ),
    ];
    html_response(
        doc_page("隐私政策", DOC_UPDATED, host.as_deref(), &sections),
        &DOC_HEADERS,
    )
}

// ------------------------------------------------------------------ 成员问卷

/// 问卷身份：先认成员的 Discord 会话（3.0 只留 Discord 登录），没有会话再认 `Authorization: Bearer <Key>`。
/// 都是 passive：不算活跃、不进防分享统计。失败的格式是 `{"detail": ...}`（旧版 `HTTPException(exc.status, exc.message)`）。
async fn survey_key(state: &AppState, parts: &axum::http::request::Parts) -> Result<Key, ApiError> {
    if let Some((_, Some(key))) = session::member_state(state, &parts.headers).await? {
        ctx_of(parts).set_key(key.id(), key.name());
        if !key.enabled() {
            return Err(HttpError::new(403, "该 Key 已被禁用").into());
        }
        if key.expired(state.now()) {
            return Err(HttpError::new(403, "该 Key 已过期").into());
        }
        return Ok(key);
    }
    match authenticate(state, parts, true).await {
        Err(ApiError::Gate(e)) => Err(HttpError::new(e.status, e.message).into()),
        other => other,
    }
}

/// 问卷提交：一人一份，重复提交覆盖。
async fn member_survey(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let key = survey_key(&state, &parts).await?;
    let mut raw = head_bytes(body, 4096).await;
    if raw.is_empty() {
        raw = b"{}".to_vec();
    }
    let Some(data) = loads(&raw) else {
        return Err(HttpError::new(400, "请求体不是合法 JSON").into());
    };
    let Obj::Dict(d) = &data else {
        return Err(HttpError::new(400, "请求体格式无效").into());
    };
    let understand = strip(&text_of(d, "understand")).to_string();
    let donate = strip(&text_of(d, "donate")).to_string();
    if understand.is_empty() || donate.is_empty() {
        return Err(HttpError::new(422, "还有必选题没填：限额理解、捐赠意愿").into());
    }
    let improve: Vec<String> = match d.get("improve") {
        Some(o) if o.truthy() => match o {
            Obj::List(items) => items.iter().map(Obj::py_str).collect(),
            _ => return Err(HttpError::new(422, "多选项格式无效").into()),
        },
        _ => vec![],
    };
    let survey = Survey {
        client: strip(&text_of(d, "client")).to_string(),
        understand,
        improve: improve
            .iter()
            .filter(|i| !strip(i).is_empty())
            .map(|i| take_chars(i, 40))
            .collect::<Vec<_>>()
            .join("、"),
        improve_other: strip(&text_of(d, "improve_other")).to_string(),
        donate,
    };
    state.db.save_survey(key.id(), &survey, state.now()).await?;
    Ok(ok_json(&json!({"ok": true})))
}

// ------------------------------------------------------------------ 公告

/// 公告是管理员存储的 HTML：沙箱化后脚本无法执行，也读不到与 /admin 同源的数据。
const ANNOUNCEMENT_HEADERS: [(&str, &str); 2] = [
    (
        "content-security-policy",
        "sandbox allow-popups allow-popups-to-escape-sandbox",
    ),
    ("x-content-type-options", "nosniff"),
];

async fn announcement(State(state): State<AppState>) -> ApiResult {
    match live::announcement_text(&state).await {
        Some(html) => Ok(html_response(html, &ANNOUNCEMENT_HEADERS)),
        None => Err(HttpError::new(404, "没有公告").into()),
    }
}

// ------------------------------------------------------------------ 实时看板

/// 登录的成员：这把 Key 的排队位置（没登录 / 没 Key / Key 被停用为 None）。
async fn viewer_key_id(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    match session::member_state(state, headers).await {
        Ok(Some((_, Some(key)))) if key.enabled() => Some(key.id()),
        _ => None,
    }
}

/// 首页实时架构图：只含汇总数字 + meta；登录的成员附上「我的排队」，看板第一格直接显示排第几。
async fn public_live(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    let snap = state.live.current(&state).await?;
    let me = viewer_key_id(&state, &parts.headers)
        .await
        .map(|id| live::mine(&state, id));
    Ok(no_store(json_text(snap.render(me.as_ref()))))
}

fn sse_response<S>(stream: S, keepalive: Duration) -> Response
where
    S: futures::Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(keepalive).text("keep-alive"))
        .into_response();
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // 反向代理（nginx 系）不要缓冲事件流；Caddy 本来就是即时转发
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

fn too_many_streams() -> Response {
    let mut r = HttpError::new(503, "实时连接太多，请稍后再试").into_response();
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
    r
}

/// 一条公开 SSE 连接的状态。
struct LiveConn {
    state: AppState,
    rx: watch::Receiver<Option<Arc<Snapshot>>>,
    /// 只用来每 30 秒重验一次成员会话（Key 被停用 / 换新 Key 后「我的排队」要消失）。
    headers: HeaderMap,
    key_id: Option<i64>,
    checked: Instant,
    last: Option<u64>,
    waited: bool,
    _slot: StreamSlot,
}

fn combine(sig: u64, extra: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sig.hash(&mut h);
    extra.hash(&mut h);
    h.finish()
}

/// 下一条要推的事件：快照每秒更新一次，但只有内容变了（含我的排队）才推。
async fn next_live(mut c: LiveConn) -> Option<(Result<Event, Infallible>, LiveConn)> {
    loop {
        if c.waited {
            tokio::select! {
                _ = c.state.shutdown.cancelled() => return None,
                r = c.rx.changed() => if r.is_err() { return None },
            }
        }
        c.waited = true;
        let Some(snap) = c.rx.borrow_and_update().clone() else {
            continue;
        };
        if c.checked.elapsed() >= Duration::from_secs(30) {
            c.key_id = viewer_key_id(&c.state, &c.headers).await;
            c.checked = Instant::now();
        }
        let me = c.key_id.map(|id| wire_text(&live::mine(&c.state, id)));
        let sig = match &me {
            Some(m) => combine(snap.sig, m),
            None => snap.sig,
        };
        if c.last == Some(sig) {
            continue;
        }
        c.last = Some(sig);
        let text = snap.render_raw(me.as_deref().unwrap_or("null"));
        return Some((Ok(Event::default().data(text)), c));
    }
}

/// 同样的看板数据，SSE 版（3.0 新增）：连上立刻推一条，之后每次内容变化推一条，15 秒无事件发保活注释。
async fn public_live_stream(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let Some(slot) = state.live.open_stream(&state, StreamKind::Public) else {
        return too_many_streams();
    };
    let rx = match state.live.subscribe(&state).await {
        Ok(rx) => rx,
        Err(e) => return e.into_response(),
    };
    let key_id = viewer_key_id(&state, &parts.headers).await;
    let keepalive = state.live.keepalive();
    let conn = LiveConn {
        state: state.clone(),
        rx,
        headers: parts.headers,
        key_id,
        checked: Instant::now(),
        last: None,
        waited: false,
        _slot: slot,
    };
    sse_response(futures::stream::unfold(conn, next_live), keepalive)
}

/// 需要 Key：这把 Key 的图现在排第几 / 是否在生成。不写用量日志。
async fn live_me(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    match authenticate(&state, &parts, true).await {
        Ok(key) => Ok(no_store(ok_json(&live::mine(&state, key.id())))),
        Err(ApiError::Gate(e)) => Ok(no_store(json_response(
            e.status,
            &json!({"mine": null, "error": e.message}),
        ))),
        Err(other) => Err(other),
    }
}

/// 落地页使用的公开状态：不含任何密钥、成员信息或计数细节。缓存 8 秒。
async fn public_status(State(state): State<AppState>) -> ApiResult {
    let body = state.live.status(&state).await?;
    Ok(no_store(ok_json(&body)))
}

async fn queue_status(State(state): State<AppState>) -> ApiResult {
    let now = state.now();
    let mono = state.nai.mono();
    let pool = state.nai.tokens().await;
    let usable: Vec<_> = pool.iter().filter(|t| t.usable(now)).collect();
    let next = usable
        .iter()
        .map(|t| (t.fields().image_next_at - mono).max(0.0))
        .reduce(f64::min)
        .unwrap_or(0.0);
    let lim = state.limits();
    let pacing = state
        .pacing
        .waiting
        .load(std::sync::atomic::Ordering::Relaxed);
    let (active, waiting) = (
        state
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed),
        state
            .global_waiting
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    Ok(ok_json(&json!({
        "global": {
            "active": active,
            "waiting": waiting + pacing,
            "concurrency": usable.iter().map(|t| t.image_slots.limit() as i64).sum::<i64>(),
        },
        "image_next_slot_in": gate_core::pyc::round(next, 1),
        "image_cooldown_remaining": state.cooldown.remaining(now),
        "queue_timeout": lim.queue_timeout as i64,
        "image_min_interval": lim.image_min_interval,
    })))
}

// ------------------------------------------------------------------ 网页脚本报错上报

/// `urllib.parse.urlsplit(url).netloc`
fn netloc(url: &str) -> String {
    // urlsplit 先去掉首部的控制字符 / 空格，并删掉 \t \r \n
    let url: String = url
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = url.as_str();
    if let Some(i) = rest.find(':')
        && i > 0
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        rest = &rest[i + 1..];
    }
    match rest.strip_prefix("//") {
        Some(r) => r.split(['/', '?', '#']).next().unwrap_or("").to_string(),
        None => String::new(),
    }
}

/// `str.splitlines()`（Python 的行分隔符比 `\n` 多得多）。
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|(_, n)| *n == '\n') {
                it.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// 手机自带浏览器（荣耀 / 华为 / 小米等）、翻译和广告插件往页面注入的脚本出的错：我们修不了，不进 Bug 追踪。
/// 特征：堆栈里有调用帧，但没有一帧来自本站文件（全是 <anonymous> / 扩展协议）；或者是没有来源的「Script error.」。
fn injected_script_error(msg: &str, stack: &str, host: &str) -> bool {
    if strip(msg) == "Script error." {
        return true;
    }
    let frames: Vec<&str> = py_splitlines(stack)
        .into_iter()
        .map(strip)
        .filter(|l| l.starts_with("at ") || l.contains('@'))
        .collect();
    if frames.is_empty() {
        return false;
    }
    // 旧版是 /static/；3.0 的前端文件在 /_owl/
    let ours: Vec<&str> = [
        "/static/",
        "/_owl/",
        "/admin",
        host,
        "127.0.0.1",
        "localhost",
    ]
    .into_iter()
    .filter(|x| !x.is_empty())
    .collect();
    !frames.iter().any(|f| ours.iter().any(|o| f.contains(o)))
}

/// 首页 / 后台网页的脚本报错上报（只收本站页面的错误；每 IP 每 10 分钟 10 条，全站每 10 分钟 100 条）。
async fn client_error(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let h = &parts.headers;
    let origin = header_str(h, "origin");
    let from = if origin.is_empty() {
        header_str(h, "referer")
    } else {
        origin
    };
    let host = header_str(h, "host").to_lowercase();
    if !from.is_empty() {
        let theirs = netloc(&from).to_lowercase();
        if !theirs.is_empty() && !host.is_empty() && theirs != host {
            return no_content();
        }
    }
    let ip = client_ip(&parts, &state.env.forwarded_allow_ips);
    if !state.live.client_error_slot(&ip, state.now()) {
        return no_content();
    }
    let raw = head_bytes(body, 4096).await;
    let raw = if raw.is_empty() { b"{}".to_vec() } else { raw };
    let Some(data) = loads(&raw) else {
        return no_content();
    };
    let Obj::Dict(d) = &data else {
        return no_content();
    };
    let page = take_chars(&text_of(d, "page"), 20).to_string();
    let page = if page == "landing" || page == "admin" {
        page
    } else {
        "other".to_string()
    };
    let msg = client_name(&text_of(d, "msg"));
    let msg = take_chars(&msg, 200);
    let msg = if msg.is_empty() { "unknown" } else { msg };
    let place = client_name(&format!("{}:{}", text_of(d, "src"), text_of(d, "line")));
    let stack = take_chars(&text_of(d, "stack"), 1500).to_string();
    if injected_script_error(msg, &stack, &host) {
        return no_content();
    }
    let ua = client_name(&header_str(h, "user-agent"));
    let source = format!("web:{page}");
    let detail = format!("{place}\n{ua}\n{stack}");
    state.bugs.capture(crate::bugs::Report {
        source: &source,
        title: msg,
        detail: &detail,
        path: &page,
        rid: &ctx_of(&parts).rid(),
        level: "warn",
        ..Default::default()
    });
    no_content()
}

// ------------------------------------------------------------------ 客户端兼容视图（/user/*）

/// 虚拟订阅视图：数字由 gate_core 算（纯函数，golden 对拍过），这里只负责把库里的数查好。
async fn subscription(state: &AppState, key: &Key) -> Result<Value, InternalError> {
    let (day, month) = (state.day(), state.month());
    let data = gate_core::client_views::SubscriptionData {
        month_anlas_used: state.db.month_anlas(key.id(), &month).await?,
        daily_v5_used: state.db.get_counter(key.id(), &day).await?.v5,
        global_v5_limit: site_flag_f64(state, &GLOBAL_DAILY_V5).await? as i64,
        global_v5_total: state.db.day_v5_total(&day).await?,
    };
    gate_core::client_views::subscription_payload(&key.0, state.now(), &data)
        .map_err(|e| InternalError(e.to_string()))
}

/// 官方 /user/priority 的形状（2026-10-13 实测 image.novelai.net 返回 200）：
/// {maxPriorityActions, nextRefillAt, taskPriority}。本站没有「买优先队列」这回事，
/// 如实回 0 次优先操作、下个整点刷新，别编造余额。
fn virtual_priority(now: f64) -> Value {
    let now = now as i64;
    json!({"maxPriorityActions": 0, "nextRefillAt": now + (3600 - now % 3600), "taskPriority": 10})
}

/// 给原生 NAI 客户端的虚拟订阅视图，不暴露真实上游账户余额。
async fn user_subscription(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    let key = authenticate(&state, &parts, false).await?;
    Ok(ok_json(&subscription(&state, &key).await?))
}

async fn user_data(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    let key = authenticate(&state, &parts, false).await?;
    let sub = subscription(&state, &key).await?;
    let public_sub: Map<String, Value> = sub
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| k.as_str() != "naiGate")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let steps = sub["trainingStepsLeft"].clone();
    Ok(ok_json(&json!({
        "subscription": public_sub,
        "priority": virtual_priority(state.now()),
        "trainingStepsLeft": steps,
        "anlas": steps["fixedTrainingStepsLeft"],
    })))
}

/// 原生客户端（NAI-Workbench 等）会探这个端点；404 会被当成「渠道不支持」。官方是 200，我们也回同样的形状。
async fn user_priority(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    authenticate(&state, &parts, false).await?;
    Ok(ok_json(&virtual_priority(state.now())))
}

/// 官方这个端点只允许浏览器会话（2026-10-13 实测：api.novelai.net → 403「Persistent access token is not
/// allowed to access this endpoint.」）。照官方的说法回 403，客户端就知道这是「令牌无权访问」而不是「网关坏了」。
async fn user_keystore(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    authenticate(&state, &parts, false).await?;
    Err(crate::error::GateError::new(
        403,
        "该项目仅限浏览器会话访问（NovelAI 官方对持久令牌同样返回 403）",
    )
    .into())
}

async fn user_information(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _) = req.into_parts();
    let key = authenticate(&state, &parts, false).await?;
    let now = state.now();
    // int(key["expires_at"] or time.time() + 3650 * 86400)
    let expires = key
        .opt_float("expires_at")
        .filter(|e| *e != 0.0)
        .map_or((now + 3650.0 * 86400.0) as i64, |e| e as i64);
    Ok(ok_json(&json!({
        "tier": 3, "active": true, "username": key.get("name"), "expiresAt": expires,
    })))
}

// ------------------------------------------------------------------ 后台 SSE

/// 后台 SSE 的最短推送间隔与保底心跳。页面把任何一条消息都当作「该刷新了」（admin/Admin.tsx 的 `onmessage = bump`），
/// 所以：真有变化才推（且至少隔 2 秒，免得队列忙时每秒刷新所有分区）；没变化也每 15 秒推一条，
/// 等价于旧版没有 SSE 时的 15 秒定时刷新（日志 / Bug / 成员列表这些不在快照里的数据靠它更新）。
const ADMIN_MIN_GAP: Duration = Duration::from_secs(2);
const ADMIN_HEARTBEAT: Duration = Duration::from_secs(15);
const BADGE_TTL: Duration = Duration::from_secs(5);

struct AdminConn {
    state: AppState,
    rx: watch::Receiver<Option<Arc<Snapshot>>>,
    headers: HeaderMap,
    badges: Option<(Instant, String)>,
    last_sig: Option<u64>,
    last_sent: Option<Instant>,
    _slot: StreamSlot,
}

/// 侧栏角标用的数字（与 BadgePoller 取的 `open` 同源）：未处理 Bug、未处理捐赠。
async fn badges(c: &mut AdminConn) -> String {
    if let Some((at, text)) = &c.badges
        && at.elapsed() < BADGE_TTL
    {
        return text.clone();
    }
    let bugs = c.state.db.open_error_count().await.unwrap_or(0);
    let donations = c.state.db.count_unhandled_donations().await.unwrap_or(0);
    let text = format!("{{\"bugs\":{bugs},\"donations\":{donations}}}");
    c.badges = Some((Instant::now(), text.clone()));
    text
}

async fn next_admin(mut c: AdminConn) -> Option<(Result<Event, Infallible>, AdminConn)> {
    loop {
        if let Some(sent) = c.last_sent {
            let due = sent + ADMIN_HEARTBEAT;
            tokio::select! {
                _ = c.state.shutdown.cancelled() => return None,
                r = c.rx.changed() => if r.is_err() { return None },
                _ = tokio::time::sleep_until(due) => {}
            }
        }
        // 改了密码 / 会话过期：结束这条流（EventSource 重连会拿到 401）
        let raw = session::cookie(&c.headers, ADMIN_COOKIE).unwrap_or_default();
        if !session::check_admin_cookie(&c.state, &raw) {
            return None;
        }
        let Some(snap) = c.rx.borrow_and_update().clone() else {
            if c.last_sent.is_none() {
                c.last_sent = Some(Instant::now());
            }
            continue;
        };
        let badge_text = badges(&mut c).await;
        let sig = combine(snap.admin_sig, &badge_text);
        let first = c.last_sent.is_none();
        let since = c.last_sent.map(|t| t.elapsed());
        let changed = c.last_sig != Some(sig);
        let heartbeat = since.is_some_and(|s| s >= ADMIN_HEARTBEAT);
        let gap_ok = since.is_none_or(|s| s >= ADMIN_MIN_GAP);
        if !(first || heartbeat || (changed && gap_ok)) {
            continue;
        }
        c.last_sig = Some(sig);
        c.last_sent = Some(Instant::now());
        let text = format!("{{\"live\":{},\"badges\":{}}}", snap.json(), badge_text);
        return Some((Ok(Event::default().data(text)), c));
    }
}

/// 后台实时事件：快照（与 /public/live 同一份，不含「我的排队」）+ 角标数字。需要后台会话。
async fn admin_events(State(state): State<AppState>, _s: AdminSession, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let Some(slot) = state.live.open_stream(&state, StreamKind::Admin) else {
        return too_many_streams();
    };
    let rx = match state.live.subscribe(&state).await {
        Ok(rx) => rx,
        Err(e) => return e.into_response(),
    };
    let keepalive = state.live.keepalive();
    let conn = AdminConn {
        state: state.clone(),
        rx,
        headers: parts.headers,
        badges: None,
        last_sig: None,
        last_sent: None,
        _slot: slot,
    };
    sse_response(futures::stream::unfold(conn, next_admin), keepalive)
}

// ------------------------------------------------------------------ 首页首屏数据

/// 登录成员的个人状态（`/public/me` 的内容，旧版 `registration_routes.public_me`）。
async fn me_payload(
    state: &AppState,
    discord_id: &str,
    key: Option<&Key>,
) -> Result<Value, InternalError> {
    let discord = if state.env.registration.is_some() {
        match state.db.discord_profile_row(discord_id).await? {
            None => json!({"id": discord_id}),
            Some((username, display_name, avatar)) => {
                json!({"id": discord_id, "username": username, "display_name": display_name, "avatar": avatar})
            }
        }
    } else {
        Value::Null
    };
    let mut out = Map::new();
    out.insert("logged_in".into(), json!(true));
    out.insert("discord".into(), discord);
    let Some(key) = key.filter(|k| k.enabled()) else {
        out.insert("has_key".into(), json!(false));
        return Ok(Value::Object(out));
    };
    let c = state.db.get_counter(key.id(), &state.day()).await?;
    let queue = state
        .guard
        .with(|g| g.queue_view(Some(key.id())))
        .get("mine")
        .cloned()
        .unwrap_or(json!([]));
    let now = state.now();
    let img_days = crate::audit::audit_image_days(&state.db).await?;
    out.insert("has_key".into(), json!(true));
    out.insert("key".into(), key.get("token").clone());
    out.insert("name".into(), key.get("name").clone());
    out.insert("expires_at".into(), key.get("expires_at").clone());
    out.insert("image_scope".into(), key.get("image_model_scope").clone());
    out.insert(
        "today".into(),
        json!({
            "images": c.images, "v5": c.v5, "legacy": c.legacy_free_images,
            "daily_images": key.get("daily_images"), "daily_v5": key.get("daily_v5"),
            "anlas": gate_core::pyc::round0(c.anlas),
            "daily_anlas": live::member_anlas_cap(state, key).await?,
            "reset_hour": state.guard.with(|g| g.reset_hour),
        }),
    );
    out.insert("queue".into(), queue);
    out.insert(
        "coupons".into(),
        json!(state.db.list_coupons(key.id(), true, now).await?),
    );
    out.insert(
        "images_stored".into(),
        json!(
            state
                .db
                .audit_image_count(key.id(), now - img_days as f64 * 86400.0)
                .await?
        ),
    );
    out.insert("image_retention_days".into(), json!(img_days));
    out.insert(
        "timing".into(),
        match state.db.user_image_timing(key.id(), now).await {
            Ok(t) => serde_json::to_value(t).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        },
    );
    Ok(Value::Object(out))
}

/// 首页 HTML 里的 `<script type="application/json" id="owl-boot">`：`{me, live, status}`，
/// 形状与 `/public/me`、`/public/live`、`/public/status` 的响应一样（web/src/pages/landing/store.ts 的 `Boot`）。
/// 页面拿到就不用再等三个请求；任何一块取不到就缺省，页面自己走接口。
pub async fn landing_boot(state: &AppState, headers: &HeaderMap) -> Option<Value> {
    let mut boot = Map::new();
    let session = session::member_state(state, headers).await.ok().flatten();
    let mut key_id = None;
    match &session {
        None => {
            boot.insert("me".into(), json!({"logged_in": false}));
        }
        Some((discord_id, key)) => {
            key_id = key.as_ref().filter(|k| k.enabled()).map(Key::id);
            match me_payload(state, discord_id, key.as_ref()).await {
                Ok(me) => {
                    boot.insert("me".into(), me);
                }
                Err(e) => tracing::warn!(target: "boot", "首屏 me 取数失败：{}", e.0),
            }
        }
    }
    match state.live.current(state).await {
        Ok(snap) => {
            boot.insert(
                "live".into(),
                snap.value(key_id.map(|id| live::mine(state, id))),
            );
        }
        Err(e) => tracing::warn!(target: "boot", "首屏 live 取数失败：{}", e.0),
    }
    match state.live.status(state).await {
        Ok(status) => {
            boot.insert("status".into(), (*status).clone());
        }
        Err(e) => tracing::warn!(target: "boot", "首屏 status 取数失败：{}", e.0),
    }
    (!boot.is_empty()).then_some(Value::Object(boot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_page_matches_python() {
        // 期望值由旧版 `_doc_page("测试 <标题>", "2026-01-02", [...])` 生成（域名换成 gate.example.top）
        let sections: Vec<(&str, Vec<String>)> = vec![
            (
                "一节 & 二",
                vec!["第一段 \"引号\"".into(), "第二段 <b>粗体</b>".into()],
            ),
            ("空节", vec![]),
        ];
        let got = doc_page(
            "测试 <标题>",
            "2026-01-02",
            Some("gate.example.top"),
            &sections,
        );
        let want = "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>测试 &lt;标题&gt; · 猫头鹰公益站</title><style>body{margin:0;background:#f6f5f1;color:#22211d;font:15px/1.75 -apple-system,\"PingFang SC\",\"Microsoft YaHei\",sans-serif;padding:2.5rem 1rem}main{max-width:46rem;margin:0 auto;background:#fff;border:1px solid #dedcd1;border-radius:12px;padding:2rem 2.2rem}h1{font-size:1.4rem;margin:.2rem 0}h2{font-size:1.05rem;margin:1.6rem 0 .4rem}.upd{color:#87867f;font-size:.85rem;margin:0 0 1rem}p{margin:.4rem 0}</style></head><body><main><h1>测试 &lt;标题&gt;</h1><p class=\"upd\">猫头鹰公益站（gate.example.top） · 最近更新：2026-01-02</p><h2>一节 &amp; 二</h2><p>第一段 &quot;引号&quot;</p><p>第二段 &lt;b&gt;粗体&lt;/b&gt;</p><h2>空节</h2></main></body></html>";
        assert_eq!(got, want);
    }

    #[test]
    fn netloc_like_urlsplit() {
        assert_eq!(netloc("https://Gate.Example.top"), "Gate.Example.top");
        assert_eq!(
            netloc("https://gate.example.top:8443/a?b#c"),
            "gate.example.top:8443"
        );
        assert_eq!(netloc("https://u:p@h.test/x"), "u:p@h.test");
        assert_eq!(netloc("null"), "");
        assert_eq!(netloc("//h.test/x"), "h.test");
        assert_eq!(netloc("h.test/x"), "");
        assert_eq!(netloc(" \thttps://h.te\nst/x"), "h.test");
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(py_splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("x\u{2028}y\x0cz"), vec!["x", "y", "z"]);
        assert!(py_splitlines("").is_empty());
    }

    #[test]
    fn injected_scripts_are_recognised() {
        let host = "gate.example.top";
        assert!(injected_script_error("Script error.", "", host));
        assert!(injected_script_error(
            "x",
            "TypeError: x\n    at hit_test (<anonymous>:1:2)\n    at native_call (<anonymous>:3:4)",
            host
        ));
        assert!(!injected_script_error(
            "x",
            "TypeError: x\n    at f (https://gate.example.top/_owl/assets/app-abc.js:1:2)",
            host
        ));
        assert!(!injected_script_error("x", "no frames here", host));
        assert!(!injected_script_error(
            "x",
            "f@https://other.test/_owl/a.js:1:1",
            host
        ));
    }
}
