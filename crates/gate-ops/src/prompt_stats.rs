//! 提示词统计 + 敏感内容自查（移植自 deploy/ops/prompt_stats.py）：本地出 HTML，不上服务器、不对外。
//!
//! 输出一个自包含的 HTML（点表头排序、带搜索框），三张表：画师串 / 标签频率 / 提示词去重，
//! 最后一节是「敏感内容自查」：只列命中条数与定位信息（时间 / 成员 / 摘要），**内容只留在本地这个文件里**。
//! 数据来源是 `generation_audit`（`audit_prompts` 开着才会存），数据库只读打开。
//! 这是给站长做内容治理用的，别把这个文件转发出去（所以文件权限是 600）。
//!
//! 与旧版的差异（都是旧版的小 bug，输出版式不变）：
//! - 表格单元格一律转义。旧版对「以 `<` 开头的字符串」原样写入——提示词以 `<` 开头就成了本地页面里的活 HTML；
//! - 敏感摘要只转义一次。旧版先转义一次、渲染时又转义一次，`&` 会显示成 `&amp;`；
//! - 搜索框按 id 取表。旧版用 `querySelectorAll('#'+id)`，第一张表的 id 带「:」，选择器抛错、搜索失效。
//!
//! 保持不变的怪癖：`--top` 只出现在「标签频率」表头的说明文字里，并不截断行数（与旧版一致）。

use crate::prompt_terms::SENSITIVE_TERMS;
use crate::util::{self, Counter, html_escape};
use anyhow::{Context, Result};
use gate_core::pyc;
use jiff::tz::TimeZone;
use regex::Regex;
use rusqlite::types::Value as Sv;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

const CSS: &str = include_str!("assets/prompt_stats.css");
const JS: &str = include_str!("assets/prompt_stats.js");

#[derive(Debug, Clone)]
pub struct AuditRow {
    pub ts: f64,
    pub key_name: String,
    pub kind: String,
    pub model: String,
    pub status: String,
    pub prompt: String,
    pub negative: String,
}

#[derive(Debug, Clone)]
pub struct Sensitive {
    pub ts: f64,
    pub who: String,
    pub kind: String,
    pub model: String,
    pub status: String,
    pub category: &'static str,
    pub hits: Vec<&'static str>,
    pub snippet: String,
}

pub struct PromptEntry {
    pub text: String,
    pub count: u64,
    pub members: HashSet<String>,
    pub first: f64,
    pub last: f64,
}

#[derive(Default)]
pub struct Summary {
    pub artists: Counter,
    pub artist_members: HashMap<String, HashSet<String>>,
    pub artist_last: HashMap<String, f64>,
    pub tags: Counter,
    /// 按提示词小写去重，保持首次出现的顺序
    pub prompts: Vec<PromptEntry>,
    pub sensitive: Vec<Sensitive>,
}

fn artist_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)artist:\s*([^,{}\n\[\]]+)").expect("固定正则"))
}

fn split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[,，\n\[\]{}]").expect("固定正则"))
}

/// `WS.sub(" ", (text or "").strip())`：连续空白折成一个空格（空白集合与 Python `\s` 一致）。
pub fn norm(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in pyc::strip(text).chars() {
        if pyc::is_space(c) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// 成员名只留首尾，避免本地文件里出现完整 ID（定位够用就行）。
pub fn mask(name: &str) -> String {
    let name = norm(name);
    if pyc::len_chars(&name) <= 2 {
        format!("{}*", pyc::take_chars(&name, 1))
    } else {
        format!(
            "{}***{}",
            pyc::take_chars(&name, 2),
            pyc::last_chars(&name, 2)
        )
    }
}

fn text_of(v: &Sv) -> String {
    match v {
        Sv::Null => String::new(),
        other => util::sv_str(other),
    }
}

fn ts_of(v: &Sv) -> f64 {
    match v {
        Sv::Integer(i) => *i as f64,
        Sv::Real(f) => *f,
        Sv::Text(s) => pyc::parse_float(s).unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 最近 `days` 天的审计记录，新的在前。
pub fn load_rows(db_path: &Path, days: i64, now: f64) -> Result<Vec<AuditRow>> {
    let conn = Connection::open_with_flags(
        util::sqlite_uri(db_path, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打不开数据库 {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let since = now - days as f64 * 86400.0;
    let mut stmt = conn.prepare(
        "SELECT ts, key_name, kind, model, status, prompt, negative FROM generation_audit \
         WHERE ts>=? ORDER BY ts DESC",
    )?;
    let rows = stmt
        .query_map([since], |r| {
            Ok(AuditRow {
                ts: ts_of(&r.get::<_, Sv>(0)?),
                key_name: text_of(&r.get::<_, Sv>(1)?),
                kind: text_of(&r.get::<_, Sv>(2)?),
                model: text_of(&r.get::<_, Sv>(3)?),
                status: text_of(&r.get::<_, Sv>(4)?),
                prompt: text_of(&r.get::<_, Sv>(5)?),
                negative: text_of(&r.get::<_, Sv>(6)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn summarize(rows: &[AuditRow]) -> Summary {
    let mut s = Summary::default();
    let mut prompt_index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let prompt = norm(&row.prompt);
        let negative = norm(&row.negative);
        let stamp = row.ts;
        let who = mask(&row.key_name);
        let blob = format!("{prompt}\n{negative}");

        for cap in artist_re().captures_iter(&blob) {
            let name = norm(&cap[1]);
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            s.artists.add(key.clone());
            s.artist_members
                .entry(key.clone())
                .or_default()
                .insert(who.clone());
            let last = s.artist_last.entry(key).or_insert(0.0);
            *last = last.max(stamp);
        }

        for raw in split_re().split(&blob) {
            let tag = norm(raw);
            if !tag.is_empty()
                && pyc::len_chars(&tag) <= 60
                && !tag.to_lowercase().starts_with("artist:")
            {
                s.tags.add(tag.to_lowercase());
            }
        }

        if !prompt.is_empty() {
            let idx = *prompt_index
                .entry(prompt.to_lowercase())
                .or_insert_with(|| {
                    s.prompts.push(PromptEntry {
                        text: prompt.clone(),
                        count: 0,
                        members: HashSet::new(),
                        first: stamp,
                        last: stamp,
                    });
                    s.prompts.len() - 1
                });
            let e = &mut s.prompts[idx];
            e.count += 1;
            e.members.insert(who.clone());
            e.first = e.first.min(stamp);
            e.last = e.last.max(stamp);
        }

        for (category, terms) in SENSITIVE_TERMS {
            let mut hits: Vec<&'static str> =
                terms.iter().copied().filter(|t| blob.contains(t)).collect();
            hits.sort_unstable();
            hits.dedup();
            if !hits.is_empty() {
                s.sensitive.push(Sensitive {
                    ts: stamp,
                    who: who.clone(),
                    kind: row.kind.clone(),
                    model: row.model.clone(),
                    status: row.status.clone(),
                    category,
                    hits,
                    snippet: pyc::take_chars(&blob, 120).to_string(),
                });
            }
        }
    }
    s
}

/// `columns`: (表头, 是否数值排序)；`rows` 与表头同顺序，单元格已是最终文本（这里负责转义）。
fn sortable_table(
    head: &str,
    columns: &[(&str, bool)],
    rows: &[Vec<String>],
    note: &str,
) -> String {
    let idx = html_escape(&head.to_lowercase().replace(' ', "_"));
    let ths: String = columns
        .iter()
        .enumerate()
        .map(|(i, (label, _))| format!("<th data-col=\"{i}\">{}</th>", html_escape(label)))
        .collect();
    let numeric = format!(
        "[{}]",
        columns
            .iter()
            .map(|(_, n)| if *n { "true" } else { "false" })
            .collect::<Vec<_>>()
            .join(", ")
    );
    let body: String = rows
        .iter()
        .map(|row| {
            format!(
                "<tr>{}</tr>",
                row.iter()
                    .map(|c| format!("<td>{}</td>", html_escape(c)))
                    .collect::<String>()
            )
        })
        .collect();
    format!(
        "<h3>{}<span class=\"muted\">{}</span></h3>\
         <div class=\"search\"><input type=\"search\" placeholder=\"搜索这一张表…\" oninput=\"filterTable(this,'{idx}')\"></div>\
         <div class=\"scroll\"><table id=\"{idx}\" data-num=\"{numeric}\"><thead><tr>{ths}</tr></thead>\
         <tbody>{body}</tbody></table></div>",
        html_escape(head),
        html_escape(note),
    )
}

pub struct RenderOpts {
    pub days: i64,
    pub top: i64,
    pub prompts: usize,
    pub now: f64,
    pub tz: TimeZone,
}

pub fn render(data: &Summary, total: usize, o: &RenderOpts) -> String {
    let share = |n: u64| {
        if total != 0 {
            format!("{:.1}%", n as f64 / total as f64 * 100.0)
        } else {
            "—".to_string()
        }
    };
    let when = |ts: f64| util::fmt_ts(ts, &o.tz, "%m-%d %H:%M");

    let artist_rows: Vec<Vec<String>> = data
        .artists
        .most_common()
        .into_iter()
        .map(|(name, count)| {
            vec![
                name.to_string(),
                count.to_string(),
                share(count),
                data.artist_members
                    .get(name)
                    .map_or(0, HashSet::len)
                    .to_string(),
                when(data.artist_last.get(name).copied().unwrap_or(0.0)),
            ]
        })
        .collect();
    let tag_rows: Vec<Vec<String>> = data
        .tags
        .most_common()
        .into_iter()
        .map(|(tag, count)| vec![tag.to_string(), count.to_string(), share(count)])
        .collect();
    let mut by_count: Vec<&PromptEntry> = data.prompts.iter().collect();
    by_count.sort_by_key(|e| std::cmp::Reverse(e.count)); // 稳定排序：并列保持首次出现的顺序
    let prompt_rows: Vec<Vec<String>> = by_count
        .into_iter()
        .take(o.prompts)
        .map(|e| {
            vec![
                e.text.clone(),
                e.count.to_string(),
                e.members.len().to_string(),
                when(e.last),
            ]
        })
        .collect();
    let sensitive_rows: String = data
        .sensitive
        .iter()
        .map(|s| {
            let cells = [
                when(s.ts),
                s.who.clone(),
                s.kind.clone(),
                s.model.clone(),
                s.status.clone(),
                s.category.to_string(),
                s.hits.join("、"),
                s.snippet.clone(),
            ];
            format!(
                "<tr>{}</tr>",
                cells
                    .iter()
                    .map(|c| format!("<td>{}</td>", html_escape(c)))
                    .collect::<String>()
            )
        })
        .collect();
    let sensitive_rows = if sensitive_rows.is_empty() {
        "<tr><td colspan=\"8\" class=\"muted\">没有命中</td></tr>".to_string()
    } else {
        sensitive_rows
    };

    let t1 = sortable_table(
        "画师串（artist:）",
        &[
            ("画师串", false),
            ("次数", true),
            ("占比", true),
            ("成员数", true),
            ("最近出现", true),
        ],
        &artist_rows,
        &format!("共 {} 个", artist_rows.len()),
    );
    let t2 = sortable_table(
        "标签频率",
        &[("标签", false), ("次数", true), ("占比", true)],
        &tag_rows,
        &format!("共 {} 个（取前 {}）", tag_rows.len(), o.top),
    );
    let t3 = sortable_table(
        "提示词去重",
        &[
            ("提示词", false),
            ("次数", true),
            ("成员数", true),
            ("最近出现", true),
        ],
        &prompt_rows,
        &format!(
            "共 {} 条不同的提示词，按次数取前 {}",
            data.prompts.len(),
            prompt_rows.len()
        ),
    );

    format!(
        "<!doctype html><html lang=\"zh\"><head><meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
<title>提示词统计（本地）· {date}</title>\n\
<style>\n{CSS}</style></head><body>\n\
<header><h1>提示词统计（本地文件，不要外传）</h1>\n\
<p>数据来源：本站出图审计记录 generation_audit（最近 {days} 天，共 {total} 条）。\n\
点表头排序；每张表带搜索框。最后一节是敏感内容自查，仅列定位信息。</p></header>\n\
<main>\n{t1}\n{t2}\n{t3}\n\
<h3>敏感内容自查<span class=\"muted\">命中 {hit_count} 条（时间 / 成员 / 类别 / 关键词）</span></h3>\n\
<div class=\"scroll\"><table><thead><tr><th>时间</th><th>成员</th><th>类型</th><th>模型</th><th>状态</th>\n\
<th>类别</th><th>命中词</th><th>摘要</th></tr></thead><tbody>\n\
{sensitive_rows}\n\
</tbody></table></div>\n\
</main>\n\
<script>\n{JS}</script></body></html>",
        date = util::fmt_ts(o.now, &o.tz, "%Y-%m-%d"),
        days = o.days,
        hit_count = data.sensitive.len(),
    )
}

/// 写文件（权限 600：里面是成员的提示词原文）。
pub fn write_report(path: &Path, html: &str) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("写 {} 失败", path.display()))?;
    std::io::Write::write_all(&mut f, html.as_bytes())?;
    Ok(())
}

/// `dict(cats)` 的 Python 写法：`{'政治人物与机构': 3, ...}`。
fn py_dict_repr(items: &[(&str, usize)]) -> String {
    let body = items
        .iter()
        .map(|(k, n)| format!("{}: {n}", gate_core::pyext::repr_str(k)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{body}}}")
}

pub struct Args {
    pub db: std::path::PathBuf,
    pub out: std::path::PathBuf,
    pub days: i64,
    pub top: i64,
    pub prompts: usize,
}

/// 返回要打印到 stdout 的几行；没有记录时返回 Err（调用方退出码 1）。
pub fn run(args: &Args, now: f64, tz: TimeZone) -> Result<String> {
    let rows = load_rows(&args.db, args.days, now)?;
    if rows.is_empty() {
        anyhow::bail!(
            "最近 {} 天没有审计记录（确认 audit_prompts 开着）",
            args.days
        );
    }
    let data = summarize(&rows);
    let html = render(
        &data,
        rows.len(),
        &RenderOpts {
            days: args.days,
            top: args.top,
            prompts: args.prompts,
            now,
            tz,
        },
    );
    write_report(&args.out, &html)?;
    let mut out = format!(
        "记录 {} 条 · 画师串 {} 个 · 标签 {} 个 · 提示词 {} 条 · 敏感命中 {} 条\n",
        rows.len(),
        data.artists.len(),
        data.tags.len(),
        data.prompts.len(),
        data.sensitive.len()
    );
    let top = data
        .artists
        .most_common()
        .into_iter()
        .take(5)
        .map(|(n, c)| format!("{n} ×{c}"))
        .collect::<Vec<_>>()
        .join("、");
    out.push_str(&format!("画师串前 5： {top}\n"));
    if !data.sensitive.is_empty() {
        let mut cats: Vec<(&str, usize)> = Vec::new();
        for s in &data.sensitive {
            match cats.iter_mut().find(|(c, _)| *c == s.category) {
                Some(e) => e.1 += 1,
                None => cats.push((s.category, 1)),
            }
        }
        out.push_str(&format!("敏感命中分布： {}\n", py_dict_repr(&cats)));
    }
    Ok(out)
}
