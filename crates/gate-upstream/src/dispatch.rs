//! 一次上游请求的完整流程：选号 → 等出图槽位 → V5 计费确认 → 发出 → 按状态码处理（429 / 403 / 401）→ 结算。
//!
//! 取消安全用 Drop 守卫实现：请求 future 在任何点被丢弃，V5 预留、计数、出图后静默与并发槽都会照常结算
//! （旧版用 `asyncio.shield` + `CancelScope(shield=True)` 达到同样效果）。

use crate::client::{Callbacks, Inner, NaiClient, RequestOpts, UpResponse};
use crate::error::{Transport, UpstreamError};
use crate::hooks::{Allowance, BoxFuture, Hooks, Observer};
use crate::http;
use crate::limiter::Permit;
use crate::pool::{self, Token};
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use wreq::header::HeaderMap;

/// 请求进行中的结算守卫：正常路径调用 `finish().await`；被取消（Drop）时在后台补做同样的结算。
pub(crate) struct InFlight<H: Hooks> {
    inner: Arc<Inner<H>>,
    token: Arc<Token>,
    succeeded: bool,
    v5_free: bool,
    image_count: usize,
    send_started: bool,
    permit: Option<Permit>,
    done: bool,
}

impl<H: Hooks> InFlight<H> {
    fn new(inner: Arc<Inner<H>>, token: Arc<Token>, v5_free: bool, image_count: usize) -> Self {
        Self {
            inner,
            token,
            succeeded: false,
            v5_free,
            image_count,
            send_started: false,
            permit: None,
            done: false,
        }
    }

    async fn finish(mut self) {
        self.done = true;
        let permit = self.permit.take();
        settle(
            &self.inner,
            &self.token,
            self.succeeded,
            self.v5_free,
            self.image_count,
            self.send_started,
        )
        .await;
        drop(permit); // 先结算（含出图后静默）再放槽：下一位一定能看到新的 image_next_at
    }
}

impl<H: Hooks> Drop for InFlight<H> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let inner = self.inner.clone();
        let token = self.token.clone();
        let permit = self.permit.take();
        let (succeeded, v5_free, image_count, send_started) = (
            self.succeeded,
            self.v5_free,
            self.image_count,
            self.send_started,
        );
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                settle(
                    &inner,
                    &token,
                    succeeded,
                    v5_free,
                    image_count,
                    send_started,
                )
                .await;
                drop(permit);
            });
        }
    }
}

async fn settle<H: Hooks>(
    inner: &Inner<H>,
    token: &Arc<Token>,
    succeeded: bool,
    v5_free: bool,
    image_count: usize,
    send_started: bool,
) {
    inner
        .pool
        .finish_v5_reservation(&inner.hooks, token, succeeded, v5_free)
        .await;
    if succeeded {
        record_images(&inner.hooks, token, image_count).await;
    }
    // 出图后静默只对真正发往上游的请求生效
    if send_started {
        inner.scheduler.after_request(token, &inner.hooks);
    }
}

/// 按上游实际成功的张数记账（失败、拒绝、限流不计）。
pub(crate) async fn record_images(hooks: &dyn Hooks, token: &Token, image_count: usize) {
    if image_count > 0 {
        let day = hooks.day();
        if let Err(e) = hooks
            .bump_upstream_images(&token.token_id, &day, image_count as i64)
            .await
        {
            tracing::error!(error = %e, "记录上游出图计数失败");
        }
    }
}

/// `parameters.n_samples`（至少 1）：限速计数按张数记，批量请求才绕不过突发窗口。
pub fn request_images(body: Option<&Value>) -> i64 {
    let n = body
        .and_then(|b| b.get("parameters"))
        .and_then(|p| p.get("n_samples"))
        .cloned()
        .unwrap_or(Value::Null);
    let parsed = match &n {
        Value::Null => Some(1),
        v if !gate_core::pyval::truthy(v) => Some(1),
        Value::Number(x) if !x.is_f64() => x.as_i64(),
        Value::Number(x) => x.as_f64().and_then(gate_core::pyc::int_of),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => gate_core::pyc::parse_int(s),
        _ => None,
    };
    parsed.unwrap_or(1).max(1)
}

/// Retry-After 解析；旧版遇到超长数字 / 越界日期会抛异常导致冷却不生效，这里按默认 20 秒。
fn retry_after_or_default(headers: &HeaderMap, now: f64) -> f64 {
    let header = headers.get("retry-after").and_then(|v| v.to_str().ok());
    gate_core::upstream_policy::parse_retry_after(header, now).unwrap_or(20.0)
}

enum Attempt {
    Done(Result<UpResponse, UpstreamError>),
    Retry,
}

const QUEUE_TIMEOUT: &str = "上游图片任务排队超时，请稍后再试";
const RATE_LIMITED_IMAGE: &str = "上游限流(429)，全站图片生成已进入冷却";
const MODEL_403: &str = "上游不认识这个模型：请换用本站白名单里的模型";
const ACCOUNT_403: &str = "上游拒绝了请求（HTTP 403），该账号已临时隔离保护";
const TOKEN_401: &str = "上游令牌已失效（401），请站长更换 NovelAI Token";

impl<H: Hooks> NaiClient<H> {
    /// 普通请求（图片工具 / 补全 / 非流式出图）。出图请求不自动重试；普通请求只对明确的 429 换 Token 一次。
    pub async fn request(
        &self,
        mut o: RequestOpts<'_>,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
    ) -> Result<UpResponse, UpstreamError> {
        let inner = &self.inner;
        let mut attempts = 0;
        while attempts < 2 {
            attempts += 1;
            let Some(token) = inner
                .pool
                .pick(
                    &inner.hooks,
                    o.requires_anlas,
                    o.v5_free,
                    o.image_lane && o.wait_for_image_slot,
                )
                .await?
            else {
                return Err(self.unavailable(o.requires_anlas, o.v5_free).await);
            };
            let mut fl = InFlight::new(inner.clone(), token.clone(), o.v5_free, o.image_count);
            let outcome = self
                .request_once(&mut o, cb, allowance, obs, &token, &mut fl)
                .await;
            fl.finish().await;
            match outcome {
                Attempt::Done(r) => return r,
                Attempt::Retry => continue,
            }
        }
        Err(UpstreamError::new(429, "上游限流(429)，请降低频率后重试"))
    }

    async fn request_once(
        &self,
        o: &mut RequestOpts<'_>,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
        token: &Arc<Token>,
        fl: &mut InFlight<H>,
    ) -> Attempt {
        let inner = &self.inner;
        let hooks: &dyn Hooks = &inner.hooks;
        let mut response_status: Option<u16> = None;
        if o.image_lane
            && !inner
                .scheduler
                .acquire(token, hooks, o.queue_timeout, &mut fl.permit)
                .await
        {
            return Attempt::Done(Err(UpstreamError::new(429, QUEUE_TIMEOUT)));
        }
        if o.v5_free
            && let (Some(resolve), Some(a)) = (cb.resolve_v5_cost.as_ref(), allowance)
        {
            match resolve_v5_cost(token, a, resolve).await {
                Ok(true) => {
                    inner
                        .pool
                        .finish_v5_reservation(hooks, token, false, true)
                        .await;
                    o.v5_free = false;
                    fl.v5_free = false;
                    o.requires_anlas = true;
                }
                Ok(false) => {}
                Err(e) => return Attempt::Done(Err(e)),
            }
        }
        // 与 set_admin_enabled 串行：之后发出的请求一定能看到新的启停状态（请求本身不持锁）
        drop(token.dispatch_lock.lock().await);
        if !token.with(|f| f.admin_enabled) {
            return Attempt::Retry;
        }
        if let Some(check) = cb.before_dispatch.as_ref()
            && let Err(e) = check()
        {
            return Attempt::Done(Err(e));
        }
        fl.send_started = true;
        obs.sent();
        if o.image_lane && o.wait_for_image_slot && hooks.has_guard() {
            hooks.record_start(&token.token_id, request_images(o.json));
        }
        let req = inner.http.build(
            token.profile_index(),
            &token.token,
            o.method.clone(),
            &o.url,
            o.json,
            o.accept,
        );
        let sent = async {
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            response_status = Some(status);
            let headers = resp.headers().clone();
            match o.max_response_bytes {
                None => Ok::<_, wreq::Error>(Ok(UpResponse {
                    status,
                    headers,
                    body: resp.bytes().await?,
                })),
                Some(cap) => match http::read_capped(resp, cap).await? {
                    None => {
                        let uncertain =
                            o.image_lane && (status == 200 || status == 201 || status >= 500);
                        Ok(Err(UpstreamError::uncertain(
                            502,
                            "上游图片工具结果过大",
                            uncertain,
                        )))
                    }
                    Some(body) => {
                        let mut headers = headers;
                        headers.remove(wreq::header::CONTENT_ENCODING);
                        headers.remove(wreq::header::CONTENT_LENGTH);
                        Ok(Ok(UpResponse {
                            status,
                            headers,
                            body,
                        }))
                    }
                },
            }
        };
        let result = if o.max_response_bytes.is_some() {
            match tokio::time::timeout(Duration::from_secs(300), sent).await {
                Ok(r) => r.map_err(Transport::Http),
                Err(_) => Err(Transport::Timeout),
            }
        } else {
            sent.await.map_err(Transport::Http)
        };
        let resp = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Attempt::Done(Err(e)),
            Err(t) => {
                if !o.image_lane {
                    let mut e = UpstreamError::new(502, format!("上游请求失败：{t}"));
                    e.transport = true;
                    return Attempt::Done(Err(e));
                }
                // 已发送请求的读写故障可能产生扣款；明确的 4xx、连接没建立起来的除外
                let uncertain = fl.send_started
                    && response_status.is_none_or(|s| s == 200 || s == 201 || s >= 500)
                    && t.after_send();
                return Attempt::Done(Err(UpstreamError::uncertain(
                    502,
                    "上游图片请求连接中断或超时，未记费；请勿自动重试",
                    uncertain,
                )));
            }
        };
        obs.status(resp.status);
        if (resp.status == 200 || resp.status == 201)
            && o.image_count > 0
            && !hooks
                .validate_image_result(resp.body.clone(), o.image_count)
                .await
        {
            return Attempt::Done(Err(UpstreamError::uncertain(
                502,
                "上游未返回完整有效的图片结果",
                true,
            )));
        }
        let succeeded = resp.status == 200 || resp.status == 201;
        fl.succeeded = succeeded;
        if resp.status == 429 {
            let retry_after = retry_after_or_default(&resp.headers, hooks.now());
            token.mark_rate_limited(hooks, retry_after);
            if let Some(cb) = cb.on_rate_limited.as_ref() {
                cb(retry_after).await;
            }
            if o.image_lane {
                return Attempt::Done(Err(UpstreamError::new(429, RATE_LIMITED_IMAGE)));
            }
            return Attempt::Retry;
        }
        if resp.status == 403 && o.image_lane {
            // 「模型不存在」也是 403：不算账号被拒
            let peek = String::from_utf8_lossy(&resp.body[..resp.body.len().min(300)]).into_owned();
            if gate_core::upstream_policy::is_model_level_403(&peek) {
                return Attempt::Done(Err(UpstreamError::new(400, MODEL_403)));
            }
            token.mark_forbidden(hooks);
            return Attempt::Done(Err(UpstreamError::new(403, ACCOUNT_403)));
        }
        pool::warn_account(hooks, resp.status);
        if resp.status == 401 {
            token.mark_unauthorized();
            pool::event_401(hooks);
            return Attempt::Done(Err(UpstreamError::new(502, TOKEN_401)));
        }
        if succeeded {
            token.mark_ok(hooks);
        }
        Attempt::Done(Ok(resp))
    }

    /// 打开图片流（不重试）。返回的 [`ImageStream`] 读完后调用 `finish(completed_images)`；
    /// 中途丢弃也会按已确认的张数结算。
    #[allow(clippy::too_many_arguments)] // 与旧版 image_stream 的参数一一对应
    pub async fn image_stream(
        &self,
        url: &str,
        body: &Value,
        requires_anlas: bool,
        v5_free: bool,
        queue_timeout: f64,
        cb: &Callbacks<'_>,
        allowance: Option<&dyn Allowance>,
        obs: &dyn Observer,
    ) -> Result<ImageStream<H>, UpstreamError> {
        let inner = &self.inner;
        let hooks: &dyn Hooks = &inner.hooks;
        let Some(token) = inner
            .pool
            .pick(hooks, requires_anlas, v5_free, true)
            .await?
        else {
            return Err(self.unavailable(requires_anlas, v5_free).await);
        };
        let mut fl = InFlight::new(inner.clone(), token.clone(), v5_free, 0);
        if !inner
            .scheduler
            .acquire(&token, hooks, queue_timeout, &mut fl.permit)
            .await
        {
            fl.finish().await;
            return Err(UpstreamError::new(429, QUEUE_TIMEOUT));
        }
        if v5_free && let (Some(resolve), Some(a)) = (cb.resolve_v5_cost.as_ref(), allowance) {
            match resolve_v5_cost(&token, a, resolve).await {
                Ok(true) => {
                    // 与旧版一致：图片流只把这次改成「非免费 V5」，不改 requires_anlas
                    inner
                        .pool
                        .finish_v5_reservation(hooks, &token, false, true)
                        .await;
                    fl.v5_free = false;
                }
                Ok(false) => {}
                Err(e) => {
                    fl.finish().await;
                    return Err(e);
                }
            }
        }
        let accept = if body
            .get("parameters")
            .and_then(|p| p.get("stream"))
            .and_then(Value::as_str)
            == Some("msgpack")
        {
            "application/x-msgpack"
        } else {
            "text/event-stream"
        };
        let req = inner.http.build(
            token.profile_index(),
            &token.token,
            wreq::Method::POST,
            url,
            Some(body),
            accept,
        );
        // 与 set_admin_enabled 串行（只在检查启停时持锁，请求本身不持锁）
        drop(token.dispatch_lock.lock().await);
        if !token.with(|f| f.admin_enabled) {
            fl.finish().await;
            return Err(UpstreamError::new(503, "上游令牌已停用，未发送生图"));
        }
        if let Some(check) = cb.before_dispatch.as_ref()
            && let Err(e) = check()
        {
            fl.finish().await;
            return Err(e);
        }
        if let Some(on) = cb.on_dispatch.as_ref() {
            on();
        }
        fl.send_started = true;
        obs.sent();
        if hooks.has_guard() {
            hooks.record_start(&token.token_id, request_images(Some(body)));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let uncertain = fl.send_started && Transport::Http(e).after_send();
                fl.finish().await;
                return Err(UpstreamError::uncertain(
                    502,
                    "上游图片流连接中断，请检查任务结果后再决定是否重试",
                    uncertain,
                ));
            }
        };
        let status = resp.status().as_u16();
        obs.status(status);
        let err = if status == 429 {
            let ra = retry_after_or_default(resp.headers(), hooks.now());
            token.mark_rate_limited(hooks, ra);
            if let Some(cb) = cb.on_rate_limited.as_ref() {
                cb(ra).await;
            }
            Some(UpstreamError::new(429, RATE_LIMITED_IMAGE))
        } else if status == 403 {
            let peek = http::peek_body(resp).await;
            if gate_core::upstream_policy::is_model_level_403(&peek) {
                fl.finish().await;
                return Err(UpstreamError::new(400, MODEL_403));
            }
            token.mark_forbidden(hooks);
            fl.finish().await;
            return Err(UpstreamError::new(403, ACCOUNT_403));
        } else {
            pool::warn_account(hooks, status);
            if status == 401 {
                token.mark_unauthorized();
                pool::event_401(hooks);
                Some(UpstreamError::new(502, TOKEN_401))
            } else if status != 200 && status != 201 {
                let s = if (400..=599).contains(&status) {
                    status
                } else {
                    502
                };
                Some(UpstreamError::uncertain(
                    s,
                    format!("上游图片流请求失败（HTTP {s}）"),
                    status >= 500,
                ))
            } else {
                None
            }
        };
        if let Some(e) = err {
            fl.finish().await;
            return Err(e);
        }
        Ok(ImageStream {
            response: Some(resp),
            flight: Some(fl),
            token,
            completed_images: 0,
        })
    }
}

/// V5 是否已按 Anlas 计费的最终确认：先查官方免费额度（与停用串行），再交给调用方的回调复核。
async fn resolve_v5_cost(
    token: &Token,
    allowance: &dyn Allowance,
    callback: &(dyn Fn(bool) -> BoxFuture<'static, Result<(), UpstreamError>> + Send + Sync + '_),
) -> Result<bool, UpstreamError> {
    let exhausted = {
        let _d = token.dispatch_lock.lock().await;
        if !token.with(|f| f.admin_enabled) {
            return Err(UpstreamError::new(
                503,
                "上游令牌已停用，未查询额度或发送生图",
            ));
        }
        allowance
            .resolve(token)
            .await
            .map_err(|m| UpstreamError::new(503, m))?
    };
    if exhausted && !token.with(|f| f.allow_anlas) {
        return Err(UpstreamError::new(
            503,
            "该上游账号 V5 额度已耗尽，且未允许使用 Anlas；未发送生图",
        ));
    }
    callback(exhausted).await?;
    Ok(exhausted)
}

/// 已打开的上游图片流。调用方逐块读取，确认完整的最终图片后累加 `completed_images`，
/// 最后调用 [`ImageStream::finish`]；被丢弃时按已确认张数在后台结算。
pub struct ImageStream<H: Hooks> {
    response: Option<wreq::Response>,
    flight: Option<InFlight<H>>,
    token: Arc<Token>,
    pub completed_images: usize,
}

impl<H: Hooks> ImageStream<H> {
    pub fn headers(&self) -> Option<&HeaderMap> {
        self.response.as_ref().map(|r| r.headers())
    }

    /// 取走正文字节流（只能取一次）。已按 Content-Encoding 解压，边收边给，不整包缓冲。
    pub fn take_body(
        &mut self,
    ) -> Option<impl futures::Stream<Item = Result<Bytes, wreq::Error>> + use<H>> {
        self.response.take().map(|r| r.bytes_stream())
    }

    /// 结算：有确认的图就记成功并标记账号健康。
    pub async fn finish(mut self) {
        self.response = None;
        if let Some(mut fl) = self.flight.take() {
            let count = self.completed_images;
            fl.succeeded = count > 0;
            fl.image_count = count;
            let inner = fl.inner.clone();
            fl.finish().await;
            if count > 0 {
                self.token.mark_ok(&inner.hooks);
            }
        }
    }
}

impl<H: Hooks> Drop for ImageStream<H> {
    fn drop(&mut self) {
        if let Some(mut fl) = self.flight.take() {
            let count = self.completed_images;
            fl.succeeded = count > 0;
            fl.image_count = count;
            if count > 0 {
                self.token.mark_ok(&fl.inner.hooks);
            }
            drop(fl); // InFlight 的 Drop 在后台完成结算
        }
    }
}
