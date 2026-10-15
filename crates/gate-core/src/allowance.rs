//! V5 订阅额度缓存的纯逻辑部分（对应 `app/allowance.py` 的 `AllowanceCache`）。
//!
//! 旧版一次 `resolve` 里夹着 HTTP、数据库和时钟；这里把它拆成「服务层做 IO、这里做判定」的几步：
//!
//! 1. 服务层持有每个 token 的锁，取单调时钟 `now_mono`，读阈值（见下），调 [`AllowanceCore::check_cache`]：
//!    `Unavailable` → 抛 [`UNAVAILABLE_MSG`]；`Cached` → 返回 `false`（不是耗尽）；`Fetch` → 去请求上游；
//! 2. 请求 `GET {host}/user/subscription`（和生图同一套浏览器请求头，8 秒总超时，不跟随重定向，
//!    响应体最多读 65536 字节），把结果装成 [`Fetched`]；
//! 3. 调 [`AllowanceCore::finish_resolve`]（传请求结束后的墙钟 / 单调时钟）：返回 [`Resolved`]
//!    （`Err(Unavailable)` 或 `Ok(是否耗尽)` 加上要做的副作用）；
//! 4. 服务层按 [`Resolved::note_period`] 调 [`AllowanceCore::note_period`] 并把返回的文本写回库，
//!    [`Resolved::low`] 为真（额度偏低）时写一行日志（旧版是 `on_low` 回调，发告警）。
//!
//! 不在这里的 IO（服务层负责）：
//! - `read_alert_threshold(db)`：`db.get_setting("v5_alert_threshold", 20)`，取到的值交给 [`normalize_alert_threshold`]；
//! - `_note_period` 的读写：第一次用时 `db.get_setting("v5_period_samples", "{}")` 交给 [`AllowanceCore::load_periods`]，
//!   之后每次 `db.set_setting("v5_period_samples", <note_period 返回的文本>)`，写失败只忽略（旧版 `except Exception: pass`）；
//! - HTTP 请求与响应体读取本身；`asyncio.Lock` 按 token 串行；`log.warning`。
//!
//! 已知边界（只在库里数据被手改坏、或上游返回非 UTF-8 JSON 时才碰到）：
//! - `v5_period_samples` 不是 `{token: [[ts, 秒], ...]}` 的形状时，不合法的部分当作没有（旧版会 TypeError / AttributeError）；
//! - 订阅响应只按 UTF-8 解析（Python `json.loads(bytes)` 还会自动识别 UTF-16 / UTF-32）。

use crate::pyc;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const SETTING: &str = "v5_alert_threshold";
pub const DEFAULT_THRESHOLD: i64 = 20;
/// 每个上游账号最近 48 小时「再恢复 1% 还要多少秒」的读数
pub const PERIOD_KEY: &str = "v5_period_samples";
pub const PERIOD_WINDOW: i64 = 48 * 3600;
/// 每个账号最多留这么多条读数
pub const PERIOD_KEEP: usize = 500;
pub const UNAVAILABLE_MSG: &str = "V5 额度暂时无法确认，请稍后重试；未发送生图，未扣积分";
pub const ERROR_MSG: &str = "额度查询失败，当前状态未确认";
/// 订阅响应体上限
pub const MAX_BODY: usize = 65536;

/// `read_alert_threshold` 的判定：库里是 TEXT，`"20"` 这样的 1～3 位 ASCII 数字串转成整数；
/// 取值必须是 1～100 的整数（bool 不算），否则回到默认值 20。
/// 没有这一行时旧版拿到的就是默认值（整数 20），传 `Value::Null` 也得到 20。
pub fn normalize_alert_threshold(value: &Value) -> i64 {
    let v = match value {
        Value::String(s)
            if s.is_ascii()
                && !s.is_empty()
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.len() <= 3 =>
        {
            s.parse::<i64>().ok()
        }
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        _ => None,
    };
    match v {
        Some(i) if (1..=100).contains(&i) => i,
        _ => DEFAULT_THRESHOLD,
    }
}

/// 缓存里一个上游账号的最近一次读数（对应旧版 `_rows[token_id]` 这个 dict；缺失的键 = `None` / 0）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub percent: Option<i64>,
    pub is_negative: Option<bool>,
    /// 墙钟（`time.time()`）
    pub checked_at: Option<f64>,
    /// 单调时钟（`time.monotonic()`），缓存新鲜度按它算
    pub at: Option<f64>,
    pub error: Option<String>,
    /// 单调时钟：在这之前不再请求
    pub retry_at: f64,
    pub next_percent_seconds: Option<i64>,
}

/// 每个账号最近 48 小时的「再恢复 1% 还要多少秒」读数，保持账号的插入顺序（回写库时与旧版同序）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Periods {
    pub tokens: Vec<(String, Vec<(i64, i64)>)>,
}

impl Periods {
    /// `json.loads(raw or "{}")`；读不懂就是空（旧版 `except (TypeError, ValueError): {}`）。
    pub fn parse(raw: Option<&str>) -> Self {
        let text = match raw {
            Some(s) if !s.is_empty() => s,
            _ => "{}",
        };
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        let tokens = m
            .into_iter()
            .map(|(k, v)| {
                let rows = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| {
                                let r = r.as_array()?;
                                Some((r.first()?.as_i64()?, r.get(1)?.as_i64()?))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (k, rows)
            })
            .collect();
        Self { tokens }
    }

    fn rows(&self, token_id: &str) -> &[(i64, i64)] {
        self.tokens
            .iter()
            .find(|(k, _)| k == token_id)
            .map_or(&[], |(_, r)| r.as_slice())
    }

    /// `json.dumps(self._periods)`（ensure_ascii，`", "` / `": "` 分隔）。
    pub fn to_json(&self) -> String {
        let obj: Map<String, Value> = self
            .tokens
            .iter()
            .map(|(k, rows)| {
                let arr = rows
                    .iter()
                    .map(|(t, s)| Value::Array(vec![Value::from(*t), Value::from(*s)]))
                    .collect();
                (k.clone(), Value::Array(arr))
            })
            .collect();
        crate::pyjson::default_str(&Value::Object(obj))
    }
}

/// 订阅接口请求的结果，由服务层装好交给 [`AllowanceCore::finish_resolve`]。
#[derive(Debug, Clone)]
pub enum Fetched<'a> {
    /// 没拿到响应：连接失败、协议错误、8 秒总超时
    Failed,
    /// 拿到了状态码。`retry_after` 是响应头 `retry-after` 原值（没有 = ""）；
    /// `body` 是状态码 200 时读到的完整响应体，`None` 表示超过 [`MAX_BODY`] 或读取中途出错。
    Response {
        status: u16,
        retry_after: &'a str,
        body: Option<&'a [u8]>,
    },
}

/// 订阅响应里有用的几个字段。
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub percent: i64,
    pub is_negative: bool,
    /// `timeUntilNextPercent`：必须是 1 ≤ x < 10^7 的整数，否则 None
    pub next_percent_seconds: Option<i64>,
}

/// 订阅响应校验：必须是对象、`active` 为 true、`tier` 为整数 3、`usage.percent` 为非负整数、
/// `usage.isNegative` 为布尔值，否则当作失败（旧版各处 `raise ValueError`）。
pub fn parse_subscription(body: &[u8]) -> Option<Subscription> {
    let body = body.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(body);
    let data: Value = serde_json::from_slice(body).ok()?;
    let usage = data.get("usage").filter(|u| u.is_object())?;
    if data.get("active") != Some(&Value::Bool(true)) {
        return None;
    }
    // type(x) is int：bool 和 float 都不算
    let int_of = |v: Option<&Value>| match v {
        Some(Value::Number(n)) if !n.is_f64() => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX))),
        _ => None,
    };
    if int_of(data.get("tier")) != Some(3) {
        return None;
    }
    let percent = int_of(usage.get("percent")).filter(|p| *p >= 0)?;
    let Some(Value::Bool(is_negative)) = usage.get("isNegative") else {
        return None;
    };
    let next_percent_seconds =
        int_of(usage.get("timeUntilNextPercent")).filter(|n| 0 < *n && *n < 10_000_000);
    Some(Subscription {
        percent,
        is_negative: *is_negative,
        next_percent_seconds,
    })
}

/// `str.isdecimal()`：非空且每个字符都是 Unicode 十进制数字（Nd）。
fn is_decimal(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| pyc::parse_int(c.encode_utf8(&mut [0; 4])).is_some())
}

/// 请求失败后多久不再请求（秒）：默认 60；上游 429 时至少 300，`retry-after` 是 1～6 位十进制数字时取
/// `max(300, min(值, 86400))`。
pub fn retry_seconds(status: Option<u16>, retry_after: &str) -> i64 {
    if status != Some(429) {
        return 60;
    }
    let mut secs = 300;
    if is_decimal(retry_after)
        && pyc::len_chars(retry_after) < 7
        && let Some(v) = pyc::parse_int(retry_after)
    {
        secs = secs.max(v.min(86400));
    }
    secs
}

/// 缓存有效期（秒）：剩 ≤2%（约 30 张）只用 15 秒（≈ 一次出图间隔），低于告警阈值 60 秒，其余 300 秒。
pub fn cache_ttl(percent: i64, threshold: i64) -> i64 {
    if percent <= 2 {
        15
    } else if percent < threshold {
        60
    } else {
        300
    }
}

/// [`AllowanceCore::check_cache`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// 还在退避期：抛 [`UNAVAILABLE_MSG`]（未发送生图、未扣积分）
    Unavailable,
    /// 缓存足够新且没耗尽：直接返回 `false`
    Cached,
    /// 需要去上游读一次
    Fetch,
}

/// 一次 `resolve` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// `Ok(is_negative)`：额度是否已耗尽；`Err(())`：抛 [`UNAVAILABLE_MSG`]
    pub result: Result<bool, ()>,
    /// 读数里带了「再恢复 1% 的秒数」：服务层接着调 [`AllowanceCore::note_period`]
    pub note_period: Option<i64>,
    /// 读数偏低（已耗尽或低于阈值）：服务层写一行日志
    pub low: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("periods 还没从库里读入：先调 load_periods")]
pub struct PeriodsNotLoaded;

#[derive(Debug, Default)]
pub struct AllowanceCore {
    pub rows: BTreeMap<String, Row>,
    /// 第一次用时才从库里读（None = 还没读）
    pub periods: Option<Periods>,
}

impl AllowanceCore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn periods_loaded(&self) -> bool {
        self.periods.is_some()
    }

    /// `_note_period` 里「第一次用时从库里读」的那一步；已经读过就什么都不做。
    pub fn load_periods(&mut self, raw: Option<&str>) {
        if self.periods.is_none() {
            self.periods = Some(Periods::parse(raw));
        }
    }

    /// `_note_period(token_id, seconds)`：官方给的是「当前这 1% 还剩多少秒」，不是恢复 1% 要多久。
    /// 剩余时间 ≤ 真实周期，所以保留 48 小时内的读数、每账号最多 500 条，取最长的当周期。
    /// 返回要写回 `v5_period_samples` 的 JSON 文本。`now` 是墙钟 `time.time()`。
    pub fn note_period(
        &mut self,
        token_id: &str,
        seconds: i64,
        now: f64,
    ) -> Result<String, PeriodsNotLoaded> {
        let periods = self.periods.as_mut().ok_or(PeriodsNotLoaded)?;
        let mut rows: Vec<(i64, i64)> = periods
            .rows(token_id)
            .iter()
            .filter(|(ts, _)| now - (*ts as f64) < PERIOD_WINDOW as f64)
            .copied()
            .collect();
        rows.push((pyc::round0(now), seconds));
        let start = rows.len().saturating_sub(PERIOD_KEEP);
        let rows = rows[start..].to_vec();
        match periods.tokens.iter_mut().find(|(k, _)| k == token_id) {
            Some((_, r)) => *r = rows,
            None => periods.tokens.push((token_id.to_string(), rows)),
        }
        Ok(periods.to_json())
    }

    /// `period(token_id)`：48 小时内最长的一次读数；没有读数 None。
    pub fn period(&self, token_id: &str, now: f64) -> Option<i64> {
        self.periods
            .as_ref()?
            .rows(token_id)
            .iter()
            .filter(|(ts, _)| now - (*ts as f64) < PERIOD_WINDOW as f64)
            .map(|(_, s)| *s)
            .max()
    }

    /// `resolve` 的开头：退避期检查 + 缓存命中判定。`now_mono` 是 `time.monotonic()`，`threshold` 是
    /// [`normalize_alert_threshold`] 的结果（旧版在退避期检查之后才读，多读一次设置没有可观察的差别）。
    pub fn check_cache(&self, token_id: &str, now_mono: f64, threshold: i64) -> Begin {
        let default = Row::default();
        let row = self.rows.get(token_id).unwrap_or(&default);
        if now_mono < row.retry_at {
            return Begin::Unavailable;
        }
        let pct = row.percent.unwrap_or(0);
        let ttl = cache_ttl(pct, threshold);
        // 不拿旧的「已耗尽」结果去把新的一张图切成付费
        if row.error.as_deref().is_none_or(str::is_empty)
            && row.is_negative == Some(false)
            && row.percent.unwrap_or(0) > 0
            && now_mono - row.at.unwrap_or(-1e9) < ttl as f64
        {
            return Begin::Cached;
        }
        Begin::Fetch
    }

    /// `resolve` 的结尾：根据请求结果更新缓存。`wall` / `mono_after` 是请求结束后的 `time.time()` /
    /// `time.monotonic()`；`threshold` 与 [`check_cache`](Self::check_cache) 用的是同一个值。
    pub fn finish_resolve(
        &mut self,
        token_id: &str,
        fetched: &Fetched<'_>,
        wall: f64,
        mono_after: f64,
        threshold: i64,
    ) -> Resolved {
        let (status, retry_after) = match fetched {
            Fetched::Failed => (None, ""),
            Fetched::Response {
                status,
                retry_after,
                ..
            } => (Some(*status), *retry_after),
        };
        let ok = match fetched {
            Fetched::Response {
                status: 200,
                body: Some(body),
                ..
            } => parse_subscription(body),
            _ => None,
        };
        let Some(sub) = ok else {
            let mut row = self.rows.get(token_id).cloned().unwrap_or_default();
            row.error = Some(ERROR_MSG.to_string());
            row.retry_at = mono_after + retry_seconds(status, retry_after) as f64;
            self.rows.insert(token_id.to_string(), row);
            return Resolved {
                result: Err(()),
                note_period: None,
                low: false,
            };
        };
        self.rows.insert(
            token_id.to_string(),
            Row {
                percent: Some(sub.percent),
                is_negative: Some(sub.is_negative),
                checked_at: Some(wall),
                at: Some(mono_after),
                error: None,
                retry_at: 0.0,
                // 官方返回「再恢复 1% 还要多少秒」，据此算出实测恢复速度（%/天）
                next_percent_seconds: sub.next_percent_seconds,
            },
        );
        Resolved {
            result: Ok(sub.is_negative),
            note_period: sub.next_percent_seconds,
            low: sub.is_negative || sub.percent < threshold,
        }
    }

    /// `snapshot(pool)` 里一个账号的那一项。`index` 从 0 起，`now_wall` / `now_mono` 是两种时钟。
    pub fn account_snapshot(
        &self,
        index: usize,
        token_id: &str,
        threshold: i64,
        now_wall: f64,
        now_mono: f64,
    ) -> Value {
        let default = Row::default();
        let row = self.rows.get(token_id).unwrap_or(&default);
        let stale = now_mono - row.at.unwrap_or(-1e9) >= 300.0;
        let low = row.is_negative == Some(true) || row.percent.is_some_and(|p| p < threshold);
        // period(...) or row.get("next_percent_seconds")：0 也算「没有」
        let nps = self
            .period(token_id, now_wall)
            .filter(|n| *n != 0)
            .or(row.next_percent_seconds.filter(|n| *n != 0));
        let error = row.error.clone().filter(|e| !e.is_empty());
        let mut out = Map::new();
        out.insert("account".into(), Value::from(index + 1));
        out.insert(
            "percent".into(),
            row.percent.map_or(Value::Null, Value::from),
        );
        out.insert(
            "recharge_per_day".into(),
            nps.map_or(Value::Null, |n| {
                crate::pyval::float(pyc::round(86400.0 / n as f64, 1))
            }),
        );
        out.insert(
            "is_negative".into(),
            row.is_negative.map_or(Value::Null, Value::Bool),
        );
        out.insert(
            "checked_at".into(),
            row.checked_at.map_or(Value::Null, crate::pyval::float),
        );
        out.insert("low".into(), Value::Bool(low));
        out.insert("uncertain".into(), Value::Bool(stale || error.is_some()));
        out.insert("error".into(), error.map_or(Value::Null, Value::String));
        out.insert("stale".into(), Value::Bool(stale));
        Value::Object(out)
    }

    /// `snapshot(pool)`：`tokens` 按池里的顺序给出 `token_id`。
    pub fn snapshot<'a>(
        &self,
        tokens: impl IntoIterator<Item = &'a str>,
        threshold: i64,
        now_wall: f64,
        now_mono: f64,
    ) -> Value {
        let accounts: Vec<Value> = tokens
            .into_iter()
            .enumerate()
            .map(|(i, t)| self.account_snapshot(i, t, threshold, now_wall, now_mono))
            .collect();
        let mut out = Map::new();
        out.insert("threshold".into(), Value::from(threshold));
        out.insert("accounts".into(), Value::Array(accounts));
        out.insert("mode".into(), Value::String("on_demand".into()));
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn threshold_parsing() {
        assert_eq!(normalize_alert_threshold(&json!("30")), 30);
        assert_eq!(normalize_alert_threshold(&json!("0")), 20);
        assert_eq!(normalize_alert_threshold(&json!("1000")), 20);
        assert_eq!(normalize_alert_threshold(&json!(true)), 20);
        assert_eq!(normalize_alert_threshold(&json!(55)), 55);
    }

    #[test]
    fn period_keeps_longest_reading() {
        let mut c = AllowanceCore::new();
        c.load_periods(None);
        for (i, s) in [16000, 7800, 1200].into_iter().enumerate() {
            c.note_period("t1", s, 1_790_000_000.0 + i as f64).unwrap();
        }
        assert_eq!(c.period("t1", 1_790_000_100.0), Some(16000));
        assert_eq!(c.period("zz", 1_790_000_100.0), None);
        // 48 小时后全部过期
        assert_eq!(c.period("t1", 1_790_000_000.0 + 48.0 * 3600.0 + 10.0), None);
    }
}
