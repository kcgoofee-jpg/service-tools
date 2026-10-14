//! 机器人桥接 `/self-register/*`（旧版 `app/registration_routes.py` 的上半部分）。
//!
//! 鉴权：`Authorization: Bearer <REGISTRATION_BRIDGE_SECRET>`；成员网页接口见 `member.rs`。
//! 错误一律是 `{"detail": ...}`；请求体的校验错误沿用 FastAPI（pydantic v2）的 422 形状。
//! 次序与旧版一致：先校验请求体（422），再鉴权（503 / 401），再校验服务器 / 管理员（403）。

use crate::action_log::log_action;
use crate::auth::{client_ip, header_str};
use crate::body::JsonObject;
use crate::error::{ApiError, ApiResult, HttpError, InternalError, ok_json};
use crate::feedback;
use crate::key::Key;
use crate::multipart::read_bounded_body;
use crate::ops;
use crate::registration::{DiscordUser, Issued, RegErr, RegistrationService, is_decimal};
use crate::state::AppState;
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use gate_core::pyc::take_chars;
use gate_core::{features, pyext};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;
use subtle::ConstantTimeEq;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/self-register/info", post(info))
        .route("/self-register/ops", post(admin_ops))
        .route("/self-register/quota", post(quota))
        .route("/self-register/resetkey", post(resetkey))
        .route("/self-register/revoke", post(revoke))
        .route("/self-register/slots", post(slots))
        .route("/self-register/issue", post(issue))
        .route("/self-register/feedback", post(feedback_submit))
        .route(
            "/self-register/feedback/questions",
            post(feedback_questions),
        )
        .route("/self-register/bot/config", get(bot_config_get))
        .route("/self-register/bot/report", post(bot_report))
}

/// 桥接 / 成员页接口的请求体上限（旧版没有上限；正常请求都在几百字节）。
pub(super) const BODY_LIMIT: usize = 1024 * 1024;

// ------------------------------------------------------------------ 请求体校验（FastAPI / pydantic v2 的 422 形状）

/// 一个已解析的 JSON 对象请求体 + 字段校验的错误清单（`finish` 时有错误就回 422）。
pub(super) struct Model {
    body: Value,
    errors: Vec<Value>,
}

impl Model {
    pub(super) fn parse(raw: &[u8]) -> Result<Self, HttpError> {
        let err = |e: Value| HttpError {
            status: 422,
            detail: Value::Array(vec![e]),
        };
        if raw.iter().all(u8::is_ascii_whitespace) {
            return Err(err(
                json!({"type": "missing", "loc": ["body"], "msg": "Field required", "input": null}),
            ));
        }
        match serde_json::from_slice::<Value>(raw) {
            Err(e) => Err(err(json!({
                "type": "json_invalid", "loc": ["body", e.column().saturating_sub(1)],
                "msg": "JSON decode error", "input": {}, "ctx": {"error": "Expecting value"},
            }))),
            Ok(v @ Value::Object(_)) => Ok(Self {
                body: v,
                errors: Vec::new(),
            }),
            Ok(other) => Err(err(json!({
                "type": "model_attributes_type", "loc": ["body"],
                "msg": "Input should be a valid dictionary or object to extract fields from",
                "input": other,
            }))),
        }
    }

    fn fail(&mut self, kind: &str, loc: Vec<&str>, msg: &str, input: &Value) {
        let mut l: Vec<Value> = vec![Value::from("body")];
        l.extend(loc.into_iter().map(Value::from));
        self.errors
            .push(json!({"type": kind, "loc": l, "msg": msg, "input": input}));
    }

    fn get(&self, name: &str) -> Option<Value> {
        self.body.get(name).cloned()
    }

    /// `name: str`（`default=None` 为必填）。
    pub(super) fn string(&mut self, name: &str, default: Option<&str>) -> String {
        match self.get(name) {
            Some(Value::String(s)) => s,
            Some(other) => {
                self.fail(
                    "string_type",
                    vec![name],
                    "Input should be a valid string",
                    &other,
                );
                String::new()
            }
            None => {
                if default.is_none() {
                    let whole = self.body.clone();
                    self.fail("missing", vec![name], "Field required", &whole);
                }
                default.unwrap_or("").to_string()
            }
        }
    }

    /// `name: dict = {}`
    fn dict(&mut self, name: &str) -> Map<String, Value> {
        match self.get(name) {
            Some(Value::Object(m)) => m,
            Some(other) => {
                self.fail(
                    "dict_type",
                    vec![name],
                    "Input should be a valid dictionary",
                    &other,
                );
                Map::new()
            }
            None => Map::new(),
        }
    }

    /// `name: dict | None = None`（pydantic 把 `X | None` 当可空类型，类型不对只报 X 的错）
    fn opt_dict(&mut self, name: &str) -> Option<Map<String, Value>> {
        match self.get(name) {
            Some(Value::Object(m)) => Some(m),
            Some(Value::Null) | None => None,
            Some(other) => {
                self.fail(
                    "dict_type",
                    vec![name],
                    "Input should be a valid dictionary",
                    &other,
                );
                None
            }
        }
    }

    pub(super) fn finish(self) -> Result<(), HttpError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(HttpError {
                status: 422,
                detail: Value::Array(self.errors),
            })
        }
    }
}

/// 读请求体并交给 [`Model::parse`]。
pub(super) async fn read_model(parts: &Parts, body: axum::body::Body) -> Result<Model, HttpError> {
    let raw = read_bounded_body(&parts.headers, body, BODY_LIMIT).await?;
    Model::parse(&raw)
}

// ------------------------------------------------------------------ 公共小件

struct Who {
    discord_id: String,
    guild_id: String,
    actor_id: String,
}

impl Who {
    fn read(m: &mut Model) -> Self {
        Self {
            discord_id: m.string("discord_id", None),
            guild_id: m.string("guild_id", None),
            actor_id: m.string("actor_id", Some("")),
        }
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// `_service(request)`：服务没配置 503；桥接密钥不对 401（`record_failure` 时也计入无效请求限流）。
fn authorize<'a>(
    state: &'a AppState,
    parts: &Parts,
    record_failure: bool,
) -> Result<&'a RegistrationService, HttpError> {
    let Some(svc) = state.registration.as_deref() else {
        return Err(HttpError::new(503, "自助领 Key 尚未配置"));
    };
    let given = header_str(&parts.headers, "authorization");
    let want = format!("Bearer {}", svc.bridge_secret());
    if !ct_eq(given.as_bytes(), want.as_bytes()) {
        // 猜桥接密钥也计入无效请求限流（拿不到对端地址时不记，同旧版 `request.client` 为空）
        if record_failure && parts.extensions.get::<ConnectInfo<SocketAddr>>().is_some() {
            let ip = client_ip(parts, &state.env.forwarded_allow_ips);
            crate::auth::record_auth_failure(state, &ip, state.now());
        }
        return Err(HttpError::new(401, "未授权"));
    }
    Ok(svc)
}

/// `_checked`：只认指定服务器；发命令的人要是纯数字 ID。
fn checked<'a>(svc: &RegistrationService, who: &'a Who) -> Result<&'a str, HttpError> {
    if who.guild_id != svc.command_guild() || !is_decimal(&who.discord_id) {
        return Err(HttpError::new(403, "请在指定服务器使用该命令。"));
    }
    Ok(&who.discord_id)
}

/// 管理类操作需要「发起者」在管理员白名单（ADMIN_DISCORD_IDS）里；白名单为空则一律拒绝（默认安全）。
fn admin_actor(svc: &RegistrationService, who: &Who) -> Result<(), HttpError> {
    if svc.admin_ids().is_empty() || !svc.admin_ids().contains(&who.actor_id) {
        return Err(HttpError::new(403, "没有管理员权限。"));
    }
    Ok(())
}

pub(super) fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    resp
}

async fn log_bot(state: &AppState, who: &str, action: &str, target: &str, detail: &str) {
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

fn internal(e: impl std::fmt::Display) -> ApiError {
    InternalError(e.to_string()).into()
}

/// 全站功能开关（读不懂是 500，同旧版）。
async fn global_flags(state: &AppState) -> Result<features::Flags, ApiError> {
    let raw = state.db.get_setting(features::GLOBAL_KEY).await?;
    features::parse_global_flags(raw.as_deref()).map_err(internal)
}

// ------------------------------------------------------------------ 接口

/// 机器人 /help 与 /quota 使用：注册是否开放、新成员默认功能、记录声明、上游状态。
async fn info(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    checked(svc, &who)?;
    let cfg = svc.settings().await?;
    let flags = global_flags(&state).await?;
    let defaults: Vec<String> = match &cfg.features {
        Some(f) => f.clone(),
        None => features::FEATURES
            .iter()
            .filter(|(n, _)| flags[n])
            .map(|(n, _)| (*n).to_string())
            .collect(),
    };
    let default_features: Vec<Value> = defaults
        .iter()
        .filter_map(|n| {
            features::FEATURES
                .iter()
                .find(|(k, _)| k == n)
                .map(|(k, label)| json!({"id": k, "label": label, "on": flags[k]}))
        })
        .collect();
    let (status, recent, failed) = state.health.status(state.now());
    Ok(no_store(ok_json(&json!({
        "open": cfg.open, "active": svc.count_active().await?, "max": cfg.max_users,
        "default_features": default_features,
        "daily_images": cfg.daily_images, "daily_v5": cfg.daily_v5,
        "notice": crate::audit::audit_disclosure(&state.db, &state.env).await?,
        "upstream": {
            "status": status, "recent": recent, "failed": failed,
            "image_cooldown_seconds": state.cooldown.remaining(state.now()),
        },
    }))))
}

/// 管理员命令（Discord 端已限制为「管理服务器」权限）：开关注册、名额、授权功能、记录开关。
async fn admin_ops(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    let action = m.string("action", None);
    let value = m.string("value", Some(""));
    let target = m.string("target", Some(""));
    let feature = m.string("feature", Some(""));
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    checked(svc, &who)?;
    admin_actor(svc, &who)?;
    let on = matches!(value.to_lowercase().as_str(), "1" | "on" | "true" | "开");
    let log_target = if target.is_empty() {
        String::new()
    } else {
        format!("Discord:{target}")
    };
    let params = [feature.as_str(), value.as_str()]
        .iter()
        .filter(|x| !x.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let op = Op {
        action: &action,
        value: &value,
        target: &target,
        feature: &feature,
        on,
    };
    match run_op(&state, svc, &op).await {
        Ok(resp) => {
            log_bot(
                &state,
                &who.actor_id,
                &format!("机器人·{action}"),
                &log_target,
                &params,
            )
            .await;
            Ok(resp)
        }
        Err(ApiError::Http(e)) => {
            let detail = format!("失败（{}）：{}", e.status, pyext::py_str(&e.detail));
            log_action(
                &state.db,
                &format!("Discord:{}", who.actor_id),
                &format!("机器人·{action}"),
                &log_target,
                take_chars(&detail, 200),
                false,
                state.now(),
            )
            .await;
            Err(ApiError::Http(e))
        }
        Err(other) => Err(other),
    }
}

struct Op<'a> {
    action: &'a str,
    value: &'a str,
    target: &'a str,
    feature: &'a str,
    on: bool,
}

async fn run_op(state: &AppState, svc: &RegistrationService, op: &Op<'_>) -> ApiResult {
    let message = |text: String| Ok(ok_json(&json!({"message": text})));
    match op.action {
        "open" => {
            let body: JsonObject = [("open".to_string(), Value::Bool(op.on))]
                .into_iter()
                .collect();
            ops::set_registration(state, &body).await?;
            message(
                if op.on {
                    "已开放领 Key。"
                } else {
                    "已关闭领 Key（已领取的人不受影响）。"
                }
                .to_string(),
            )
        }
        "limit" => {
            if !is_decimal(op.value) {
                return Err(HttpError::new(422, "请输入数字").into());
            }
            let n = pyext::py_int_str(op.value).map_err(|e| internal(e.to_string()))?;
            // 超大数字也走范围检查（`set_registration` 会拒绝并给出原因）
            let body: JsonObject = [(
                "max_users".to_string(),
                json!(n.min(i128::from(i64::MAX)) as i64),
            )]
            .into_iter()
            .collect();
            // 旧版这里的校验失败是未捕获的 ValueError（500）；`ops` 回 422 + 原因，机器人把 detail 原样转给管理员
            ops::set_registration(state, &body).await?;
            let cap = ops::v5_capacity(state).await?;
            let tip = if cap["short"].as_bool().unwrap_or(false) {
                format!(
                    "\n⚠ {}（全站 V5 在网页后台「设置」修改）",
                    cap["message"].as_str().unwrap_or("")
                )
            } else {
                String::new()
            };
            let shown = if n == 0 {
                "不限".to_string()
            } else {
                n.to_string()
            };
            message(format!("名额上限已设为 {shown}。{tip}"))
        }
        "grant" => {
            let Some((_, label)) = features::FEATURES.iter().find(|(n, _)| *n == op.feature) else {
                let names: Vec<&str> = features::FEATURES.iter().map(|(n, _)| *n).collect();
                return Err(HttpError::new(422, format!("未知功能：{}", names.join(","))).into());
            };
            let Some(key) = svc.key_row_for(op.target).await? else {
                return Err(HttpError::new(404, "该成员还没有领取 Key。").into());
            };
            let current = features::key_features(&key.0);
            let mut names: Vec<String> = match current {
                None => features::FEATURES
                    .iter()
                    .map(|(n, _)| (*n).to_string())
                    .collect(),
                Some(c) => c,
            };
            if op.on {
                names.push(op.feature.to_string());
            } else {
                names.retain(|n| n != op.feature);
            }
            let mut fields = Map::new();
            fields.insert(
                "features".into(),
                features::dump(Some(names)).map_or(Value::Null, Value::String),
            );
            state.db.update_key(key.id(), &fields).await?;
            message(format!(
                "已{}：{label}。",
                if op.on { "开通" } else { "关闭" }
            ))
        }
        "ban" => {
            if !is_decimal(op.target) {
                return Err(HttpError::new(422, "缺少目标成员").into());
            }
            svc.ban(op.target, "", "").await?;
            message("已永久禁止该账号领取，并撤销其 Key 与身份组。".into())
        }
        "unban" => {
            let done = svc.unban(op.target).await?;
            message(
                if done {
                    "已解除禁止。"
                } else {
                    "该账号不在禁止名单中。"
                }
                .into(),
            )
        }
        "audit" => {
            let body: JsonObject = [
                ("prompts".to_string(), Value::Bool(op.on)),
                ("thumbs".to_string(), Value::Bool(op.on)),
                ("notify".to_string(), Value::Bool(true)),
            ]
            .into_iter()
            .collect();
            let result = ops::set_audit(state, &body).await?;
            let mut out = Map::new();
            out.insert(
                "message".into(),
                Value::String(
                    if op.on {
                        "已开启生成记录并通知成员。"
                    } else {
                        "已关闭生成记录并通知成员。"
                    }
                    .into(),
                ),
            );
            if let Value::Object(r) = result {
                out.extend(r);
            }
            Ok(ok_json(&Value::Object(out)))
        }
        _ => Err(HttpError::new(422, "未知操作").into()),
    }
}

async fn quota(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    let discord_id = checked(svc, &who)?;
    let Some(key) = svc.key_row_for(discord_id).await? else {
        return Err(HttpError::new(404, "你还没有领取 Key，请先使用 /register。").into());
    };
    let flags = global_flags(&state).await?;
    let granted: Vec<String> = match features::key_features(&key.0) {
        None => features::FEATURES
            .iter()
            .map(|(n, _)| (*n).to_string())
            .collect(),
        Some(own) => own,
    };
    let counter = state.db.get_counter(key.id(), &state.day()).await?;
    let now = state.now();
    let (reset_hour, quiet_now, start, end) = state.guard.with(|g| {
        (
            g.reset_hour,
            g.in_quiet(now),
            g.values.get("quiet_start").copied().unwrap_or(0),
            g.values.get("quiet_end").copied().unwrap_or(0),
        )
    });
    let feature_rows: Vec<Value> = features::FEATURES
        .iter()
        .map(|(n, label)| json!({"id": n, "label": label, "on": granted.iter().any(|g| g == n) && flags[n]}))
        .collect();
    Ok(no_store(ok_json(&json!({
        "enabled": key.enabled(), "expires_at": key.get("expires_at"),
        "daily_images": key.get("daily_images"), "images": counter.images,
        "legacy": counter.legacy_free_images,
        "anlas": gate_core::pyc::round0(counter.anlas),
        "daily_anlas": crate::live::member_anlas_cap(&state, &key).await?,
        "daily_v5": key.get("daily_v5"), "v5": counter.v5,
        "image_model_scope": key.get("image_model_scope"),
        "reset_hour": reset_hour,
        "quiet_now": quiet_now,
        "quiet_window": if quiet_now { format!("{start}:00–{end}:00") } else { String::new() },
        "features": feature_rows,
    }))))
}

async fn resetkey(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    let discord_id = checked(svc, &who)?;
    let Some(key) = svc.key_row_for(discord_id).await? else {
        return Err(HttpError::new(404, "你还没有领取 Key，请先使用 /register。").into());
    };
    let token = gate_core::policy::gen_key("nai", &rand::random::<[u8; 24]>());
    state.db.rotate_key_token(key.id(), &token).await?;
    log_bot(
        &state,
        &who.discord_id,
        "成员重置 Key（/resetkey）",
        &format!("Key #{} {}", key.id(), key.name()),
        "",
    )
    .await;
    Ok(no_store(ok_json(&json!({"key": token}))))
}

async fn revoke(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    checked(svc, &who)?;
    admin_actor(svc, &who)?;
    if !svc.revoke(&who.discord_id, true).await? {
        return Err(HttpError::new(404, "该用户没有已领取的 Key。").into());
    }
    log_bot(
        &state,
        &who.actor_id,
        "机器人·revoke（撤销 Key、释放名额）",
        &format!("Discord:{}", who.discord_id),
        "",
    )
    .await;
    Ok(no_store(ok_json(&json!({"ok": true}))))
}

async fn slots(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    checked(svc, &who)?;
    admin_actor(svc, &who)?;
    let cfg = svc.settings().await?;
    Ok(no_store(ok_json(&json!({
        "active": svc.count_active().await?, "max": cfg.max_users,
        "open": cfg.open, "reset_at": svc.reset_at(),
    }))))
}

/// /register 直发 Key：斜杠命令已验明身份，直接发 Key（不走 OAuth、不私信），临时消息里回给本人。
/// 鉴权：桥接密钥。这是唯一的发 Key 路径（旧的 /intent + /callback OAuth 领取已删除）。
async fn issue(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let discord_id = m.string("discord_id", None);
    let guild_id = m.string("guild_id", None);
    let name = m.string("name", Some(""));
    let username = m.string("username", Some(""));
    let global_name = m.string("global_name", Some(""));
    let avatar = m.string("avatar", Some(""));
    let invite_key = m.string("invite_key", Some(""));
    m.finish()?;
    // 这条接口历来不把猜密钥计入无效请求限流
    let svc = authorize(&state, &parts, false)?;
    let _ = name; // 旧版只截断后传入，发 Key 不用它
    let user = DiscordUser {
        id: discord_id.clone(),
        username: take_chars(&username, 80).to_string(),
        global_name: take_chars(&global_name, 80).to_string(),
        avatar: take_chars(&avatar, 120).to_string(),
    };
    let invite = take_chars(&invite_key, 80).to_string();
    let result = match svc
        .issue_direct(&discord_id, &guild_id, &user, &invite)
        .await
    {
        Ok(r) => r,
        Err(RegErr::Denied(msg)) => {
            log_bot(
                &state,
                &discord_id,
                "领取 Key 失败（/register）",
                "",
                take_chars(&msg, 200),
            )
            .await;
            return Err(HttpError::new(403, msg).into());
        }
        Err(RegErr::Internal(msg)) => return Err(internal(msg)),
    };
    match result {
        Issued::Bound { key_id, message } => {
            log_bot(
                &state,
                &discord_id,
                "绑定邀请 Key（/register）",
                &format!("Key #{key_id}"),
                "",
            )
            .await;
            Ok(no_store(ok_json(&json!({"message": message}))))
        }
        Issued::Provisioned(p) => {
            log_bot(&state, &discord_id, "领取 Key（/register 直发）", "", "").await;
            Ok(no_store(ok_json(
                &json!({"key": p.key, "message": p.message}),
            )))
        }
    }
}

/// /反馈 表单提交（机器人转来）。没领 Key 的成员也能提交。
async fn feedback_submit(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let who = Who::read(&mut m);
    let username = m.string("username", Some(""));
    let answers = m.dict("answers");
    m.finish()?;
    let svc = authorize(&state, &parts, true)?;
    let discord_id = checked(svc, &who)?;
    let key = svc.key_row_for(discord_id).await?;
    let message = feedback::submit(
        &state.db,
        discord_id,
        &username,
        key.as_ref().map(Key::id),
        &answers,
        state.now(),
    )
    .await
    .map_err(|e| match e {
        feedback::SubmitError::Rejected(m) => ApiError::from(HttpError::new(400, m)),
        feedback::SubmitError::Db(e) => e.into(),
    })?;
    log_bot(&state, discord_id, "提交反馈（/反馈）", "", "").await;
    Ok(ok_json(&json!({"message": message})))
}

/// 机器人启动时来读问卷题目（改题目不用改机器人代码）。
async fn feedback_questions(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _body) = req.into_parts();
    authorize(&state, &parts, true)?;
    Ok(ok_json(&json!({"questions": feedback::questions_json()})))
}

/// 机器人每分钟来读一次配置（后台「Discord」页可改，不用重启机器人）。
async fn bot_config_get(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, _body) = req.into_parts();
    authorize(&state, &parts, true)?;
    let cfg = crate::bot_config::load(&state.db).await?;
    Ok(no_store(ok_json(&Value::Object(cfg))))
}

/// 机器人上报心跳和动作（点赞 / 评论），后台显示在线状态和最近记录。
async fn bot_report(State(state): State<AppState>, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let mut m = read_model(&parts, body).await?;
    let status = m.opt_dict("status");
    let event = m.opt_dict("event");
    m.finish()?;
    authorize(&state, &parts, true)?;
    crate::bot_config::report(&state.db, status.as_ref(), event.as_ref(), state.now())
        .await
        .map_err(|e| match e {
            crate::bot_config::BotConfigError::Db(e) => ApiError::from(e),
            other => internal(other),
        })?;
    Ok(ok_json(&json!({"ok": true})))
}
