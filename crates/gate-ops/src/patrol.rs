//! 7×24 巡检（移植自旧版 patrol.py，见标签 py-final）：只读地检查站点数据、上游容量、成员出图请求与注册链路。
//!
//! 退出码：0 正常 · 1 注意 · 2 危险 · **3 巡检程序自己出错**（旧版崩溃时是 1，systemd 分不出
//! 「有警告」和「巡检坏了」；见 docs/v3-deviations.md #5）。
//! 数据库用 `mode=ro` 打开，不写任何业务表；告警冷却状态与重启历史写在 `PATROL_STATE_DIR`。
//!
//! 所有阈值 / 文案与旧版逐字一致。已删除功能的检查（候补 `waitlist`、防分享证据 `share_evidence`）
//! 整段去掉——这两张表在 3.0 的迁移里会被 DROP，再查就是「no such table」。

use crate::discord::{self, Discord};
use crate::util::{self, sv_str};
use anyhow::{Context, Result};
use gate_core::pyc;
use jiff::tz::TimeZone;
use rusqlite::types::Value as Sv;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 站点按 Asia/Shanghai 记「天」。
const NZ: &str = "+8 hours";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sev {
    Ok,
    Warn,
    Red,
}

impl Sev {
    pub fn rank(self) -> i32 {
        match self {
            Sev::Ok => 0,
            Sev::Warn => 1,
            Sev::Red => 2,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Sev::Ok => "ok",
            Sev::Warn => "warn",
            Sev::Red => "red",
        }
    }
    fn upper(self) -> &'static str {
        match self {
            Sev::Ok => "OK",
            Sev::Warn => "WARN",
            Sev::Red => "RED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub sev: Sev,
    pub area: &'static str,
    pub text: String,
    /// 检查项的稳定标识（告警去重用）：同一项检查，数字变了也是同一个问题
    pub key: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub rows: Vec<Row>,
    pub data: Map<String, Value>,
}

/// 没给标识时用文本，但把数字抹掉：「成功率 60%」和「成功率 59%」是同一个问题。
/// （10-15 的教训：带数字的文本当标识，每 5 分钟就算一个「新问题」，6 小时冷却形同虚设。）
fn stable_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !out.ends_with('#') {
                out.push('#');
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Report {
    pub fn add(&mut self, sev: Sev, area: &'static str, text: impl Into<String>) {
        self.add_key(sev, area, text, "");
    }

    pub fn add_key(&mut self, sev: Sev, area: &'static str, text: impl Into<String>, key: &str) {
        let text = text.into();
        let key = if key.is_empty() {
            stable_key(&text)
        } else {
            key.to_string()
        };
        self.rows.push(Row {
            sev,
            area,
            text,
            key,
        });
    }

    /// 危险项的稳定标识（排好序），私信去重用。
    pub fn red_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .rows
            .iter()
            .filter(|r| r.sev == Sev::Red)
            .map(|r| r.key.as_str())
            .collect();
        keys.sort_unstable();
        keys
    }

    /// 已知 / 已不再复现的事项：照常显示给人看，但不参与告警。
    ///
    /// 2026-10-13 的教训：「按设计」的提示每 10 分钟告警一次，站长只能把告警当背景音，
    /// 真出问题（局部重绘集体 500）反而被淹没。「按设计」的东西不要做成检查项——需要站长拍板的，挂一条后台说明。
    pub fn known(&mut self, area: &'static str, text: &str) {
        self.rows.push(Row {
            sev: Sev::Ok,
            area,
            text: format!("（已知，不告警）{text}"),
            key: String::new(),
        });
    }

    pub fn worst(&self) -> Sev {
        self.rows
            .iter()
            .map(|r| r.sev)
            .max_by_key(|s| s.rank())
            .unwrap_or(Sev::Ok)
    }

    pub fn problems(&self) -> Vec<&Row> {
        self.rows.iter().filter(|r| r.sev != Sev::Ok).collect()
    }

    fn set(&mut self, key: &str, v: impl Into<Value>) {
        self.data.insert(key.to_string(), v.into());
    }
}

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone)]
pub struct Config {
    pub db: PathBuf,
    pub env_file: PathBuf,
    pub health_url: String,
    pub queue_url: String,
    pub state_dir: PathBuf,
    pub cooldown: i64,
    /// 45 分钟没再出现 = 旧记录，不告警
    pub stale_seconds: i64,
    pub api_base: String,
    /// 要检查的容器（第一个是网关，重启记录按它算）。`NAI_GATE_CONTAINERS` 逗号分隔。
    pub containers: Vec<String>,
}

impl Config {
    pub fn from_env(db: Option<&Path>) -> Result<Self> {
        let get = |k: &str, d: &str| util::env_nonempty(k).unwrap_or_else(|| d.to_string());
        let int = |k: &str, d: i64| -> Result<i64> {
            match util::env_nonempty(k) {
                None => Ok(d),
                Some(v) => {
                    pyc::parse_int(&v).with_context(|| format!("环境变量 {k}={v:?} 不是整数"))
                }
            }
        };
        Ok(Self {
            db: util::resolve_db(db),
            env_file: get("NAI_GATE_ENV", "/opt/service-tools/.env").into(),
            health_url: get("NAI_GATE_HEALTH", "http://127.0.0.1:3003/healthz"),
            queue_url: get("NAI_GATE_QUEUE", "http://127.0.0.1:3003/queue-status"),
            state_dir: get("PATROL_STATE_DIR", "/var/lib/nai-patrol").into(),
            cooldown: int("PATROL_COOLDOWN", 6 * 3600)?,
            stale_seconds: int("PATROL_STALE", 2700)?,
            api_base: discord::api_base_from_env(),
            containers: get("NAI_GATE_CONTAINERS", "owl-gate,owl-naimei,caddy")
                .split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
        })
    }
}

// ---------------------------------------------------------------- Python 风格的取值小工具

/// 一行一列的查询；没有行按 NULL（旧版 `q1` 返回 None）。
fn q1(conn: &Connection, sql: &str, params: impl Params) -> rusqlite::Result<Sv> {
    Ok(conn
        .query_row(sql, params, |r| r.get::<_, Sv>(0))
        .optional()?
        .unwrap_or(Sv::Null))
}

/// 旧版 `num(raw, default)`：`int(float(raw))`，失败（None / 非数字 / 无穷）按默认值。
fn num(v: &Sv, default: i64) -> i64 {
    match v {
        Sv::Integer(i) => *i,
        Sv::Real(f) => pyc::int_of(*f).unwrap_or(default),
        Sv::Text(s) => pyc::parse_int_via_float(s).unwrap_or(default),
        _ => default,
    }
}

fn num0(v: &Sv) -> i64 {
    num(v, 0)
}

fn num_json(v: Option<&Value>, default: i64) -> i64 {
    match v {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => i,
            None => n.as_f64().and_then(pyc::int_of).unwrap_or(default),
        },
        Some(Value::String(s)) => pyc::parse_int_via_float(s).unwrap_or(default),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => default,
    }
}

fn sv_text(v: &Sv) -> Option<String> {
    match v {
        Sv::Null => None,
        other => Some(sv_str(other)),
    }
}

fn read_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    Ok(sv_text(&q1(
        conn,
        "SELECT value FROM site_settings WHERE key=?",
        [key],
    )?))
}

fn setting_num(conn: &Connection, key: &str, default: i64) -> rusqlite::Result<i64> {
    Ok(read_setting(conn, key)?
        .map_or(default, |s| pyc::parse_int_via_float(&s).unwrap_or(default)))
}

// ---------------------------------------------------------------- 系统探测（docker / http / 命令行）

/// `check_infra` 需要的外部事实；探测和判断分开，判断部分可以不碰 docker 直接测。
#[derive(Debug)]
pub struct Infra {
    /// None = 读不到容器状态（docker 不可用）
    pub containers: Option<Vec<(String, bool)>>,
    pub health_code: u16,
    pub queue: Value,
    /// (可用内存 MB, 磁盘已用 %)；Err = 旧版会抛的异常类名
    pub resources: std::result::Result<(i64, i64), &'static str>,
    pub db_bytes: Option<u64>,
}

/// 跑外部命令取 stdout；错误用 Python 对应的异常类名表示（报告里会写出来）。
async fn run_cmd(
    prog: &str,
    args: &[&str],
    secs: u64,
) -> std::result::Result<String, &'static str> {
    let child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(secs), child).await {
        Err(_) => Err("TimeoutExpired"),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err("FileNotFoundError"),
        Ok(Err(_)) => Err("OSError"),
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
    }
}

async fn http_code(http: &reqwest::Client, url: &str) -> u16 {
    match http.get(url).send().await {
        Ok(r) => r.status().as_u16(),
        Err(_) => 0, // 连不上就是连不上，交给调用方判断
    }
}

async fn http_json(http: &reqwest::Client, url: &str) -> Value {
    let Ok(resp) = http.get(url).send().await else {
        return json!({});
    };
    match resp
        .text()
        .await
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v) => v,
        None => json!({}),
    }
}

async fn docker_running(wanted: &[String]) -> Option<Vec<(String, bool)>> {
    let out = run_cmd("docker", &["ps", "--format", "{{.Names}}"], 10)
        .await
        .ok()?;
    let names: Vec<&str> = out.split_whitespace().collect();
    Some(
        wanted
            .iter()
            .map(|n| (n.clone(), names.contains(&n.as_str())))
            .collect(),
    )
}

pub fn parse_resources(
    free_out: &str,
    df_out: &str,
) -> std::result::Result<(i64, i64), &'static str> {
    let line = free_out
        .lines()
        .find(|l| l.starts_with("Mem:"))
        .ok_or("IndexError")?;
    let avail = line.split_whitespace().nth(6).ok_or("IndexError")?;
    let avail = pyc::parse_int(avail).ok_or("ValueError")?;
    let used = df_out
        .lines()
        .nth(1)
        .ok_or("IndexError")?
        .split_whitespace()
        .nth(4)
        .ok_or("IndexError")?;
    let disk = pyc::parse_int(used.trim_end_matches('%')).ok_or("ValueError")?;
    Ok((avail, disk))
}

async fn read_resources() -> std::result::Result<(i64, i64), &'static str> {
    let mem = run_cmd("free", &["-m"], 5).await?;
    let disk = run_cmd("df", &["/"], 5).await?;
    parse_resources(&mem, &disk)
}

pub async fn gather_infra(cfg: &Config, http: &reqwest::Client) -> Infra {
    Infra {
        containers: docker_running(&cfg.containers).await,
        health_code: http_code(http, &cfg.health_url).await,
        queue: http_json(http, &cfg.queue_url).await,
        resources: read_resources().await,
        db_bytes: std::fs::metadata(&cfg.db).ok().map(|m| m.len()),
    }
}

/// docker 的 StartedAt 是 RFC3339Nano（最多 9 位小数纳秒）。
///
/// 2026-10-13 踩到两次：Python 3.10 的 fromisoformat 既不认 'Z'，也只吃 6 位小数。
/// 这里自己解析：小数截到微秒，偏移接受 `Z` / `+08:00` / `+0800`；解析不了返回 0.0。
pub fn parse_docker_time(text: &str) -> f64 {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:?\d{2})$")
            .expect("固定正则")
    });
    let text = pyc::strip(text);
    let Some(c) = re.captures(text) else {
        return 0.0;
    };
    let n = |i: usize| c[i].parse::<i16>().unwrap_or(0);
    let Ok(dt) = jiff::civil::DateTime::new(
        n(1),
        n(2) as i8,
        n(3) as i8,
        n(4) as i8,
        n(5) as i8,
        n(6) as i8,
        0,
    ) else {
        return 0.0;
    };
    let offset_secs: i32 = match &c[8] {
        "Z" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let digits: String = o[1..].chars().filter(char::is_ascii_digit).collect();
            let h: i32 = digits[..2].parse().unwrap_or(0);
            let m: i32 = digits[2..].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        }
    };
    let Ok(off) = jiff::tz::Offset::from_seconds(offset_secs) else {
        return 0.0;
    };
    let Ok(zoned) = dt.to_zoned(TimeZone::fixed(off)) else {
        return 0.0;
    };
    let micros: i64 = c.get(7).map_or(0, |m| {
        let mut f = m.as_str().chars().take(6).collect::<String>();
        while f.len() < 6 {
            f.push('0');
        }
        f.parse().unwrap_or(0)
    });
    // 整数微秒 / 1e6：与 Python `datetime.timestamp()` 一样是正确舍入的除法
    (zoned.timestamp().as_second() * 1_000_000 + micros) as f64 / 1e6
}

async fn container_started_at(name: &str) -> f64 {
    match run_cmd(
        "docker",
        &["inspect", "-f", "{{.State.StartedAt}}", name],
        10,
    )
    .await
    {
        Ok(out) => parse_docker_time(&out),
        Err(_) => 0.0,
    }
}

/// 网关重启时间的历史（最近 20 次，存在 `state_dir/restarts.json`）。
///
/// 只认「最近一次重启」不够用：一晚上连发三次版，上一轮窗口里失败的前端请求，
/// 到下一轮巡检时已经不算「重启后 10 分钟内」了，于是又被当成故障。
pub fn restart_history(state_dir: &Path, started: f64) -> Vec<f64> {
    let path = state_dir.join("restarts.json");
    let mut hist: Vec<f64> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Array(a) => a
                .iter()
                .map(|x| match x {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) => pyc::parse_float(s),
                    _ => None,
                })
                .collect::<Option<Vec<f64>>>(),
            _ => None,
        })
        .unwrap_or_default();
    if started != 0.0 && hist.last().is_none_or(|last| started > last + 1.0) {
        hist.push(started);
    }
    hist.sort_by(f64::total_cmp);
    hist.dedup();
    let keep = hist.len().saturating_sub(20);
    let hist = hist.split_off(keep);
    let body = gate_core::pyjson::default_str(&Value::Array(
        hist.iter().map(|x| gate_core::pyval::float(*x)).collect(),
    ));
    if std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(&path, body);
    }
    hist
}

// ---------------------------------------------------------------- 各项检查

pub struct Ctx<'a> {
    pub now: f64,
    pub cfg: &'a Config,
    /// 报告里 `HH:MM` 用的本地时区
    pub tz: TimeZone,
    pub restarts: Vec<f64>,
}

pub fn status_of_queue(queue: &Value) -> (Sev, &'static str, String) {
    let Some(obj) = queue.as_object().filter(|o| !o.is_empty()) else {
        return (Sev::Warn, "系统", "队列状态读取失败".into());
    };
    let global = obj.get("global").and_then(Value::as_object);
    let waiting = num_json(global.and_then(|g| g.get("waiting")), 0);
    let active = num_json(global.and_then(|g| g.get("active")), 0);
    let cool = num_json(obj.get("image_cooldown_remaining"), 0);
    let sev = if waiting <= 20 && cool == 0 {
        Sev::Ok
    } else {
        Sev::Warn
    };
    let mut text = format!("队列 active {active} / waiting {waiting}");
    if cool != 0 {
        text.push_str(&format!(" · 上游冷却 {cool}s"));
    }
    (sev, "系统", text)
}

pub fn check_infra(rep: &mut Report, infra: &Infra) {
    match &infra.containers {
        None => rep.add(Sev::Warn, "系统", "读不到容器状态（docker 不可用？）"),
        Some(list) => {
            for (name, up) in list {
                rep.add(
                    if *up { Sev::Ok } else { Sev::Red },
                    "系统",
                    format!("{name} 容器 {}", if *up { "运行中" } else { "没有运行" }),
                );
            }
        }
    }
    let code = infra.health_code;
    let shown = if code == 0 {
        "无响应".to_string()
    } else {
        code.to_string()
    };
    rep.add(
        if code == 200 { Sev::Ok } else { Sev::Red },
        "系统",
        format!("/healthz → {shown}"),
    );
    let (sev, area, text) = status_of_queue(&infra.queue);
    rep.add(sev, area, text);
    match infra.resources {
        Ok((avail, disk)) => {
            rep.set("mem_available_mb", avail);
            rep.set("disk_used_pct", disk);
            rep.add(
                if avail < 200 {
                    Sev::Red
                } else if avail < 350 {
                    Sev::Warn
                } else {
                    Sev::Ok
                },
                "系统",
                format!("可用内存 {avail}MB · 磁盘 {disk}%"),
            );
            rep.add(
                if disk > 90 {
                    Sev::Red
                } else if disk > 80 {
                    Sev::Warn
                } else {
                    Sev::Ok
                },
                "系统",
                format!("磁盘已用 {disk}%"),
            );
        }
        Err(name) => rep.add(Sev::Warn, "系统", format!("资源读取失败：{name}")),
    }
    if let Some(bytes) = infra.db_bytes {
        let mb = pyc::round(bytes as f64 / 1048576.0, 1);
        rep.set("db_mb", gate_core::pyval::float(mb));
        rep.add(Sev::Ok, "系统", format!("数据库 {}MB", pyc::float_repr(mb)));
    }
}

pub fn check_upstream(rep: &mut Report, conn: &Connection) -> rusqlite::Result<()> {
    let day = q1(conn, "SELECT date('now',?)", [NZ])?;
    let used = num0(&q1(
        conn,
        "SELECT COALESCE(SUM(images),0) FROM upstream_token_counters WHERE day=?",
        [sv_text(&day)],
    )?);
    let daily_cap = setting_num(conn, "guard_account_daily_cap", 1000)?;
    // 账号数按最近两天取（当天 0 点后还没有计数行时也能知道有几个账号）
    let yesterday = q1(conn, "SELECT date('now',?,'-1 day')", [NZ])?;
    let accounts = Some(num0(&q1(
        conn,
        "SELECT COUNT(DISTINCT token_id) FROM upstream_token_counters WHERE day>=?",
        [sv_text(&yesterday)],
    )?))
    .filter(|n| *n != 0)
    .unwrap_or(1);
    let site_daily = daily_cap * accounts.max(1);
    let share = if site_daily != 0 {
        used as f64 / site_daily as f64
    } else {
        0.0
    };
    rep.set("today_images", used);
    rep.set("site_daily", site_daily);
    // 旧版：site_daily 为 0 时 share 是整数 0，round(0, 1) 仍是整数 0
    rep.set(
        "capacity_used_pct",
        if site_daily != 0 {
            gate_core::pyval::float(pyc::round(share * 100.0, 1))
        } else {
            json!(0)
        },
    );
    rep.add(
        if share >= 0.9 { Sev::Warn } else { Sev::Ok },
        "上游",
        format!(
            "今日已出图 {used} / {site_daily} 张（{:.0}%）· {accounts} 个账号 × {daily_cap} 张",
            share * 100.0
        ),
    );

    let algo = read_setting(conn, "quota_algo_last")?
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    let v5 = algo
        .get("v5")
        .filter(|v| gate_core::pyval::truthy(v))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(pct) = v5.get("percent").filter(|p| !p.is_null()) {
        rep.set("v5_percent", pct.clone());
        let low = match pct {
            Value::Number(n) => n.as_f64().is_some_and(|f| f <= 2.0),
            // float(True) == 1.0、float(False) == 0.0，都 ≤ 2
            Value::Bool(_) => true,
            Value::String(s) => pyc::parse_float(s).is_some_and(|f| f <= 2.0),
            _ => false,
        };
        rep.add(
            if low { Sev::Warn } else { Sev::Ok },
            "上游",
            format!(
                "V5 剩余 {}%（恢复 {}%/天）",
                gate_core::pyext::py_str(pct),
                v5.get("rate")
                    .map_or("None".to_string(), gate_core::pyext::py_str)
            ),
        );
    }
    Ok(())
}

/// 成员生图请求：成功率、等待、拒绝结构、每 Key 出图间隔。
pub fn check_members(rep: &mut Report, conn: &Connection, now: f64) -> rusqlite::Result<()> {
    let (ok_n, ok_img, worst_wait, avg_wait): (i64, i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(images),0), COALESCE(MAX(wait_ms),0), COALESCE(CAST(AVG(wait_ms) AS INT),0) \
         FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'",
        [now - 3600.0],
        |r| Ok((num0(&r.get(0)?), num0(&r.get(1)?), num0(&r.get(2)?), num0(&r.get(3)?))),
    )?;
    rep.set("hour_ok", ok_n);
    rep.set("hour_images", ok_img);
    rep.set("hour_worst_wait_ms", worst_wait);
    rep.add(
        if worst_wait > 120_000 {
            Sev::Warn
        } else {
            Sev::Ok
        },
        "成员",
        format!(
            "近 1 小时成功 {ok_n} 次 / {ok_img} 张 · 平均等待 {:.0}s · 最长 {:.0}s",
            avg_wait as f64 / 1000.0,
            worst_wait as f64 / 1000.0
        ),
    );

    let mut stat: Vec<(String, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM usage_log WHERE ts>? AND kind LIKE 'image%' GROUP BY status",
        )?;
        let rows = stmt.query_map([now - 86400.0], |r| {
            Ok((sv_str(&r.get::<_, Sv>(0)?), num0(&r.get(1)?)))
        })?;
        for row in rows {
            let (k, n) = row?;
            // dict(...)：同名键后者覆盖（GROUP BY 不会重复，这里仅为严谨）
            match stat.iter_mut().find(|(s, _)| *s == k) {
                Some(e) => e.1 = n,
                None => stat.push((k, n)),
            }
        }
    }
    let get = |k: &str| stat.iter().find(|(s, _)| s == k).map_or(0, |(_, n)| *n);
    rep.set(
        "day_status",
        Value::Object(stat.iter().map(|(k, n)| (k.clone(), json!(n))).collect()),
    );
    // 只看「出错」占「真正发到上游的请求」的比例：被拒（限流 / 额度 / 排队满）是 PVP 下按设计的结果，不是故障。
    // 旧版用 成功 ÷ 全部，限流一多就常年「成功率 60%」挂着警告（10-15）。少于 5 次出错不报，免得小样本误报。
    let (ok, err) = (get("ok"), get("error"));
    let err_rate = if ok + err > 0 {
        err as f64 / (ok + err) as f64
    } else {
        0.0
    };
    rep.add(
        if err >= 5 && err_rate > 0.05 {
            Sev::Warn
        } else {
            Sev::Ok
        },
        "成员",
        format!(
            "24 小时：成功 {ok} · 被拒 {}（限流 / 额度，按设计）· 出错 {err}（出错率 {:.1}%）",
            get("rejected"),
            err_rate * 100.0
        ),
    );

    let reasons: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(reason,'(空)'), COUNT(*) FROM usage_log WHERE ts>? AND status='rejected' \
             GROUP BY 1 ORDER BY 2 DESC LIMIT 5",
        )?;
        stmt.query_map([now - 86400.0], |r| {
            Ok((sv_str(&r.get::<_, Sv>(0)?), num0(&r.get(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    rep.set(
        "day_reasons",
        Value::Array(reasons.iter().map(|(k, n)| json!([k, n])).collect()),
    );
    if !reasons.is_empty() {
        let top = reasons
            .iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join("、");
        let capacity: i64 = reasons
            .iter()
            .filter(|(name, _)| matches!(name.as_str(), "paced" | "daily_cap" | "hourly_cap"))
            .map(|(_, n)| *n)
            .sum();
        let all: i64 = reasons.iter().map(|(_, n)| *n).sum();
        rep.add(
            if capacity as f64 / all.max(1) as f64 > 0.7 {
                Sev::Warn
            } else {
                Sev::Ok
            },
            "成员",
            format!("24 小时拒绝原因：{top}"),
        );
    }

    // 每 Key 出图间隔应 ≥ 15 秒（网关硬限制）；出现连续两次 < 12 秒说明间隔失效
    let pairs = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM (
             SELECT key_id, ts - LAG(ts) OVER (PARTITION BY key_id ORDER BY ts) AS gap
             FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%' AND COALESCE(key_id,0)>0)
           WHERE gap IS NOT NULL AND gap BETWEEN 0 AND 12",
        [now - 3600.0],
    )?);
    rep.add(
        if pairs > 3 { Sev::Warn } else { Sev::Ok },
        "成员",
        format!("近 1 小时间隔疑似被绕过（同 Key <12 秒连发）：{pairs} 次"),
    );

    let active = q1(
        conn,
        "SELECT COUNT(DISTINCT key_id) FROM usage_log WHERE ts>? AND status='ok' \
         AND kind LIKE 'image%' AND COALESCE(key_id,0) NOT IN (SELECT id FROM api_keys WHERE is_test=1)",
        [now - 86400.0],
    )?;
    rep.set("active_members_24h", num0(&active));
    Ok(())
}

/// 注册状态：开关、名额、链路、领取速度。
///
/// 不做「每人额度 × 人数 vs 日容量」「后台额度 vs 环境变量」这类对比：额度是 PVP（全站共用、先到先得），
/// 每人额度只是上限不是承诺，名额和每人额度又会被自动驾驶 / 动态额度逐日改写，这些对比永远是噪音。
pub fn check_registration(
    rep: &mut Report,
    conn: &Connection,
    now: f64,
    env_file: &Path,
) -> rusqlite::Result<()> {
    let open_now = matches!(
        read_setting(conn, "register_open")?
            .filter(|s| !s.is_empty())
            .as_deref()
            .unwrap_or("0"),
        "1" | "true" | "yes" | "on"
    );
    let per = setting_num(conn, "register_daily_images", 30)?;
    let v5_per = setting_num(conn, "register_daily_v5", 0)?;
    let max_users = setting_num(conn, "register_max_users", 0)?;
    let hourly = setting_num(conn, "issue_hourly_cap", 12)?;
    let registered = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id \
         WHERE k.expires_at IS NULL OR k.expires_at>?",
        [now],
    )?);
    let new24 = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM discord_registrations WHERE created_at>?",
        [now - 86400.0],
    )?);
    let new1h = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM discord_registrations WHERE created_at>?",
        [now - 3600.0],
    )?);
    rep.set("register_open", open_now);
    rep.set("registered", registered);
    rep.set("register_daily_images", per);
    rep.set("register_daily_v5", v5_per);
    rep.set("register_max_users", max_users);
    rep.set("new_24h", new24);
    rep.set("new_1h", new1h);
    rep.set("issue_hourly_cap", hourly);
    let quota_text = if max_users != 0 {
        max_users.to_string()
    } else {
        "不限".to_string()
    };
    // 每小时领取上限 0 = 不限（后台显示「不限」）
    let hourly_text = if hourly != 0 {
        format!("{hourly} 个")
    } else {
        "不限".to_string()
    };
    rep.add(
        Sev::Ok,
        "注册",
        format!(
            "{} · 已领 {registered} 人 / 名额 {quota_text} · 每人 {per} 张（V5 {v5_per}）· 每小时最多发 {hourly_text}",
            if open_now { "开放" } else { "关闭" },
        ),
    );
    rep.add(
        if new24 <= 288 { Sev::Ok } else { Sev::Warn },
        "注册",
        format!("新注册：24 小时 {new24} 人 · 1 小时 {new1h} 人"),
    );
    if open_now && hourly != 0 && new1h >= hourly {
        rep.add(
            Sev::Warn,
            "注册",
            format!("本小时领取已达上限（{new1h}/{hourly}），成员会看到限流提示"),
        );
    }
    if util::env_file_value(env_file, "REGISTRATION_BRIDGE_SECRET").is_empty() {
        rep.add(
            if open_now { Sev::Red } else { Sev::Warn },
            "注册",
            "网关没有 REGISTRATION_BRIDGE_SECRET：/register 会直接失败",
        );
    }
    Ok(())
}

/// 记账一致性 + Bug 追踪。
pub fn check_data(rep: &mut Report, conn: &Connection, ctx: &Ctx<'_>) -> rusqlite::Result<()> {
    let day = sv_text(&q1(conn, "SELECT date('now',?)", [NZ])?);
    let counters = num0(&q1(
        conn,
        "SELECT COALESCE(SUM(images),0) FROM counters WHERE day=?",
        [&day],
    )?);
    let upstream = num0(&q1(
        conn,
        "SELECT COALESCE(SUM(images),0) FROM upstream_token_counters WHERE day=?",
        [&day],
    )?);
    let logged = num0(&q1(
        conn,
        "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE status='ok' AND kind LIKE 'image%' \
         AND ts>=strftime('%s','now','+8 hours','start of day','-8 hours')",
        rusqlite::params![],
    )?);
    rep.set("today_counters", counters);
    rep.set("today_logged", logged);
    let same = ((counters - upstream).abs() as f64) <= 2.0_f64.max(counters as f64 * 0.02);
    rep.add_key(
        if same { Sev::Ok } else { Sev::Warn },
        "数据",
        format!("今日计数：成员 {counters} · 上游 {upstream} · 日志 {logged} 张"),
        "counts",
    );
    let mut bugs: Vec<(String, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT level, COUNT(*) FROM error_events WHERE resolved_at IS NULL GROUP BY level",
        )?;
        for row in stmt.query_map([], |r| Ok((sv_str(&r.get::<_, Sv>(0)?), num0(&r.get(1)?))))? {
            bugs.push(row?);
        }
    }
    let bug = |k: &str| bugs.iter().find(|(l, _)| l == k).map_or(0, |(_, n)| *n);
    rep.set(
        "bugs_open",
        Value::Object(bugs.iter().map(|(k, n)| (k.clone(), json!(n))).collect()),
    );
    // 有未处理的 error 级 Bug 只算「注意」：代码缺陷不是站长能半夜处理的事，后台总览和 Bug 追踪里看得到；
    // 真正影响成员的故障（网关挂了、健康检查不通）由上面的系统检查报「危险」（docs/v3-deviations.md #43）
    rep.add_key(
        if bug("error") != 0 {
            Sev::Warn
        } else {
            Sev::Ok
        },
        "数据",
        format!("未解决 Bug：error {} · warn {}", bug("error"), bug("warn")),
        "bug-count",
    );
    let recent: Vec<(String, String, Sv, Sv)> = {
        let mut stmt = conn.prepare(
            "SELECT level, substr(title,1,58), count, last_seen FROM error_events \
             WHERE resolved_at IS NULL ORDER BY last_seen DESC LIMIT 6",
        )?;
        stmt.query_map([], |r| {
            Ok((
                sv_str(&r.get::<_, Sv>(0)?),
                sv_str(&r.get::<_, Sv>(1)?),
                r.get::<_, Sv>(2)?,
                r.get::<_, Sv>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (level, title, count, last_seen) in recent {
        let count = sv_str(&count);
        let last = num0(&last_seen);
        let hit = ctx
            .restarts
            .iter()
            .copied()
            .find(|r| *r <= last as f64 && (last as f64) < r + 180.0);
        if let (true, Some(hit)) = (title.starts_with("live:"), hit.filter(|h| *h != 0.0)) {
            // 前端每秒轮询 /public/live，网关重启那十几秒必然失败 —— 我们自己的部署不算故障
            rep.known(
                "数据",
                &format!(
                    "{title}（{count} 次）— 最近一次在网关重启（{}）后 3 分钟内，部署窗口的正常失败",
                    util::fmt_ts(hit, &ctx.tz, "%H:%M")
                ),
            );
            continue;
        }
        if (last as f64) < ctx.now - ctx.cfg.stale_seconds as f64 {
            // 已不再出现的旧记录：留着给站长看，但不该每 10 分钟吵一次
            rep.known(
                "数据",
                &format!(
                    "[{level}] {title}（{count} 次，最近 {} 分钟没再出现）",
                    ((ctx.now - last as f64) / 60.0).trunc() as i64
                ),
            );
        } else {
            rep.add_key(
                if level == "warn" || level == "error" {
                    Sev::Warn
                } else {
                    Sev::Ok
                },
                "数据",
                format!("[{level}] {title}（{count} 次）"),
                &format!("bug:{title}"),
            );
        }
    }
    Ok(())
}

/// 各条业务通道「现在真的能用吗」——文本与局部重绘修好之后，回归要立刻看得见。
///
/// - 文本 / 聊天：近 30 分钟成功率（修好之前是 0%，近 7 天一次都没成功过）
/// - 出图上游 500：局部重绘缺蒙版 / 带 SMEA 就是这个签名（上游不给可读原因）
/// - 局部重绘：近 1 小时的成功与失败，重绘坏了要单独一眼看到
pub fn check_channels(rep: &mut Report, conn: &Connection, now: f64) -> rusqlite::Result<()> {
    let (total, ok): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), SUM(status='ok') FROM usage_log WHERE ts>? AND kind IN ('chat','text','text_stream','voice')",
        [now - 1800.0],
        |r| Ok((num0(&r.get(0)?), num0(&r.get(1)?))),
    )?;
    rep.set("text_30m", total);
    rep.set("text_ok_30m", ok);
    if total != 0 {
        let rate = ok as f64 / total as f64;
        // 文本是次要功能：成功率再低也只算「注意」，不私信（docs/v3-deviations.md #48）
        rep.add_key(
            if rate >= 0.9 { Sev::Ok } else { Sev::Warn },
            "通道",
            format!(
                "文本/聊天 30 分钟：{ok}/{total} 成功（{:.0}%）",
                rate * 100.0
            ),
            "text-channel",
        );
        if rate < 0.9 {
            let last: Option<(String, Sv, String)> = conn
                .query_row(
                    "SELECT COALESCE(detail,''), datetime(ts,'unixepoch','localtime'), COALESCE(rid,'') \
                     FROM usage_log WHERE ts>? AND kind IN ('chat','text','text_stream') \
                     AND status!='ok' ORDER BY ts DESC LIMIT 1",
                    [now - 1800.0],
                    |r| Ok((sv_str(&r.get::<_, Sv>(0)?), r.get::<_, Sv>(1)?, sv_str(&r.get::<_, Sv>(2)?))),
                )
                .optional()?;
            if let Some((detail, at, rid)) = last {
                rep.add_key(
                    Sev::Warn,
                    "通道",
                    format!(
                        "最近一条文本失败：{} {}（rid {}）",
                        sv_str(&at),
                        pyc::take_chars(&detail, 50),
                        if rid.is_empty() { "-" } else { &rid }
                    ),
                    "text-last-error",
                );
            }
        }
    }
    let five = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM usage_log WHERE ts>? AND kind LIKE 'image%' \
         AND (detail LIKE '%upstream 500%' OR up_status=500)",
        [now - 1800.0],
    )?);
    if five != 0 {
        // 别在告警里猜原因：那批 500 全是「最普通的 V4.5 文生图」，
        // 而局部重绘缺蒙版只是已知的一种形态。写清楚怎么查（rid → 日志里的请求形态）就行。
        // 只算「注意」：一位成员反复提交上游处理不了的请求就能刷出一串 500（10-14 夜就是这样），
        // 站长也做不了什么；上游整体出问题时状态页和后台总览会标出来（docs/v3-deviations.md #48）
        rep.add_key(
            Sev::Warn,
            "通道",
            format!("近 30 分钟出图上游 500 × {five}（上游没给原因：按 rid 查 [upstream-5xx] 日志里的请求形态）"),
            "image-500",
        );
    }
    let (ip_total, ip_ok): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), SUM(status='ok') FROM usage_log WHERE ts>? AND model LIKE '%inpainting%'",
        [now - 3600.0],
        |r| Ok((num0(&r.get(0)?), num0(&r.get(1)?))),
    )?;
    if ip_total != 0 {
        rep.set("inpaint_1h", ip_total);
        rep.set("inpaint_ok_1h", ip_ok);
        rep.add_key(
            if ip_ok == ip_total {
                Sev::Ok
            } else {
                Sev::Warn
            },
            "通道",
            format!("局部重绘 1 小时：{ip_ok}/{ip_total} 成功"),
            "inpaint",
        );
    }

    // 上游 429 = 账号级红线（2026-10-13 01:30 第一次遇到）：一定要让站长立刻知道。
    // 我们的限流器只能挡住自己放行出去的量，挡不住「绕过网关直连上游」的突发。
    let last_429 = setting_num(conn, "guard_last_upstream_429", 0)?;
    if last_429 != 0 && now - (last_429 as f64) < 3600.0 {
        rep.add_key(
            Sev::Red,
            "通道",
            format!(
                "上游 {} 分钟前返回 429（限流）：上限已自动减半并进入冷却，请确认没有额外突发打到上游",
                1.max(((now - last_429 as f64) / 60.0).trunc() as i64)
            ),
            "upstream-429",
        );
    }
    let n429 = num0(&q1(
        conn,
        "SELECT COUNT(*) FROM usage_log WHERE ts>? AND up_status=429",
        [now - 3600.0],
    )?);
    if n429 != 0 {
        rep.add_key(
            Sev::Red,
            "通道",
            format!("近 60 分钟上游 429 × {n429}（账号级限流，必须立刻降速）"),
            "upstream-429-count",
        );
    }
    // 余量：离当前每小时上限还有多少 —— 决定要不要再往下降一档
    let cap = setting_num(conn, "guard_account_hourly_cap", 0)?;
    let used = num0(&q1(
        conn,
        "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'",
        [now - 3600.0],
    )?);
    rep.set("hourly_cap", cap);
    rep.set("hourly_used", used);
    if cap != 0 {
        rep.add_key(
            if (used as f64) < cap as f64 * 0.9 {
                Sev::Ok
            } else {
                Sev::Warn
            },
            "通道",
            format!(
                "近 60 分钟 {used} 张 / 每小时上限 {cap} 张（余量 {}）",
                0.max(cap - used)
            ),
            "hourly-headroom",
        );
    }
    Ok(())
}

/// 旧版 `main()` 里 check_* 的调用顺序（报告行与 `data` 键的顺序由它决定，不要调换）。
pub fn run_checks(
    rep: &mut Report,
    conn: &Connection,
    infra: &Infra,
    ctx: &Ctx<'_>,
) -> rusqlite::Result<()> {
    check_infra(rep, infra);
    check_data(rep, conn, ctx)?;
    check_registration(rep, conn, ctx.now, &ctx.cfg.env_file)?;
    check_upstream(rep, conn)?;
    check_members(rep, conn, ctx.now)?;
    check_channels(rep, conn, ctx.now)?;
    Ok(())
}

// ---------------------------------------------------------------- 输出

fn ranked(rep: &Report) -> Vec<&Row> {
    let mut v = rep.problems();
    v.sort_by_key(|r| std::cmp::Reverse(r.sev.rank())); // 稳定排序：同级保持原顺序
    v
}

pub fn render_json(rep: &Report) -> String {
    let v = json!({
        "worst": rep.worst().as_str(),
        "data": Value::Object(rep.data.clone()),
        "rows": rep.rows.iter().map(|r| json!({"severity": r.sev.as_str(), "area": r.area, "text": r.text})).collect::<Vec<_>>(),
    });
    util::dumps_indent2(&v)
}

pub fn render_oneline(rep: &Report, now: f64, tz: &TimeZone) -> String {
    // 按严重度排序再截取：RED 不能被排在前面的 warn 挤出头条（2026-10-13 凌晨，429 的 RED 就被挤掉了）
    let head = ranked(rep)
        .iter()
        .take(3)
        .map(|r| format!("{}: {}", r.area, r.text))
        .collect::<Vec<_>>()
        .join("；");
    let head = if head.is_empty() {
        "全部正常".to_string()
    } else {
        head
    };
    format!(
        "[patrol {}] {} {head}",
        rep.worst().upper(),
        util::fmt_ts(now, tz, "%m-%d %H:%M")
    )
}

pub fn render_human(rep: &Report, now: f64, tz: &TimeZone) -> String {
    let worst = rep.worst();
    let mut out = format!(
        "=== 巡检 {} → {} ===\n",
        util::fmt_ts(now, tz, "%Y-%m-%d %H:%M:%S"),
        worst.upper()
    );
    for area in ["系统", "上游", "通道", "注册", "成员", "数据"] {
        for r in rep.rows.iter().filter(|r| r.area == area) {
            let tag = match r.sev {
                Sev::Ok => "ok  ",
                Sev::Warn => "warn",
                Sev::Red => "RED ",
            };
            out.push_str(&format!("  [{tag}] {}\n", r.text));
        }
    }
    out.push_str(&format!(
        "=== 结论：{}（{} 项需要关注）===\n",
        worst.upper(),
        rep.problems().len()
    ));
    out
}

/// 私信正文：只列危险项（最多 12 条）；「注意」只报个数——它们在巡检日志和后台总览里。
pub fn alert_body(rep: &Report, now: f64, tz: &TimeZone) -> String {
    let red: Vec<&Row> = rep.rows.iter().filter(|r| r.sev == Sev::Red).collect();
    let mut lines: Vec<String> = red
        .iter()
        .take(12)
        .map(|r| format!("❗ {}：{}", r.area, r.text))
        .collect();
    let warn = rep.rows.iter().filter(|r| r.sev == Sev::Warn).count();
    if warn > 0 {
        lines.push(format!(
            "（另有 {warn} 项「注意」，不单独提醒，后台总览可看）"
        ));
    }
    format!(
        "🔎 **巡检** {}\n{}",
        util::fmt_ts(now, tz, "%m-%d %H:%M"),
        lines.join("\n")
    )
}

// ---------------------------------------------------------------- 告警

async fn send_alert(cfg: &Config, text: &str) -> bool {
    let token = util::env_file_value(&cfg.env_file, "DISCORD_BOT_TOKEN");
    let user = util::env_file_value(&cfg.env_file, "ALERT_USER_ID");
    let channel = util::env_file_value(&cfg.env_file, "ALERT_CHANNEL_ID");
    if token.is_empty() || (user.is_empty() && channel.is_empty()) {
        eprintln!("[patrol] 未配置告警通道（ALERT_USER_ID / ALERT_CHANNEL_ID），只记日志");
        return false;
    }
    let report_err = |e: &anyhow::Error| {
        let name = e.downcast_ref::<reqwest::Error>().map_or("Error", |r| {
            if r.is_timeout() {
                "TimeoutError"
            } else if r.is_connect() {
                "URLError"
            } else {
                "Error"
            }
        });
        eprintln!("[patrol] 告警发送失败：{name}");
    };
    let report_status = |status: u16, body: &str| {
        // 403 + Cloudflare 1010 = UA 不合规
        let hint = if status == 403 && pyc::take_chars(body, 200).contains("1010") {
            "（Cloudflare 1010：User-Agent 被拦）"
        } else {
            ""
        };
        eprintln!("[patrol] 告警发送失败：HTTP {status}{hint}");
    };
    let discord = match Discord::new(&cfg.api_base, &token, Duration::from_secs(10)) {
        Ok(d) => d,
        Err(e) => {
            report_err(&e);
            return false;
        }
    };
    let channel_id = if channel.is_empty() {
        match discord.create_dm(&user).await {
            Err(e) => {
                report_err(&e);
                return false;
            }
            Ok(r) if !r.ok() => {
                report_status(r.status, &r.body);
                return false;
            }
            Ok(r) => match r
                .json()
                .and_then(|j| j.get("id").and_then(Value::as_str).map(String::from))
            {
                Some(id) => id,
                None => {
                    eprintln!("[patrol] 告警发送失败：KeyError");
                    return false;
                }
            },
        }
    } else {
        channel
    };
    // 与旧版不同：关掉 @ 提及（告警正文里可能带报错标题，不能让它真的 @everyone）
    match discord
        .post_message(&channel_id, &discord::message_body(text))
        .await
    {
        Err(e) => {
            report_err(&e);
            false
        }
        Ok(r) if !r.ok() => {
            report_status(r.status, &r.body);
            false
        }
        Ok(_) => true,
    }
}

/// 只有**危险**才私信站长（站长 10-15：后台能看，非重大事故不私信）。
///
/// - 同一批危险项（按检查项标识去重，不看数字）冷却期内只发一次，出现新的危险项立刻发；
/// - 危险全部消失后补一条「已恢复」；
/// - 「注意」只打印在巡检日志里。
///
/// 状态文件 `last_alert.json`：`{digest, at, red, summary}`；没发成功不记状态，下一轮重试。
pub async fn maybe_alert(cfg: &Config, rep: &Report, now: f64, tz: &TimeZone) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("创建状态目录 {} 失败", cfg.state_dir.display()))?;
    let mark = cfg.state_dir.join("last_alert.json");
    let last = std::fs::read_to_string(&mark)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    let was_red = last.get("red").and_then(Value::as_bool).unwrap_or(false);
    let save = |v: Value| -> Result<()> {
        std::fs::write(&mark, gate_core::pyjson::default_str(&v))
            .with_context(|| format!("写 {} 失败", mark.display()))
    };

    let keys = rep.red_keys();
    if keys.is_empty() {
        if was_red {
            let since = last.get("since").and_then(Value::as_f64).unwrap_or(now);
            let summary = last.get("summary").and_then(Value::as_str).unwrap_or("");
            let text = format!(
                "✅ **巡检** {}\n已恢复（持续约 {} 分钟）：{summary}",
                util::fmt_ts(now, tz, "%m-%d %H:%M"),
                ((now - since) / 60.0).round().max(1.0) as i64
            );
            if send_alert(cfg, &text).await {
                save(json!({"red": false, "at": gate_core::pyval::float(now)}))?;
                eprintln!("[patrol] 已发送恢复通知");
            }
        }
        return Ok(());
    }

    let digest = hex::encode(Sha256::digest(keys.join("|").as_bytes()))[..16].to_string();
    let last_at = last.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    if was_red
        && last.get("digest").and_then(Value::as_str) == Some(digest.as_str())
        && now - last_at < cfg.cooldown as f64
    {
        return Ok(());
    }
    if send_alert(cfg, &alert_body(rep, now, tz)).await {
        let since = if was_red {
            last.get("since").and_then(Value::as_f64).unwrap_or(now)
        } else {
            now
        };
        let summary: Vec<&str> = rep
            .rows
            .iter()
            .filter(|r| r.sev == Sev::Red)
            .map(|r| r.text.as_str())
            .collect();
        save(json!({
            "red": true,
            "digest": digest,
            "at": gate_core::pyval::float(now),
            "since": gate_core::pyval::float(since),
            "summary": pyc::take_chars(&summary.join("；"), 300),
        }))?;
        eprintln!("[patrol] 已发送告警");
    }
    Ok(())
}

// ---------------------------------------------------------------- 主流程

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub alert: bool,
    pub json: bool,
    pub oneline: bool,
}

/// 只读打开数据库；打不开返回 Err（调用方按旧版打印并退出 2）。
fn open_ro(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        util::sqlite_uri(path, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    Ok(conn)
}

/// 返回退出码：0 / 1 / 2；内部错误由调用方（main）转成 3。
pub async fn run(cfg: &Config, args: &Args) -> Result<i32> {
    let conn = match open_ro(&cfg.db) {
        Ok(c) => c,
        Err(e) => {
            println!("[patrol] 打不开数据库 {}：{e}", cfg.db.display());
            return Ok(2);
        }
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let infra = gather_infra(cfg, &http).await;
    let gate = cfg.containers.first().map_or("owl-gate", String::as_str);
    let restarts = restart_history(&cfg.state_dir, container_started_at(gate).await);
    run_with(
        cfg,
        args,
        &conn,
        &infra,
        restarts,
        util::now_secs(),
        util::local_tz(),
    )
    .await
}

/// 拿到系统事实之后的部分：跑检查、打印、（需要时）告警。测试可以直接喂合成的 `Infra`。
pub async fn run_with(
    cfg: &Config,
    args: &Args,
    conn: &Connection,
    infra: &Infra,
    restarts: Vec<f64>,
    now: f64,
    tz: TimeZone,
) -> Result<i32> {
    let ctx = Ctx {
        now,
        cfg,
        tz: tz.clone(),
        restarts,
    };
    let mut rep = Report::default();
    run_checks(&mut rep, conn, infra, &ctx).context("巡检查询失败")?;

    let worst = rep.worst();
    if args.json {
        println!("{}", render_json(&rep));
    } else if args.oneline {
        println!("{}", render_oneline(&rep, now, &tz));
    } else {
        print!("{}", render_human(&rep, now, &tz));
    }
    if args.alert {
        maybe_alert(cfg, &rep, now, &tz).await?;
    }
    Ok(worst.rank())
}
