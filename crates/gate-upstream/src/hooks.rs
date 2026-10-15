//! 网关注入给上游客户端的能力（全部由 gate-server 实现；测试里用内存版）。

use crate::pool::Token;
use bytes::Bytes;
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type HookResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 时间、事件告警、账号保护（guard）、上游计数（数据库）、结果校验。
pub trait Hooks: Send + Sync + 'static {
    /// 当前服务日（`state.day()`）。
    fn day(&self) -> String;
    /// 墙钟时间（Unix 秒）。
    fn now(&self) -> f64;
    /// 告警事件（同类有冷却，由实现方处理）。
    fn event(&self, kind: &str, message: &str, cooldown: f64);

    /// 是否接了账号保护（旧版 `self.guard is not None`）。
    fn has_guard(&self) -> bool;
    /// 账号保护：这把 Token 现在能不能出图；不能时返回 (给成员的原因, 原因码)。
    fn token_block_reason<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<Option<(String, String)>>>;
    /// 账号保护：记一次出图开始（按张数）。
    fn record_start(&self, token_id: &str, images: i64);
    /// 账号保护：本次出图间隔的随机抖动（秒）。
    fn jitter(&self) -> f64;

    fn upstream_v5_counter<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<i64>>;
    fn bump_upstream_v5<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
    ) -> BoxFuture<'a, HookResult<()>>;
    fn bump_upstream_images<'a>(
        &'a self,
        token_id: &'a str,
        day: &'a str,
        n: i64,
    ) -> BoxFuture<'a, HookResult<()>>;

    /// 校验上游返回的出图结果是否完整（`image_tools.validate_result`），在阻塞线程里跑。
    fn validate_image_result(
        &self,
        body: Bytes,
        expected_images: usize,
    ) -> BoxFuture<'static, bool>;
}

/// 单次请求的观察者（旧版 `request_timing` 的 contextvars）：记录「已发往上游」与上游状态码。
pub trait Observer: Send + Sync {
    fn sent(&self) {}
    fn status(&self, _code: u16) {}
}

pub struct NoObserver;
impl Observer for NoObserver {}

/// V5 额度确认（`AllowanceCache.resolve`）：由 gate-server 实现（需要库里的阈值与周期样本）。
pub trait Allowance: Send + Sync {
    /// 返回官方免费额度是否已耗尽；查询失败返回 Err(给成员的文案)。
    fn resolve<'a>(&'a self, token: &'a Token) -> BoxFuture<'a, Result<bool, String>>;
}
