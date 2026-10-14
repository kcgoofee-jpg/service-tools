//! 排队影子分析（`app/shadow.py`）：用真实请求日志复盘排队过程——有几位其他成员在排队、
//! 15 分钟窗口的利用率与繁忙 / 空闲状态、当天谁用满了每人 N 张。只计算，不改变实际出图顺序。
//!
//! # 没有移植的部分
//! - `replay`（先到先出 / DRR 重排）以及 `analyze` 里依赖它的 `fifo` / `drr` / `by_accounts` 三个输出字段：
//!   DRR / HTB 调度器已从线上移除，`replay` 是遗留代码（2026-10-14 评审）。
//!   `analyze` 的输出因此少了 `fifo`、`drr`、`by_accounts`，其余字段（`samples` `actual` `contention` `load`
//!   `quota` `params`）与旧版逐位一致，键顺序也一致。
//! - `collect`（I/O）。它的查询：
//!
//!   ```sql
//!   SELECT ts, key_id, key_name, status, images, wait_ms, dur_ms FROM usage_log
//!   WHERE ts>=? AND kind IN ('image','image_stream') AND status IN ('ok','error')
//!     AND <NOT_TEST>
//!   ORDER BY ts
//!   ```
//!
//!   `since = time.time() - hours * 3600`（后台接口默认 24 小时）；取最近 `MAX_ROWS` 条；
//!   `slots = sum(可用账号.image_slots.limit) or 1`，`interval = float(image_min_interval)`，
//!   `key_interval = float(key_image_min_interval)`，`accounts = max(1, 可用账号数)`，
//!   `account_cap = guard.values["account_daily_cap"]`，`per_person = int(float(设置 quota_ceiling))`（缺省 0）。
//!
//! # 谁在用这些输出
//! 旧版只有后台接口 `GET /api/admin/shadow` 原样返回它；内核校验（kernel / modules）里已经没有
//! 「③ 公平调度：没有 Key 垄断队列（影子统计 ≤ 60%）」这一项（`modules.py` 只剩一行悬空注释，
//! `SYSTEM-FACTS.md` 的描述已过期）。如果以后要恢复该校验，需要的是 `contention.contended_share` 与 `actual`。

use std::collections::HashMap;

use serde::Serialize;

use crate::pyc;

/// 每把 Key 每天保底张数（V4.5）；线上以「账号保护与排队」的设置为准。
pub const BASE_QUOTA: i64 = 100;
/// 空闲时最多借到的张数。
pub const CEIL_QUOTA: i64 = 300;
/// 每个上游账号每天总上限。
pub const ACCOUNT_DAILY_CAP: i64 = 1000;
pub const BUSY_UTIL: f64 = 0.70;
pub const IDLE_UTIL: f64 = 0.50;
/// 利用率按 15 分钟窗口计算。
pub const UTIL_WINDOW: i64 = 15 * 60;
/// 当天出图 ≤10 张算轻度用户。
pub const LIGHT_USER_MAX: i64 = 10;
/// `collect` 只取最近这么多行（contention 是 O(n²)）。
pub const MAX_ROWS: usize = 4000;

/// 日志行：`(ts, key_id, key_name, status, images, wait_ms, dur_ms)`。
#[derive(Debug, Clone)]
pub struct ShadowRow {
    pub ts: f64,
    pub key_id: Option<i64>,
    pub name: Option<String>,
    pub status: String,
    pub images: i64,
    pub wait_ms: i64,
    pub dur_ms: i64,
}

/// `_requests` 的一项（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Req {
    pub key: i64,
    pub name: Option<String>,
    pub arrival: f64,
    pub start: f64,
    pub dur: f64,
    pub wait: f64,
    pub ok: bool,
    pub images: i64,
}

/// `_pct(values, q)`：向上取整的最近秩百分位，结果保留 1 位小数。
pub fn pct(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (q * v.len() as f64).ceil() as i64;
    let idx = (v.len() as i64 - 1).min((rank - 1).max(0));
    Some(pyc::round(v[idx as usize], 1))
}

/// `_requests(rows)`：只取有耗时的出图请求，按到达时间稳定排序。
pub fn requests(rows: &[ShadowRow]) -> Vec<Req> {
    let mut out: Vec<Req> = rows
        .iter()
        .filter(|r| r.dur_ms > 0)
        .filter_map(|r| {
            let key = r.key_id?;
            let start = r.ts - r.dur_ms as f64 / 1000.0;
            Some(Req {
                key,
                name: r.name.clone(),
                arrival: start - r.wait_ms as f64 / 1000.0,
                start,
                dur: r.dur_ms as f64 / 1000.0,
                wait: r.wait_ms as f64 / 1000.0,
                ok: r.status == "ok",
                images: r.images,
            })
        })
        .collect();
    // Python 的 list.sort 是稳定的；sort_by 同样稳定
    out.sort_by(|a, b| {
        a.arrival
            .partial_cmp(&b.arrival)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// 「其他成员在排队」人数的直方图（键 `"0"` `"1"` `"2"` `"3+"`）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Hist {
    #[serde(rename = "0")]
    pub zero: usize,
    #[serde(rename = "1")]
    pub one: usize,
    #[serde(rename = "2")]
    pub two: usize,
    #[serde(rename = "3+")]
    pub three_plus: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Contention {
    pub hist: Hist,
    pub dispatches: usize,
    pub peak_waiting_members: usize,
    pub contended_share: Option<f64>,
}

/// `contention(reqs)`：每次真正发往上游时，还有几位「其他成员」在排队。
pub fn contention(reqs: &[Req]) -> Contention {
    let mut hist = Hist {
        zero: 0,
        one: 0,
        two: 0,
        three_plus: 0,
    };
    let mut peak = 0usize;
    for r in reqs {
        let mut others: Vec<i64> = reqs
            .iter()
            .filter(|o| o.key != r.key && o.arrival <= r.start && r.start < o.start)
            .map(|o| o.key)
            .collect();
        others.sort_unstable();
        others.dedup();
        let n = others.len();
        peak = peak.max(n);
        match n {
            0 => hist.zero += 1,
            1 => hist.one += 1,
            2 => hist.two += 1,
            _ => hist.three_plus += 1,
        }
    }
    let total = hist.zero + hist.one + hist.two + hist.three_plus;
    Contention {
        contended_share: (total > 0)
            .then(|| pyc::round((total - hist.zero) as f64 / total as f64, 3)),
        hist,
        dispatches: total,
        peak_waiting_members: peak,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Window {
    pub t: i64,
    pub util: f64,
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Load {
    pub windows: Vec<Window>,
    pub busy_minutes: i64,
    pub peak_util: Option<f64>,
    pub state: &'static str,
}

/// Python `max(a, b)`：只有 b 严格更大才换。
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `busy_windows(reqs, slots=, interval=)`：15 分钟窗口利用率 + 滞回状态机——多少时间处于「繁忙」。
pub fn busy_windows(reqs: &[Req], slots: i64, interval: f64) -> Load {
    if reqs.is_empty() {
        return Load {
            windows: Vec::new(),
            busy_minutes: 0,
            peak_util: None,
            state: "idle",
        };
    }
    let start = (reqs[0].arrival / UTIL_WINDOW as f64).floor() as i64 * UTIL_WINDOW;
    let end = reqs.iter().map(|r| r.start).fold(f64::NEG_INFINITY, py_max);
    let mut windows = Vec::new();
    let mut state = "idle";
    let mut busy = 0;
    let mut w = start;
    while w as f64 <= end {
        let used = pyc::sum(
            reqs.iter()
                .filter(|r| w as f64 <= r.start && r.start < (w + UTIL_WINDOW) as f64)
                .map(|r| py_max(interval, r.dur)),
        );
        let util = used / (UTIL_WINDOW * slots.max(1)) as f64;
        if state == "idle" && util > BUSY_UTIL {
            state = "busy";
        } else if state == "busy" && util < IDLE_UTIL {
            state = "idle";
        }
        if state == "busy" {
            busy += UTIL_WINDOW / 60;
        }
        windows.push(Window {
            t: w,
            util: pyc::round(util, 3),
            state,
        });
        w += UTIL_WINDOW;
    }
    let peak = windows
        .iter()
        .map(|x| x.util)
        .fold(f64::NEG_INFINITY, py_max);
    Load {
        windows,
        busy_minutes: busy,
        peak_util: Some(peak),
        state,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemberRow {
    pub key: i64,
    pub name: Option<String>,
    pub images: i64,
    pub full: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub members: Vec<MemberRow>,
    pub total: i64,
    pub account_cap: i64,
    pub cap_used: Option<f64>,
    pub per_person: i64,
    pub full: usize,
}

/// `per_member(day_images, names, accounts=, per_person=, account_cap=)`：当天每把 Key 用了多少、
/// 有没有用满每人 N 张，以及账号总量相对每日上限。`day_images` 按插入顺序（同样张数时保持这个顺序）。
pub fn per_member(
    day_images: &[(i64, i64)],
    names: &HashMap<i64, Option<String>>,
    accounts: i64,
    per_person: i64,
    account_cap: i64,
) -> Quota {
    let mut sorted: Vec<(i64, i64)> = day_images.to_vec();
    sorted.sort_by_key(|&(_, n)| std::cmp::Reverse(n)); // 稳定排序，同张数保持原顺序
    let rows: Vec<MemberRow> = sorted
        .iter()
        .map(|&(key, n)| MemberRow {
            key,
            // names.get(key, f"#{key}")：有记录就用记录（哪怕是 None）
            name: names
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Some(format!("#{key}"))),
            images: n,
            full: per_person != 0 && n >= per_person,
        })
        .collect();
    let total: i64 = day_images.iter().map(|&(_, n)| n).sum();
    let cap = if account_cap != 0 {
        account_cap * accounts.max(1)
    } else {
        0
    };
    Quota {
        full: rows.iter().filter(|r| r.full).count(),
        members: rows,
        total,
        account_cap: cap,
        cap_used: (cap != 0).then(|| pyc::round(total as f64 / cap as f64, 3)),
        per_person,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Actual {
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
    pub light_p90: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShadowParams {
    pub per_person: i64,
    pub account_cap: i64,
    pub busy_util: f64,
    pub idle_util: f64,
    pub slots: i64,
    pub interval: f64,
}

/// 分析输入参数。
#[derive(Debug, Clone, Copy)]
pub struct ShadowOptions {
    pub slots: i64,
    /// 全站请求最小间隔（秒）。
    pub interval: f64,
    /// 旧版只在 `replay` 里用它，所以这里不参与计算。
    pub key_interval: f64,
    pub accounts: i64,
    /// 每人每天 N 张。
    pub per_person: i64,
    pub account_cap: i64,
}

impl Default for ShadowOptions {
    /// 旧版 `analyze` 的默认值（线上 `collect` 总是显式传值）。
    fn default() -> Self {
        Self {
            slots: 1,
            interval: 15.0,
            key_interval: 15.0,
            accounts: 1,
            per_person: 0,
            account_cap: ACCOUNT_DAILY_CAP,
        }
    }
}

/// `analyze` 的输出——**不含** `fifo` / `drr` / `by_accounts`（见模块文档）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShadowReport {
    pub samples: usize,
    pub actual: Actual,
    pub contention: Contention,
    pub load: Load,
    pub quota: Quota,
    pub params: ShadowParams,
}

pub fn analyze(rows: &[ShadowRow], opts: &ShadowOptions) -> ShadowReport {
    let reqs = requests(rows);
    // 当天各 Key 的出图张数（只算成功的请求；包括没有耗时的旧日志）
    let mut day_images: Vec<(i64, i64)> = Vec::new();
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut names: HashMap<i64, Option<String>> = HashMap::new();
    for r in rows {
        if r.status == "ok"
            && let Some(key) = r.key_id
        {
            match index.get(&key) {
                Some(&i) => day_images[i].1 += r.images,
                None => {
                    index.insert(key, day_images.len());
                    day_images.push((key, r.images));
                }
            }
            names.insert(key, r.name.clone());
        }
    }
    let actual: Vec<f64> = reqs.iter().map(|r| r.wait).collect();
    let is_light = |key: i64| {
        day_images
            .iter()
            .any(|&(k, n)| k == key && n <= LIGHT_USER_MAX)
    };
    let light: Vec<f64> = reqs
        .iter()
        .filter(|r| is_light(r.key))
        .map(|r| r.wait)
        .collect();
    ShadowReport {
        samples: reqs.len(),
        actual: Actual {
            p50: pct(&actual, 0.5),
            p90: pct(&actual, 0.9),
            max: pct(&actual, 1.0),
            light_p90: pct(&light, 0.9),
        },
        contention: contention(&reqs),
        load: busy_windows(&reqs, opts.slots, opts.interval),
        quota: per_member(
            &day_images,
            &names,
            opts.accounts,
            opts.per_person,
            opts.account_cap,
        ),
        params: ShadowParams {
            per_person: opts.per_person,
            account_cap: opts.account_cap,
            busy_util: BUSY_UTIL,
            idle_util: IDLE_UTIL,
            slots: opts.slots,
            interval: opts.interval,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: f64, key: i64, wait: f64, dur: f64) -> ShadowRow {
        ShadowRow {
            ts,
            key_id: Some(key),
            name: Some(format!("k{key}")),
            status: "ok".into(),
            images: 1,
            wait_ms: (wait * 1000.0) as i64,
            dur_ms: (dur * 1000.0) as i64,
        }
    }

    #[test]
    fn quiet_day_has_no_contention() {
        let rows: Vec<_> = (0..12)
            .map(|i| row(1000.0 + 300.0 * i as f64, 1 + i % 3, 0.0, 7.0))
            .collect();
        let r = analyze(&rows, &ShadowOptions::default());
        assert_eq!(r.contention.contended_share, Some(0.0));
        assert_eq!(r.quota.full, 0);
        assert_eq!(r.load.busy_minutes, 0);
    }

    #[test]
    fn untimed_rows_count_for_quota_not_replay() {
        let mut old = row(100.0, 1, 0.0, 0.0);
        old.dur_ms = 0;
        let rows = vec![old, row(200.0, 1, 0.0, 7.0)];
        let r = analyze(&rows, &ShadowOptions::default());
        assert_eq!(r.samples, 1);
        assert_eq!(r.quota.total, 2);
    }
}
