//! 站点公告、Discord 公告频道发送、测试告警、Discord 机器人（奶妹）在线状态。

use crate::action_log::log_action;
use crate::body::read_json_body;
use crate::error::{ApiResult, HttpError, InternalError, ok_json};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::routing::{get, post};
use gate_core::bot_config::{self, BotConfigError};
use gate_core::{pyc, pyext};
use serde_json::json;

const BODY_LIMIT: usize = 1024 * 1024;
/// 站点公告 HTML 的长度上限（字符）。
const ANNOUNCEMENT_MAX: usize = 20_000;
/// Discord 单条消息 2000 字符，留余量。
const SEND_MAX: usize = 1800;
/// 两次测试告警至少间隔这么多秒。
const ALERT_TEST_GAP: f64 = 10.0;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/api/announcement",
            get(get_announcement).put(put_announcement),
        )
        .route("/admin/api/announcement/send", post(announcement_send))
        .route("/admin/api/alerts/test", post(alerts_test))
        .route("/admin/api/discord-bot", get(discord_bot_get))
}

// ------------------------------------------------------------------ 站点公告

/// 首页公告（HTML 原样保存在数据目录下的 announcement.html）。
/// 与 Python 的 `read_text` 一致：按「通用换行」读，`\r\n` / `\r` 都变成 `\n`。
async fn get_announcement(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let html = match std::fs::read_to_string(state.env.announcement_path()) {
        Ok(text) => text.replace("\r\n", "\n").replace('\r', "\n"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(InternalError(format!("读取站点公告失败：{e}")).into()),
    };
    Ok(ok_json(&json!({"html": html})))
}

async fn put_announcement(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let html = body.get("html").map(pyext::py_str).unwrap_or_default();
    let html = pyc::take_chars(&html, ANNOUNCEMENT_MAX);
    std::fs::write(state.env.announcement_path(), html)
        .map_err(|e| InternalError(format!("保存站点公告失败：{e}")))?;
    Ok(ok_json(&json!({"ok": true})))
}

/// 让奶妹把一条消息发到 Discord 公告频道（ANNOUNCE_CHANNEL_ID）；`@here` / `@everyone` 不会真的提醒
/// （发送时带 `allowed_mentions: {parse: []}`）。
async fn announcement_send(
    State(state): State<AppState>,
    _s: AdminSession,
    req: Request,
) -> ApiResult {
    let (parts, body) = req.into_parts();
    let body = read_json_body(&parts.headers, body, BODY_LIMIT).await?;
    let text = body
        .get("text")
        .map(|v| pyc::strip(&pyext::py_str(v)).to_string())
        .unwrap_or_default();
    if text.is_empty() {
        return Err(HttpError::new(422, "公告内容不能为空").into());
    }
    if pyc::len_chars(&text) > SEND_MAX {
        return Err(HttpError::new(422, "公告太长（Discord 单条 2000 字符，留了余量）").into());
    }
    if !state.announcer.configured() {
        return Err(HttpError::new(503, "公告频道未配置（.env 的 ANNOUNCE_CHANNEL_ID）").into());
    }
    state.announcer.post(&text);
    // 只记这一条（带字数和正文摘要），中间件不再重复记
    log_action(
        &state.db,
        &super::super::actor(&state, &parts),
        "发送公告",
        &format!("{} 字", pyc::len_chars(&text)),
        pyc::take_chars(&text, 200),
        true,
        state.now(),
    )
    .await;
    let mut resp = ok_json(&json!({"ok": true}));
    resp.extensions_mut().insert(super::super::ActionLogged);
    Ok(resp)
}

// ------------------------------------------------------------------ 测试告警

async fn alerts_test(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let shared = &state.ops;
    let now = state.now();
    let last = *shared.alert_test_at.lock().expect("alert_test_at");
    if now - last < ALERT_TEST_GAP {
        return Err(HttpError::new(
            429,
            format!(
                "测试告警发送过于频繁，请等待 {} 秒后再试",
                (ALERT_TEST_GAP - (now - last)) as i64
            ),
        )
        .into());
    }
    if !state.alerter.configured() {
        return Err(HttpError::new(
            409,
            "尚未配置告警私信（DISCORD_BOT_TOKEN 与 ALERT_USER_ID）",
        )
        .into());
    }
    *shared.alert_test_at.lock().expect("alert_test_at") = now;
    state
        .alerter
        .notify("test", "这是一条测试告警，收到说明告警渠道正常。", 0.0);
    Ok(ok_json(&json!({"ok": true})))
}

// ------------------------------------------------------------------ Discord 机器人

fn bot_internal(e: BotConfigError) -> InternalError {
    InternalError(e.to_string())
}

/// 机器人在线状态（心跳）。跑图分享设置和最近动作已删除：docs/v3-deviations.md #54。
async fn discord_bot_get(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let status = state.db.get_setting(bot_config::STATUS_KEY).await?;
    let snapshot = bot_config::snapshot(status.as_deref(), state.now()).map_err(bot_internal)?;
    Ok(ok_json(&snapshot))
}
