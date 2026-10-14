//! 网关给上游客户端注入的能力：服务日、告警、账号保护、上游计数、结果校验。

use crate::alerts::Alerter;
use crate::guard::Guard;
use bytes::Bytes;
use gate_core::clock::Clock;
use gate_db::Db;
use gate_upstream::client::{BoxFuture, HookResult, Hooks};
use std::sync::Arc;

pub struct GateHooks {
    db: Db,
    clock: Clock,
    guard: Arc<Guard>,
    alerter: Arc<Alerter>,
}

impl GateHooks {
    pub fn new(db: Db, clock: Clock, guard: Arc<Guard>, alerter: Arc<Alerter>) -> Self {
        Self { db, clock, guard, alerter }
    }
}

fn boxed<E: std::error::Error + Send + Sync + 'static>(e: E) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

impl Hooks for GateHooks {
    fn day(&self) -> String {
        self.clock.day(crate::time::now())
    }

    fn now(&self) -> f64 {
        crate::time::now()
    }

    fn event(&self, kind: &str, message: &str, cooldown: f64) {
        self.alerter.notify(kind, message, cooldown);
    }

    fn has_guard(&self) -> bool {
        true
    }

    fn token_block_reason<'a>(&'a self, token_id: &'a str, day: &'a str) -> BoxFuture<'a, HookResult<Option<(String, String)>>> {
        Box::pin(async move {
            let r = self.guard.token_block_reason(token_id, day, crate::time::now()).await.map_err(boxed)?;
            Ok(r.map(|r| (r.text().to_string(), r.code().to_string())))
        })
    }

    fn record_start(&self, token_id: &str, images: i64) {
        self.guard.record_start(token_id, images, crate::time::now());
    }

    fn jitter(&self) -> f64 {
        self.guard.jitter()
    }

    fn upstream_v5_counter<'a>(&'a self, token_id: &'a str, day: &'a str) -> BoxFuture<'a, HookResult<i64>> {
        Box::pin(async move { self.db.get_upstream_v5_counter(token_id, day).await.map_err(boxed) })
    }

    fn bump_upstream_v5<'a>(&'a self, token_id: &'a str, day: &'a str) -> BoxFuture<'a, HookResult<()>> {
        Box::pin(async move { self.db.bump_upstream_v5_counter(token_id, day).await.map_err(boxed) })
    }

    fn bump_upstream_images<'a>(&'a self, token_id: &'a str, day: &'a str, n: i64) -> BoxFuture<'a, HookResult<()>> {
        Box::pin(async move { self.db.bump_upstream_image_counter(token_id, day, n, 1.0).await.map_err(boxed) })
    }

    fn validate_image_result(&self, body: Bytes, expected_images: usize) -> BoxFuture<'static, bool> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::media::validate_result(&body, "generate-image", Some(expected_images)).is_ok())
                .await
                .unwrap_or(false)
        })
    }
}
