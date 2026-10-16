//! `nai-ops notify`：跑一遍巡检，**状态变化**时给站长发一条 Discord 私信（只发给 `ALERT_USER_ID`）。
//!
//! 规则（站长：正常事件不汇报，只报异常；不私信成员）：
//! - 正常 → 注意 / 危险：发一条；注意 ↔ 危险 互相变化：发一条；
//! - 变回正常：只发一次「已恢复」（前提是之前真的通知过）；
//! - 一直正常：永远不发；
//! - 同一级别的通知有冷却（默认 60 分钟）：来回抖动时不连发；
//! - 危险持续不解决：每 `red_repeat` 分钟（默认 6 小时）提醒一次，「注意」不重复提醒；
//! - 没发成功不记「已通知」，下一轮重试。
//!
//! 状态文件（默认 `/opt/backups/patrol_notify_state.json`，环境变量 `PATROL_NOTIFY_STATE` 可改，权限 600）：
//! `{"level", "notified_level", "last_sent": {"warn", "red", "ok"}, "since"}`。
//! 消息是纯中文文本，只含巡检行（计数和容器 / 证书状态），不含成员信息、Key、令牌。

use crate::checks::ExtraCtx;
use crate::discord;
use crate::patrol::{self, Config, Report, Sev};
use crate::util;
use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_STATE: &str = "/opt/backups/patrol_notify_state.json";
pub const DEFAULT_COOLDOWN_MIN: i64 = 60;
pub const DEFAULT_RED_REPEAT_MIN: i64 = 360;
/// 一条通知里最多列几项。
const MAX_LINES: usize = 8;

#[derive(Debug, Clone)]
pub struct NotifyArgs {
    pub dry_run: bool,
    pub state_path: PathBuf,
    pub cooldown_min: i64,
    pub red_repeat_min: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotifyState {
    /// 上一轮观察到的级别：ok / warn / red
    #[serde(default = "ok_str")]
    pub level: String,
    /// 最近一次**成功通知**所对应的级别（ok = 当前没有未解除的通知）
    #[serde(default = "ok_str")]
    pub notified_level: String,
    /// 每个级别最近一次发送成功的时间（Unix 秒）
    #[serde(default)]
    pub last_sent: BTreeMap<String, f64>,
    /// 离开「正常」的时间（用来写「持续约 N 分钟」）
    #[serde(default)]
    pub since: f64,
}

fn ok_str() -> String {
    "ok".into()
}

impl Default for NotifyState {
    fn default() -> Self {
        Self {
            level: ok_str(),
            notified_level: ok_str(),
            last_sent: BTreeMap::new(),
            since: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 级别变化（含正常 → 异常）
    Changed,
    /// 危险持续未解决的提醒
    Reminder,
    /// 变回正常
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quiet,
    Send(Kind),
}

/// 要不要发、发哪一种。纯函数。
pub fn decide(st: &NotifyState, cur: Sev, now: f64, cooldown_s: f64, red_repeat_s: f64) -> Action {
    let cur_s = cur.as_str();
    if cur == Sev::Ok {
        return if st.notified_level == "ok" {
            Action::Quiet
        } else {
            Action::Send(Kind::Recovery)
        };
    }
    let last = st
        .last_sent
        .get(cur_s)
        .copied()
        .unwrap_or(f64::NEG_INFINITY);
    if st.notified_level != cur_s {
        if now - last >= cooldown_s {
            Action::Send(Kind::Changed)
        } else {
            Action::Quiet
        }
    } else if cur == Sev::Red && now - last >= red_repeat_s {
        Action::Send(Kind::Reminder)
    } else {
        Action::Quiet
    }
}

fn level_name(s: Sev) -> &'static str {
    match s {
        Sev::Ok => "正常",
        Sev::Warn => "注意",
        Sev::Red => "危险",
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// 通知正文（纯文本）。
pub fn format_message(
    kind: Kind,
    cur: Sev,
    prev_notified: &str,
    st: &NotifyState,
    rep: &Report,
    now: f64,
    tz: &TimeZone,
) -> String {
    let when = util::fmt_ts(now, tz, "%m-%d %H:%M");
    if kind == Kind::Recovery {
        let was = match prev_notified {
            "red" => "危险",
            "warn" => "注意",
            _ => "异常",
        };
        let lasted = if st.since > 0.0 {
            format!(
                "（持续约 {} 分钟）",
                ((now - st.since) / 60.0).round().max(1.0) as i64
            )
        } else {
            String::new()
        };
        return format!(
            "【猫头鹰巡检 · 已恢复】{when}\n之前的「{was}」已解除{lasted}，当前全部正常。"
        );
    }
    let lines: Vec<&crate::patrol::Row> = rep.rows.iter().filter(|r| r.sev == cur).collect();
    let mut out = format!("【猫头鹰巡检 · {}】{when}", level_name(cur));
    match kind {
        Kind::Reminder => {
            let hours = if st.since > 0.0 {
                format!(
                    "，已持续约 {:.0} 小时",
                    ((now - st.since) / 3600.0).max(1.0)
                )
            } else {
                String::new()
            };
            out.push_str(&format!("\n危险仍未解除{hours}："));
        }
        _ if prev_notified == "red" && cur == Sev::Warn => {
            out.push_str("\n危险已解除，降为「注意」：")
        }
        _ => out.push('\n'),
    }
    for r in lines.iter().take(MAX_LINES) {
        out.push_str(&format!("\n- {}：{}", r.area, clip(&r.text, 160)));
    }
    if lines.len() > MAX_LINES {
        out.push_str(&format!(
            "\n（另有 {} 项，见巡检日志）",
            lines.len() - MAX_LINES
        ));
    }
    out.push_str("\n详细：在服务器上运行 /opt/owl/nai-ops patrol");
    out
}

pub fn load_state(path: &Path) -> NotifyState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(path: &Path, st: &NotifyState) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st)?)
        .with_context(|| format!("写 {} 失败", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("写 {} 失败", path.display()))?;
    Ok(())
}

/// 私信凭据：进程环境变量优先，其次 `.env` 文件。
fn credentials(cfg: &Config) -> (String, String) {
    let get =
        |k: &str| util::env_nonempty(k).unwrap_or_else(|| util::env_file_value(&cfg.env_file, k));
    (get("DISCORD_BOT_TOKEN"), get("ALERT_USER_ID"))
}

/// 完整流程：巡检 → 判断 → （发送）→ 记状态。返回退出码（巡检的 0/1/2；发送失败 3）。
pub async fn run(cfg: &Config, args: &NotifyArgs) -> Result<i32> {
    let conn = match patrol::open_ro(&cfg.db) {
        Ok(c) => c,
        Err(e) => {
            // 打不开库本身就是危险：照样走通知逻辑
            let mut rep = Report::default();
            rep.add_key(
                Sev::Red,
                "系统",
                format!("巡检打不开数据库 {}：{e}", cfg.db.display()),
                "db-open",
            );
            return run_with(cfg, args, &rep, util::now_secs(), &util::local_tz()).await;
        }
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let infra = patrol::gather_infra(cfg, &http).await;
    let extra = ExtraCtx::gather(cfg).await;
    let now = util::now_secs();
    let rep = patrol::build_report(cfg, &conn, &infra, Some(&extra), now)?;
    run_with(cfg, args, &rep, now, &util::local_tz()).await
}

/// 拿到报告之后的部分（测试直接喂合成的报告）。
pub async fn run_with(
    cfg: &Config,
    args: &NotifyArgs,
    rep: &Report,
    now: f64,
    tz: &TimeZone,
) -> Result<i32> {
    let worst = rep.worst();
    println!("{}", patrol::render_oneline(rep, now, tz));
    let mut st = load_state(&args.state_path);
    let action = decide(
        &st,
        worst,
        now,
        (args.cooldown_min * 60) as f64,
        (args.red_repeat_min * 60) as f64,
    );
    let prev_notified = st.notified_level.clone();
    let code = worst.rank();
    // 离开 / 回到正常的时间点
    if worst != Sev::Ok && st.level == "ok" {
        st.since = now;
    }
    let Action::Send(kind) = action else {
        st.level = worst.as_str().into();
        if !args.dry_run {
            save_state(&args.state_path, &st)?;
        }
        println!(
            "[notify] 不需要通知（当前 {}，已通知级别 {}）",
            level_name(worst),
            match prev_notified.as_str() {
                "red" => "危险",
                "warn" => "注意",
                _ => "无",
            }
        );
        return Ok(code);
    };
    let text = format_message(kind, worst, &prev_notified, &st, rep, now, tz);
    if args.dry_run {
        println!("[notify] --dry-run：本轮会发送以下私信（不会真的发，也不改状态文件）：\n{text}");
        return Ok(code);
    }
    let (token, user) = credentials(cfg);
    if token.is_empty() || user.is_empty() {
        eprintln!("[notify] 未配置私信凭据（DISCORD_BOT_TOKEN 与 ALERT_USER_ID），没有发送");
        return Ok(3);
    }
    match discord::send_dm(&cfg.api_base, &token, &user, &text).await {
        Ok(()) => {
            st.level = worst.as_str().into();
            st.notified_level = worst.as_str().into();
            st.last_sent.insert(worst.as_str().into(), now);
            save_state(&args.state_path, &st)?;
            println!("[notify] 已发送（{}）", level_name(worst));
            Ok(code)
        }
        Err(why) => {
            // 没发成功：不记「已通知」，下一轮重试
            st.level = worst.as_str().into();
            save_state(&args.state_path, &st)?;
            eprintln!("[notify] 发送失败：{why}");
            Ok(3)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(notified: &str, last: &[(&str, f64)]) -> NotifyState {
        NotifyState {
            level: notified.into(),
            notified_level: notified.into(),
            last_sent: last.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
            since: 0.0,
        }
    }
    const CD: f64 = 3600.0;
    const RP: f64 = 21600.0;

    #[test]
    fn ok_never_notifies() {
        assert_eq!(decide(&st("ok", &[]), Sev::Ok, 1e9, CD, RP), Action::Quiet);
    }

    #[test]
    fn change_to_warn_or_red_sends_once() {
        let s = st("ok", &[]);
        assert_eq!(
            decide(&s, Sev::Warn, 1e9, CD, RP),
            Action::Send(Kind::Changed)
        );
        assert_eq!(
            decide(&s, Sev::Red, 1e9, CD, RP),
            Action::Send(Kind::Changed)
        );
        // 已经通知过同一级别：不重复
        let s = st("warn", &[("warn", 1e9)]);
        assert_eq!(decide(&s, Sev::Warn, 1e9 + 7200.0, CD, RP), Action::Quiet);
    }

    #[test]
    fn escalation_and_downgrade_send() {
        let s = st("warn", &[("warn", 1e9)]);
        assert_eq!(
            decide(&s, Sev::Red, 1e9 + 60.0, CD, RP),
            Action::Send(Kind::Changed)
        );
        let s = st("red", &[("red", 1e9)]);
        assert_eq!(
            decide(&s, Sev::Warn, 1e9 + 60.0, CD, RP),
            Action::Send(Kind::Changed)
        );
    }

    #[test]
    fn recovery_once_and_only_after_a_notification() {
        let s = st("red", &[("red", 1e9)]);
        assert_eq!(
            decide(&s, Sev::Ok, 1e9 + 60.0, CD, RP),
            Action::Send(Kind::Recovery)
        );
        // 恢复发出后 notified 变 ok，之后不再发
        assert_eq!(
            decide(&st("ok", &[("red", 1e9)]), Sev::Ok, 1e9 + 120.0, CD, RP),
            Action::Quiet
        );
    }

    #[test]
    fn flapping_is_rate_limited_per_level() {
        // 刚发过 warn（10 分钟前）又恢复又复发：复发不再发
        let s = st("ok", &[("warn", 1e9), ("ok", 1e9 + 300.0)]);
        assert_eq!(decide(&s, Sev::Warn, 1e9 + 600.0, CD, RP), Action::Quiet);
        // 冷却过了才发
        assert_eq!(
            decide(&s, Sev::Warn, 1e9 + 3700.0, CD, RP),
            Action::Send(Kind::Changed)
        );
    }

    #[test]
    fn red_reminds_but_warn_does_not() {
        let s = st("red", &[("red", 1e9)]);
        assert_eq!(decide(&s, Sev::Red, 1e9 + 3600.0, CD, RP), Action::Quiet);
        assert_eq!(
            decide(&s, Sev::Red, 1e9 + 22000.0, CD, RP),
            Action::Send(Kind::Reminder)
        );
        let s = st("warn", &[("warn", 1e9)]);
        assert_eq!(decide(&s, Sev::Warn, 1e9 + 99999.0, CD, RP), Action::Quiet);
    }

    #[test]
    fn corrupt_state_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_state(&p), NotifyState::default());
        let mut s = NotifyState::default();
        s.last_sent.insert("red".into(), 5.0);
        save_state(&p, &s).unwrap();
        assert_eq!(load_state(&p), s);
    }
}
