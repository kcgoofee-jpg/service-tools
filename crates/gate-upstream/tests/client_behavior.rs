//! 上游客户端行为测试（对假上游）：对照 app/nai.py 的分支逐条验证。

use bytes::Bytes;
use gate_testkit::{MockUpstream, Reply};
use gate_upstream::client::{
    BoxFuture, Callbacks, Config, HookResult, Hooks, NaiClient, NoObserver, RequestOpts,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Clone)]
struct TestHooks(Arc<Inner>);

#[derive(Default)]
struct Inner {
    v5: Mutex<HashMap<String, i64>>,
    images: Mutex<HashMap<String, i64>>,
    events: Mutex<Vec<String>>,
    starts: AtomicUsize,
    block: Mutex<Option<(String, String)>>,
}

impl Hooks for TestHooks {
    fn day(&self) -> String {
        "2026-10-14".into()
    }
    fn now(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }
    fn event(&self, kind: &str, _message: &str, _cooldown: f64) {
        self.0.events.lock().unwrap().push(kind.to_string());
    }
    fn has_guard(&self) -> bool {
        true
    }
    fn token_block_reason<'a>(
        &'a self,
        _t: &'a str,
        _d: &'a str,
    ) -> BoxFuture<'a, HookResult<Option<(String, String)>>> {
        let r = self.0.block.lock().unwrap().clone();
        Box::pin(async move { Ok(r) })
    }
    fn record_start(&self, _t: &str, images: i64) {
        self.0.starts.fetch_add(images as usize, Ordering::SeqCst);
    }
    fn jitter(&self) -> f64 {
        0.0
    }
    fn upstream_v5_counter<'a>(
        &'a self,
        t: &'a str,
        _d: &'a str,
    ) -> BoxFuture<'a, HookResult<i64>> {
        let n = *self.0.v5.lock().unwrap().get(t).unwrap_or(&0);
        Box::pin(async move { Ok(n) })
    }
    fn bump_upstream_v5<'a>(&'a self, t: &'a str, _d: &'a str) -> BoxFuture<'a, HookResult<()>> {
        *self.0.v5.lock().unwrap().entry(t.to_string()).or_insert(0) += 1;
        Box::pin(async { Ok(()) })
    }
    fn bump_upstream_images<'a>(
        &'a self,
        t: &'a str,
        _d: &'a str,
        n: i64,
    ) -> BoxFuture<'a, HookResult<()>> {
        *self
            .0
            .images
            .lock()
            .unwrap()
            .entry(t.to_string())
            .or_insert(0) += n;
        Box::pin(async { Ok(()) })
    }
    fn validate_image_result(&self, body: Bytes, _expected: usize) -> BoxFuture<'static, bool> {
        Box::pin(async move { body.starts_with(b"PK") })
    }
}

fn config(url: &str, interval: f64) -> Config {
    Config {
        image_host: url.into(),
        proxy: None,
        custom_user_agent: None,
        http2: false,
        post_jitter_min: 1.0,
        post_jitter_max: 3.0,
        single_slot_enforced: true,
        image_min_interval: interval,
    }
}

const T1: &str = "pst-aaaaaaaaaaaaaaaaaaaa";
const T2: &str = "pst-bbbbbbbbbbbbbbbbbbbb";

fn image_body(n: i64) -> serde_json::Value {
    json!({"input": "1girl", "model": "nai-diffusion-4-5-full", "action": "generate",
           "parameters": {"width": 832, "height": 1216, "steps": 28, "n_samples": n, "scale": 5.0}})
}

fn image_opts<'a>(url: &str, body: &'a serde_json::Value, n: usize) -> RequestOpts<'a> {
    let mut o = RequestOpts::post(format!("{url}/ai/generate-image"), body);
    o.image_lane = true;
    o.image_count = n;
    o.max_response_bytes = Some(64 << 20);
    o
}

#[tokio::test]
async fn image_success_counts_and_body_is_python_json() {
    let up = MockUpstream::start().await;
    let hooks = TestHooks::default();
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        hooks.clone(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let body = image_body(2);
    let resp = c
        .request(
            image_opts(&up.url(), &body, 2),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.starts_with(b"PK"));
    let req = &up.requests_to("/ai/generate-image")[0];
    // 发出的正文与 Python json.dumps(separators=(",", ":"), ensure_ascii=False) 逐字节一致
    assert_eq!(req.body.as_ref(), gate_core::pyjson::wire(&body).as_slice());
    assert_eq!(
        req.header("authorization"),
        Some(format!("Bearer {T1}").as_str())
    );
    assert_eq!(req.header("content-type"), Some("application/json"));
    // 结算：张数记到上游计数；按张数记 guard 开始
    let tid = gate_upstream::headers::token_id(T1);
    assert_eq!(hooks.0.images.lock().unwrap().get(&tid), Some(&2));
    assert_eq!(hooks.0.starts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn image_429_cools_token_and_calls_back() {
    let up = MockUpstream::start().await;
    up.push("/ai/generate-image", Reply::rate_limited("120"));
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        TestHooks::default(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(None));
    let s2 = seen.clone();
    let cb = Callbacks {
        on_rate_limited: Some(Box::new(move |ra| {
            *s2.lock().unwrap() = Some(ra);
            Box::pin(async {})
        })),
        ..Default::default()
    };
    let body = image_body(1);
    let err = c
        .request(image_opts(&up.url(), &body, 1), &cb, None, &NoObserver)
        .await
        .unwrap_err();
    assert_eq!(
        (err.status, err.message.as_str()),
        (429, "上游限流(429)，全站图片生成已进入冷却")
    );
    assert_eq!(*seen.lock().unwrap(), Some(120.0));
    // 冷却中：没有可用 Token，给出「冷却中、不是额度用完」的说明
    let err = c
        .request(
            image_opts(&up.url(), &body, 1),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap_err();
    assert_eq!(err.status, 503);
    assert!(
        err.message.starts_with("上游账号暂时限流冷却中"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn non_image_429_switches_token_once() {
    let up = MockUpstream::start().await;
    up.push("/ai/generate-image", Reply::rate_limited("30"));
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        TestHooks::default(),
        vec![(T1.into(), 0, false), (T2.into(), 0, false)],
    )
    .unwrap();
    let body = image_body(1);
    let opts = RequestOpts::post(format!("{}/ai/generate-image", up.url()), &body);
    let resp = c
        .request(opts, &Callbacks::default(), None, &NoObserver)
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let reqs = up.requests_to("/ai/generate-image");
    assert_eq!(reqs.len(), 2);
    assert_ne!(
        reqs[0].header("authorization"),
        reqs[1].header("authorization")
    );
}

#[tokio::test]
async fn forbidden_model_is_400_and_account_403_trips_after_three() {
    let up = MockUpstream::start().await;
    let hooks = TestHooks::default();
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        hooks.clone(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let body = image_body(1);
    up.push("/ai/generate-image", Reply::model_403("nai-glm-4-6"));
    let e = c
        .request(
            image_opts(&up.url(), &body, 1),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.status, e.message.as_str()),
        (400, "上游不认识这个模型：请换用本站白名单里的模型")
    );
    for _ in 0..3 {
        up.push("/ai/generate-image", Reply::account_403());
        let e = c
            .request(
                image_opts(&up.url(), &body, 1),
                &Callbacks::default(),
                None,
                &NoObserver,
            )
            .await
            .unwrap_err();
        assert_eq!(e.status, 403);
    }
    assert!(
        hooks
            .0
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|k| k == "upstream_403_cooldown")
    );
    let f = c.tokens().await[0].fields();
    assert_eq!((f.forbidden_trips, f.forbidden_streak), (1, 0));
    assert!(f.blocked_until > hooks.now() + 299.0, "第一轮冷却 5 分钟");
}

#[tokio::test]
async fn unauthorized_disables_token() {
    let up = MockUpstream::start().await;
    up.push("/ai/generate-image", Reply::status(401, "nope"));
    let hooks = TestHooks::default();
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        hooks.clone(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let body = image_body(1);
    let e = c
        .request(
            image_opts(&up.url(), &body, 1),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.status, e.message.as_str()),
        (502, "上游令牌已失效（401），请站长更换 NovelAI Token")
    );
    assert!(c.tokens().await[0].fields().disabled);
    assert!(
        hooks
            .0
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|k| k == "upstream_401")
    );
}

#[tokio::test]
async fn image_interval_is_enforced_between_requests() {
    let up = MockUpstream::start().await;
    // 间隔 0.4 秒（≤ 0.5 秒时不加请求后抖动，与旧版测试环境一致）
    let c = NaiClient::new(
        config(&up.url(), 0.4),
        TestHooks::default(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let body = image_body(1);
    let t0 = Instant::now();
    for _ in 0..3 {
        c.request(
            image_opts(&up.url(), &body, 1),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap();
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(800),
        "三张图至少隔两个间隔：{:?}",
        t0.elapsed()
    );
}

#[tokio::test]
async fn guard_block_reason_is_429() {
    let up = MockUpstream::start().await;
    let hooks = TestHooks::default();
    *hooks.0.block.lock().unwrap() = Some(("本小时已到上限".into(), "hourly_cap".into()));
    let c = NaiClient::new(config(&up.url(), 0.0), hooks, vec![(T1.into(), 0, false)]).unwrap();
    let body = image_body(1);
    let e = c
        .request(
            image_opts(&up.url(), &body, 1),
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.status, e.message.as_str(), e.code.as_str()),
        (429, "本小时已到上限", "hourly_cap")
    );
    assert!(up.requests().is_empty(), "被账号保护挡住时不发上游");
}

#[tokio::test]
async fn v5_reservation_released_when_cancelled() {
    let up = MockUpstream::start().await;
    up.push("/ai/generate-image", Reply::Hang);
    let hooks = TestHooks::default();
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        hooks.clone(),
        vec![(T1.into(), 5, false)],
    )
    .unwrap();
    let body = image_body(1);
    let c2 = c.clone();
    let url = up.url();
    let b2 = body.clone();
    let task = tokio::spawn(async move {
        let mut o = image_opts(&url, &b2, 1);
        o.v5_free = true;
        c2.request(o, &Callbacks::default(), None, &NoObserver)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(c.tokens().await[0].fields().pending_v5, 1);
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = &c.tokens().await[0];
    assert_eq!(t.fields().pending_v5, 0, "取消后 V5 预留必须归还");
    assert_eq!(t.image_slots.active(), 0, "取消后并发槽必须归还");
    // 之后的请求照常
    let mut o = image_opts(&up.url(), &body, 1);
    o.v5_free = true;
    c.request(o, &Callbacks::default(), None, &NoObserver)
        .await
        .unwrap();
    let tid = gate_upstream::headers::token_id(T1);
    assert_eq!(hooks.0.v5.lock().unwrap().get(&tid), Some(&1));
}

#[tokio::test]
async fn image_stream_settles_completed_images() {
    let up = MockUpstream::start().await;
    let hooks = TestHooks::default();
    let c = NaiClient::new(
        config(&up.url(), 0.0),
        hooks.clone(),
        vec![(T1.into(), 0, false)],
    )
    .unwrap();
    let mut body = image_body(2);
    body["parameters"]["stream"] = json!("msgpack");
    let mut s = c
        .image_stream(
            &format!("{}/ai/generate-image-stream", up.url()),
            &body,
            false,
            false,
            90.0,
            &Callbacks::default(),
            None,
            &NoObserver,
        )
        .await
        .unwrap();
    use futures::StreamExt;
    let mut bytes = 0;
    let mut stream = Box::pin(s.take_body().unwrap());
    while let Some(chunk) = stream.next().await {
        bytes += chunk.unwrap().len();
    }
    assert!(bytes > 0);
    let req = &up.requests_to("/ai/generate-image-stream")[0];
    assert_eq!(req.header("accept"), Some("application/x-msgpack"));
    s.completed_images = 2;
    s.finish().await;
    let tid = gate_upstream::headers::token_id(T1);
    assert_eq!(hooks.0.images.lock().unwrap().get(&tid), Some(&2));
}
