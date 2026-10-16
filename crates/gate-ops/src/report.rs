//! `nai-ops report`：只读的「周报」——每把 Key 的滥用 / 健康信号。
//!
//! 输出里**只有 Key 编号、显示名和计数**：不打印提示词、UA 原文、IP、Discord ID。
//! 内容风险一节的词表放在服务器上的外部文件（默认 `/opt/owl/risk-terms.txt`，环境变量 `NAI_RISK_TERMS`
//! 或 `--terms` 可改），仓库只带一个全是注释的示例（`deploy/risk-terms.example.txt`）；报告里只有每个分类的
//! 命中条数和 Key 编号。输出顺序固定（按 Key 编号），配合合成数据库可做快照测试。
//!
//! 词表格式：`# 分类名` 开一个分类，其后每行一条正则（不区分大小写）；`##` 开头是注释；空行忽略。
//! 各项阈值见下面的常量。

use crate::util;
use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use regex::{Regex, RegexBuilder};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const DEFAULT_DAYS: i64 = 7;
pub const DEFAULT_TERMS: &str = "/opt/owl/risk-terms.txt";

/// key_rate + key_busy 拒绝次数 ≥ 这个数才列出。
pub const HAMMER_MIN: usize = 20;
/// 两次请求间隔不超过这么多秒，算「连续不断」的一段。
pub const RUN_GAP_S: f64 = 120.0;
/// 最长连续一段 ≥ 这么多分钟才列出。
pub const LONG_RUN_MIN: f64 = 180.0;
/// 一周里有这么多天碰到个人额度上限才列出。
pub const SATURATION_DAYS_MIN: usize = 3;
/// 提示词至少这么长才参与「多 Key 相同提示词」比对。
pub const LONG_PROMPT_CHARS: usize = 80;
/// UA 摘要至少这么长才算「具体设备」（短的只是客户端名称，很多人共用）。
pub const SPECIFIC_UA_CHARS: usize = 24;
/// Discord 账号在领 Key 时不足这么多天算新号。
pub const NEW_ACCOUNT_DAYS: f64 = 7.0;
/// 领取突发：这么多秒内……
pub const BURST_WINDOW_S: f64 = 600.0;
/// ……领了不少于这么多把。
pub const BURST_MIN: usize = 5;
/// 一行里最多列几把 Key。
const LIST_MAX: usize = 15;

#[derive(Debug, Clone)]
pub struct ReportArgs {
    pub days: i64,
    pub terms: Option<PathBuf>,
    /// 统计窗口的结束时刻（Unix 秒）
    pub until: f64,
}

// ---------------------------------------------------------------- 纯逻辑

pub fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// 最长的「连续不断」一段：返回 (张数, 持续秒数)。`ts` 需升序。
pub fn longest_run(ts: &[f64], max_gap: f64) -> (usize, f64) {
    let mut best = (0usize, 0.0f64);
    let mut start = 0usize;
    for i in 0..ts.len() {
        if i > 0 && ts[i] - ts[i - 1] > max_gap {
            start = i;
        }
        let cand = (i - start + 1, ts[i] - ts[start]);
        if cand.1 > best.1 || (cand.1 == best.1 && cand.0 > best.0) {
            best = cand;
        }
    }
    best
}

/// 设备类别（凭 UA 摘要里的关键词；认不出归「其它」，不参与「多设备」判断）。
pub fn device_class(client: &str) -> &'static str {
    let c = client.to_ascii_lowercase();
    if c.contains("iphone") || c.contains("ipad") || c.contains("ios") {
        "ios"
    } else if c.contains("android") {
        "android"
    } else if c.contains("windows") {
        "windows"
    } else if c.contains("macintosh") || c.contains("mac os") || c.contains("macos") {
        "mac"
    } else if c.contains("linux") || c.contains("x11") {
        "linux"
    } else {
        "other"
    }
}

/// Discord 雪花 ID → 账号创建时间（Unix 秒）。
pub fn snowflake_secs(id: &str) -> Option<f64> {
    let n: u64 = id.trim().parse().ok()?;
    Some(((n >> 22) + 1_420_070_400_000) as f64 / 1000.0)
}

/// 领取突发：滑动窗口里 ≥ `min` 次领取。`regs` 需按时间升序。返回 (起点, 张数, Key 编号)。
pub fn claim_bursts(regs: &[(f64, i64)], window: f64, min: usize) -> Vec<(f64, usize, Vec<i64>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < regs.len() {
        let mut j = i;
        while j + 1 < regs.len() && regs[j + 1].0 - regs[i].0 <= window {
            j += 1;
        }
        if j - i + 1 >= min {
            out.push((
                regs[i].0,
                j - i + 1,
                regs[i..=j].iter().map(|r| r.1).collect(),
            ));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// 解析词表：返回 (分类, 正则) 与无法编译的行号。
pub fn parse_terms(text: &str) -> (Vec<(String, Regex)>, Vec<usize>) {
    let mut cat = "未分类".to_string();
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("##") {
            continue;
        }
        if let Some(name) = line.strip_prefix('#') {
            let name = name.trim();
            if !name.is_empty() {
                cat = name.to_string();
            }
            continue;
        }
        match RegexBuilder::new(line)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
        {
            Ok(re) => out.push((cat.clone(), re)),
            Err(_) => bad.push(i + 1),
        }
    }
    (out, bad)
}

fn clean(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(24)
        .collect()
}

// ---------------------------------------------------------------- 取数与排版

struct Keys {
    names: BTreeMap<i64, String>,
    test: BTreeSet<i64>,
}

impl Keys {
    fn label(&self, id: i64) -> String {
        match self.names.get(&id) {
            Some(n) if !n.is_empty() => format!("#{id} {n}"),
            _ => format!("#{id}"),
        }
    }
    fn list(&self, ids: &BTreeSet<i64>) -> String {
        let shown: Vec<String> = ids.iter().take(LIST_MAX).map(|i| self.label(*i)).collect();
        let more = ids.len().saturating_sub(LIST_MAX);
        if more > 0 {
            format!("{}……另有 {more} 把", shown.join("、"))
        } else {
            shown.join("、")
        }
    }
}

fn section(out: &mut String, title: &str, lines: Vec<String>) {
    out.push_str(&format!("\n[{title}]\n"));
    if lines.is_empty() {
        out.push_str("  （无）\n");
    }
    for l in lines {
        out.push_str(&format!("  {l}\n"));
    }
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
        [name],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

struct LogRow {
    ts: f64,
    key: i64,
    kind: String,
    reason: String,
    client: String,
}

pub fn build(conn: &Connection, args: &ReportArgs, tz: &TimeZone) -> Result<String> {
    let until = args.until;
    let since = until - args.days as f64 * 86400.0;
    let day = |ts: f64| util::fmt_ts(ts, tz, "%Y-%m-%d");

    // Key 名单
    let mut keys = Keys {
        names: BTreeMap::new(),
        test: BTreeSet::new(),
    };
    {
        let mut st = conn.prepare("SELECT id, name, is_test FROM api_keys")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for r in rows {
            let (id, name, t) = r?;
            keys.names.insert(id, clean(&name));
            if t != 0 {
                keys.test.insert(id);
            }
        }
    }

    // 用量日志
    let mut logs: Vec<LogRow> = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT ts, key_id, kind, reason, client FROM usage_log \
             WHERE ts>=? AND ts<? AND key_id IS NOT NULL ORDER BY ts, id",
        )?;
        let rows = st.query_map([since, until], |r| {
            Ok(LogRow {
                ts: r.get(0)?,
                key: r.get(1)?,
                kind: r.get(2)?,
                reason: r.get(3)?,
                client: r.get(4)?,
            })
        })?;
        for r in rows {
            let r = r?;
            if !keys.test.contains(&r.key) {
                logs.push(r);
            }
        }
    }
    let active: BTreeSet<i64> = logs.iter().map(|l| l.key).collect();

    let mut out = format!(
        "=== 周报：近 {} 天（{} ~ {}）===\n有请求的 Key：{} 把（测试 Key 已排除）\n",
        args.days,
        util::fmt_ts(since, tz, "%Y-%m-%d %H:%M"),
        util::fmt_ts(until, tz, "%Y-%m-%d %H:%M"),
        active.len()
    );

    // 1. 重试轰炸
    let mut rate: BTreeMap<i64, (usize, usize, Vec<f64>)> = BTreeMap::new();
    for l in &logs {
        let (a, b) = match l.reason.as_str() {
            "key_rate" => (1, 0),
            "key_busy" => (0, 1),
            _ => continue,
        };
        let e = rate.entry(l.key).or_default();
        e.0 += a;
        e.1 += b;
        e.2.push(l.ts);
    }
    let mut lines = Vec::new();
    for (k, (a, b, ts)) in &rate {
        if a + b < HAMMER_MIN {
            continue;
        }
        let mut gaps: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
        let med = median(&mut gaps).map_or("n/a".into(), |m| format!("{m:.1}s"));
        lines.push(format!(
            "{}  key_rate={a} key_busy={b} 中位间隔={med}",
            keys.label(*k)
        ));
    }
    section(
        &mut out,
        &format!("重试轰炸：key_rate + key_busy ≥ {HAMMER_MIN}"),
        lines,
    );

    // 2. 最长连续
    let mut img_ts: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for l in logs.iter().filter(|l| l.kind.starts_with("image")) {
        img_ts.entry(l.key).or_default().push(l.ts);
    }
    let mut lines = Vec::new();
    for (k, ts) in &img_ts {
        let (n, dur) = longest_run(ts, RUN_GAP_S);
        if dur / 60.0 >= LONG_RUN_MIN {
            lines.push(format!(
                "{}  最长连续 {:.1} 小时 · {n} 张（间隔不超过 {RUN_GAP_S:.0} 秒算连续）",
                keys.label(*k),
                dur / 3600.0
            ));
        }
    }
    section(
        &mut out,
        &format!("最长连续使用 ≥ {LONG_RUN_MIN:.0} 分钟"),
        lines,
    );

    // 3. 额度打满的天数
    let mut sat: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for l in logs.iter().filter(|l| l.reason == "member_quota") {
        sat.entry(l.key).or_default().insert(day(l.ts));
    }
    let lines = sat
        .iter()
        .filter(|(_, d)| d.len() >= SATURATION_DAYS_MIN)
        .map(|(k, d)| format!("{}  {} 天碰到个人额度上限", keys.label(*k), d.len()))
        .collect();
    section(
        &mut out,
        &format!("个人额度打满的天数 ≥ {SATURATION_DAYS_MIN}"),
        lines,
    );

    // 4. 同一天多种设备类别
    let mut classes: BTreeMap<(i64, String), BTreeSet<&'static str>> = BTreeMap::new();
    for l in logs.iter().filter(|l| l.kind.starts_with("image")) {
        let c = device_class(&l.client);
        if c != "other" {
            classes.entry((l.key, day(l.ts))).or_default().insert(c);
        }
    }
    let mut multi: BTreeMap<i64, usize> = BTreeMap::new();
    for ((k, _), set) in &classes {
        if set.len() >= 2 {
            *multi.entry(*k).or_default() += 1;
        }
    }
    let lines = multi
        .iter()
        .map(|(k, n)| format!("{}  {n} 天里有出现 ≥ 2 类设备的", keys.label(*k)))
        .collect();
    section(&mut out, "同一天多种设备类别", lines);

    // 5. 多把 Key 共用具体设备 UA（只显示短哈希）
    let mut ua: BTreeMap<String, (BTreeSet<i64>, &'static str)> = BTreeMap::new();
    for l in logs
        .iter()
        .filter(|l| l.client.chars().count() >= SPECIFIC_UA_CHARS)
    {
        let h = hex::encode(Sha256::digest(l.client.as_bytes()))[..8].to_string();
        let e = ua
            .entry(h)
            .or_insert_with(|| (BTreeSet::new(), device_class(&l.client)));
        e.0.insert(l.key);
    }
    let lines = ua
        .iter()
        .filter(|(_, (ks, _))| ks.len() >= 2)
        .map(|(h, (ks, cls))| {
            format!(
                "设备#{h}（{cls}）被 {} 把 Key 共用：{}",
                ks.len(),
                keys.list(ks)
            )
        })
        .collect();
    section(&mut out, "多把 Key 共用同一具体设备 UA", lines);

    // 6. 多把 Key 相同的长提示词（只显示短哈希）
    let mut lines = Vec::new();
    if has_table(conn, "generation_audit")? {
        let mut st = conn.prepare(
            "SELECT key_id, prompt FROM generation_audit WHERE ts>=? AND ts<? AND key_id IS NOT NULL \
             AND length(prompt)>=? ORDER BY id",
        )?;
        let rows = st.query_map(
            rusqlite::params![since, until, LONG_PROMPT_CHARS as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut by: BTreeMap<String, (BTreeSet<i64>, usize)> = BTreeMap::new();
        for r in rows {
            let (k, p) = r?;
            if keys.test.contains(&k) || p.chars().count() < LONG_PROMPT_CHARS {
                continue;
            }
            let h = hex::encode(Sha256::digest(p.trim().as_bytes()))[..8].to_string();
            let e = by.entry(h).or_default();
            e.0.insert(k);
            e.1 += 1;
        }
        for (h, (ks, n)) in by {
            if ks.len() >= 2 {
                lines.push(format!(
                    "提示词#{h}：{n} 条，出现在 {} 把 Key：{}",
                    ks.len(),
                    keys.list(&ks)
                ));
            }
        }
    }
    section(
        &mut out,
        &format!("多把 Key 相同的长提示词（≥ {LONG_PROMPT_CHARS} 字）"),
        lines,
    );

    // 7 + 8. 领取时的 Discord 账号年龄、领取突发
    let mut regs: Vec<(f64, i64, Option<f64>)> = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT created_at, key_id, discord_id FROM discord_registrations \
             WHERE created_at>=? AND created_at<? ORDER BY created_at, key_id",
        )?;
        let rows = st.query_map([since, until], |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for r in rows {
            let (ts, k, did) = r?;
            regs.push((ts, k, snowflake_secs(&did).map(|c| (ts - c) / 86400.0)));
        }
    }
    let lines = regs
        .iter()
        .filter_map(|(_, k, age)| age.filter(|a| *a < NEW_ACCOUNT_DAYS).map(|a| (k, a)))
        .map(|(k, a)| format!("{}  领 Key 时 Discord 账号才 {a:.1} 天", keys.label(*k)))
        .collect();
    section(
        &mut out,
        &format!("领 Key 时 Discord 账号不足 {NEW_ACCOUNT_DAYS:.0} 天"),
        lines,
    );
    let plain: Vec<(f64, i64)> = regs.iter().map(|r| (r.0, r.1)).collect();
    let lines = claim_bursts(&plain, BURST_WINDOW_S, BURST_MIN)
        .into_iter()
        .map(|(start, n, ids)| {
            let set: BTreeSet<i64> = ids.into_iter().collect();
            format!(
                "{} 起 {:.0} 分钟内领了 {n} 把：{}",
                util::fmt_ts(start, tz, "%m-%d %H:%M"),
                BURST_WINDOW_S / 60.0,
                keys.list(&set)
            )
        })
        .collect();
    section(
        &mut out,
        &format!(
            "领取突发：{:.0} 分钟内 ≥ {BURST_MIN} 把",
            BURST_WINDOW_S / 60.0
        ),
        lines,
    );

    // 9. 现有暂停
    let mut lines = Vec::new();
    if has_table(conn, "share_state")? {
        let mut st = conn.prepare(
            "SELECT key_id, paused_until, strikes FROM share_state WHERE paused_until>? ORDER BY key_id",
        )?;
        let rows = st.query_map([until], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for r in rows {
            let (k, until_ts, strikes) = r?;
            lines.push(format!(
                "{}  暂停到 {} · 累计 {strikes} 次",
                keys.label(k),
                util::fmt_ts(until_ts, tz, "%m-%d %H:%M")
            ));
        }
    }
    section(&mut out, "目前被暂停的 Key", lines);

    // 10. 内容风险（外部词表）
    out.push_str("\n[内容风险（只列分类计数和 Key，不显示提示词）]\n");
    let path = args
        .terms
        .clone()
        .or_else(|| util::env_nonempty("NAI_RISK_TERMS").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TERMS));
    match std::fs::read_to_string(&path) {
        Err(_) => out.push_str(&format!(
            "  未配置词表（{}），跳过。示例见仓库 deploy/risk-terms.example.txt\n",
            path.display()
        )),
        Ok(text) => {
            let (terms, bad) = parse_terms(&text);
            let mut cats: Vec<String> = Vec::new();
            for (c, _) in &terms {
                if !cats.contains(c) {
                    cats.push(c.clone());
                }
            }
            if cats.is_empty() {
                out.push_str("  词表里没有任何规则，跳过\n");
            } else {
                let mut hits: BTreeMap<&str, (usize, BTreeSet<i64>)> = BTreeMap::new();
                if has_table(conn, "generation_audit")? {
                    let mut st = conn.prepare(
                        "SELECT key_id, prompt FROM generation_audit WHERE ts>=? AND ts<? AND key_id IS NOT NULL ORDER BY id",
                    )?;
                    let rows = st.query_map([since, until], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?;
                    for r in rows {
                        let (k, p) = r?;
                        if keys.test.contains(&k) {
                            continue;
                        }
                        for c in &cats {
                            if terms.iter().any(|(tc, re)| tc == c && re.is_match(&p)) {
                                let e = hits.entry(c.as_str()).or_default();
                                e.0 += 1;
                                e.1.insert(k);
                            }
                        }
                    }
                }
                for c in &cats {
                    let (n, ks) = hits.get(c.as_str()).cloned().unwrap_or_default();
                    if n == 0 {
                        out.push_str(&format!("  {c}：0 条\n"));
                    } else {
                        out.push_str(&format!(
                            "  {c}：{n} 条 · {} 把 Key：{}\n",
                            ks.len(),
                            keys.list(&ks)
                        ));
                    }
                }
            }
            if !bad.is_empty() {
                out.push_str(&format!(
                    "  词表第 {} 行不是合法正则，已跳过\n",
                    bad.iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join("、")
                ));
            }
        }
    }
    Ok(out)
}

/// 命令入口：只读打开数据库，窗口结束于「现在」。
pub fn run(db: &Path, days: i64, terms: Option<PathBuf>) -> Result<String> {
    let conn = Connection::open_with_flags(
        util::sqlite_uri(db, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("打不开数据库 {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    build(
        &conn,
        &ReportArgs {
            days: days.clamp(1, 90),
            terms,
            until: util::now_secs(),
        },
        &util::local_tz(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_even_odd() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [1.0, 2.0, 3.0, 10.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn longest_run_splits_on_gap() {
        let ts = [0.0, 60.0, 120.0, 1000.0, 1050.0];
        assert_eq!(longest_run(&ts, 120.0), (3, 120.0));
        assert_eq!(longest_run(&[], 120.0), (0, 0.0));
        assert_eq!(longest_run(&[5.0], 120.0), (1, 0.0));
    }

    #[test]
    fn device_classes() {
        assert_eq!(
            device_class("Mozilla/5.0 (iPhone; CPU iPhone OS 17)"),
            "ios"
        );
        assert_eq!(device_class("Mozilla/5.0 (Linux; Android 14)"), "android");
        assert_eq!(device_class("Mozilla/5.0 (Windows NT 10.0)"), "windows");
        assert_eq!(device_class("SomeClient 1.2"), "other");
    }

    #[test]
    fn snowflake_roundtrip() {
        // 2020-01-01 00:00:00 UTC = 1577836800
        let ms = 1_577_836_800_000u64 - 1_420_070_400_000;
        let id = (ms << 22).to_string();
        assert_eq!(snowflake_secs(&id), Some(1_577_836_800.0));
        assert_eq!(snowflake_secs("abc"), None);
    }

    #[test]
    fn bursts() {
        let regs: Vec<(f64, i64)> = (0..6).map(|i| (i as f64 * 60.0, i)).collect();
        let b = claim_bursts(&regs, 600.0, 5);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].1, 6);
        let spread: Vec<(f64, i64)> = (0..6).map(|i| (i as f64 * 1000.0, i)).collect();
        assert!(claim_bursts(&spread, 600.0, 5).is_empty());
    }

    #[test]
    fn terms_file() {
        let (t, bad) = parse_terms("## 注释\n\n# 分类甲\nfoo\nba+r\n# 分类乙\n(unclosed\nbaz\n");
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].0, "分类甲");
        assert_eq!(t[2].0, "分类乙");
        assert_eq!(bad, vec![7]);
        assert!(t[0].1.is_match("xFOOx"));
        // 只有注释的示例文件 = 没有规则
        assert!(parse_terms("## a\n## b\n").0.is_empty());
    }
}
