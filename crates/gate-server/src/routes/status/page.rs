//! `/status` 页面（旧版 `status_page.render`）：版式 / 类名 / 文案照搬旧页（Atlassian Statuspage 模板）。
//!
//! 与旧页的差异：样式与脚本改成同源外链（CSP 不再 `'unsafe-inline'`），
//! 所以所有 `style="..."` 换成类名；过去事件的标题有了真正的链接；增加「返回首页」；对比度达标的配色。

use super::feed::{status_label, worst_active_impact};
use super::snapshot::{
    DAYS, Day, HISTORY_DAYS, IMPACT_COMPONENT, Snapshot, days_before, local_date, midnight,
};
use gate_core::clock::Clock;
use gate_db::repo::status_page::{Incident, IncidentUpdate};
use jiff::civil::Date;
use maud::{DOCTYPE, Markup, html};

/// 色条（SVG `fill`，Statuspage 原色）：无数据 / 正常 / 性能下降 / 部分中断 / 严重中断。
fn bar_fill(level: Option<usize>) -> &'static str {
    match level {
        None => "#B3BAC5",
        Some(0) => "#76AD2A",
        Some(1) => "#FAA72A",
        Some(2) => "#E86235",
        _ => "#E04343",
    }
}

/// 严重度 → 配色类（`IMPACT_COLOR.get(impact, "#E86235")`：未知的按 major）。
fn tone(impact: &str) -> &'static str {
    match impact {
        "none" => "none",
        "minor" => "minor",
        "critical" => "critical",
        _ => "major",
    }
}

/// `_stamp`：`2026年10月14日 12:00 UTC+8`（过去事件里不带年）。时间超出 Python `datetime` 的范围写「时间无效」。
fn stamp(clock: &Clock, ts: f64, with_year: bool) -> String {
    // datetime 只认公元 1–9999 年
    if !ts.is_finite() || !(-62_135_596_800.0..=253_402_300_799.0).contains(&ts) {
        return "时间无效".into();
    }
    let parts: Vec<i64> = clock
        .format(ts, "%Y|%m|%d|%H|%M")
        .split('|')
        .filter_map(|p| p.parse().ok())
        .collect();
    let [y, mo, d, h, mi] = parts[..] else {
        return "时间无效".into();
    };
    format!(
        "{}{mo}月{d}日 {h:02}:{mi:02} UTC+8",
        if with_year {
            format!("{y}年")
        } else {
            String::new()
        }
    )
}

fn update_html(clock: &Clock, u: &IncidentUpdate, with_year: bool) -> Markup {
    html! {
        div class=(format!("update font-regular {}", u.status)) {
            strong { (status_label(&u.status)) } " - "
            span class="whitespace-pre-wrap" { (u.body) } br;
            small { (stamp(clock, u.at, with_year)) }
        }
    }
}

fn bars_svg(days: &[Day]) -> Markup {
    html! {
        svg class="availability-time-line-graphic" preserveAspectRatio="none" height="34"
            viewBox=(format!("0 0 {} 34", days.len() * 5 - 2)) {
            @for (i, d) in days.iter().enumerate() {
                @let tip = match d.level {
                    None => format!("{}：无数据", d.day),
                    Some(_) if d.titles.is_empty() => format!("{}：没有记录的停机", d.day),
                    Some(_) => format!("{}：{}", d.day, d.titles.join("；")),
                };
                rect height="34" width="3" x=(i * 5) y="0" fill=(bar_fill(d.level)) { title { (tip) } }
            }
        }
    }
}

fn history_day(clock: &Clock, snap: &Snapshot, day: Date, is_today: bool) -> Markup {
    let d0 = midnight(clock, day);
    let incs: Vec<&Incident> = snap
        .incidents
        .iter()
        .filter(|i| d0 <= i.started_at && i.started_at < d0 + 86400.0)
        .collect();
    html! {
        div class=(if incs.is_empty() { "status-day font-regular no-incidents" } else { "status-day font-regular" }) {
            h3 class="date border-color font-large" { (day.year()) "年" (day.month()) "月" (day.day()) "日" }
            @if incs.is_empty() {
                p class="color-secondary" { (if is_today { "今天没有报告事件。" } else { "没有报告事件。" }) }
            }
            @for inc in incs {
                div class="incident-container" id=(format!("incident-{}", inc.id)) {
                    div class=(format!("incident-title impact-{} font-large", inc.impact)) {
                        a class="whitespace-pre-wrap" href=(format!("#incident-{}", inc.id)) { (inc.title) }
                    }
                    div class="updates-container" {
                        @for u in &inc.updates { (update_html(clock, u, false)) }
                    }
                }
            }
        }
    }
}

/// 页面引用的静态资源地址（带内容哈希，见 `routes::status`）。
pub struct AssetRefs<'a> {
    pub css: &'a str,
    pub js: &'a str,
}

pub fn render(snap: &Snapshot, clock: &Clock, subscribe_url: &str, assets: &AssetRefs) -> Markup {
    let worst = snap.components.iter().map(|c| c.level).max().unwrap_or(0);
    let status_cls = match worst_active_impact(snap) {
        Some(impact) => format!("status-{impact}"),
        None if worst == 0 => "status-none".to_string(),
        None => "status-minor".to_string(),
    };
    let today = local_date(clock, snap.now);
    html! {
        (DOCTYPE)
        html lang="zh-CN" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { "猫头鹰公益站 状态" }
                link rel="icon" href="/favicon.svg" type="image/svg+xml";
                link rel="stylesheet" href=(assets.css);
                script src=(assets.js) {}
            }
            body class=(format!("status index {status_cls}")) {
                div class="layout-content status status-index" {
                    div class="masthead" {
                        a class="logo" href="/status" { span class="mark" { "🦉" } "猫头鹰公益站 状态" }
                        div class="masthead-actions" {
                            a class="home-link" href="/" { "返回首页" }
                            button id="themeBtn" class="theme-btn" type="button"
                                aria-label="切换深色 / 浅色主题" title="切换深色 / 浅色主题" { "🌙" }
                            a class="show-updates-dropdown" href=(subscribe_url) rel="noopener" { "订阅更新" }
                        }
                    }
                    @if !snap.active.is_empty() {
                        div class="unresolved-incidents" {
                            @for &i in &snap.active {
                                @let inc = &snap.incidents[i];
                                div class=(format!("unresolved-incident impact-{} tone-{}", inc.impact, tone(&inc.impact))) {
                                    div class="incident-title font-large" {
                                        span class="whitespace-pre-wrap actual-title" { (inc.title) }
                                        a class="subscribe" href=(subscribe_url) rel="noopener" { "订阅" }
                                    }
                                    div class="updates font-regular" {
                                        @for u in &inc.updates { (update_html(clock, u, true)) }
                                    }
                                }
                            }
                        }
                    } @else if worst > 0 {
                        div class=(format!("page-status status-minor level-{worst}")) {
                            span class="status font-large" { "部分系统" (IMPACT_COMPONENT[worst]) }
                        }
                    } @else {
                        div class="page-status status-none" {
                            span class="status font-large" { "所有系统运行正常" }
                        }
                    }
                    div class="components-section font-regular" {
                        div class="components-container one-column" {
                            @for comp in &snap.components {
                                div class="component-container border-color" {
                                    div class="component-inner-container" {
                                        span class="name" role="heading" aria-level="2" { (comp.name) }
                                        span class=(format!("component-status level-{}", comp.level)) { (comp.status) }
                                    }
                                    div class="shared-partial uptime-90-days-wrapper" {
                                        (bars_svg(&comp.days))
                                        div class="legend" {
                                            div class="legend-item light legend-item-date-range" { (DAYS) " 天前" }
                                            div class="spacer" {}
                                            div class="legend-item legend-item-uptime-value" {
                                                (match comp.uptime {
                                                    Some(u) => format!("{u:.2} % 正常运行时间"),
                                                    None => "无数据".to_string(),
                                                })
                                            }
                                            div class="spacer" {}
                                            div class="legend-item light legend-item-date-range" { "今天" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div class="incidents-list format-expanded" {
                        h2 class="font-largest no-link" id="past-incidents" { "过去的事件" }
                        @for i in 0..HISTORY_DAYS {
                            (history_day(clock, snap, days_before(today, i), i == 0))
                        }
                    }
                    div class="page-footer" {
                        small { "所有时间均为北京时间（UTC+8）。" }
                        small { "事件按 调查中 → 已确定 → 监控中 → 已解决 更新" }
                    }
                }
            }
        }
    }
}
