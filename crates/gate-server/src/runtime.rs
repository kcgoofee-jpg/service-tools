//! 运行期共享状态（旧版 `state.py` 的 GateState 里与请求相关的部分）：
//! - 一个通用滑动窗口 [`SlidingWindow`]，取代旧版 5 份几乎相同的实现（RPM / 无效 Key / 后台登录 / 捐赠 / 前端报错）；
//! - 每把 Key 的出图间隔（含「被本地拒绝时退还」）；
//! - 防爆破（IPv6 聚合到 /64）；
//! - 全站图片冷却（上游 429 后）；
//! - Key 暂停闸门（自动驾驶限流等）。
//!
//! 时间：墙钟用 `crate::time::now()`，间隔类用 tokio 单调时钟（测试可暂停）。

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// 滑动窗口计数器。
pub struct SlidingWindow<K: Eq + Hash + Clone> {
    inner: Mutex<HashMap<K, VecDeque<f64>>>,
    /// 键太多时顺手清理（旧版：RPM 1024、登录 / 防爆破 2048）。
    gc_over: usize,
}

impl<K: Eq + Hash + Clone> SlidingWindow<K> {
    pub fn new(gc_over: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            gc_over,
        }
    }

    /// 先查后记（RPM、后台登录）：窗口内已有 `limit` 次则拒绝且不记；否则记一次放行。
    /// 过期判定与旧版一致：`now - t > window` 才算出窗。
    pub fn check_then_push(&self, key: &K, limit: usize, window: f64, now: f64) -> bool {
        let mut map = self.inner.lock().expect("window");
        let win = map.entry(key.clone()).or_default();
        while win.front().is_some_and(|t| now - t > window) {
            win.pop_front();
        }
        if win.len() >= limit.max(1) {
            return false;
        }
        win.push_back(now);
        if map.len() > self.gc_over {
            map.retain(|k, w| k == key || w.back().is_some_and(|t| now - t <= window));
        }
        true
    }

    /// 先记后查（无效 Key 计数）：记一次，返回窗口内的次数。
    pub fn push_then_count(&self, key: &K, window: f64, now: f64) -> usize {
        let mut map = self.inner.lock().expect("window");
        let win = map.entry(key.clone()).or_default();
        win.push_back(now);
        while win.front().is_some_and(|t| now - t > window) {
            win.pop_front();
        }
        let n = win.len();
        if map.len() > self.gc_over {
            map.retain(|k, w| k == key || w.back().is_some_and(|t| now - t <= window));
        }
        n
    }

    pub fn clear(&self, key: &K) {
        if let Some(w) = self.inner.lock().expect("window").get_mut(key) {
            w.clear();
        }
    }
}

/// 防爆破桶的键：IPv6 聚合到 /64（同一用户的临时地址都在一个 /64 里轮换）；IPv4 / 无法解析的原样。
pub fn bruteforce_bucket(client_id: &str) -> String {
    match client_id.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) | Err(_) => client_id.to_string(),
        Ok(IpAddr::V6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.to_string();
            }
            let seg = v6.segments();
            std::net::Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], 0, 0, 0, 0).to_string()
        }
    }
}

/// IP 打码：IPv4 只留后两段，其余只留最后 6 个字符（告警用）。
pub fn mask_ip(value: &str) -> String {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() == 4 {
        format!("*.*.{}.{}", parts[2], parts[3])
    } else {
        format!("…{}", gate_core::pyc::last_chars(value, 6))
    }
}

/// 无效 Key 防爆破。
pub struct AuthGuard {
    fails: SlidingWindow<String>,
    blocked_until: Mutex<HashMap<String, f64>>,
}

impl Default for AuthGuard {
    fn default() -> Self {
        Self {
            fails: SlidingWindow::new(2048),
            blocked_until: Mutex::new(HashMap::new()),
        }
    }
}

impl AuthGuard {
    /// 该 IP 因多次无效 Key 被临时拦截时的剩余秒数；否则 0。
    pub fn blocked(&self, client_id: &str, now: f64) -> i64 {
        let until = *self
            .blocked_until
            .lock()
            .expect("auth")
            .get(&bruteforce_bucket(client_id))
            .unwrap_or(&0.0);
        ((until - now) as i64).max(0)
    }

    /// 记一次无效 Key；达到阈值时拦截并返回 true（调用方发告警）。
    pub fn record_failure(
        &self,
        client_id: &str,
        max: i64,
        window: i64,
        block_seconds: i64,
        now: f64,
    ) -> bool {
        let bucket = bruteforce_bucket(client_id);
        let window = window.max(1) as f64;
        let n = self.fails.push_then_count(&bucket, window, now);
        if n as i64 >= max.max(1) {
            let mut b = self.blocked_until.lock().expect("auth");
            b.insert(bucket.clone(), now + block_seconds.max(1) as f64);
            if b.len() > 2048 {
                b.retain(|_, t| *t >= now);
            }
            drop(b);
            self.fails.clear(&bucket);
            return true;
        }
        false
    }
}

/// 排队超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueTimeout;

/// 同一把 Key 同时进行中的请求已达上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyInflight;

/// 每把 Key 的出图间隔（旧版 `wait_for_key_image_slot` / `refund_key_image_slot`）。
#[derive(Default)]
pub struct KeyPacing {
    inner: tokio::sync::Mutex<PacingState>,
    /// 正在等间隔的请求数（首页「排队」显示用）。
    pub waiting: std::sync::atomic::AtomicI64,
}

#[derive(Default)]
struct PacingState {
    next_at: HashMap<i64, Instant>,
    taken: HashMap<i64, (Option<Instant>, Instant)>,
}

impl KeyPacing {
    /// 等到这把 Key 的间隔到点并占位；超过 `timeout` 返回 Err。
    pub async fn wait(&self, key_id: i64, interval: f64, timeout: f64) -> Result<(), QueueTimeout> {
        let fut = async {
            loop {
                let wait = {
                    let mut s = self.inner.lock().await;
                    let now = Instant::now();
                    let next = s.next_at.get(&key_id).copied();
                    match next.filter(|n| *n > now) {
                        None => {
                            let new = now + Duration::from_secs_f64(interval.max(0.0));
                            s.next_at.insert(key_id, new);
                            s.taken.insert(key_id, (next, new));
                            if s.next_at.len() > 1024 {
                                let stale: Vec<i64> = s
                                    .next_at
                                    .iter()
                                    .filter(|(_, v)| **v <= now)
                                    .map(|(k, _)| *k)
                                    .collect();
                                for k in stale {
                                    s.next_at.remove(&k);
                                    s.taken.remove(&k);
                                }
                            }
                            return;
                        }
                        Some(n) => n - now,
                    }
                };
                self.waiting
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(wait).await;
                self.waiting
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        };
        tokio::time::timeout(Duration::from_secs_f64(timeout.max(0.0)), fut)
            .await
            .map_err(|_| QueueTimeout)
    }

    /// 请求在到达上游前被本地拒绝时归还占用；已被后续请求覆盖则不动。
    pub async fn refund(&self, key_id: i64) {
        let mut s = self.inner.lock().await;
        if let Some((prev, new)) = s.taken.remove(&key_id)
            && s.next_at.get(&key_id) == Some(&new)
        {
            match prev {
                Some(p) => {
                    s.next_at.insert(key_id, p);
                }
                None => {
                    s.next_at.remove(&key_id);
                }
            }
        }
    }

    /// 调大间隔时，正在冷却的 Key 立即按新间隔顺延。
    pub async fn extend(&self, interval: f64) {
        let mut s = self.inner.lock().await;
        let now = Instant::now();
        let new = now + Duration::from_secs_f64(interval.max(0.0));
        for v in s.next_at.values_mut() {
            if *v > now && new > *v {
                *v = new;
            }
        }
    }
}

/// 全站图片冷却（上游 429 后）。
#[derive(Default)]
pub struct ImageCooldown {
    until: Mutex<f64>,
}

impl ImageCooldown {
    pub fn remaining(&self, now: f64) -> i64 {
        ((*self.until.lock().expect("cooldown") - now) as i64).max(0)
    }

    /// 从库里恢复（`image_cooldown_until`）：非数 / 超过 1 小时的值视为无效。
    pub fn load(&self, raw: Option<&str>, now: f64) {
        let v = raw.and_then(gate_core::pyc::parse_float).unwrap_or(0.0);
        let v = if v.is_nan() || v > now + 3600.0 {
            0.0
        } else {
            v.max(0.0)
        };
        *self.until.lock().expect("cooldown") = v;
    }

    /// 延长冷却，返回新的截止时间（调用方写库 + 告警）。
    pub fn block(&self, retry_after: f64, now: f64) -> f64 {
        let mut u = self.until.lock().expect("cooldown");
        *u = u.max(now + gate_core::upstream_policy::clamp_retry_after_f64(retry_after));
        *u
    }
}

/// Key 暂停闸门（旧版 ShareGuard 只剩的部分）。
#[derive(Default)]
pub struct KeyPauses {
    inner: Mutex<HashMap<i64, (f64, String)>>,
}

impl KeyPauses {
    pub fn load(&self, rows: Vec<(i64, f64, String)>) {
        *self.inner.lock().expect("pauses") =
            rows.into_iter().map(|(k, u, r)| (k, (u, r))).collect();
    }

    /// 仍在暂停中：返回 (截止时间, 原因)。
    pub fn paused(&self, key_id: i64, now: f64) -> Option<(f64, String)> {
        self.inner
            .lock()
            .expect("pauses")
            .get(&key_id)
            .filter(|(u, _)| *u > now)
            .cloned()
    }

    pub fn set(&self, key_id: i64, until: f64, reason: String) {
        self.inner
            .lock()
            .expect("pauses")
            .insert(key_id, (until, reason));
    }

    pub fn remove(&self, key_id: i64) {
        self.inner.lock().expect("pauses").remove(&key_id);
    }

    pub fn snapshot(&self) -> HashMap<i64, (f64, String)> {
        self.inner.lock().expect("pauses").clone()
    }
}

/// 每个 Authorization 同时进行中的请求数（读请求体之前就限制：25 MB 的生图请求在 2 GB 机器上很贵）。
#[derive(Default)]
pub struct Inflight {
    inner: Mutex<HashMap<String, usize>>,
}

pub const MAX_INFLIGHT_PER_KEY: usize = 4;

pub struct InflightPermit<'a> {
    owner: &'a Inflight,
    ident: String,
}

impl Drop for InflightPermit<'_> {
    fn drop(&mut self) {
        let mut m = self.owner.inner.lock().expect("inflight");
        if let Some(n) = m.get_mut(&self.ident) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ident);
            }
        }
    }
}

impl Inflight {
    /// 与鉴权同样归一化：「Bearer K」「bearer K」「Bearer  K」是同一把 Key。没有凭据不限制（返回 Ok(None)）。
    pub fn acquire(
        &self,
        authorization: &str,
    ) -> Result<Option<InflightPermit<'_>>, TooManyInflight> {
        let raw = gate_core::pyc::strip(gate_core::pyc::take_chars(authorization, 512));
        let (scheme, rest) = raw.split_once(' ').unwrap_or((raw, ""));
        let ident = if scheme.eq_ignore_ascii_case("bearer") {
            gate_core::pyc::strip(rest)
        } else {
            raw
        }
        .to_string();
        if ident.is_empty() {
            return Ok(None);
        }
        let mut m = self.inner.lock().expect("inflight");
        let n = m.entry(ident.clone()).or_insert(0);
        if *n >= MAX_INFLIGHT_PER_KEY {
            return Err(TooManyInflight);
        }
        *n += 1;
        Ok(Some(InflightPermit { owner: self, ident }))
    }
}

/// 最近 10 分钟上游调用结果（判断「上游不稳定」并告警）。
#[derive(Default)]
pub struct UpstreamHealth {
    events: Mutex<VecDeque<(f64, bool)>>,
    degraded: Mutex<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthChange {
    Degraded,
    Recovered,
}

impl UpstreamHealth {
    /// 记一次结果；状态翻转时返回变化（调用方发告警）。
    pub fn record(&self, ok: bool, now: f64) -> Option<HealthChange> {
        {
            let mut e = self.events.lock().expect("health");
            e.push_back((now, ok));
            while e.len() > 40 {
                e.pop_front();
            }
        }
        let status = self.status(now).0;
        let mut d = self.degraded.lock().expect("health");
        match status {
            "degraded" if !*d => {
                *d = true;
                Some(HealthChange::Degraded)
            }
            "ok" if *d => {
                *d = false;
                Some(HealthChange::Recovered)
            }
            _ => None,
        }
    }

    /// (status, recent, failed)：近 10 分钟 ≥ 4 次且失败过半为 degraded；没有记录为 idle。
    pub fn status(&self, now: f64) -> (&'static str, usize, usize) {
        let e = self.events.lock().expect("health");
        let recent: Vec<bool> = e
            .iter()
            .filter(|(t, _)| *t >= now - 600.0)
            .map(|(_, ok)| *ok)
            .collect();
        let failed = recent.iter().filter(|ok| !**ok).count();
        let status = if recent.len() >= 4 && failed as f64 / recent.len() as f64 >= 0.5 {
            "degraded"
        } else if recent.is_empty() {
            "idle"
        } else {
            "ok"
        };
        (status, recent.len(), failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_semantics() {
        let w: SlidingWindow<i64> = SlidingWindow::new(1024);
        assert!(w.check_then_push(&1, 2, 60.0, 0.0));
        assert!(w.check_then_push(&1, 2, 60.0, 1.0));
        assert!(!w.check_then_push(&1, 2, 60.0, 2.0));
        // 恰好 60 秒不出窗（旧版用 > 判定）
        assert!(!w.check_then_push(&1, 2, 60.0, 60.0));
        assert!(w.check_then_push(&1, 2, 60.0, 60.5));
    }

    #[test]
    fn buckets() {
        assert_eq!(bruteforce_bucket("203.0.113.9"), "203.0.113.9");
        assert_eq!(bruteforce_bucket("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::");
        assert_eq!(bruteforce_bucket("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(bruteforce_bucket("unknown"), "unknown");
        assert_eq!(mask_ip("203.0.113.9"), "*.*.113.9");
    }

    #[test]
    fn auth_guard_blocks_after_max() {
        let g = AuthGuard::default();
        for i in 0..19 {
            assert!(!g.record_failure("203.0.113.9", 20, 600, 900, i as f64));
        }
        assert!(g.record_failure("203.0.113.9", 20, 600, 900, 19.0));
        assert_eq!(g.blocked("203.0.113.9", 20.0), 899);
        assert_eq!(g.blocked("203.0.113.10", 20.0), 0);
    }

    #[test]
    fn inflight_normalizes_bearer() {
        let f = Inflight::default();
        let a = f.acquire("Bearer K").unwrap();
        let b = f.acquire("bearer  K").unwrap();
        let c = f.acquire("BEARER K").unwrap();
        let d = f.acquire(" Bearer K ").unwrap();
        assert!(f.acquire("Bearer K").is_err());
        drop((a, b, c, d));
        assert!(f.acquire("Bearer K").is_ok());
        assert!(f.acquire("").unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn pacing_and_refund() {
        let p = KeyPacing::default();
        p.wait(7, 15.0, 90.0).await.unwrap();
        // 被本地拒绝：退还，下一次立即可发
        p.refund(7).await;
        let t0 = Instant::now();
        p.wait(7, 15.0, 90.0).await.unwrap();
        assert_eq!(Instant::now(), t0);
        // 没退还：要等满 15 秒
        p.wait(7, 15.0, 90.0).await.unwrap();
        assert!(Instant::now() - t0 >= Duration::from_secs(15));
        // 排队超时
        assert!(p.wait(7, 15.0, 1.0).await.is_err());
    }
}
