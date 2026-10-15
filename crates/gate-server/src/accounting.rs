//! 记账（旧版 `main.record` / `settle_record` / `bug`）与成员接口的错误收尾（旧版两个异常处理器里
//! 「记一条拒绝 / 记 Bug」的部分）。
//!
//! - `record`：写用量日志；成功请求同时计入每日配额。写库在后台任务里做（关停时等它们写完）。
//! - `settle_record`：成功出图的那一笔必须在额度预留 / 并发槽释放之前落库。后台任务不随调用方取消，
//!   所以「等它写完」本身就是旧版 `asyncio.shield` 的语义。

use crate::bugs::Report;
use crate::error::{GateErrorInfo, InternalErrorInfo};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use gate_db::repo::usage::{NewLog, Success};

/// 一条用量记录（旧版 `record()` 的关键字参数）。
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub kind: String,
    pub model: String,
    pub status: String,
    pub images: i64,
    pub anlas: f64,
    pub tokens: i64,
    pub v5: i64,
    pub legacy_free_images: i64,
    pub detail: String,
    pub unconfirmed_anlas: f64,
    pub reason: String,
}

impl Entry {
    pub fn new(kind: &str, model: &str, status: &str) -> Self {
        Self {
            kind: kind.into(),
            model: model.into(),
            status: status.into(),
            ..Default::default()
        }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = r.into();
        self
    }
    pub fn unconfirmed(mut self, a: f64) -> Self {
        self.unconfirmed_anlas = a;
        self
    }
}

/// 路径 → 用量日志里的 kind（旧版 `_PATH_KIND`，按顺序子串匹配）。
const PATH_KIND: [(&str, &str); 6] = [
    ("generate-image-stream", "image_stream"),
    ("suggest-tags", "tags"),
    ("generate-image", "image"),
    ("encode-vibe", "vibe_encode"),
    ("upscale", "upscale"),
    ("augment-image", "augment-image"),
];

pub fn kind_for_path(path: &str) -> &'static str {
    PATH_KIND
        .iter()
        .find(|(needle, _)| path.contains(needle))
        .map_or("account", |(_, k)| k)
}

impl AppState {
    /// 记一个 Bug（自动带上当前请求的编号和 Key）。
    pub fn bug(&self, source: &str, error: Option<(&str, &str)>, ctx: &ReqCtx) -> String {
        self.bugs.capture(Report {
            source,
            error,
            rid: &ctx.rid(),
            key_id: ctx.key().map(|(id, _)| id),
            ..Default::default()
        })
    }

    /// 写一条用量日志（后台任务）；成功请求额外计入每日配额。返回任务句柄。
    pub fn record(&self, ctx: &ReqCtx, key: &Key, e: Entry) -> tokio::task::JoinHandle<()> {
        ctx.mark_logged();
        self.live_note(&e.status);
        let t = ctx.snapshot();
        let (key_id, key_name) = (key.id(), key.name().to_string());
        let state = self.clone();
        let ctx = ctx.clone();
        self.tasks.spawn(async move {
            let now = state.now();
            let res = if e.status == "ok" {
                state
                    .db
                    .record_success(
                        &Success {
                            key_id,
                            key_name,
                            kind: e.kind.clone(),
                            model: e.model,
                            day: state.day(),
                            images: e.images,
                            anlas: e.anlas,
                            tokens: e.tokens,
                            v5: e.v5,
                            legacy_free_images: e.legacy_free_images,
                            detail: e.detail,
                            unconfirmed_anlas: e.unconfirmed_anlas,
                            wait_ms: t.wait_ms,
                            dur_ms: t.dur_ms,
                            up_status: i64::from(t.up_status),
                            rid: t.rid,
                        },
                        now,
                    )
                    .await
            } else {
                state
                    .db
                    .add_log(
                        &NewLog {
                            key_id: Some(key_id),
                            key_name,
                            kind: e.kind.clone(),
                            model: e.model,
                            status: e.status.clone(),
                            images: e.images,
                            anlas: e.anlas,
                            tokens: e.tokens,
                            detail: e.detail,
                            unconfirmed_anlas: e.unconfirmed_anlas,
                            wait_ms: t.wait_ms,
                            dur_ms: t.dur_ms,
                            up_status: i64::from(t.up_status),
                            rid: t.rid,
                            reason: e.reason,
                        },
                        now,
                    )
                    .await
            };
            if let Err(err) = res {
                tracing::error!("request accounting failed");
                // 「成员拿到了图但账没落库」的唯一线索：带上定位信息
                state.bugs.capture(Report {
                    source: "accounting",
                    error: Some(("DbError", &err.to_string())),
                    detail: &format!(
                        "key_id={key_id} kind={} status={} images={}",
                        e.kind, e.status, e.images
                    ),
                    rid: &ctx.rid(),
                    key_id: Some(key_id),
                    ..Default::default()
                });
            }
        })
    }

    /// 成功的那笔账在释放额度预留 / 并发槽之前写完；调用方被取消也不会中断写入。
    pub async fn settle_record(&self, ctx: &ReqCtx, key: &Key, e: Entry) {
        let _ = self.record(ctx, key, e).await;
    }
}

/// 成员接口错误的收尾（放在请求上下文中间件里面）：
/// - 鉴权之后被拒（4xx）且本请求还没记过日志：统一记一条「rejected」，方便排查成员问题；
/// - 5xx 的 GateError：按消息归并成 Bug（网关自己的故障）；原因在上游的（`GateError::upstream`）不记——
///   上游故障在用量日志和「上游」页里看得到，不是本站的缺陷；
/// - 未预料的内部错误：记 Bug，有 Key 且没记过日志时补一条「error」；上游传输层失败
///   （`error::upstream_transport`）对成员仍是 500，但同样不记 Bug。
pub async fn error_ledger(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let ctx = req.extensions().get::<ReqCtx>().cloned();
    let resp = next.run(req).await;
    let Some(ctx) = ctx else { return resp };
    if let Some(GateErrorInfo(e)) = resp.extensions().get::<GateErrorInfo>() {
        if let Some((key_id, key_name)) = ctx.key()
            && (400..500).contains(&e.status)
            && !ctx.logged()
        {
            let key = Key(serde_json::json!({"id": key_id, "name": key_name}));
            let detail =
                gate_core::pyc::take_chars(&format!("{} {}", e.status, e.message), 160).to_string();
            state.record(
                &ctx,
                &key,
                Entry::new(kind_for_path(&path), &ctx.model(), "rejected")
                    .detail(detail)
                    .reason(e.code.clone()),
            );
        }
        if e.status >= 500 && !e.upstream {
            let title =
                gate_core::pyc::take_chars(&format!("{} {}", e.status, e.message), 200).to_string();
            state.bugs.capture(Report {
                source: if matches!(e.status, 502..=504) {
                    "upstream"
                } else {
                    "gate"
                },
                title: &title,
                path: &path,
                level: "warn",
                rid: &ctx.rid(),
                key_id: ctx.key().map(|(id, _)| id),
                ..Default::default()
            });
        }
    } else if let Some(InternalErrorInfo(msg)) = resp.extensions().get::<InternalErrorInfo>() {
        if !ctx.upstream_fault() {
            state.bugs.capture(Report {
                source: "request",
                error: Some(("InternalError", msg)),
                path: &path,
                rid: &ctx.rid(),
                key_id: ctx.key().map(|(id, _)| id),
                ..Default::default()
            });
        }
        if let Some((key_id, key_name)) = ctx.key()
            && !ctx.logged()
        {
            let t = ctx.snapshot();
            let db = state.db.clone();
            let now = state.now();
            let kind = kind_for_path(&path).to_string();
            state.tasks.spawn(async move {
                let _ = db
                    .add_log(
                        &NewLog {
                            key_id: Some(key_id),
                            key_name,
                            kind,
                            status: "error".into(),
                            detail: "500 InternalError".into(),
                            rid: t.rid,
                            ..Default::default()
                        },
                        now,
                    )
                    .await;
            });
        }
    }
    resp
}

impl AppState {
    /// 按后台设置记录提示词和原图（缩略图编码较慢，放后台任务；任何失败都不得影响生图结果）。
    #[allow(clippy::too_many_arguments)]
    pub async fn audit_generation(
        &self,
        ctx: &ReqCtx,
        key: &Key,
        kind: &str,
        model: &str,
        status: &str,
        body: &serde_json::Value,
        content: Option<bytes::Bytes>,
    ) {
        crate::gen_audit::run(self, ctx, key, kind, model, status, body, content).await;
    }

    /// 不等结果的版本（成功出图后）。
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_audit(
        &self,
        ctx: &ReqCtx,
        key: &Key,
        kind: &str,
        model: &str,
        status: &str,
        body: serde_json::Value,
        content: Option<bytes::Bytes>,
    ) {
        let (s, c, k) = (self.clone(), ctx.clone(), key.clone());
        let (kind, model, status) = (kind.to_string(), model.to_string(), status.to_string());
        self.tasks.spawn(async move {
            crate::gen_audit::run(&s, &c, &k, &kind, &model, &status, &body, content).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_kinds() {
        assert_eq!(kind_for_path("/ai/generate-image-stream"), "image_stream");
        assert_eq!(kind_for_path("/nai/ai/generate-image"), "image");
        assert_eq!(kind_for_path("/ai/generate-image/suggest-tags"), "tags");
        // 文本接口已删除，这些路径不会再走到记账；按「账户查询」兜底
        assert_eq!(kind_for_path("/ai/generate"), "account");
        assert_eq!(kind_for_path("/user/subscription"), "account");
    }
}
