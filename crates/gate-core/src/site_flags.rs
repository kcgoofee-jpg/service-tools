//! 跨模块共用的站点设置登记表（对应 `app/site_flags.py`）：键名、类型、默认值、范围、归属只在这里定义一次。
//!
//! 不在这里的（服务层负责）：
//! - `get(db, spec, settings)`：`db.get_setting(spec.key, None)` 读 `site_settings` 表的 `spec.key` 一行（TEXT 或没有），
//!   再交给 [`get_from_raw`]（= `parse(spec, raw, default_of(spec, settings))`）；
//! - `put(db, spec, value)`：`db.set_setting(spec.key, encode(spec, value))`，即 `site_settings` 写 `spec.key`，
//!   值是 [`encode`] 的返回串。
//!
//! 与旧版一致的怪癖：`parse` 里的 `max(lo, num)` / `min(hi, num)` 返回的是「胜出的那个对象」，
//! 所以 float 类型的设置被夹到边界时得到的是 **int**（`lo` / `hi` 在登记表里都是整数字面量），
//! `encode` 因此写出 `"0"` 而不是 `"0.0"`；没夹到边界才是 float。

use crate::{pyc, pyext, pyfmt, pyval};
use serde_json::Value;

/// 设置项的类型（`Spec.kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// "1" / "0"
    Flag,
    Int,
    Float,
    Str,
    Choice,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Flag => "flag",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Str => "str",
            Kind::Choice => "choice",
        }
    }
}

/// `Spec.default`（Python 里是任意对象；None 表示「取 .env / 回落到 0」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecDefault {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'static str),
}

impl SpecDefault {
    fn to_value(self) -> Value {
        match self {
            SpecDefault::None => Value::Null,
            SpecDefault::Bool(b) => Value::Bool(b),
            SpecDefault::Int(i) => Value::from(i),
            SpecDefault::Float(f) => pyval::float(f),
            SpecDefault::Str(s) => Value::String(s.into()),
        }
    }

    /// `repr(default)`
    fn repr(self) -> String {
        match self {
            SpecDefault::None => "None".into(),
            SpecDefault::Bool(b) => pyc::bool_str(b).into(),
            SpecDefault::Int(i) => i.to_string(),
            SpecDefault::Float(f) => pyc::float_repr(f),
            SpecDefault::Str(s) => pyext::repr_str(s),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub key: &'static str,
    pub kind: Kind,
    pub default: SpecDefault,
    pub owner: &'static str,
    pub doc: &'static str,
    /// 登记表里的上下限都是整数字面量（见模块说明里的类型怪癖）
    pub lo: Option<i64>,
    pub hi: Option<i64>,
    pub choices: &'static [&'static str],
    /// 默认值取自 `config.Settings` 的这个属性（.env 可调），空串 = 不取
    pub env: &'static str,
}

const fn spec(
    key: &'static str,
    kind: Kind,
    default: SpecDefault,
    owner: &'static str,
    doc: &'static str,
) -> Spec {
    Spec {
        key,
        kind,
        default,
        owner,
        doc,
        lo: None,
        hi: None,
        choices: &[],
        env: "",
    }
}

const fn bounded(s: Spec, lo: i64, hi: i64) -> Spec {
    Spec {
        lo: Some(lo),
        hi: Some(hi),
        ..s
    }
}

const fn with_env(s: Spec, env: &'static str) -> Spec {
    Spec { env, ..s }
}

pub static WEB_LOGIN_PAUSED: Spec = spec(
    "web_login_paused",
    Kind::Flag,
    SpecDefault::Bool(true),
    "registration_routes",
    "网页 Discord 登录暂停（Discord 回调修复 / 审核期间）。网页只有 Discord 登录，暂停期间成员只能在 Discord 里用 /quota 看额度",
);
pub static DM_ENABLED: Spec = spec(
    "dm_enabled",
    Kind::Flag,
    SpecDefault::Bool(false),
    "registration",
    "私信总闸门（关 = 任何私信都不发）",
);
pub static ISSUE_HOURLY_CAP: Spec = bounded(
    spec(
        "issue_hourly_cap",
        Kind::Int,
        SpecDefault::Int(12),
        "registration",
        "每小时最多发放的新 Key",
    ),
    0,
    500,
);
pub static REGISTER_OPEN: Spec = spec(
    "register_open",
    Kind::Flag,
    SpecDefault::Bool(false),
    "modules / registration",
    "开放 /register 领取",
);
pub static HOME_JOIN_OPEN: Spec = spec(
    "home_join_open",
    Kind::Flag,
    SpecDefault::Bool(true),
    "landing / admin",
    "首页「加入 Discord」按钮（独立于 register_open：只管首页按钮显示，关 = 置灰「关闭注册」）",
);
/// 动态额度每轮写入；没设过按 150。2.x 的默认值取环境变量 `GLOBAL_DAILY_V5`，3.0 删了这个环境变量
/// （docs/v3-deviations.md #70）。
pub static GLOBAL_DAILY_V5: Spec = bounded(
    spec(
        "global_daily_v5",
        Kind::Int,
        SpecDefault::Int(150),
        "额度 / 首页 / 客户端",
        "全站每日 V5 张数（0 = 不限）",
    ),
    0,
    100000,
);
pub static GLOBAL_DAILY_ANLAS: Spec = with_env(
    bounded(
        spec(
            "global_daily_anlas",
            Kind::Int,
            SpecDefault::None,
            "额度 / Vibe PVP",
            "全站每日 Anlas 额度（0 = 不限，Vibe PVP 抢额度）",
        ),
        0,
        100000,
    ),
    "global_daily_anlas",
);
pub static GLOBAL_DAILY_ANLAS_UNTIL: Spec = bounded(
    spec(
        "global_daily_anlas_until",
        Kind::Float,
        SpecDefault::Float(0.0),
        "额度 / Vibe PVP",
        "全站每日 Anlas 开放截止时间戳（0 = 不限时）",
    ),
    0,
    2_000_000_000,
);
pub static GLOBAL_MONTHLY_ANLAS: Spec = with_env(
    bounded(
        spec(
            "global_monthly_anlas",
            Kind::Float,
            SpecDefault::None,
            "额度 / Anlas 池",
            "全站每月 Anlas 预算（0 = 不限）",
        ),
        0,
        1_000_000,
    ),
    "global_monthly_anlas",
);
pub static MEMBER_DAILY_ANLAS: Spec = bounded(
    spec(
        "member_daily_anlas",
        Kind::Int,
        SpecDefault::Int(50),
        "额度 / Anlas 池",
        "每人每日 Anlas 上限（仅对未手动设置 Anlas 日额度的 Key 生效；0 = 不限）。Vibe 编码一张参考图约 2 点，50 点 ≈ 25 次/天，防单人吃光全站 PVP 池",
    ),
    0,
    100000,
);
pub static ALGO_NOTICE: Spec = spec(
    "algo_notice",
    Kind::Str,
    SpecDefault::Str(""),
    "quota_algo → 首页",
    "首页显示的今日额度说明",
);
pub static IMAGE_RETENTION: Spec = bounded(
    spec(
        "audit_image_retention_days",
        Kind::Int,
        SpecDefault::Int(3),
        "audit",
        "（已停用，2026-10-11 起原图按容量上限清理）",
    ),
    0,
    365,
);
pub static IMAGE_CAP_MB: Spec = bounded(
    spec(
        "audit_image_cap_mb",
        Kind::Int,
        SpecDefault::Int(8192),
        "audit",
        "原图占用上限（MB）：超过后自动删掉最旧的原图；0 = 不限",
    ),
    0,
    500_000,
);
pub static IMAGE_KEEP_HOURS: Spec = bounded(
    spec(
        "audit_image_keep_hours",
        Kind::Int,
        SpecDefault::Int(24),
        "audit",
        "原图在服务器上保存几小时，到点自动删；0 = 只按容量上限删",
    ),
    0,
    8760,
);
pub static MEMBER_SWEEP: Spec = spec(
    "member_sweep",
    Kind::Flag,
    SpecDefault::Bool(true),
    "registration",
    "退群回收：每人每天核对一次是否还在服务器，不在就删 Key",
);

/// 登记表（顺序与旧版 `ALL` 一致，`facts_table` 按这个顺序出行）。
pub static ALL: [&Spec; 14] = [
    &MEMBER_SWEEP,
    &WEB_LOGIN_PAUSED,
    &DM_ENABLED,
    &ISSUE_HOURLY_CAP,
    &REGISTER_OPEN,
    &HOME_JOIN_OPEN,
    &GLOBAL_DAILY_V5,
    &GLOBAL_DAILY_ANLAS,
    &GLOBAL_DAILY_ANLAS_UNTIL,
    &GLOBAL_MONTHLY_ANLAS,
    &ALGO_NOTICE,
    &IMAGE_RETENTION,
    &IMAGE_CAP_MB,
    &IMAGE_KEEP_HOURS,
];

/// `default_of(spec, settings)`。`settings` 是 `config.Settings` 的 JSON 视图（对象；属性存在 = 键存在）。
pub fn default_of(spec: &Spec, settings: Option<&Value>) -> Value {
    if !spec.env.is_empty()
        && let Some(raw) = settings.and_then(|s| pyval::get(s, spec.env))
    {
        return parse(spec, raw, Value::from(0));
    }
    match spec.default {
        SpecDefault::None => Value::from(0),
        d => d.to_value(),
    }
}

/// `parse(spec, raw, fallback)`：库里的原始值（字符串 / `Settings` 属性 / 请求体里的任意值）→ 带类型的值。
/// 转不了返回 `fallback`。`raw` 为 `Value::Null` 即 Python 的 `None`。
pub fn parse(spec: &Spec, raw: &Value, fallback: Value) -> Value {
    if raw.is_null() {
        return fallback;
    }
    if let Value::String(s) = raw
        && pyc::strip(s).is_empty()
        && spec.kind != Kind::Str
    {
        return fallback;
    }
    let s = pyc::strip(&pyext::py_str(raw)).to_string();
    let low = s.to_lowercase();
    match spec.kind {
        // 认不出的写法回到默认值（fail-closed），不能当成「关」
        Kind::Flag => match low.as_str() {
            "1" | "true" | "yes" | "on" => Value::Bool(true),
            "0" | "false" | "no" | "off" => Value::Bool(false),
            _ => fallback,
        },
        Kind::Choice => {
            if spec.choices.contains(&s.as_str()) {
                Value::String(s)
            } else {
                fallback
            }
        }
        Kind::Str => Value::String(pyext::py_str(raw)),
        Kind::Int | Kind::Float => {
            let Some(f) = pyc::parse_float(&s) else {
                return fallback;
            };
            if f.is_nan() {
                return fallback;
            }
            // max(lo, num) / min(hi, num)：num 不严格胜出时返回的是边界那个 int（见模块说明）
            let mut num = Num::Float(f);
            if let Some(lo) = spec.lo
                && num.as_f64() <= lo as f64
            {
                num = Num::Int(lo);
            }
            if let Some(hi) = spec.hi
                && num.as_f64() >= hi as f64
            {
                num = Num::Int(hi);
            }
            match (spec.kind, num) {
                (Kind::Int, Num::Float(x)) => Value::from(x.trunc() as i64),
                (_, Num::Float(x)) => pyval::float(x),
                (_, Num::Int(i)) => Value::from(i),
            }
        }
    }
}

/// `max` / `min` 之后的数：保留 Python 里「是 int 还是 float」的类型信息。
#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

/// `get(db, spec, settings)` 的纯部分：`raw` 是 `db.get_setting(spec.key, None)` 的结果（没有这一行 = None）。
pub fn get_from_raw(spec: &Spec, raw: Option<&str>, settings: Option<&Value>) -> Value {
    let fallback = default_of(spec, settings);
    match raw {
        None => fallback,
        Some(s) => parse(spec, &Value::String(s.to_string()), fallback),
    }
}

/// `encode(spec, value)` 的错误（Python 抛 `ValueError`，带中文原因）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct EncodeError(pub String);

/// `encode(spec, value)`：要写进 `site_settings` 的字符串。
pub fn encode(spec: &Spec, value: &Value) -> Result<String, EncodeError> {
    match spec.kind {
        Kind::Flag => Ok(if pyval::truthy(value) { "1" } else { "0" }.into()),
        Kind::Choice => {
            let ok = matches!(value, Value::String(s) if spec.choices.contains(&s.as_str()));
            if !ok {
                return Err(EncodeError(format!(
                    "{} 只能是 {}",
                    spec.key,
                    spec.choices.join(" / ")
                )));
            }
            Ok(pyext::py_str(value))
        }
        Kind::Int | Kind::Float => {
            let v = parse(spec, value, Value::Null);
            if v.is_null() {
                return Err(EncodeError(format!("{} 必须是数字", spec.key)));
            }
            Ok(pyext::py_str(&v))
        }
        Kind::Str => Ok(pyext::py_str(value)),
    }
}

/// `facts_table()`：SYSTEM-FACTS 里的 Markdown 表格。
pub fn facts_table() -> String {
    let mut rows = vec![
        "| 键 | 类型 | 默认 | 范围 | 归属 | 说明 |".to_string(),
        "|---|---|---|---|---|---|".to_string(),
    ];
    for s in ALL {
        let d = if s.env.is_empty() {
            s.default.repr()
        } else {
            format!("`.env {}`", s.env)
        };
        let rng = if !s.choices.is_empty() {
            s.choices.join(" / ")
        } else if let (Some(lo), Some(hi)) = (s.lo, s.hi) {
            format!("{}–{}", pyfmt::fmt_g(lo as f64), pyfmt::fmt_g(hi as f64))
        } else {
            String::new()
        };
        rows.push(format!(
            "| `{}` | {} | {} | {} | {} | {} |",
            s.key,
            s.kind.as_str(),
            d,
            rng,
            s.owner,
            s.doc
        ));
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamps_with_int_edges() {
        // float 设置被夹到边界时是 int，不是 float
        let v = parse(&GLOBAL_DAILY_ANLAS_UNTIL, &json!("-5"), Value::Null);
        assert!(v.is_i64() && v == json!(0));
        let v = parse(&GLOBAL_DAILY_ANLAS_UNTIL, &json!("12.5"), Value::Null);
        assert!(v.is_f64());
        assert_eq!(
            encode(&GLOBAL_MONTHLY_ANLAS, &json!("5000000")).unwrap(),
            "1000000"
        );
    }

    #[test]
    fn flags_fail_closed() {
        assert_eq!(
            parse(&DM_ENABLED, &json!("maybe"), Value::Bool(false)),
            Value::Bool(false)
        );
        assert_eq!(
            parse(&WEB_LOGIN_PAUSED, &json!(" OFF "), Value::Bool(true)),
            Value::Bool(false)
        );
        assert_eq!(
            get_from_raw(&ISSUE_HOURLY_CAP, Some("9999"), None),
            json!(500)
        );
    }
}
