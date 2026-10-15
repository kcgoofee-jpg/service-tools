//! 站长可在运行中调整的开关（旧版 `app/ops.py`）：自助领 Key（注册）、全局功能、生成记录。
//!
//! 所有值保存在 `site_settings`，没设过时用 [`crate::defaults`] 里的常量（没有环境变量）；机器人每次命令都实时读取，所以改了立刻生效。
//! 校验失败的文案、存进库里的字符串（整数不带小数点、开关 "1" / "0"）都与旧版逐字一致——旧 Python 还能读这些行。
//!
//! 另外放几个后台接口共用的「按数据库区分的单例」（旧版是模块级全局变量）：
//! 测试告警的 10 秒节流、运行期限制的写锁、Anlas 核对的互斥锁。

use crate::audit::{audit_flags, audit_image_cap_mb, audit_image_keep_hours};
use crate::defaults;
use crate::error::{ApiError, HttpError, InternalError};
use crate::state::AppState;
use gate_core::features::{self, FEATURES, GLOBAL_KEY};
use gate_core::pyext::{self, PyErr};
use gate_core::site_flags::{self, GLOBAL_DAILY_V5, REGISTER_OPEN};
use gate_core::{jsondump, pyc, pyval};
use gate_db::{Db, DbError};
use serde_json::{Map, Value, json};
use std::sync::Mutex;

/// 后台这几个接口里的错误：参数不合法回 422，归动态额度管的设置回 400，库里的数据坏了回 500。
#[derive(Debug)]
pub enum OpsError {
    /// 旧版 `ValueError` / `TypeError`：路由层回 422，原文作为 `detail`。
    Invalid(String),
    /// 动态额度开着时想改它管的设置：400（docs/v3-deviations.md #70）。
    Managed(String),
    Db(DbError),
    /// 库里的存档坏了（旧版会抛未捕获的异常 → 500）。
    Corrupt(String),
}

impl From<DbError> for OpsError {
    fn from(e: DbError) -> Self {
        OpsError::Db(e)
    }
}

impl From<OpsError> for ApiError {
    fn from(e: OpsError) -> Self {
        match e {
            OpsError::Invalid(m) => HttpError::new(422, m).into(),
            OpsError::Managed(m) => HttpError::new(400, m).into(),
            OpsError::Db(e) => e.into(),
            OpsError::Corrupt(m) => InternalError(m).into(),
        }
    }
}

// ------------------------------------------------------------------ 进程内共享状态

/// 旧版的模块级全局变量，挂在 `AppState::ops` 上。
#[derive(Default)]
pub struct Shared {
    /// 上一次发测试告警的时间（旧版 `_last_alert_test_time`）。
    pub alert_test_at: Mutex<f64>,
    /// 旧版 `_runtime_limits_lock`：写库 + 生效必须连在一起。
    pub runtime_limits: tokio::sync::Mutex<()>,
    /// 旧版 `ManualReconciliation.lock`：同一时刻只允许一次 Anlas 核对。
    pub reconcile: tokio::sync::Mutex<()>,
}

// ------------------------------------------------------------------ 动态额度管着的设置

/// 动态额度每轮都写的四个设置：新 Key 的每日 V4.5 / V5 张数与模型范围、全站每日 V5。
/// 读取只走 [`algo_owned`]；动态额度开着时后台不能改（[`managed_error`]）。
/// 2.x 的环境变量 `REGISTER_DAILY_IMAGES` / `REGISTER_DAILY_V5` / `REGISTER_IMAGE_SCOPE` / `GLOBAL_DAILY_V5`
/// 已删除，没设过时用这里的起点值（docs/v3-deviations.md #70）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlgoOwned {
    /// `register_daily_images`，起点 30。
    pub daily_images: i64,
    /// `register_daily_v5`，起点 0。
    pub daily_v5: i64,
    /// `register_image_scope`，起点 `legacy`。
    pub image_scope: String,
    /// `global_daily_v5`，起点 150（0 = 不限）。
    pub global_daily_v5: i64,
}

pub async fn algo_owned(db: &Db) -> Result<AlgoOwned, DbError> {
    let daily_images = to_int(
        db.get_setting("register_daily_images").await?.as_deref(),
        30,
    );
    let daily_v5 = to_int(db.get_setting("register_daily_v5").await?.as_deref(), 0);
    let image_scope = db
        .get_setting("register_image_scope")
        .await?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "legacy".to_string());
    let raw = db.get_setting(GLOBAL_DAILY_V5.key).await?;
    let global = site_flags::get_from_raw(&GLOBAL_DAILY_V5, raw.as_deref(), None);
    Ok(AlgoOwned {
        daily_images,
        daily_v5,
        image_scope,
        global_daily_v5: pyval::number_like(&global).map_or(0, |f| f.trunc() as i64),
    })
}

/// 动态额度开着吗：`quota_auto_enabled`（后台「动态额度」开关与模块「分配」开关是同一个设置）。
/// 读不懂（比如写成 inf）按开着算——宁可先拦住手改。
pub async fn quota_algo_enabled(db: &Db) -> Result<bool, DbError> {
    let mut settings = std::collections::HashMap::new();
    if let Some(v) = db.get_setting("quota_auto_enabled").await? {
        settings.insert("quota_auto_enabled".to_string(), v);
    }
    Ok(gate_core::quota::quota_cfg(&settings).map_or(true, |c| c.quota_auto_enabled != 0))
}

/// 动态额度开着时想改它管的设置：返回 400 的说明（`names` 是请求里的字段名）。
pub async fn managed_error(db: &Db, names: &[&str]) -> Result<Option<String>, DbError> {
    if names.is_empty() || !quota_algo_enabled(db).await? {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} 由动态额度管理：关闭动态额度后才能手动修改",
        names.join("、")
    )))
}

// ------------------------------------------------------------------ 注册设置

/// 生效的注册设置：数据库里保存的优先，其次是 [`crate::defaults`] 里的常量（旧版 `registration_settings`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    pub configured: bool,
    pub open: bool,
    pub max_users: i64,
    /// None = 全部已开放功能（含以后新开放的）。
    pub features: Option<Vec<String>>,
    pub daily_images: i64,
    pub daily_v5: i64,
    pub image_scope: String,
    pub expires_days: i64,
    pub role_guild: String,
    pub role_id: String,
    pub role_note: String,
}

impl Registration {
    /// 键顺序与旧版 dict 一致。
    pub fn to_map(&self) -> Map<String, Value> {
        let features = match &self.features {
            Some(list) => json!(list),
            None => Value::Null,
        };
        let v = json!({
            "configured": self.configured,
            "open": self.open,
            "max_users": self.max_users,
            "features": features,
            "daily_images": self.daily_images,
            "daily_v5": self.daily_v5,
            "image_scope": self.image_scope,
            "expires_days": self.expires_days,
            "role_guild": self.role_guild,
            "role_id": self.role_id,
            "role_note": self.role_note,
        });
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }
}

/// `max(0, int(float(value)))`，读不懂就用默认值。
fn to_int(raw: Option<&str>, default: i64) -> i64 {
    raw.and_then(pyc::parse_float)
        .and_then(pyc::int_of)
        .map_or(default, |n| n.max(0))
}

pub async fn registration_settings(state: &AppState) -> Result<Registration, DbError> {
    registration_settings_for(&state.db, state.env.registration.is_some()).await
}

/// 同 [`registration_settings`]，但只要库和「注册服务有没有配齐」（`RegistrationService` 里没有 `AppState`，用这个）。
/// 没配注册服务时名额不限、功能不限（没有人能自助领 Key，这些值只给后台看）。
pub async fn registration_settings_for(db: &Db, configured: bool) -> Result<Registration, DbError> {
    let read = |name: &'static str| async move { db.get_setting(name).await };
    let open_raw = read("register_open").await?;
    let feats_raw = read("register_features").await?;
    let open = site_flags::parse(
        &REGISTER_OPEN,
        &open_raw.map_or(Value::Null, Value::String),
        Value::Bool(false), // 默认关闭：站长明确开放后才接受注册
    )
    .as_bool()
    .unwrap_or(false);
    let max_users = to_int(
        read("register_max_users").await?.as_deref(),
        if configured {
            defaults::REGISTER_MAX_USERS
        } else {
            0
        },
    );
    let features = match feats_raw {
        Some(s) if s == "*" => None,
        Some(s) => features::parse_list(&Value::String(s)),
        None if configured => {
            features::parse_list(&Value::String(defaults::REGISTER_FEATURES.into()))
        }
        None => None,
    };
    let owned = algo_owned(db).await?;
    // 默认不过期（站长 10-15）
    let expires_days = to_int(
        read("register_expires_days").await?.as_deref(),
        defaults::REGISTER_EXPIRES_DAYS,
    );
    // 限定身份组：只有在这个 Discord 服务器里拥有这个身份组的人能领 Key（可以是别的社区的服务器）。
    // 都为空 = 不限定（环境变量 DISCORD_ROLE_ID 已删除，身份组只在后台「领 Key」里设，docs/v3-deviations.md #93）。
    let role_guild = read("register_role_guild").await?.unwrap_or_default();
    let role_id = read("register_role_id").await?.unwrap_or_default();
    let role_note = read("register_role_note").await?.unwrap_or_default();
    Ok(Registration {
        configured,
        open,
        max_users,
        features,
        daily_images: owned.daily_images,
        daily_v5: owned.daily_v5,
        image_scope: owned.image_scope,
        expires_days,
        role_guild,
        role_id,
        role_note,
    })
}

// 2.x 这里还有 `v5_capacity`（「名额 × 每人 V5 是否超过全站 V5 日限」的超卖提示）。额度是 PVP，
// 每人上限只是上限，「名额 × 每人」本来就可以超过全站池；3.0 删除（docs/v3-deviations.md #69）。

/// 占用名额的人数（旧版 `_active_count`，与实际名额检查同一口径）。
pub async fn active_count(state: &AppState) -> Result<i64, DbError> {
    state.db.registered_member_count(state.now()).await
}

/// `int(x)` 失败时旧版路由回的文案：`str(exc) or "参数无效"`。
fn py_err_text(e: &PyErr) -> String {
    let m = e.message();
    if m.is_empty() {
        "参数无效".into()
    } else {
        m.to_string()
    }
}

fn range_int(
    body: &Map<String, Value>,
    name: &str,
    low: i64,
    high: i64,
) -> Result<Option<i64>, OpsError> {
    let Some(raw) = body.get(name) else {
        return Ok(None);
    };
    let n = pyext::py_int(raw).map_err(|e| OpsError::Invalid(py_err_text(&e)))?;
    if !(i128::from(low)..=i128::from(high)).contains(&n) {
        return Err(OpsError::Invalid(format!("{name} 超出范围 {low}～{high}")));
    }
    Ok(Some(n as i64))
}

/// `str(body[name] or "").strip()`。
fn text_field(v: &Value) -> String {
    if pyval::truthy(v) {
        pyc::strip(&pyext::py_str(v)).to_string()
    } else {
        String::new()
    }
}

/// 开放 / 关闭或名额上限变化时发给成员的公告（旧版 `set_registration` 末尾）；没变化返回 None。
pub fn registration_notice(before: (bool, i64), after: (bool, i64), active: i64) -> Option<String> {
    if before == after {
        return None;
    }
    let cap = after.1;
    let left = if cap != 0 {
        format!("，当前剩余名额 {}（上限 {cap}）", (cap - active).max(0))
    } else {
        "，名额不限".to_string()
    };
    Some(if !after.0 {
        "📢 **名额公告**：自助领取已暂停，已领取的成员不受影响。".to_string()
    } else if !before.0 {
        format!("📢 **名额公告**：自助领取已开放{left}。在任意频道输入 `/register` 即可。")
    } else {
        format!("📢 **名额公告**：名额上限已调整{left}。")
    })
}

/// 保存注册设置；开放 / 关闭或名额上限变化会在公告频道通知成员（`notify: false` 可关闭）。
/// 新 Key 的每日张数 / V5 / 模型范围归动态额度管：动态额度开着时带这几个字段回 400（docs/v3-deviations.md #70）。
pub async fn set_registration(state: &AppState, body: &Map<String, Value>) -> Result<(), OpsError> {
    let db = &state.db;
    let managed: Vec<&str> = ["daily_images", "daily_v5", "image_scope"]
        .into_iter()
        .filter(|k| body.contains_key(*k))
        .collect();
    if let Some(m) = managed_error(db, &managed).await? {
        return Err(OpsError::Managed(m));
    }
    let before = registration_settings(state).await?;
    let mut values: Vec<(String, String)> = Vec::new();
    if let Some(v) = body.get("open") {
        values.push((
            "register_open".into(),
            if pyval::truthy(v) { "1" } else { "0" }.into(),
        ));
    }
    for (name, low, high) in [
        ("max_users", 0, 1000),
        ("daily_images", 0, 100_000),
        ("daily_v5", 0, 100_000),
        ("expires_days", 0, 3650),
    ] {
        if let Some(n) = range_int(body, name, low, high)? {
            values.push((format!("register_{name}"), n.to_string()));
        }
    }
    if let Some(v) = body.get("image_scope") {
        let scope = if v.as_str() == Some("all") {
            "all"
        } else {
            "legacy"
        };
        values.push(("register_image_scope".into(), scope.into()));
    }
    for name in ["role_guild", "role_id"] {
        if let Some(v) = body.get(name) {
            let t = text_field(v);
            let digits = !t.is_empty() && t.chars().all(pyext::is_nd);
            let len = pyc::len_chars(&t);
            if !t.is_empty() && !(digits && (15..=21).contains(&len)) {
                return Err(OpsError::Invalid(format!(
                    "{name} 需要是 Discord 的数字 ID"
                )));
            }
            values.push((format!("register_{name}"), t));
        }
    }
    if body.contains_key("role_guild") || body.contains_key("role_id") {
        let current = |name: &'static str| async move {
            Ok::<String, DbError>(
                db.get_setting(name)
                    .await?
                    .map(|s| pyc::strip(&s).to_string())
                    .unwrap_or_default(),
            )
        };
        let g = match body.get("role_guild") {
            Some(v) => text_field(v),
            None => current("register_role_guild").await?,
        };
        let r = match body.get("role_id") {
            Some(v) => text_field(v),
            None => current("register_role_id").await?,
        };
        if g.is_empty() != r.is_empty() {
            return Err(OpsError::Invalid(
                "限定身份组的服务器 ID 与身份组 ID 必须同时填写，或全部留空".into(),
            ));
        }
    }
    if let Some(v) = body.get("role_note") {
        values.push((
            "register_role_note".into(),
            pyc::take_chars(&text_field(v), 60).to_string(),
        ));
    }
    if let Some(v) = body.get("features") {
        // null = 全部已开放功能，用 "*" 显式保存；空串表示「一个功能都不开」。
        let stored = match v {
            Value::Null => "*".to_string(),
            Value::Array(list) => {
                let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                features::dump(Some(names)).unwrap_or_default()
            }
            _ => String::new(),
        };
        values.push(("register_features".into(), stored));
    }
    if !values.is_empty() {
        db.set_settings_bulk(&values).await?;
    }
    if body.get("notify").is_none_or(pyval::truthy) {
        let after = registration_settings(state).await?;
        let active = active_count(state).await?;
        if let Some(text) = registration_notice(
            (before.open, before.max_users),
            (after.open, after.max_users),
            active,
        ) {
            state.announcer.post(&text);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ 全局功能开关

/// 全局开关：功能名 → 是否开启，按 `FEATURES` 的顺序（旧版 `global_flags`）。缺省全部开启。
pub async fn global_flags(state: &AppState) -> Result<Map<String, Value>, OpsError> {
    let raw = state.db.get_setting(GLOBAL_KEY).await?;
    let flags = features::parse_global_flags(raw.as_deref())
        .map_err(|e| OpsError::Corrupt(format!("{GLOBAL_KEY} 存档损坏：{e}")))?;
    Ok(FEATURES
        .iter()
        .map(|(name, _)| {
            (
                (*name).to_string(),
                Value::Bool(flags.get(name).copied().unwrap_or(true)),
            )
        })
        .collect())
}

pub async fn set_global_features(
    state: &AppState,
    flags: &Map<String, Value>,
) -> Result<(), OpsError> {
    let mut current = global_flags(state).await?;
    for (name, value) in flags {
        if FEATURES.iter().any(|(n, _)| n == name) && value.is_boolean() {
            current.insert(name.clone(), value.clone());
        }
    }
    state
        .db
        .set_setting(GLOBAL_KEY, &jsondump::dumps(&Value::Object(current)))
        .await?;
    Ok(())
}

// ------------------------------------------------------------------ 生成记录

/// 保存记录开关与保留天数；返回生效后的值（机器人 `/ops audit` 把它连同提示语一起回给管理员）。
pub async fn set_audit(state: &AppState, body: &Map<String, Value>) -> Result<Value, OpsError> {
    let db = &state.db;
    let (mut prompts, mut thumbs, mut days) = audit_flags(db).await?;
    let mut cap_mb = audit_image_cap_mb(db).await?;
    let mut keep_hours = audit_image_keep_hours(db).await?;
    // 旧版：int(...) 失败（ValueError / TypeError）统一回 422「参数无效」
    let bad = |_: PyErr| OpsError::Invalid("参数无效".into());
    let clamped = |v: &Value, hi: i64| -> Result<i64, OpsError> {
        let n = pyext::py_int(v).map_err(bad)?;
        Ok(n.clamp(0, i128::from(hi)) as i64)
    };
    if let Some(v) = body.get("prompts") {
        prompts = pyval::truthy(v);
    }
    if let Some(v) = body.get("thumbs") {
        thumbs = pyval::truthy(v);
    }
    if let Some(v) = body.get("retention_days") {
        days = clamped(v, 3650)?; // 0 = 长期保留
    }
    if let Some(v) = body.get("image_cap_mb") {
        cap_mb = clamped(v, 500_000)?; // 0 = 不限
    }
    if let Some(v) = body.get("image_keep_hours") {
        keep_hours = clamped(v, 8760)?; // 0 = 只按容量上限
    }
    // site_flags.put(spec, value)：写 encode 之后的串
    let spec = &site_flags::IMAGE_KEEP_HOURS;
    let text =
        site_flags::encode(spec, &Value::from(keep_hours)).map_err(|e| OpsError::Invalid(e.0))?;
    db.set_setting(spec.key, &text).await?;
    db.set_settings_bulk(&[
        (
            "audit_prompts".into(),
            if prompts { "1" } else { "0" }.into(),
        ),
        ("audit_thumbs".into(), if thumbs { "1" } else { "0" }.into()),
        ("audit_retention_days".into(), days.to_string()),
        (site_flags::IMAGE_CAP_MB.key.into(), cap_mb.to_string()),
    ])
    .await?;
    Ok(json!({
        "prompts": prompts, "thumbs": thumbs, "retention_days": days,
        "image_cap_mb": cap_mb, "image_keep_hours": keep_hours,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_texts_match_python() {
        // 开放（原来关着）
        assert_eq!(
            registration_notice((false, 10), (true, 10), 3).as_deref(),
            Some(
                "📢 **名额公告**：自助领取已开放，当前剩余名额 7（上限 10）。在任意频道输入 `/register` 即可。"
            )
        );
        // 暂停
        assert_eq!(
            registration_notice((true, 10), (false, 10), 3).as_deref(),
            Some("📢 **名额公告**：自助领取已暂停，已领取的成员不受影响。")
        );
        // 名额不限
        assert_eq!(
            registration_notice((false, 0), (true, 0), 3).as_deref(),
            Some("📢 **名额公告**：自助领取已开放，名额不限。在任意频道输入 `/register` 即可。")
        );
        // 调整名额：剩余不会小于 0
        assert_eq!(
            registration_notice((true, 10), (true, 2), 5).as_deref(),
            Some("📢 **名额公告**：名额上限已调整，当前剩余名额 0（上限 2）。")
        );
        // 没变化：不发
        assert_eq!(registration_notice((true, 10), (true, 10), 3), None);
    }

    #[test]
    fn to_int_is_forgiving() {
        assert_eq!(to_int(Some("12"), 5), 12);
        assert_eq!(to_int(Some("12.9"), 5), 12);
        assert_eq!(to_int(Some("-3"), 5), 0);
        assert_eq!(to_int(Some("abc"), 5), 5);
        assert_eq!(to_int(None, 5), 5);
    }
}
