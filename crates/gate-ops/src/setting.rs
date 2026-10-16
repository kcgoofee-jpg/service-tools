//! `nai-ops set <name> <value>` / `nai-ops get <name>`：后台设置接口的薄客户端。
//!
//! 不直接碰数据库：全部走网关已有的 `/admin/api/*` 接口，所以校验、操作日志、内存里的即时生效都和后台页面完全一致。
//! 只认白名单里的名字（`set --list` 可看）。改之前先读当前值，打印「改前 → 改后」，没有 `--yes` 不写
//! （`--dry-run` 只演示）。密码读 `ADMIN_PASSWORD`（环境变量，其次 `.env`），登录后的 Cookie 只放在内存里，
//! 任何输出和错误信息都不带密码 / Cookie。
//! 注意：后台改过登录密码的话，以库里保存的为准，`.env` 里的 `ADMIN_PASSWORD` 只是初始值。

use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:3003/admin/api";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ep {
    /// `/settings`：Anlas / V5 预算
    Settings,
    /// `/guard`：账号保护与排队
    Guard,
    /// `/runtime-limits`：排队与冷却
    RuntimeLimits,
    /// `/ops/registration`：领 Key
    Registration,
    /// 没有后台接口：明确不支持（不去写数据库）
    Unsupported(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Float,
    Bool,
    Text,
    Choice(&'static [&'static str]),
    /// `服务器ID:身份组ID`，两者同时设置；留空 = 两个都清空
    GuildRole,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub name: &'static str,
    pub ep: Ep,
    pub kind: Kind,
    /// 接口里的字段名
    pub field: &'static str,
    pub desc: &'static str,
}

const fn s(name: &'static str, ep: Ep, kind: Kind, desc: &'static str) -> Spec {
    Spec {
        name,
        ep,
        kind,
        field: name,
        desc,
    }
}

/// 白名单。范围以服务端为准（这里只做类型检查，服务端不接受会原样转述原因）。
pub const SPECS: &[Spec] = &[
    s(
        "global_daily_anlas",
        Ep::Settings,
        Kind::Int,
        "全站每日 Anlas 池（0~100000，0 = 不限）",
    ),
    s(
        "global_monthly_anlas",
        Ep::Settings,
        Kind::Float,
        "全站每月 Anlas 预算（0~1000000，0 = 不限）",
    ),
    s(
        "member_daily_anlas",
        Ep::Settings,
        Kind::Int,
        "每人每日 Anlas 上限（0~100000，0 = 不限）",
    ),
    s(
        "global_daily_v5",
        Ep::Settings,
        Kind::Int,
        "全站每日 V5 张数（0~100000；动态额度开着时服务端会拒绝手改）",
    ),
    s(
        "v5_alert_threshold",
        Ep::Settings,
        Kind::Int,
        "V5 余量低提示线（1~100，百分比）",
    ),
    s(
        "v5_medium_mode",
        Ep::Settings,
        Kind::Choice(&["off", "on", "auto"]),
        "V5 Medium 降档：off / on / auto",
    ),
    Spec {
        name: "register_max_users",
        ep: Ep::Registration,
        kind: Kind::Int,
        field: "max_users",
        desc: "自助领 Key 名额上限（0~1000）",
    },
    Spec {
        name: "register_open",
        ep: Ep::Registration,
        kind: Kind::Bool,
        field: "open",
        desc: "是否开放自助领 Key（on / off）",
    },
    Spec {
        name: "role_note",
        ep: Ep::Registration,
        kind: Kind::Text,
        field: "role_note",
        desc: "限定身份组的名称说明（最多 60 字，首页被挡时显示）",
    },
    Spec {
        name: "role_guild",
        ep: Ep::Registration,
        kind: Kind::Text,
        field: "role_guild",
        desc: "限定身份组所在服务器 ID（数字；必须与 role_id 同时有或同时空，单独改常被拒，推荐用 role_gate）",
    },
    Spec {
        name: "role_id",
        ep: Ep::Registration,
        kind: Kind::Text,
        field: "role_id",
        desc: "限定身份组 ID（数字；同上，推荐用 role_gate）",
    },
    Spec {
        name: "role_gate",
        ep: Ep::Registration,
        kind: Kind::GuildRole,
        field: "role_gate",
        desc: "同时设服务器和身份组：服务器ID:身份组ID；传 \"\" 清空两者",
    },
    Spec {
        name: "issue_hourly_cap",
        ep: Ep::Unsupported(
            "后台目前没有 issue_hourly_cap（每小时最多发放的新 Key）的修改接口。nai-ops 不会绕过后台直接写数据库（那样没有校验、没有操作日志、网关内存也不会刷新）；需要改请先在网关加后台接口",
        ),
        kind: Kind::Int,
        field: "issue_hourly_cap",
        desc: "每小时最多发放的新 Key（不支持：后台没有接口）",
    },
    s(
        "account_daily_cap",
        Ep::Guard,
        Kind::Int,
        "每个上游账号每天最多出图张数",
    ),
    s(
        "account_hourly_cap",
        Ep::Guard,
        Kind::Int,
        "每个上游账号每小时最多出图张数",
    ),
    s(
        "queue_per_account",
        Ep::Guard,
        Kind::Int,
        "每个上游账号全站最多同时排几张图",
    ),
    s(
        "key_image_queue",
        Ep::Guard,
        Kind::Int,
        "每把 Key 在生成中的那张之外最多再排几张",
    ),
    s(
        "image_min_interval",
        Ep::RuntimeLimits,
        Kind::Int,
        "账号出图最小间隔（秒，15~120）",
    ),
    s(
        "queue_timeout",
        Ep::RuntimeLimits,
        Kind::Int,
        "排队超时（秒，15~300）",
    ),
];

pub fn find(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

fn kind_label(k: Kind) -> String {
    match k {
        Kind::Int => "整数".into(),
        Kind::Float => "数字".into(),
        Kind::Bool => "on/off".into(),
        Kind::Text => "文本".into(),
        Kind::Choice(c) => c.join("/"),
        Kind::GuildRole => "服务器ID:身份组ID".into(),
    }
}

fn ep_label(ep: Ep) -> &'static str {
    match ep {
        Ep::Settings => "PUT /settings",
        Ep::Guard => "PUT /guard",
        Ep::RuntimeLimits => "PUT /runtime-limits",
        Ep::Registration => "PUT /ops/registration",
        Ep::Unsupported(_) => "（不支持）",
    }
}

/// `set --list` 的输出。
pub fn list_text() -> String {
    let mut out = String::from("可用设置名（全部走后台接口，校验与操作日志同后台页面）：\n");
    for sp in SPECS {
        out.push_str(&format!(
            "  {:<22} {:<22} {:<18} {}\n",
            sp.name,
            ep_label(sp.ep),
            kind_label(sp.kind),
            sp.desc
        ));
    }
    out.push_str("\n用法：nai-ops set <名字> <值> [--dry-run | --yes]    nai-ops get <名字>\n");
    out
}

/// 把命令行文本变成接口要的 JSON；类型不对返回原因。
pub fn parse_value(kind: Kind, raw: &str) -> Result<Value, String> {
    let t = raw.trim();
    match kind {
        Kind::Int => t
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{raw:?} 不是整数")),
        Kind::Float => match t.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(json!(f)),
            _ => Err(format!("{raw:?} 不是数字")),
        },
        Kind::Bool => match t.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => Ok(Value::Bool(true)),
            "off" | "false" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("{raw:?} 不是 on / off")),
        },
        Kind::Text => Ok(Value::String(raw.to_string())),
        Kind::Choice(c) => {
            if c.contains(&t) {
                Ok(Value::String(t.to_string()))
            } else {
                Err(format!("{raw:?} 不在 {} 之中", c.join(" / ")))
            }
        }
        Kind::GuildRole => {
            if t.is_empty() {
                return Ok(Value::String(String::new()));
            }
            match t.split_once(':') {
                Some((g, r)) if !g.trim().is_empty() && !r.trim().is_empty() => {
                    Ok(Value::String(format!("{}:{}", g.trim(), r.trim())))
                }
                _ => Err("需要写成 服务器ID:身份组ID，或传 \"\" 清空".into()),
            }
        }
    }
}

/// 展示用文本。
pub fn show(v: &Value) -> String {
    match v {
        Value::Null => "(未设置)".into(),
        Value::String(s) if s.is_empty() => "(空)".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n
            .as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() < 1e15)
            .map_or_else(|| n.to_string(), |f| format!("{}", f as i64)),
        other => other.to_string(),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
        _ => a == b,
    }
}

/// PUT 的请求体。
fn put_body(sp: &Spec, v: &Value) -> Value {
    if sp.kind == Kind::GuildRole {
        let t = v.as_str().unwrap_or("");
        let (g, r) = t.split_once(':').unwrap_or(("", ""));
        return json!({"role_guild": g, "role_id": r});
    }
    json!({ sp.field: v })
}

fn put_path(ep: Ep) -> &'static str {
    match ep {
        Ep::Settings => "/settings",
        Ep::Guard => "/guard",
        Ep::RuntimeLimits => "/runtime-limits",
        Ep::Registration => "/ops/registration",
        Ep::Unsupported(_) => "",
    }
}

/// 从 GET 响应里取当前值。
fn read_current(sp: &Spec, ep_json: &Value) -> Option<Value> {
    match sp.ep {
        Ep::Settings => ep_json.get(sp.field).cloned(),
        Ep::Guard => ep_json.pointer(&format!("/values/{}", sp.field)).cloned(),
        Ep::RuntimeLimits => ep_json.get(sp.field).cloned(),
        Ep::Registration => {
            if sp.kind == Kind::GuildRole {
                let g = ep_json.pointer("/registration/role_guild")?.as_str()?;
                let r = ep_json.pointer("/registration/role_id")?.as_str()?;
                Some(Value::String(if g.is_empty() && r.is_empty() {
                    String::new()
                } else {
                    format!("{g}:{r}")
                }))
            } else {
                ep_json
                    .pointer(&format!("/registration/{}", sp.field))
                    .cloned()
            }
        }
        Ep::Unsupported(_) => None,
    }
}

fn get_path(ep: Ep) -> &'static str {
    match ep {
        Ep::Settings => "/settings",
        Ep::Guard => "/guard",
        Ep::RuntimeLimits => "/runtime-limits",
        Ep::Registration => "/ops",
        Ep::Unsupported(_) => "",
    }
}

// ---------------------------------------------------------------- 后台接口客户端

pub struct Admin {
    base: String,
    http: reqwest::Client,
    cookie: String,
}

fn error_text(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body).ok().and_then(|j| {
        j.get("detail")
            .or_else(|| j.get("message"))
            .map(|d| d.as_str().map_or_else(|| d.to_string(), String::from))
    });
    let detail = detail.unwrap_or_else(|| body.chars().take(200).collect());
    format!("HTTP {status}：{detail}")
}

impl Admin {
    pub async fn login(base: &str, password: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        let base = base.trim_end_matches('/').to_string();
        let resp = http
            .post(format!("{base}/login"))
            .json(&json!({"password": password}))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("连不上后台接口 {base}（{}）", e.without_url()))?;
        let status = resp.status().as_u16();
        let cookie = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .filter(|kv| kv.contains('='))
            .collect::<Vec<_>>()
            .join("; ");
        let body = resp.text().await.unwrap_or_default();
        if status == 401 {
            bail!(
                "后台登录失败：密码不对。后台改过密码的话以库里保存的为准，.env 里的 ADMIN_PASSWORD 只是初始值"
            );
        }
        if !(200..300).contains(&status) {
            bail!("后台登录失败：{}", error_text(status, &body));
        }
        if cookie.is_empty() {
            bail!("后台登录成功但没有收到会话 Cookie");
        }
        Ok(Self { base, http, cookie })
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req
            .header("Cookie", &self.cookie)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("请求后台失败（{}）", e.without_url()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            bail!("{}", error_text(status, &body));
        }
        serde_json::from_str(&body).context("后台返回的不是 JSON")
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.call(self.http.put(format!("{}{path}", self.base)).json(body))
            .await
    }
}

/// 管理密码：环境变量优先，其次 `.env`（`NAI_GATE_ENV`，默认 `/opt/service-tools/.env`）。
pub fn admin_password() -> String {
    util::env_nonempty("ADMIN_PASSWORD").unwrap_or_else(|| {
        let f = util::env_nonempty("NAI_GATE_ENV")
            .unwrap_or_else(|| "/opt/service-tools/.env".to_string());
        util::env_file_value(Path::new(&f), "ADMIN_PASSWORD")
    })
}

pub fn api_base() -> String {
    util::env_nonempty("ADMIN_API_BASE").unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

async fn connect() -> Result<Admin> {
    let pw = admin_password();
    if pw.is_empty() {
        bail!("没有找到 ADMIN_PASSWORD（环境变量或 .env）");
    }
    Admin::login(&api_base(), &pw).await
}

// ---------------------------------------------------------------- 命令

/// 返回 (退出码, 输出文本)。0 成功；2 = 用法错误 / 被拒绝 / 不支持。
pub async fn run_get(name: &str) -> Result<(i32, String)> {
    let Some(sp) = find(name) else {
        return Ok((
            2,
            format!("未知设置名 {name:?}；nai-ops set --list 可看全部\n"),
        ));
    };
    if let Ep::Unsupported(why) = sp.ep {
        return Ok((2, format!("{}：{why}\n", sp.name)));
    }
    let admin = connect().await?;
    run_get_with(&admin, sp).await
}

pub async fn run_get_with(admin: &Admin, sp: &Spec) -> Result<(i32, String)> {
    let j = admin.get(get_path(sp.ep)).await?;
    let cur = read_current(sp, &j).with_context(|| format!("后台返回里没有 {}", sp.name))?;
    Ok((0, format!("{} = {}\n", sp.name, show(&cur))))
}

pub async fn run_set(name: &str, raw: &str, yes: bool, dry_run: bool) -> Result<(i32, String)> {
    let Some(sp) = find(name) else {
        return Ok((
            2,
            format!("未知设置名 {name:?}；nai-ops set --list 可看全部\n"),
        ));
    };
    if let Ep::Unsupported(why) = sp.ep {
        return Ok((2, format!("{}：不支持。{why}\n", sp.name)));
    }
    let new = match parse_value(sp.kind, raw) {
        Ok(v) => v,
        Err(why) => return Ok((2, format!("{}：{why}\n", sp.name))),
    };
    let admin = connect().await?;
    run_set_with(&admin, sp, new, yes, dry_run).await
}

pub async fn run_set_with(
    admin: &Admin,
    sp: &Spec,
    new: Value,
    yes: bool,
    dry_run: bool,
) -> Result<(i32, String)> {
    let before_json = admin.get(get_path(sp.ep)).await?;
    let before =
        read_current(sp, &before_json).with_context(|| format!("后台返回里没有 {}", sp.name))?;
    let plan = format!("{}: {} → {}\n", sp.name, show(&before), show(&new));
    if same(&before, &new) {
        return Ok((0, format!("{plan}（值没有变化，不写）\n")));
    }
    if dry_run {
        return Ok((0, format!("{plan}（--dry-run：没有写入）\n")));
    }
    if !yes {
        return Ok((
            2,
            format!("{plan}没有写入：要真的修改请加 --yes（先预览用 --dry-run）\n"),
        ));
    }
    admin.put(put_path(sp.ep), &put_body(sp, &new)).await?;
    let after_json = admin.get(get_path(sp.ep)).await?;
    let after =
        read_current(sp, &after_json).with_context(|| format!("后台返回里没有 {}", sp.name))?;
    let mut out = format!("{}: {} → {}\n", sp.name, show(&before), show(&after));
    if same(&after, &new) {
        out.push_str("已写入（后台操作日志里可见，网关内存已即时生效）\n");
        Ok((0, out))
    } else {
        out.push_str("警告：写入后读回的值与期望不一致，请到后台核对\n");
        Ok((1, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_values() {
        assert_eq!(parse_value(Kind::Int, " 12 "), Ok(json!(12)));
        assert!(parse_value(Kind::Int, "1.5").is_err());
        assert_eq!(parse_value(Kind::Float, "2.5"), Ok(json!(2.5)));
        assert!(parse_value(Kind::Float, "nan").is_err());
        assert_eq!(parse_value(Kind::Bool, "ON"), Ok(json!(true)));
        assert_eq!(parse_value(Kind::Bool, "0"), Ok(json!(false)));
        assert!(parse_value(Kind::Bool, "maybe").is_err());
        assert!(parse_value(Kind::Choice(&["off", "on"]), "auto").is_err());
        assert_eq!(parse_value(Kind::GuildRole, ""), Ok(json!("")));
        assert_eq!(parse_value(Kind::GuildRole, "1:2"), Ok(json!("1:2")));
        assert!(parse_value(Kind::GuildRole, "12").is_err());
    }

    #[test]
    fn show_numbers() {
        assert_eq!(show(&json!(5000.0)), "5000");
        assert_eq!(show(&json!(2.5)), "2.5");
        assert_eq!(show(&json!("")), "(空)");
    }

    #[test]
    fn names_are_unique_and_guard_fields_exist() {
        let mut names: Vec<_> = SPECS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
        for sp in SPECS.iter().filter(|s| s.ep == Ep::Guard) {
            assert!(
                gate_core::guard::FIELDS.iter().any(|f| f.name == sp.field),
                "{} 不是 guard 字段",
                sp.name
            );
        }
        assert!(list_text().contains("issue_hourly_cap"));
    }

    #[test]
    fn put_body_shapes() {
        let sp = find("register_open").unwrap();
        assert_eq!(put_body(sp, &json!(true)), json!({"open": true}));
        let sp = find("role_gate").unwrap();
        assert_eq!(
            put_body(sp, &json!("11:22")),
            json!({"role_guild": "11", "role_id": "22"})
        );
        assert_eq!(
            put_body(sp, &json!("")),
            json!({"role_guild": "", "role_id": ""})
        );
    }
}
