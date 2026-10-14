//! 上游表现分析（`app/perf.py`）：生成速度、排队延迟、限流 / 失败、实际产能，按 V4.5 / V5 分开统计。
//!
//! 「最近 1 小时 / 24 小时」与「过去 7 天（不含最近 24 小时）」的基线对比；变慢、限流增多、失败率上升、
//! 账号受限时给出提示（由维护循环私信站长）。纯计算：数据由调用方查好传进来。
//!
//! # `collect`（不移植，I/O）
//! 旧版 `perf.collect(state, now)` 先查日志：
//!
//! ```sql
//! SELECT ts, model, status, images, wait_ms, dur_ms, up_status, detail FROM usage_log
//! WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error')
//!   AND <NOT_TEST>            -- 排除测试 Key
//! ORDER BY ts
//! ```
//!
//! 参数 `since = now - BASELINE_DAYS * DAY`。再取号池里 `usable` 的上游账号：
//! `slots = sum(账号.image_slots.limit) or 1`；`hourly_cap = guard.hourly_cap(now) * max(1, 账号数)`（没有 guard 则 0）；
//! `site_interval = float(settings.image_min_interval)`，`key_interval = float(settings.key_image_min_interval)`，
//! 最后调用 [`analyze`]。旧版 `analyze` 有默认参数（`site_interval=15` 是 int），但 `collect` 总是传 float，
//! 所以输出里 `site_interval` / `key_interval` / `generated_at` 一律是 float。

use serde::{Serialize, Serializer};

use crate::pyc;

pub const FAMILIES: [&str; 3] = ["V4.5", "V5", "其他"];
pub const HOUR: i64 = 3600;
pub const DAY: i64 = 24 * HOUR;
pub const BASELINE_DAYS: i64 = 7;
/// 少于这么多样本不下结论。
pub const MIN_SAMPLES: usize = 5;
/// 中位生成耗时比基线慢 50% 视为变慢。
pub const SLOW_RATIO: f64 = 1.5;
/// 最近 1 小时 ≥3 次 429。
pub const THROTTLE_COUNT_1H: usize = 3;
/// 或 429 占比 ≥10%。
pub const THROTTLE_SHARE: f64 = 0.10;
/// 成功率比基线低 10 个百分点。
pub const FAIL_DROP: f64 = 0.10;
/// 最近 1 小时成功率低于 80%。
pub const FAIL_FLOOR_1H: f64 = 0.80;

/// 日志行：`(ts, model, status, images, wait_ms, dur_ms, up_status, detail)`。
#[derive(Debug, Clone)]
pub struct PerfRow {
    pub ts: f64,
    pub model: Option<String>,
    pub status: String,
    pub images: i64,
    pub wait_ms: i64,
    pub dur_ms: i64,
    pub up_status: i64,
    pub detail: Option<String>,
}

/// `family(model)`：`(model or "").strip().lower()` 的前缀判断（先判 V5 再判 V4.5，与旧版顺序一致）。
pub fn family(model: Option<&str>) -> &'static str {
    let m = pyc::strip(model.unwrap_or("")).to_lowercase();
    if m.starts_with("nai-diffusion-5") {
        "V5"
    } else if m.starts_with("nai-diffusion-4-5") {
        "V4.5"
    } else {
        "其他"
    }
}

/// `_pct(values, q)`：向上取整的最近秩百分位。
pub fn pct(values: &[i64], q: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = (q * v.len() as f64).ceil() as i64;
    let idx = (v.len() as i64 - 1).min((rank - 1).max(0));
    Some(v[idx as usize])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ok,
    Throttled,
    Auth,
    Server,
    Other,
}

impl Kind {
    /// 旧版 `_kind` 返回的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Throttled => "429",
            Kind::Auth => "auth",
            Kind::Server => "5xx",
            Kind::Other => "other",
        }
    }
}

/// `_kind(status, up_status, detail)`
pub fn kind(status: &str, up_status: i64, detail: Option<&str>) -> Kind {
    if status == "ok" {
        return Kind::Ok;
    }
    if up_status == 429 || detail.unwrap_or("").contains("429") {
        return Kind::Throttled;
    }
    if matches!(up_status, 401..=403) {
        return Kind::Auth;
    }
    if up_status >= 500 {
        return Kind::Server;
    }
    Kind::Other
}

fn row_kind(r: &PerfRow) -> Kind {
    kind(&r.status, r.up_status, r.detail.as_deref())
}

/// Python `max(a, b)`：只有 b 严格更大才换（NaN 时保留 a）。
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `_stats` 的输出（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerfStats {
    pub requests: usize,
    pub ok: usize,
    pub images: i64,
    pub rate_limited: usize,
    pub auth: usize,
    pub server: usize,
    pub other: usize,
    pub success_rate: Option<f64>,
    pub samples: usize,
    pub p50_ms: Option<i64>,
    pub p90_ms: Option<i64>,
    pub avg_wait_ms: Option<i64>,
    pub p90_wait_ms: Option<i64>,
    pub actual_per_hour: Option<f64>,
    pub capacity_per_hour: Option<i64>,
    pub member_capacity_per_hour: Option<i64>,
}

/// 分析参数。
#[derive(Debug, Clone, Copy)]
pub struct PerfParams {
    /// 全站出图通道数（可用上游账号的并发之和，至少 1）。
    pub slots: i64,
    /// 全站请求最小间隔（秒）。
    pub site_interval: f64,
    /// 单个 Key 的请求最小间隔（秒）。
    pub key_interval: f64,
    /// 全站每小时出图上限（账号保护；0 = 不按它封顶）。
    pub hourly_cap: i64,
}

impl Default for PerfParams {
    /// 旧版 `analyze` 的默认值（线上 `collect` 总是显式传值）。
    fn default() -> Self {
        Self {
            slots: 1,
            site_interval: 15.0,
            key_interval: 15.0,
            hourly_cap: 0,
        }
    }
}

/// `_stats(rows, slots, site_interval, key_interval, hours, hourly_cap)`。
/// `hours`：这一窗口的小时数（算实际每小时张数）；`hourly_cap`：全站每小时上限（产能不能超过它）。
pub fn stats(rows: &[&PerfRow], params: &PerfParams, hours: f64) -> PerfStats {
    let ok: Vec<&&PerfRow> = rows.iter().filter(|r| r.status == "ok").collect();
    let kinds: Vec<Kind> = rows.iter().map(|r| row_kind(r)).collect();
    let count = |k: Kind| kinds.iter().filter(|&&x| x == k).count();
    let durs: Vec<i64> = ok
        .iter()
        .filter(|r| r.dur_ms > 0)
        .map(|r| r.dur_ms)
        .collect();
    let waits: Vec<i64> = ok
        .iter()
        .filter(|r| r.dur_ms > 0)
        .map(|r| r.wait_ms)
        .collect();
    let total = rows.len();
    let p50 = pct(&durs, 0.5);
    let images: i64 = ok.iter().map(|r| r.images).sum();
    let mut out = PerfStats {
        requests: total,
        ok: ok.len(),
        images,
        rate_limited: count(Kind::Throttled),
        auth: count(Kind::Auth),
        server: count(Kind::Server),
        other: count(Kind::Other),
        success_rate: (total > 0).then(|| pyc::round(ok.len() as f64 / total as f64, 3)),
        samples: durs.len(),
        p50_ms: p50,
        p90_ms: pct(&durs, 0.9),
        avg_wait_ms: (!waits.is_empty())
            .then(|| (waits.iter().sum::<i64>() as f64 / waits.len() as f64) as i64),
        p90_wait_ms: pct(&waits, 0.9),
        // 实际每小时出图：随负载变化的真实数字（产能是上限，平时不动；这个才反映「现在用了多少」）
        actual_per_hour: (hours > 0.0 && total > 0).then(|| pyc::round(images as f64 / hours, 1)),
        capacity_per_hour: None,
        member_capacity_per_hour: None,
    };
    if let Some(p50) = p50.filter(|&p| p != 0) {
        // 产能上限 = min(「生成耗时 / 请求间隔」决定的速度, 全站每小时上限)。
        // 生成（几秒）通常比 15s 间隔快，所以间隔那一项常年是 3600/15=240；真正卡住的是每小时上限。
        let secs = p50 as f64 / 1000.0;
        let per_slot = HOUR as f64 / py_max(secs, params.site_interval);
        let mut cap = (per_slot * params.slots.max(1) as f64) as i64;
        if params.hourly_cap > 0 {
            cap = cap.min(params.hourly_cap);
        }
        out.capacity_per_hour = Some(cap);
        out.member_capacity_per_hour =
            Some(((HOUR as f64 / py_max(secs, params.key_interval)) as i64).min(cap));
    }
    out
}

/// 按小时的趋势桶（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerfBucket {
    pub t: i64,
    pub ok: i64,
    pub err: i64,
    pub r429: i64,
    pub images: i64,
    pub p50_ms: Option<i64>,
}

/// CPython 的 `float // float`（`float_floor_div`）：先用 fmod 求余再「吸附」到最近整数，
/// 与 `(a / b).floor()` 在个别边界值上不同，这里照搬算法。
pub fn py_floordiv_f(a: f64, b: f64) -> f64 {
    let modulo = a % b;
    let mut div = (a - modulo) / b;
    // 余数与除数异号时，商再减 1（余数本身这里不需要）
    if modulo != 0.0 && (b < 0.0) != (modulo < 0.0) {
        div -= 1.0;
    }
    if div != 0.0 {
        let mut fl = div.floor();
        if div - fl > 0.5 {
            fl += 1.0;
        }
        fl
    } else {
        0.0_f64.copysign(a / b)
    }
}

/// `_hourly(rows, now)`：最近 24 个整点小时的桶。
pub fn hourly(rows: &[&PerfRow], now: f64) -> Vec<PerfBucket> {
    let now_i = now.trunc() as i64;
    let start = pyc::floordiv(now_i, HOUR) * HOUR - 23 * HOUR;
    struct Acc {
        b: PerfBucket,
        durs: Vec<i64>,
    }
    let mut buckets: Vec<Acc> = (0..24)
        .map(|i| Acc {
            b: PerfBucket {
                t: start + i * HOUR,
                ok: 0,
                err: 0,
                r429: 0,
                images: 0,
                p50_ms: None,
            },
            durs: Vec::new(),
        })
        .collect();
    for r in rows {
        let q = py_floordiv_f(r.ts - start as f64, HOUR as f64);
        if !(0.0..24.0).contains(&q) {
            continue;
        }
        let acc = &mut buckets[q as usize];
        if r.status == "ok" {
            acc.b.ok += 1;
            acc.b.images += r.images;
            if r.dur_ms > 0 {
                acc.durs.push(r.dur_ms);
            }
        } else {
            acc.b.err += 1;
            if row_kind(r) == Kind::Throttled {
                acc.b.r429 += 1;
            }
        }
    }
    buckets
        .into_iter()
        .map(|mut a| {
            a.b.p50_ms = pct(&a.durs, 0.5);
            a.b
        })
        .collect()
}

/// 提示（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PerfFlag {
    pub code: &'static str,
    pub family: String,
    pub level: &'static str,
    pub text: String,
}

/// `_flags(fam, h1, d1, base)`
pub fn flags(fam: &str, h1: &PerfStats, d1: &PerfStats, base: &PerfStats) -> Vec<PerfFlag> {
    let mut out = Vec::new();
    let mut add = |code: &'static str, level: &'static str, text: String| {
        out.push(PerfFlag {
            code,
            family: fam.to_string(),
            level,
            text: format!("{fam}：{text}"),
        });
    };

    if d1.auth > 0 {
        add(
            "account",
            "bad",
            format!(
                "24 小时内上游返回 401/402/403 共 {} 次，账号可能被限制，请登录 NovelAI 检查。",
                d1.auth
            ),
        );
    }
    let r429_share = if h1.requests > 0 {
        h1.rate_limited as f64 / h1.requests as f64
    } else {
        0.0
    };
    if h1.rate_limited >= THROTTLE_COUNT_1H
        || (h1.requests >= MIN_SAMPLES && r429_share >= THROTTLE_SHARE)
    {
        add(
            "throttle",
            "warn",
            format!(
                "最近 1 小时被上游限流（429）{} 次，占 {}%。",
                h1.rate_limited,
                pyc::round0(r429_share * 100.0)
            ),
        );
    }
    let use_h1 = h1.samples >= MIN_SAMPLES;
    let recent = if use_h1 { h1 } else { d1 };
    let label = if use_h1 {
        "最近 1 小时"
    } else {
        "最近 24 小时"
    };
    if recent.samples >= MIN_SAMPLES
        && base.samples >= MIN_SAMPLES
        && base.p50_ms.is_some_and(|p| p != 0)
    {
        let (rp, bp) = (
            recent.p50_ms.unwrap_or(0) as f64,
            base.p50_ms.unwrap_or(0) as f64,
        );
        let ratio = rp / bp;
        if ratio >= SLOW_RATIO {
            add(
                "slow",
                "warn",
                format!(
                    "{label}生成 P50（中位）耗时 {:.1}s，是过去 7 天的 {ratio:.1} 倍（{:.1}s）。",
                    rp / 1000.0,
                    bp / 1000.0
                ),
            );
        }
    }
    if h1.requests >= MIN_SAMPLES && h1.success_rate.is_some_and(|s| s < FAIL_FLOOR_1H) {
        add(
            "fail",
            "bad",
            format!(
                "最近 1 小时成功率 {}%。",
                pyc::round0(h1.success_rate.unwrap_or(0.0) * 100.0)
            ),
        );
    } else if d1.requests >= 10
        && base.requests >= 10
        && let (Some(ds), Some(bs)) = (d1.success_rate, base.success_rate)
        && ds < bs - FAIL_DROP
    {
        add(
            "fail",
            "warn",
            format!(
                "24 小时成功率 {}%，低于过去 7 天的 {}%。",
                pyc::round0(ds * 100.0),
                pyc::round0(bs * 100.0)
            ),
        );
    }
    out
}

/// 一个机型家族的分析结果。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerfFamily {
    pub h1: PerfStats,
    pub d1: PerfStats,
    pub baseline: PerfStats,
    pub hourly: Vec<PerfBucket>,
    pub baseline_ready: bool,
}

/// 按 `FAMILIES` 顺序、只含有数据的家族；序列化成 JSON 对象（键顺序即家族顺序）。
#[derive(Debug, Clone, PartialEq)]
pub struct Families(pub Vec<(&'static str, PerfFamily)>);

impl Serialize for Families {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

/// `analyze` 的输出（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerfReport {
    pub families: Families,
    pub flags: Vec<PerfFlag>,
    pub slots: i64,
    pub site_interval: f64,
    pub key_interval: f64,
    pub generated_at: f64,
}

/// `analyze(rows, now, ...)`：`rows` 应覆盖最近 `BASELINE_DAYS` 天。
pub fn analyze(rows: &[PerfRow], now: f64, params: &PerfParams) -> PerfReport {
    let mut grouped: [Vec<&PerfRow>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for r in rows {
        let f = family(r.model.as_deref());
        let i = FAMILIES.iter().position(|&x| x == f).unwrap_or(2);
        grouped[i].push(r);
    }
    let mut families = Vec::new();
    let mut all_flags = Vec::new();
    for (fam, items) in FAMILIES.iter().zip(&grouped) {
        if items.is_empty() {
            continue;
        }
        let pick = |pred: &dyn Fn(f64) -> bool| -> Vec<&PerfRow> {
            items.iter().copied().filter(|r| pred(r.ts)).collect()
        };
        let h1 = stats(&pick(&|ts| ts >= now - HOUR as f64), params, 1.0);
        let d1_rows = pick(&|ts| ts >= now - DAY as f64);
        let d1 = stats(&d1_rows, params, 24.0);
        let base = stats(
            &pick(&|ts| now - (BASELINE_DAYS * DAY) as f64 <= ts && ts < now - DAY as f64),
            params,
            ((BASELINE_DAYS - 1) * 24) as f64,
        );
        all_flags.extend(flags(fam, &h1, &d1, &base));
        families.push((
            *fam,
            PerfFamily {
                hourly: hourly(&d1_rows, now),
                baseline_ready: base.samples >= MIN_SAMPLES,
                h1,
                d1,
                baseline: base,
            },
        ));
    }
    PerfReport {
        families: Families(families),
        flags: all_flags,
        slots: params.slots,
        site_interval: params.site_interval,
        key_interval: params.key_interval,
        generated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_prefixes() {
        assert_eq!(family(Some(" NAI-Diffusion-5-curated ")), "V5");
        assert_eq!(family(Some("nai-diffusion-4-5-full")), "V4.5");
        assert_eq!(family(Some("nai-diffusion-4-curated")), "其他");
        assert_eq!(family(None), "其他");
    }

    #[test]
    fn percentile_is_nearest_rank_up() {
        assert_eq!(pct(&[], 0.5), None);
        assert_eq!(pct(&[3, 1, 2], 0.5), Some(2));
        assert_eq!(pct(&[1, 2, 3, 4], 0.5), Some(2));
        assert_eq!(pct(&[1, 2, 3, 4], 0.9), Some(4));
        assert_eq!(pct(&[7], 0.0), Some(7));
    }

    #[test]
    fn floordiv_matches_cpython_snap() {
        assert_eq!(py_floordiv_f(7200.0, 3600.0), 2.0);
        assert_eq!(py_floordiv_f(-1.0, 3600.0), -1.0);
        assert_eq!(py_floordiv_f(3599.999, 3600.0), 0.0);
    }
}
