//! 每个请求的上下文：请求编号、计时（排队 / 上游耗时）、客户端名、上游状态码、当前 Key、是否已记日志。
//!
//! 旧版用 ContextVar + 共享 dict（`request_timing.py`、`core/context.py`），子任务里写的值外层也能看到。
//! 这里用 `Arc<Mutex<..>>` 放进请求扩展，同时用 tokio task-local 让错误响应等「拿不到 Request 的地方」
//! 也能读到请求编号；派生出去的任务只要带上 `ReqCtx` 的克隆，同样能写回。

use std::sync::{Arc, Mutex};
use std::time::Instant;

tokio::task_local! {
    static CURRENT: ReqCtx;
}

#[derive(Clone)]
pub struct ReqCtx(Arc<Mutex<Inner>>);

impl std::fmt::Debug for ReqCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReqCtx").field(&self.rid()).finish()
    }
}

struct Inner {
    rid: String,
    t0: Instant,
    sent: Option<Instant>,
    client: String,
    up_status: u16,
    logged: bool,
    model: String,
    key_id: Option<i64>,
    key_name: String,
    src: String,
    /// 第一次发往上游时调用一次（实时图把这张图从「排队」移到「生成中」）。
    on_sent: Option<Box<dyn FnOnce() + Send>>,
}

/// `snapshot()`：写进用量日志的计时字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    pub wait_ms: i64,
    pub dur_ms: i64,
    pub client: String,
    pub up_status: u16,
    pub rid: String,
    pub src: String,
}

impl ReqCtx {
    pub fn new(client: String) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            rid: new_rid(),
            t0: Instant::now(),
            sent: None,
            client,
            up_status: 0,
            logged: false,
            model: String::new(),
            key_id: None,
            key_name: String::new(),
            src: String::new(),
            on_sent: None,
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.0.lock().expect("reqctx"))
    }

    pub fn rid(&self) -> String {
        self.with(|i| i.rid.clone())
    }

    /// 第一次真正发往上游时调用；重试不覆盖。
    pub fn mark_sent(&self) {
        let hook = self.with(|i| {
            if i.sent.is_none() {
                i.sent = Some(Instant::now());
                i.on_sent.take()
            } else {
                None
            }
        });
        if let Some(h) = hook {
            h();
        }
    }

    /// 登记「第一次发往上游」的回调（只保留最后一次登记的）。
    pub fn on_sent(&self, hook: impl FnOnce() + Send + 'static) {
        self.with(|i| i.on_sent = Some(Box::new(hook)));
    }

    /// 来源网络打码标签（写进用量日志）。
    pub fn set_source(&self, label: &str) {
        let label = label.to_string();
        self.with(|i| i.src = label);
    }

    /// 无明确自报 UA 的客户端，按请求体参数指纹识别（小白盒、dsh-tavern-image 等）。
    pub fn refine_client_from_payload(&self, body: &serde_json::Value) {
        self.with(|i| {
            if let Some(c) = gate_core::request_timing::refine_client_from_payload(&i.client, body)
            {
                i.client = c;
            }
        });
    }

    pub fn was_sent(&self) -> bool {
        self.with(|i| i.sent.is_some())
    }

    pub fn mark_status(&self, code: u16) {
        self.with(|i| i.up_status = code);
    }

    pub fn mark_logged(&self) {
        self.with(|i| i.logged = true);
    }

    pub fn logged(&self) -> bool {
        self.with(|i| i.logged)
    }

    /// 记下本请求的模型（截到 80 个码点），被拒的请求也能按模型统计。
    pub fn set_model(&self, model: &str) {
        let m = gate_core::pyc::take_chars(model, 80).to_string();
        self.with(|i| i.model = m);
    }

    pub fn model(&self) -> String {
        self.with(|i| i.model.clone())
    }

    pub fn set_key(&self, id: i64, name: &str) {
        let name = name.to_string();
        self.with(|i| {
            i.key_id = Some(id);
            i.key_name = name;
        });
    }

    pub fn key(&self) -> Option<(i64, String)> {
        self.with(|i| i.key_id.map(|id| (id, i.key_name.clone())))
    }

    pub fn client(&self) -> String {
        self.with(|i| i.client.clone())
    }

    pub fn set_client(&self, client: String) {
        self.with(|i| i.client = client);
    }

    /// wait_ms：收到请求到发往上游；dur_ms：上游耗时（没发到上游为 0）。
    pub fn snapshot(&self) -> Timing {
        self.with(|i| {
            let now = Instant::now();
            let (wait, dur) = match i.sent {
                None => (now - i.t0, std::time::Duration::ZERO),
                Some(s) => (s - i.t0, now - s),
            };
            Timing {
                wait_ms: wait.as_millis() as i64,
                dur_ms: dur.as_millis() as i64,
                client: i.client.clone(),
                up_status: i.up_status,
                rid: i.rid.clone(),
                src: i.src.clone(),
            }
        })
    }

    /// 在这个上下文里运行一个 future（task-local 生效）。
    pub async fn scope<F: std::future::Future>(self, fut: F) -> F::Output {
        CURRENT.scope(self, fut).await
    }
}

/// 当前请求的上下文（不在请求里时为 None）。
pub fn current() -> Option<ReqCtx> {
    CURRENT.try_with(Clone::clone).ok()
}

/// 当前请求编号（不在请求里时为空串）。
pub fn current_rid() -> String {
    current().map(|c| c.rid()).unwrap_or_default()
}

/// 8 位十六进制（`secrets.token_hex(4)`）。
fn new_rid() -> String {
    let n: u32 = rand::random();
    format!("{n:08x}")
}

/// 上游观察者：把 gate-upstream 的「已发送 / 状态码」写回请求上下文。
impl gate_upstream::client::Observer for ReqCtx {
    fn sent(&self) {
        self.mark_sent();
    }
    fn status(&self, code: u16) {
        self.mark_status(code);
    }
}
