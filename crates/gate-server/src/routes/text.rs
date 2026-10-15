//! 文本生成、流式响应、语音合成与 OpenAI 兼容聊天（旧版 `app/routers/text_routes.py`，
//! 连同 `app/main.py` 里只服务这些接口的 `_text_url` / `_require_text_model` / `_text_quota_check` /
//! `TEXT_BODY_MB` / `TextStreamResponse`）。
//!
//! 流式语义（与旧版一致）：
//! - 响应头在上游接受请求（HTTP 200）之后立刻发出；在此之前上游拒绝，回的是一个普通的 JSON 错误；
//!   在此之后上游的任何故障都只能编码进事件流（原生接口是 `event: error`，聊天接口是 `data: {"error": ..}`）；
//! - 推流在后台任务里跑（旧版 `TextStreamResponse` 把上游连接 / 并发槽 / 在途名额的所有权交给响应）：
//!   成员断开后立刻停止读取上游，已经读到的 token 照常记账（状态 ok），然后才释放名额、并发槽和上游连接；
//! - 非流式请求一旦开始上游调用，就在后台任务里跑完并记账（成员断开也不中断）。
//!
//! 记账口径（与旧版一致）：token 数按「收到的 token 事件个数」算；撞上每日上限时那一个 token 也算数，
//! 但不转发（原生接口补一个 `data: [DONE]`，聊天接口走正常的 stop + `[DONE]` 收尾）。

use crate::accounting::Entry;
use crate::admission::{Slots, acquire_slots};
use crate::auth::{authenticate, check_rpm, ctx_of, limit_inflight, require_feature};
use crate::body::{TEXT_BODY_LIMIT, read_json};
use crate::error::{ApiError, ApiResult, GateError, InternalError, ok_json, upstream_transport};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::runtime::InflightPermit;
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use events::{Item, StreamError, TextEvents};
use futures::StreamExt;
use gate_core::features::FEATURES;
use gate_core::jsondump;
use gate_core::policy::{TEXT_INPUT_BUDGET_TOKENS, clamp_text_params, estimate_tokens};
use gate_core::pyext::{self, PyErr, py_str};
use gate_core::pyloads::{self, Options};
use gate_core::pyobj::Obj;
use gate_core::pyval::{self, truthy};
use gate_core::sse::encode_sse;
use gate_core::upstream_errors::upstream_error_message;
use gate_core::{policy, pyc};
use gate_upstream::client::{Callbacks, RequestOpts, UpstreamError};
use serde_json::{Value, json};
use std::future::Future;
use tokio::sync::mpsc;

/// 上游真正存在的文本模型：2026-10-13 用网关自己的请求形态（明文 input + use_string）
/// 逐个实测 /ai/generate-stream 得到 200 的只有这两个。以前这份名单里还写着 clio-v1 /
/// nai-glm-4-6 / nai-xialong，上游回的是 403 {"message":"model '…' doesn't exist"} ——
/// 被当成「账号被拒」报给成员，还会污染共享账号的健康度（判据见 gate_core::upstream_policy 的模型级 403）。
pub const TEXT_MODELS: [&str; 2] = ["llama-3-erato-v1", "kayra-v1"];

/// 曾经出现在名单里、但上游不认的模型名：本地直接给出诚实原因，不要打到上游换一个 403。
pub const TEXT_MODELS_RETIRED: [(&str, &str); 5] = [
    (
        "clio-v1",
        "上游已没有这个模型（NovelAI 老模型 clio 已下线）",
    ),
    (
        "nai-glm-4-6",
        "上游没有这个模型名；GLM-4.6 只能走 NovelAI 的 OpenAI 兼容接口，本站暂未开放",
    ),
    (
        "glm-4-6",
        "GLM-4.6 只能走 NovelAI 的 OpenAI 兼容接口，本站暂未开放",
    ),
    (
        "nai-xialong",
        "上游没有这个模型名；Xialong 只能走 NovelAI 的 OpenAI 兼容接口，本站暂未开放",
    ),
    (
        "xialong",
        "上游没有这个模型名；Xialong 只能走 NovelAI 的 OpenAI 兼容接口，本站暂未开放",
    ),
];

/// 聊天接口里不认识的模型名一律落到这个默认模型。
const DEFAULT_CHAT_MODEL: &str = "llama-3-erato-v1";

pub fn router() -> Router<AppState> {
    let mut r = Router::new();
    for p in ["/ai/generate-stream", "/nai/ai/generate-stream"] {
        r = r.route(p, post(generate_stream));
    }
    for p in ["/ai/generate", "/nai/ai/generate"] {
        r = r.route(p, post(generate_text));
    }
    for p in ["/ai/generate-voice", "/nai/ai/generate-voice"] {
        r = r.route(p, post(generate_voice));
    }
    r.route("/v1/models", get(v1_models))
        .route("/v1", get(v1_models))
        .route("/v1/chat/completions", post(v1_chat))
        .route("/v1/me", get(v1_me))
}

// ------------------------------------------------------------------ 旧版 main.py 里的文本小件

/// `_text_url`：新模型走 text 主机，老模型走 legacy 主机。
fn text_url(state: &AppState, model: &str, stream: bool) -> String {
    let host = policy::text_model_host(model, state.nai.text_host(), state.nai.legacy_text_host());
    format!("{host}/ai/generate{}", if stream { "-stream" } else { "" })
}

/// `f"{model}：{reason}；可用："` + 白名单（原生接口与聊天接口共用的拒绝文案）。
fn retired_message(model: &str, why: &str) -> String {
    format!("{model}：{why}；可用：{}", TEXT_MODELS.join("、"))
}

fn retired_reason(model: &str) -> Option<&'static str> {
    TEXT_MODELS_RETIRED
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, why)| *why)
}

/// `_require_text_model`：原生文本入口只放行白名单模型，避免任意 model 字符串打到上游。
///
/// 已知上游不认的名字（`TEXT_MODELS_RETIRED`）对任何 Key 都直接拒绝 —— 包括站长号：
/// 上游对「模型不存在」回的是 403，那条 403 会被当成账号被拒（还会记进账号健康度），
/// 所以连站长测试也应该在本地就拿到明确原因，而不是往上打一枪。
fn require_text_model(
    state: &AppState,
    ctx: &ReqCtx,
    key: &Key,
    model: &str,
    kind: &str,
) -> Result<(), GateError> {
    if let Some(why) = retired_reason(model) {
        state.record(
            ctx,
            key,
            Entry::new(kind, model, "rejected").detail("model retired upstream"),
        );
        return Err(GateError::new(400, retired_message(model, why)));
    }
    if key.is_admin() || TEXT_MODELS.contains(&model) {
        return Ok(());
    }
    state.record(
        ctx,
        key,
        Entry::new(kind, model, "rejected").detail("model not allowed"),
    );
    Err(GateError::new(400, "不支持的文本模型"))
}

/// `_text_quota_check`
async fn text_quota_check(state: &AppState, key: &Key) -> Result<(), ApiError> {
    if key.is_admin() {
        return Ok(());
    }
    let c = state.db.get_counter(key.id(), &state.day()).await?;
    let limit = key.daily_text_tokens();
    if limit >= 0 && c.text_tokens >= limit {
        return Err(GateError::new(
            402,
            format!("已达今日文本额度（{limit} tokens/天），明日再来吧"),
        )
        .into());
    }
    Ok(())
}

// ------------------------------------------------------------------ 公共小件

fn respond(r: ApiResult) -> Response {
    r.unwrap_or_else(IntoResponse::into_response)
}

/// Python 抛出的未捕获 TypeError / ValueError：旧版是 500。
fn py500(e: PyErr) -> ApiError {
    InternalError(e.to_string()).into()
}

/// 上游错误 → 成员接口错误（旧版 `upstream_call` 的转换）：只有 429 / 503 原样透出，其余一律 502。
fn up_to_gate(e: UpstreamError) -> GateError {
    let status = if matches!(e.status, 429 | 503) {
        e.status
    } else {
        502
    };
    GateError::with_code(status, e.message, e.code)
        .uncertain(e.billing_uncertain)
        .from_upstream(!e.local)
}

/// 已经开始的上游操作：放进后台任务跑完（含记账），调用方断开也不中断。
async fn shielded<T: Send + 'static>(
    state: &AppState,
    ctx: &ReqCtx,
    fut: impl Future<Output = Result<T, ApiError>> + Send + 'static,
) -> Result<T, ApiError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.tasks.spawn(ctx.clone().scope(async move {
        let _ = tx.send(fut.await);
    }));
    rx.await
        .unwrap_or_else(|_| Err(InternalError("后台任务异常结束".into()).into()))
}

/// Starlette `Response(content, media_type=…)`：`text/*` 没写字符集时补 `; charset=utf-8`。
fn media_response(content: Bytes, media_type: &[u8]) -> Response {
    let mut ct = media_type.to_vec();
    let lower = String::from_utf8_lossy(&ct).to_lowercase();
    if lower.starts_with("text/") && !lower.contains("charset=") {
        ct.extend_from_slice(b"; charset=utf-8");
    }
    let mut r = Response::new(Body::from(content));
    if let Ok(v) = HeaderValue::from_bytes(&ct) {
        r.headers_mut().insert("content-type", v);
    }
    r
}

/// 打开上游文本流（不重试）。错误处理与旧版一致：先记一条 error，再把状态码收敛到 400 / 422 / 429 / 503 / 502；
/// 传输层故障（旧版是未捕获的 httpx 异常）是 500。
async fn open_stream(
    state: &AppState,
    ctx: &ReqCtx,
    key: &Key,
    kind: &str,
    model: &str,
    body: &Value,
) -> Result<events::ByteStream, ApiError> {
    let url = text_url(state, model, true);
    match state.nai.text_stream(&url, body, ctx).await {
        Ok(resp) => Ok(Box::pin(resp.bytes_stream().map(|r| r.map_err(|_| ())))),
        Err(e) if e.transport => Err(upstream_transport(ctx, e.message)),
        Err(e) => {
            state.record(
                ctx,
                key,
                Entry::new(kind, model, "error").detail(e.message.clone()),
            );
            let status = if matches!(e.status, 400 | 422 | 429 | 503) {
                e.status
            } else {
                502
            };
            Err(GateError::with_code(status, e.message, e.code)
                .from_upstream(!e.local)
                .into())
        }
    }
}

/// httpx 的 `json=` 不接受 NaN / Infinity（`allow_nan=False`）：旧版在发往上游的那一步抛 ValueError → 500。
fn require_finite(floats: &[f64]) -> Result<(), ApiError> {
    if floats.iter().all(|f| f.is_finite()) {
        Ok(())
    } else {
        Err(
            InternalError("ValueError: Out of range float values are not JSON compliant".into())
                .into(),
        )
    }
}

// ------------------------------------------------------------------ 上游文本事件流（旧版 `text_stream_events`）

mod events {
    use bytes::Bytes;
    use futures::{Stream, StreamExt};
    use gate_core::pyloads::{self, Options};
    use gate_core::pyobj::Obj;
    use gate_core::sse::{SseDecoder, SseEvent};
    use std::collections::VecDeque;
    use std::pin::Pin;

    /// 上游响应体的字节块；读失败（旧版的 `httpx.HTTPError`）不带内容，只让我们知道连接断了。
    pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ()>> + Send>>;

    /// 旧版的 `UpstreamError(502, …)`：文案只描述失败，不带任何上游响应内容。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StreamError {
        pub status: u16,
        pub message: &'static str,
    }

    const FAILED: StreamError = StreamError {
        status: 502,
        message: "上游文本生成失败，请稍后重试",
    };
    const BAD_FORMAT: StreamError = StreamError {
        status: 502,
        message: "上游文本事件流格式无效",
    };
    const CONN_LOST: StreamError = StreamError {
        status: 502,
        message: "上游文本流连接中断，请稍后重试",
    };

    /// 解出的一个事件：`raw` / `event` 保持上游原样，原生接口按它们重新编码转发。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Item {
        Token {
            raw: Vec<u8>,
            event: Vec<u8>,
            token: String,
        },
        /// `data: [DONE]`：旧版 `payload is None`。之后不再读上游。
        Done { raw: Vec<u8>, event: Vec<u8> },
    }

    enum Step {
        Skip,
        Item(Item),
    }

    /// 旧版循环体：判定一个完整事件。
    fn classify(ev: SseEvent) -> Result<Step, StreamError> {
        if ev.event == b"error" {
            return Err(FAILED);
        }
        if ev.data == b"[DONE]" {
            return Ok(Step::Item(Item::Done {
                raw: ev.data,
                event: ev.event,
            }));
        }
        // json.loads(raw.decode("utf-8"))：编码错 / 语法错 / 嵌套过深都是「格式无效」
        let text = std::str::from_utf8(&ev.data).map_err(|_| BAD_FORMAT)?;
        let loaded = pyloads::loads_str(text, Options::LOADS).map_err(|_| BAD_FORMAT)?;
        let Obj::Dict(d) = &loaded.value else {
            return Err(BAD_FORMAT);
        };
        if d.contains("error")
            || d.contains("message")
            || matches!(d.get("event_type"), Some(Obj::Str(s)) if s == "error")
        {
            return Err(FAILED);
        }
        let Some(token) = d.get("token") else {
            return Ok(Step::Skip);
        };
        let Obj::Str(token) = token else {
            return Err(BAD_FORMAT);
        };
        let token = token.clone();
        Ok(Step::Item(Item::Token {
            raw: ev.data,
            event: ev.event,
            token,
        }))
    }

    /// 旧版 `text_stream_events` 的异步生成器：每次 `next()` 给出下一个有意义的事件；
    /// 流正常读完（没有 `[DONE]` 也算）给 `Ok(None)`；出错后即告终止。
    pub struct TextEvents {
        body: ByteStream,
        decoder: SseDecoder,
        queue: VecDeque<SseEvent>,
        /// 解码器在某一块的后半段超限：先把此前解出的事件处理完，再报错（旧版 `feed` 是生成器）。
        overflow: bool,
        over: bool,
    }

    impl TextEvents {
        pub fn new(body: ByteStream) -> Self {
            Self {
                body,
                decoder: SseDecoder::new(1024 * 1024, 128 * 1024 * 1024),
                queue: VecDeque::new(),
                overflow: false,
                over: false,
            }
        }

        fn finish(&mut self) {
            self.over = true;
            self.queue.clear();
            self.decoder.finish();
        }

        pub async fn next(&mut self) -> Result<Option<Item>, StreamError> {
            loop {
                if self.over {
                    return Ok(None);
                }
                if let Some(ev) = self.queue.pop_front() {
                    match classify(ev) {
                        Ok(Step::Skip) => continue,
                        Ok(Step::Item(item)) => {
                            if matches!(item, Item::Done { .. }) {
                                self.finish();
                            }
                            return Ok(Some(item));
                        }
                        Err(e) => {
                            self.finish();
                            return Err(e);
                        }
                    }
                }
                if self.overflow {
                    self.finish();
                    return Err(BAD_FORMAT);
                }
                match self.body.next().await {
                    None => {
                        self.finish();
                        return Ok(None);
                    }
                    Some(Err(())) => {
                        self.finish();
                        return Err(CONN_LOST);
                    }
                    Some(Ok(chunk)) => {
                        let feed = self.decoder.feed(&chunk);
                        self.queue.extend(feed.events);
                        self.overflow = feed.error.is_some();
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use futures::stream;

        fn events(chunks: Vec<Result<&'static [u8], ()>>) -> TextEvents {
            TextEvents::new(Box::pin(stream::iter(
                chunks
                    .into_iter()
                    .map(|c| c.map(Bytes::from_static))
                    .collect::<Vec<_>>(),
            )))
        }

        async fn drain(mut ev: TextEvents) -> (Vec<Item>, Option<StreamError>) {
            let mut items = Vec::new();
            loop {
                match ev.next().await {
                    Ok(Some(i)) => items.push(i),
                    Ok(None) => return (items, None),
                    Err(e) => return (items, Some(e)),
                }
            }
        }

        fn token(raw: &str, event: &str, token: &str) -> Item {
            Item::Token {
                raw: raw.as_bytes().to_vec(),
                event: event.as_bytes().to_vec(),
                token: token.into(),
            }
        }

        #[tokio::test]
        async fn byte_by_byte_with_crlf_and_done_stops_reading() {
            let wire: &'static [u8] =
                b"event: newToken\r\ndata: {\"token\": \"a\"}\r\n\r\ndata: [DONE]\n\ndata: {\"token\":\"late\"}\n\n";
            let chunks = wire.chunks(1).map(Ok).collect();
            let (items, err) = drain(events(chunks)).await;
            assert_eq!(err, None);
            assert_eq!(
                items,
                vec![
                    token("{\"token\": \"a\"}", "newToken", "a"),
                    Item::Done {
                        raw: b"[DONE]".to_vec(),
                        event: vec![]
                    }
                ]
            );
        }

        #[tokio::test]
        async fn events_without_token_are_skipped_and_eof_without_done_is_fine() {
            let (items, err) = drain(events(vec![
                Ok(b"data: {\"foo\": 1}\n\ndata: {\"token\": \"\"}\n\n"),
                Ok(b"data: {\"token\": \"b\"}\n\n"),
            ]))
            .await;
            assert_eq!(err, None);
            assert_eq!(
                items,
                vec![
                    token("{\"token\": \"\"}", "", ""),
                    token("{\"token\": \"b\"}", "", "b")
                ]
            );
        }

        #[tokio::test]
        async fn upstream_failure_shapes_map_to_fixed_messages() {
            for (wire, msg) in [
                (
                    &b"event: error\ndata: {\"message\":\"private\"}\n\n"[..],
                    "上游文本生成失败，请稍后重试",
                ),
                (
                    b"data: {\"error\":\"private\"}\n\n",
                    "上游文本生成失败，请稍后重试",
                ),
                (
                    b"data: {\"token\":\"a\",\"message\":null}\n\n",
                    "上游文本生成失败，请稍后重试",
                ),
                (
                    b"data: {\"event_type\":\"error\"}\n\n",
                    "上游文本生成失败，请稍后重试",
                ),
                (b"data: <html>private</html>\n\n", "上游文本事件流格式无效"),
                (b"data: [1]\n\n", "上游文本事件流格式无效"),
                (b"data: {\"token\": 5}\n\n", "上游文本事件流格式无效"),
                (b"data: \xff\xfe\n\n", "上游文本事件流格式无效"),
                (
                    b"data: \xef\xbb\xbf{\"token\":\"a\"}\n\n",
                    "上游文本事件流格式无效",
                ),
            ] {
                let (items, err) = drain(events(vec![Ok(wire)])).await;
                assert!(items.is_empty(), "{wire:?}");
                assert_eq!(
                    err.map(|e| (e.status, e.message)),
                    Some((502, msg)),
                    "{wire:?}"
                );
            }
        }

        #[tokio::test]
        async fn python_json_quirks_are_accepted() {
            // json.loads 接受 NaN / Infinity，也接受重复键（最后一个生效）
            let (items, err) = drain(events(vec![Ok(
                b"data: {\"token\": \"a\", \"x\": NaN, \"token\": \"b\"}\n\n",
            )]))
            .await;
            assert_eq!(err, None);
            assert!(matches!(&items[0], Item::Token { token, .. } if token == "b"));
        }

        #[tokio::test]
        async fn events_before_a_failure_are_still_delivered() {
            let (items, err) = drain(events(vec![Ok(
                b"data: {\"token\": \"a\"}\n\ndata: {\"error\": 1}\n\ndata: {\"token\": \"c\"}\n\n",
            )]))
            .await;
            assert_eq!(items.len(), 1);
            assert_eq!(err, Some(FAILED));
        }

        #[tokio::test]
        async fn broken_connection_and_event_limit() {
            let (items, err) =
                drain(events(vec![Ok(b"data: {\"token\": \"a\"}\n\n"), Err(())])).await;
            assert_eq!(items.len(), 1);
            assert_eq!(
                err.map(|e| e.message),
                Some("上游文本流连接中断，请稍后重试")
            );
            // 单个事件超过 1 MB：此前完整的事件照常送达，随后报格式无效
            let mut big = b"data: {\"token\": \"a\"}\n\ndata: ".to_vec();
            big.extend(std::iter::repeat_n(b'x', 1024 * 1024 + 8));
            let body: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(big))]));
            let (items, err) = drain(TextEvents::new(body)).await;
            assert_eq!(items.len(), 1);
            assert_eq!(err, Some(BAD_FORMAT));
        }

        #[tokio::test]
        async fn nothing_after_an_error() {
            let mut ev = events(vec![
                Ok(b"data: {\"error\": 1}\n\n"),
                Ok(b"data: {\"token\": \"a\"}\n\n"),
            ]);
            assert!(ev.next().await.is_err());
            assert_eq!(ev.next().await, Ok(None));
        }
    }
}

// ------------------------------------------------------------------ 推流泵（旧版 passthrough / run_stream 的共同底座）

enum Next {
    Ev(Result<Option<Item>, StreamError>),
    /// 成员已断开（响应体被丢弃）
    Closed,
}

/// 等接收端关闭；非流式（没有通道）永远不会关闭。
async fn closed(tx: &Option<mpsc::Sender<Bytes>>) {
    match tx {
        Some(tx) => tx.closed().await,
        None => std::future::pending().await,
    }
}

/// 一次文本流的现场：当日已用 token、要写给成员的通道（上游事件 `TextEvents` 单独传，它不是 `Sync`）。
struct Pump {
    state: AppState,
    ctx: ReqCtx,
    key: Key,
    model: String,
    /// 开始时今日已用的文本 token 数
    already: i64,
    /// 管理员 -1（不限）
    daily_limit: i64,
    tx: Option<mpsc::Sender<Bytes>>,
}

impl Pump {
    /// `0 <= daily_limit <= already + counted`
    fn limit_reached(&self, counted: i64) -> bool {
        self.daily_limit >= 0 && self.daily_limit <= self.already + counted
    }

    /// 读上游的下一个事件；成员一断开就不再等（旧版靠 CancelledError 打断读取）。
    async fn next(&self, events: &mut TextEvents) -> Next {
        tokio::select! {
            biased;
            _ = closed(&self.tx) => Next::Closed,
            ev = events.next() => Next::Ev(ev),
        }
    }

    /// 写一帧给成员；对方已断开返回 false。
    async fn send(&self, data: Vec<u8>) -> bool {
        match &self.tx {
            Some(tx) => tx.send(Bytes::from(data)).await.is_ok(),
            None => true,
        }
    }

    fn record_error(&self, kind: &str, message: &str) -> tokio::task::JoinHandle<()> {
        self.state.record(
            &self.ctx,
            &self.key,
            Entry::new(kind, &self.model, "error").detail(message),
        )
    }
}

fn sse_response(rx: mpsc::Receiver<Bytes>) -> Response {
    let mut rx = rx;
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
        .map(Ok::<Bytes, std::convert::Infallible>);
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    // Starlette 给 text/* 补的字符集
    h.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

// ------------------------------------------------------------------ 原生文本接口（/ai/generate-stream、/ai/generate）

/// 校验、整理之后的原生文本请求。
struct Native {
    key: Key,
    body: Value,
    model: String,
    notes: Vec<String>,
}

/// 两个原生接口共有的前半段：鉴权 → 限频 → 功能开关 → 读请求体 → 模型白名单 → 钳制参数 → 额度。
async fn native_preflight(
    state: &AppState,
    ctx: &ReqCtx,
    parts: &Parts,
    body: Body,
) -> Result<Native, ApiError> {
    let key = authenticate(state, parts, false).await?;
    check_rpm(state, &key)?;
    require_feature(state, &key, "text").await?;
    let body = read_json(&parts.headers, body, TEXT_BODY_LIMIT).await?;
    let model = body
        .get("model")
        .map_or_else(|| "?".to_string(), pyext::py_str);
    require_text_model(state, ctx, &key, &model, "text")?;
    let (body, notes, problem) = clamp_text_params(
        &Value::Object(body),
        state.env.max_text_output_tokens,
        state.env.max_input_chars,
        TEXT_INPUT_BUDGET_TOKENS,
    )
    .map_err(py500)?;
    if let Some(problem) = problem {
        state.record(
            ctx,
            &key,
            Entry::new("text", &model, "rejected").detail(problem.clone()),
        );
        return Err(GateError::new(400, problem).into());
    }
    text_quota_check(state, &key).await?;
    Ok(Native {
        key,
        body,
        model,
        notes,
    })
}

async fn generate_stream(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    respond(native_stream(state, req, permit).await)
}

async fn native_stream(state: AppState, req: Request, permit: Option<InflightPermit>) -> ApiResult {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let n = native_preflight(&state, &ctx, &parts, body).await?;
    let daily_limit = if n.key.is_admin() {
        -1
    } else {
        n.key.daily_text_tokens()
    };
    let slots = acquire_slots(&state, &n.key, false).await?;
    let already = state
        .db
        .get_counter(n.key.id(), &state.day())
        .await?
        .text_tokens;
    let body = open_stream(&state, &ctx, &n.key, "text", &n.model, &n.body).await?;

    let (tx, rx) = mpsc::channel::<Bytes>(32);
    let pump = Pump {
        state: state.clone(),
        ctx: ctx.clone(),
        key: n.key,
        model: n.model,
        already,
        daily_limit,
        tx: Some(tx),
    };
    state.tasks.spawn(ctx.scope(run_native(
        pump,
        TextEvents::new(body),
        n.notes,
        permit,
        slots,
    )));
    Ok(sse_response(rx))
}

/// 旧版 `passthrough()`：原样转发上游事件，沿途数 token，收尾时记账。
async fn run_native(
    p: Pump,
    mut events: TextEvents,
    notes: Vec<String>,
    permit: Option<InflightPermit>,
    slots: Slots,
) {
    // 在途名额和并发槽占到推流和结算都结束
    let _held = (permit, slots);
    let mut counted: i64 = 0;
    let mut hard_cut = false;
    let mut error_row = None;
    loop {
        match p.next(&mut events).await {
            Next::Closed => break,
            Next::Ev(Ok(Some(Item::Token { raw, event, .. }))) => {
                counted += 1;
                if p.limit_reached(counted) {
                    hard_cut = true;
                    p.send(DONE_FRAME.to_vec()).await;
                    break;
                }
                if !p.send(encode_sse(&raw, &event)).await {
                    break;
                }
            }
            Next::Ev(Ok(Some(Item::Done { raw, event }))) => {
                p.send(encode_sse(&raw, &event)).await;
                break;
            }
            Next::Ev(Ok(None)) => break,
            Next::Ev(Err(e)) => {
                error_row = Some(p.record_error("text", e.message));
                let data = jsondump::dumps(
                    &json!({"error": e.message, "message": e.message, "status_code": e.status}),
                );
                p.send(encode_sse(data.as_bytes(), b"error")).await;
                break;
            }
        }
    }
    // 先把 error 那条写完，再写 ok（旧版两条按创建顺序落库）
    if let Some(h) = error_row {
        let _ = h.await;
    }
    if counted > 0 {
        let d = format!(
            "{}{}",
            if hard_cut {
                "达到每日上限被截断; "
            } else {
                ""
            },
            notes.join("; ")
        );
        // d.strip("; ")：两端的分号和空格都去掉
        let detail = d.trim_matches(|c| c == ';' || c == ' ').to_string();
        p.state
            .settle_record(
                &p.ctx,
                &p.key,
                Entry {
                    tokens: counted,
                    detail,
                    ..Entry::new("text", &p.model, "ok")
                },
            )
            .await;
    }
    // 收尾顺序与旧版一致：先结束响应（成员看到流结束），再关上游连接，最后放名额和并发槽
    drop(p);
    drop(events);
}

async fn generate_text(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    respond(native_text(state, req, permit).await)
}

async fn native_text(state: AppState, req: Request, permit: Option<InflightPermit>) -> ApiResult {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let n = native_preflight(&state, &ctx, &parts, body).await?;
    let slots = acquire_slots(&state, &n.key, false).await?;
    let (s, c) = (state.clone(), ctx.clone());
    shielded(&state, &ctx, async move {
        let _held = (permit, slots);
        text_call(&s, &c, n).await
    })
    .await
}

/// 旧版 `generate_text` 的后半段：普通（非流式）补全。
async fn text_call(state: &AppState, ctx: &ReqCtx, n: Native) -> ApiResult {
    let url = text_url(state, &n.model, false);
    let mut opts = RequestOpts::post(url, &n.body);
    opts.accept = "application/json";
    let resp = match state
        .nai
        .request(opts, &Callbacks::default(), None, ctx)
        .await
    {
        Ok(r) => r,
        Err(e) if e.transport => return Err(upstream_transport(ctx, e.message)),
        Err(e) => return Err(up_to_gate(e).into()),
    };
    if resp.status != 200 {
        state.record(
            ctx,
            &n.key,
            Entry::new("text", &n.model, "error").detail(format!("upstream {}", resp.status)),
        );
        return Err(
            GateError::new(resp.status, upstream_error_message(i64::from(resp.status)))
                .from_upstream(true)
                .into(),
        );
    }
    let tokens = estimate_output_tokens(&resp.body)?;
    state.record(
        ctx,
        &n.key,
        Entry {
            tokens,
            detail: n.notes.join("; "),
            ..Entry::new("text", &n.model, "ok")
        },
    );
    Ok(media_response(resp.body, b"application/json"))
}

/// `estimate_tokens(out)`，其中 `out = (resp.json() or {}).get("output", "")`（解析失败 / 不是对象 → ""）。
/// 旧版 `estimate_tokens` 对非字符串直接 `len()`：数字 / 布尔会抛 TypeError → 500，列表 / 字典按元素个数估。
fn estimate_output_tokens(body: &[u8]) -> Result<i64, ApiError> {
    let loaded = pyloads::loads_bytes(body, Options::LOADS).ok();
    let output = loaded.as_ref().and_then(|l| match &l.value {
        Obj::Dict(d) => d.get("output"),
        _ => None,
    });
    let by_len = |n: usize| std::cmp::max(1, (n as f64 / 3.5).ceil() as i64);
    match output {
        None => Ok(0),
        Some(o) if !o.truthy() => Ok(0),
        Some(Obj::Str(s)) => Ok(estimate_tokens(s)),
        Some(Obj::List(l)) => Ok(by_len(l.len())),
        Some(Obj::Dict(d)) => Ok(by_len(d.len())),
        Some(other) => Err(InternalError(format!(
            "TypeError: object of type '{}' has no len()",
            match other {
                Obj::Bool(_) => "bool",
                Obj::Int(_) | Obj::BigInt { .. } => "int",
                _ => "float",
            }
        ))
        .into()),
    }
}

// ------------------------------------------------------------------ 语音合成

async fn generate_voice(State(state): State<AppState>, req: Request) -> Response {
    // 旧版没给语音套 limit_inflight
    respond(voice(state, req).await)
}

async fn voice(state: AppState, req: Request) -> ApiResult {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_rpm(&state, &key)?;
    require_feature(&state, &key, "voice").await?;
    if !key.is_admin() {
        state.record(
            &ctx,
            &key,
            Entry::new("voice", "", "rejected").detail("voice is admin-only"),
        );
        return Err(GateError::new(
            403,
            "语音合成目前仅限管理员 Key（它按 Anlas 计费，暂未接入额度统计）",
        )
        .into());
    }
    let body = Value::Object(read_json(&parts.headers, body, TEXT_BODY_LIMIT).await?);
    let slots = acquire_slots(&state, &key, false).await?;
    let (s, c) = (state.clone(), ctx.clone());
    shielded(&state, &ctx, async move {
        let _held = slots;
        let url = format!("{}/ai/generate-voice", s.nai.legacy_text_host());
        let resp = match s
            .nai
            .request(
                RequestOpts::post(url, &body),
                &Callbacks::default(),
                None,
                &c,
            )
            .await
        {
            Ok(r) => r,
            Err(e) if e.transport => return Err(upstream_transport(&c, e.message)),
            Err(e) => return Err(up_to_gate(e).into()),
        };
        if resp.status != 200 {
            return Err(GateError::new(
                resp.status,
                upstream_error_message(i64::from(resp.status)),
            )
            .from_upstream(true)
            .into());
        }
        let voice = body.get("voice").map_or_else(String::new, py_str);
        s.record(&c, &key, Entry::new("voice", &voice, "ok"));
        let media = resp
            .headers
            .get("content-type")
            .map_or(&b"audio/mpeg"[..], |v| v.as_bytes());
        Ok(media_response(resp.body, media))
    })
    .await
}

// ------------------------------------------------------------------ /v1/models、/v1/me

async fn v1_models() -> Response {
    ok_json(&json!({
        "object": "list",
        "data": TEXT_MODELS
            .iter()
            .map(|m| json!({"id": m, "object": "model", "owned_by": "novelai"}))
            .collect::<Vec<_>>(),
    }))
}

/// `_feature_view`：该 Key 当前各项功能是否可用（已计入全局开关）。
async fn feature_view(state: &AppState, key: &Key) -> Result<Vec<Value>, ApiError> {
    let mut view = Vec::new();
    for (name, label) in FEATURES {
        // require_feature 只在被拒时返回 Gate 错误（对应旧版 `check(...) is not None`）
        let mut on = match require_feature(state, key, name).await {
            Ok(()) => true,
            Err(ApiError::Gate(_)) => false,
            Err(e) => return Err(e),
        };
        if name == "voice" && !key.is_admin() {
            on = false;
        }
        view.push(json!({"id": name, "label": label, "on": on}));
    }
    Ok(view)
}

async fn v1_me(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    respond(me(&state, &parts).await)
}

async fn me(state: &AppState, parts: &Parts) -> ApiResult {
    let key = authenticate(state, parts, false).await?;
    let c = state.db.get_counter(key.id(), &state.day()).await?;
    let generated = state.db.generated_image_totals(Some(key.id())).await?;
    let generated_total = generated
        .iter()
        .find(|(id, _)| *id == key.id())
        .map_or(0, |(_, n)| *n);
    // get_counter 对 ≤ 0 的 anlas 给整数 0；float(0) 之后仍是 0.0
    let anlas_today = if c.anlas > 0.0 { c.anlas } else { 0.0 };
    let anlas_month = state.db.month_anlas(key.id(), &state.month()).await?;
    let features = feature_view(state, &key).await?;
    // 3.0 去掉的字段（docs/v3-deviations.md #61 / #66）：`daily_images_base`（保底）、`anlas_auto`（V5 续杯）
    Ok(ok_json(&json!({
        "name": key.name(),
        "is_admin": key.is_admin(),
        "generated_images_total": generated_total,
        "today": {
            "images": c.images,
            "daily_images": key.get("daily_images"),
            "legacy_free_images_today": c.legacy_free_images,
            "anlas_today": pyc::round(anlas_today, 2),
            "daily_anlas": key.get("daily_anlas"),
            "v5_today": c.v5,
            "daily_v5": key.get("daily_v5"),
            "image_model_scope": key.get("image_model_scope"),
            "anlas_month": pyc::round(anlas_month, 2),
            "monthly_anlas": key.get("monthly_anlas"),
            "text_tokens": c.text_tokens,
            "daily_text_tokens": key.get("daily_text_tokens"),
            "requests": c.requests,
        },
        "expires_at": key.get("expires_at"),
        "inactivity_delete_days": if key.is_admin() { 0 } else { state.env.key_inactivity_delete_days },
        "features": features,
    })))
}

// ------------------------------------------------------------------ OpenAI 兼容聊天

async fn v1_chat(State(state): State<AppState>, req: Request) -> Response {
    let permit = match limit_inflight(&state, req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    respond(chat(state, req, permit).await)
}

/// `"\n".join(str(m.get("content", "")) for m in msgs if isinstance(m, dict)).strip()`，
/// 其中 `msgs = body.get("messages") or []`。字符串 / 字典也能迭代（只是里面没有 dict），数字 / True 不能迭代（TypeError → 500）。
fn chat_input(messages: Option<&Value>) -> Result<String, ApiError> {
    let Some(messages) = messages.filter(|v| truthy(v)) else {
        return Ok(String::new());
    };
    let texts: Vec<String> = match messages {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_object)
            .map(|m| m.get("content").map_or_else(String::new, py_str))
            .collect(),
        Value::String(_) | Value::Object(_) => Vec::new(),
        _ => {
            return Err(InternalError("TypeError: object is not iterable".into()).into());
        }
    };
    Ok(pyc::strip(&texts.join("\n")).to_string())
}

/// `float(body.get(name, 0.9) or 0.9)`
fn float_or_default(v: Option<&Value>) -> Result<f64, ApiError> {
    let Some(v) = v.filter(|v| truthy(v)) else {
        return Ok(0.9);
    };
    match v {
        Value::Bool(_) | Value::Number(_) => Ok(pyval::number_like(v).unwrap_or(0.9)),
        Value::String(s) => pyc::parse_float(s).ok_or_else(|| {
            InternalError(format!(
                "ValueError: could not convert string to float: {}",
                pyext::repr_str(s)
            ))
            .into()
        }),
        other => Err(InternalError(format!(
            "TypeError: float() argument must be a string or a real number, not '{}'",
            pyext::type_name(other)
        ))
        .into()),
    }
}

/// 一个 OpenAI 流式分块（`json.dumps(..., ensure_ascii=False)`：默认分隔符带空格）。
fn openai_chunk(state: &AppState, model: &str, content: &str, finish: Option<&str>) -> Vec<u8> {
    let delta = if content.is_empty() {
        json!({})
    } else {
        json!({"content": content})
    };
    let body = jsondump::dumps(&json!({
        "id": "chatcmpl-naigate", "object": "chat.completion.chunk",
        "created": state.now() as i64, "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }));
    format!("data: {body}\n\n").into_bytes()
}

async fn chat(state: AppState, req: Request, permit: Option<InflightPermit>) -> ApiResult {
    let (parts, body) = req.into_parts();
    let ctx = ctx_of(&parts);
    let key = authenticate(&state, &parts, false).await?;
    check_rpm(&state, &key)?;
    require_feature(&state, &key, "text").await?;
    let body = read_json(&parts.headers, body, TEXT_BODY_LIMIT).await?;
    let want_stream = body.get("stream").is_some_and(truthy);

    let input_text = chat_input(body.get("messages"))?;
    let model_in = body.get("model").map_or_else(String::new, py_str);
    if let Some(why) = retired_reason(&model_in) {
        // 已知上游不认的名字：直接说清楚，别让上游用 403 回复
        return Err(GateError::new(400, retired_message(&model_in, why)).into());
    }
    let model = if TEXT_MODELS.contains(&model_in.as_str()) {
        model_in
    } else {
        DEFAULT_CHAT_MODEL.to_string()
    };
    let max_tokens = pyext::int_or(body.get("max_tokens"), 150, 150).map_err(py500)?;
    let max_length = std::cmp::min(
        std::cmp::max(1, max_tokens),
        i128::from(state.env.max_text_output_tokens),
    );
    let temperature = float_or_default(body.get("temperature"))?;
    let top_p = float_or_default(body.get("top_p"))?;
    let payload = json!({
        "input": input_text,
        "model": model,
        "parameters": {
            "max_length": pyext::json_int(max_length),
            "min_length": 1,
            "temperature": pyval::float(temperature),
            "top_p": pyval::float(top_p),
            "top_k": 40,
            "use_string": true,
        },
    });
    let (nai_body, _, problem) = clamp_text_params(
        &payload,
        state.env.max_text_output_tokens,
        state.env.max_input_chars,
        TEXT_INPUT_BUDGET_TOKENS,
    )
    .map_err(py500)?;
    if let Some(problem) = problem {
        return Err(GateError::new(400, problem).into());
    }
    text_quota_check(&state, &key).await?;

    let daily_limit = if key.is_admin() {
        -1
    } else {
        key.daily_text_tokens()
    };
    let slots = acquire_slots(&state, &key, false).await?;
    let already = state
        .db
        .get_counter(key.id(), &state.day())
        .await?
        .text_tokens;
    require_finite(&[temperature, top_p])?;
    let upstream = open_stream(&state, &ctx, &key, "chat", &model, &nai_body).await?;

    let (tx, rx) = if want_stream {
        let (tx, rx) = mpsc::channel::<Bytes>(32);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let pump = Pump {
        state: state.clone(),
        ctx: ctx.clone(),
        key,
        model,
        already,
        daily_limit,
        tx,
    };
    let events = TextEvents::new(upstream);
    if let Some(rx) = rx {
        state
            .tasks
            .spawn(ctx.scope(run_chat_stream(pump, events, permit, slots)));
        return Ok(sse_response(rx));
    }
    shielded(&state, &ctx.clone(), async move {
        let _held = (permit, slots);
        chat_collect(pump, events, &input_text).await
    })
    .await
}

#[derive(Default)]
struct ChatRun {
    counted: i64,
    collected: String,
    error: Option<StreamError>,
}

/// 旧版 `run_stream()`：把上游 token 译成 OpenAI 分块（流式）或收集起来（非流式）。
/// 记账由调用方在返回之后做（对应旧版的 `finally`）。
async fn pump_chat(
    p: &Pump,
    events: &mut TextEvents,
    want_stream: bool,
) -> (ChatRun, Option<tokio::task::JoinHandle<()>>) {
    let mut run = ChatRun::default();
    let mut error_row = None;
    if want_stream && !p.send(openai_chunk(&p.state, &p.model, "", None)).await {
        return (run, None);
    }
    loop {
        match p.next(events).await {
            Next::Closed => return (run, None),
            Next::Ev(Ok(Some(Item::Token { token, .. }))) => {
                if token.is_empty() {
                    continue;
                }
                run.counted += 1;
                if p.limit_reached(run.counted) {
                    break;
                }
                if want_stream {
                    if !p.send(openai_chunk(&p.state, &p.model, &token, None)).await {
                        return (run, None);
                    }
                } else {
                    run.collected.push_str(&token);
                }
            }
            Next::Ev(Ok(Some(Item::Done { .. }))) | Next::Ev(Ok(None)) => break,
            Next::Ev(Err(e)) => {
                error_row = Some(p.record_error("chat", e.message));
                if want_stream {
                    let data = jsondump::dumps(
                        &json!({"error": {"message": e.message, "status": e.status}}),
                    );
                    p.send(encode_sse(data.as_bytes(), b"")).await;
                    p.send(DONE_FRAME.to_vec()).await;
                }
                run.error = Some(e);
                return (run, error_row);
            }
        }
    }
    if want_stream {
        if !p
            .send(openai_chunk(&p.state, &p.model, "", Some("stop")))
            .await
        {
            return (run, None);
        }
        p.send(DONE_FRAME.to_vec()).await;
    }
    (run, error_row)
}

/// 收尾记账：先等 error 那条写完，再写 ok（按创建顺序落库）。
async fn settle_chat(p: &Pump, run: &ChatRun, error_row: Option<tokio::task::JoinHandle<()>>) {
    if let Some(h) = error_row {
        let _ = h.await;
    }
    if run.counted > 0 {
        p.state
            .settle_record(
                &p.ctx,
                &p.key,
                Entry {
                    tokens: run.counted,
                    ..Entry::new("chat", &p.model, "ok")
                },
            )
            .await;
    }
}

async fn run_chat_stream(
    p: Pump,
    mut events: TextEvents,
    permit: Option<InflightPermit>,
    slots: Slots,
) {
    let _held = (permit, slots);
    let (run, error_row) = pump_chat(&p, &mut events, true).await;
    settle_chat(&p, &run, error_row).await;
    // 先结束响应，再关上游连接，最后放名额和并发槽（与旧版一致）
    drop(p);
    drop(events);
}

async fn chat_collect(p: Pump, mut events: TextEvents, input_text: &str) -> ApiResult {
    let (run, error_row) = pump_chat(&p, &mut events, false).await;
    settle_chat(&p, &run, error_row).await;
    if let Some(e) = run.error {
        return Err(GateError::new(e.status, e.message)
            .from_upstream(true)
            .into());
    }
    let prompt = estimate_tokens(input_text);
    let completion = std::cmp::max(run.counted, 1);
    Ok(ok_json(&json!({
        "id": "chatcmpl-naigate", "object": "chat.completion",
        "created": p.state.now() as i64, "model": p.model,
        "choices": [{"index": 0,
                     "message": {"role": "assistant", "content": run.collected},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
                  "total_tokens": prompt + completion},
    })))
}
