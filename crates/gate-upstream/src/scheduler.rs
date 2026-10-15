//! 出图节奏：「这个上游账号的下一张什么时候能发」只在这里算。
//!
//! - 开始间隔：同一把 Token 两次开始生成至少隔 `image_min_interval`（后台运行期限制）+ 开始抖动
//!   （账号保护 `interval_jitter`，均匀随机 0～N 秒，经 [`Hooks::jitter`]）；
//! - 出图后静默：出图结束（成功或失败，只要真的发往了上游）后，下一张最早 = max(原定, 现在 + 静默)，
//!   静默在 [下限, 上限] 里均匀随机，上限不超过开始间隔；间隔 ≤ 0.5 秒（测试环境）时不静默；
//! - 等槽位：先占这把 Token 的出图并发槽，再等间隔到点，整体受排队超时约束。
//!
//! 时钟是单调时钟（客户端启动以来的秒），[`crate::pool::TokenFields::image_next_at`] 用的就是它。
//! `image_next_at` 与间隔的读改写都在调度器自己的锁下串行（不跨 await）。

use crate::hooks::Hooks;
use crate::limiter::Permit;
use crate::pool::Token;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct Scheduler {
    base: tokio::time::Instant,
    /// 账号出图间隔（秒）。这把锁同时串行化所有 `image_next_at` 的读改写。
    interval: Mutex<f64>,
    post_jitter: (f64, f64),
}

impl Scheduler {
    pub(crate) fn new(interval: f64, post_jitter: (f64, f64)) -> Self {
        Self {
            base: tokio::time::Instant::now(),
            interval: Mutex::new(interval.max(0.0)),
            post_jitter,
        }
    }

    /// 客户端启动以来的单调秒数。
    pub(crate) fn mono(&self) -> f64 {
        self.base.elapsed().as_secs_f64()
    }

    /// 调大间隔时，正在冷却的 Token 也立即按新间隔顺延。
    pub(crate) fn set_interval(&self, tokens: &[Arc<Token>], seconds: f64) {
        let mut interval = self.interval.lock().expect("scheduler");
        let now = self.mono();
        if seconds > *interval {
            for t in tokens {
                t.with(|f| {
                    if f.image_next_at > now {
                        f.image_next_at = f.image_next_at.max(now + seconds);
                    }
                });
            }
        }
        *interval = seconds;
    }

    /// 等到这把 Token 的出图间隔到点，然后占位（`now + 间隔 + 开始抖动`）。取消等待不会挤占后续生图。
    pub(crate) async fn wait_turn(&self, token: &Token, hooks: &dyn Hooks) {
        loop {
            let wait = {
                let interval = self.interval.lock().expect("scheduler");
                let now = self.mono();
                let wait = (token.with(|f| f.image_next_at) - now).max(0.0);
                if wait == 0.0 {
                    let jitter = if hooks.has_guard() {
                        hooks.jitter()
                    } else {
                        0.0
                    };
                    let next = now + *interval + jitter;
                    token.with(|f| f.image_next_at = next);
                    return;
                }
                wait
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    /// 出图槽位：先占并发槽（放进 `permit`，由调用方结算后归还），再等间隔到点。
    /// 超过 `queue_timeout` 秒返回 false（已占到的槽仍在 `permit` 里）。
    pub(crate) async fn acquire(
        &self,
        token: &Token,
        hooks: &dyn Hooks,
        queue_timeout: f64,
        permit: &mut Option<Permit>,
    ) -> bool {
        let wait = async {
            *permit = Some(token.image_slots.acquire().await);
            self.wait_turn(token, hooks).await;
        };
        tokio::time::timeout(Duration::from_secs_f64(queue_timeout.max(0.0)), wait)
            .await
            .is_ok()
    }

    /// 出图后静默（拟人化：消除「上一张一结束下一张立刻开始」的贴脸节奏）。
    /// 调用方只对真正发往上游的请求调用。
    pub(crate) fn after_request(&self, token: &Token) {
        let interval = *self.interval.lock().expect("scheduler");
        let (min, max) = self.post_jitter;
        let jmin = min.max(0.0);
        let jmax = max.max(jmin);
        if interval > 0.5 && jmax > 0.0 {
            let now = self.mono();
            let hi = jmax.min(interval);
            let lo = jmin.min(hi);
            let delay = if hi > 0.0 { uniform(lo, hi) } else { 0.0 };
            if delay > 0.0 {
                let _guard = self.interval.lock().expect("scheduler");
                token.with(|f| f.image_next_at = f.image_next_at.max(now + delay));
            }
        }
    }
}

/// Python `random.uniform(a, b)` = `a + (b - a) * random()`。
fn uniform(a: f64, b: f64) -> f64 {
    a + (b - a) * rand::random::<f64>()
}
