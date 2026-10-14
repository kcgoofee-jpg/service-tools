//! 七个模块的运行部分（旧版 `app/modules.py`）：开关、tick、交叉校验、面板参数。
//! 校验的判定与文案在 `gate_core::modules`（golden 对拍过）；这里只负责取数。

use super::{KernelError, KernelRegistrar, algo, risk};
use crate::action_log::log_action;
use crate::state::AppState;
use gate_core::modules::{
    self as gm, AllocationInputs, CapacityInputs, Check, MODULES, ModuleMeta, RegistrarInfo,
    RegistrationReadyInputs,
};
use gate_core::quota;
use gate_core::site_flags::{self, ISSUE_HOURLY_CAP, REGISTER_OPEN};
use gate_core::{pyc, pyval};
use serde_json::Value;
use std::sync::Arc;

fn meta(name: &str) -> Result<&'static ModuleMeta, KernelError> {
    MODULES
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| KernelError::new("KeyError", format!("'{name}'")))
}

fn registrar_err(e: String) -> KernelError {
    KernelError::new("RegistrarError", e)
}

// ---------------------------------------------------------------------------
// 开关
// ---------------------------------------------------------------------------

/// 模块是否启用：默认读 `module_<名字>`；「领 Key」直接对接 `register_open`。
pub(super) async fn enabled(state: &AppState, name: &str) -> Result<bool, KernelError> {
    let m = meta(name)?;
    if m.custom_enabled {
        let v = crate::auth::site_flag(state, &REGISTER_OPEN).await?;
        return Ok(pyval::truthy(&v));
    }
    let raw = state.db.get_setting(&format!("module_{name}")).await?;
    Ok(match raw {
        None => m.default_enabled,
        Some(s) => s == "1",
    })
}

pub(super) async fn set_enabled(state: &AppState, name: &str, on: bool) -> Result<(), KernelError> {
    let m = meta(name)?;
    if m.custom_enabled {
        let text = site_flags::encode(&REGISTER_OPEN, &Value::Bool(on))
            .map_err(|e| KernelError::new("ValueError", e.0))?;
        state.db.set_setting(REGISTER_OPEN.key, &text).await?;
    } else {
        state
            .db
            .set_setting(&format!("module_{name}"), if on { "1" } else { "0" })
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tick
// ---------------------------------------------------------------------------

/// 运行一个模块的周期任务，返回一句摘要（写进 `kernel_last.<名字>.summary`）。
pub(super) async fn tick(state: &AppState, name: &str) -> Result<String, KernelError> {
    match name {
        "capacity" => capacity_tick(state).await,
        "allocation" => {
            let r = algo::run_quota(state).await?;
            Ok(gm::allocation_tick_summary(Some(&r)))
        }
        "autopilot" => {
            let r = algo::run_autopilot(state).await?;
            Ok(gm::autopilot_tick_summary(&r))
        }
        _ => Ok(String::new()),
    }
}

async fn capacity_tick(state: &AppState) -> Result<String, KernelError> {
    let now = state.now();
    let last = state.db.latest_snapshot_ts().await?;
    if gm::snapshot_due(last, now) {
        // 每小时记一次上游真实状态；主动读一次，夜里没人用 V5 时也是真实值
        let fresh = state.kernel.allowance_source().refresh(state).await;
        let (pct, rate) = quota::allowance_summary(&algo::allowance_accounts(state).await?);
        let (ts, p, r, anlas, fresh) = gm::snapshot_row(now, pct, rate, fresh);
        state
            .db
            .insert_upstream_snapshot(ts, p, r, anlas, fresh)
            .await?;
    }
    let changed = state.guard.adapt_daily(state.now()).await?;
    if let Some(c) = changed {
        log_action(
            &state.db,
            "系统",
            "自动调整：每小时上限",
            "",
            &gm::capacity_adjust_log_detail(c),
            true,
            state.now(),
        )
        .await;
    }
    let cap = state.guard.with(|g| g.values["account_hourly_cap"]);
    Ok(gm::capacity_tick_summary(cap, changed))
}

// ---------------------------------------------------------------------------
// 交叉校验
// ---------------------------------------------------------------------------

pub(super) async fn checks(state: &AppState, name: &str) -> Result<Vec<Check>, KernelError> {
    match name {
        "observation" => observation_checks(state).await,
        "capacity" => capacity_checks(state).await,
        "allocation" => allocation_checks(state).await,
        "risk" => risk_checks(state).await,
        "registration" => registration_checks(state).await,
        "registration_ready" => registration_ready_checks(state).await,
        _ => Ok(Vec::new()),
    }
}

async fn usable_token_ids(state: &AppState) -> Vec<String> {
    state
        .nai
        .tokens()
        .await
        .iter()
        .filter(|t| t.usable(state.now()))
        .map(|t| t.token_id.clone())
        .collect()
}

async fn observation_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    // 生成记录（audit）和用量日志是两条独立的写入路径；「每日计数 ↔ 日志」在同一个事务里写，是恒等式
    let day = state.clock.day(state.now());
    let start = gm::observation_start(&state.clock, &day)?;
    let logged = state.db.images_ok_from(start).await?;
    let audited = state.db.audited_ok_from(start).await?;
    let raw = state.db.get_setting(quota::STATE_KEY).await?;
    Ok(gm::observation_checks(logged, audited, raw.as_deref())?)
}

async fn capacity_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    let now = state.now();
    let tokens = usable_token_ids(state).await;
    let logged = state.db.images_ok_after(now - 3600.0).await?;
    if tokens.is_empty() {
        return Ok(gm::capacity_checks(&CapacityInputs {
            tokens: 0,
            logged,
            mem: 0,
            cap_ceiling: 0,
            snaps: Vec::new(),
            counted: 0,
        }));
    }
    let mem = tokens
        .iter()
        .map(|t| state.guard.with(|g| g.hour_count(t, now)))
        .max()
        .unwrap_or(0);
    // 用「窗口内出现过的最高上限」比：上限刚被 429 减半 / 刚进安静时段时，窗口里那些按当时更高上限放行的图不该算超限
    let cap_ceiling = state.guard.with(|g| g.hourly_cap_ceiling(now));
    let snaps = state.db.snapshot_rows_since(now - 86400.0).await?;
    let counted = match gm::capacity_v5_count_window(&snaps) {
        Some((t0, t1)) => state.db.v5_images_between(t0, t1).await?,
        None => 0,
    };
    Ok(gm::capacity_checks(&CapacityInputs {
        tokens: tokens.len(),
        logged,
        mem,
        cap_ceiling,
        snaps,
        counted,
    }))
}

async fn allocation_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    let raw = state.db.get_setting(quota::STATE_KEY).await?;
    let Some(q) = gm::allocation_query(raw.as_deref())? else {
        return Ok(Vec::new());
    };
    let (keys, drift) = state.db.managed_keys_drift(&q.a, q.each).await?;
    let ack_oversell = state
        .db
        .get_setting("ack_v5_oversell")
        .await?
        .is_some_and(|s| !s.is_empty());
    let base = Some(state.guard.with(|g| g.values["base_daily_images"]));
    Ok(gm::allocation_checks(&AllocationInputs {
        last_raw: raw,
        keys,
        drift,
        ack_oversell,
        base,
    })?)
}

async fn risk_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    let data = risk::collect(state, state.now()).await;
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(gm::risk_checks(&items)?)
}

/// 注册服务给内核的数字（`registrar.settings()` + `count_active()` + 服务对象上的环境默认值）。
async fn registrar_info(r: &Arc<dyn KernelRegistrar>) -> Result<RegistrarInfo, KernelError> {
    let cfg = r.settings().await.map_err(registrar_err)?;
    let active = r.count_active().await.map_err(registrar_err)?;
    let (env_daily_images, env_daily_v5, env_max_users) = r.env_defaults();
    Ok(RegistrarInfo {
        max_users: cfg.max_users,
        daily_images: cfg.daily_images,
        daily_v5: cfg.daily_v5,
        active,
        env_daily_images,
        env_daily_v5,
        env_max_users,
    })
}

async fn registration_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    let info = match state.kernel.registrar() {
        Some(r) => Some(registrar_info(&r).await?),
        None => None,
    };
    Ok(gm::registration_checks(info.as_ref()))
}

async fn registration_ready_checks(state: &AppState) -> Result<Vec<Check>, KernelError> {
    let reg = state.kernel.registrar();
    // 面板参数要读注册服务对象，就在校验时记一份下来
    state.kernel.remember_registrar(reg.clone());
    let open_now = pyval::truthy(&crate::auth::site_flag(state, &REGISTER_OPEN).await?);
    let registrar = match &reg {
        Some(r) => Some(registrar_info(r).await?),
        None => None,
    };
    let tokens = usable_token_ids(state).await.len();
    let hourly = gm::issue_hourly_cap(state.db.get_setting(ISSUE_HOURLY_CAP.key).await?.as_deref());
    let ack = state
        .db
        .get_setting("ack_registration_ready")
        .await?
        .is_some_and(|s| !s.is_empty());
    let account_daily_cap = state.guard.with(|g| g.values["account_daily_cap"]);
    Ok(gm::registration_ready_checks(&RegistrationReadyInputs {
        open_now,
        registrar,
        account_daily_cap,
        tokens,
        hourly,
        ack,
    }))
}

// ---------------------------------------------------------------------------
// 面板参数
// ---------------------------------------------------------------------------

fn failed(e: impl std::fmt::Display) -> Value {
    Value::String(format!("读取失败：{e}"))
}

/// Python 的类型名（`.get` 抛 AttributeError 时的消息用）。
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `_setting_json`：设置值解析成 JSON，空 / 解析失败都当 `{}`。
fn setting_json(raw: Option<&str>) -> Value {
    let text = raw.filter(|s| !s.is_empty()).unwrap_or("{}");
    gate_core::jsondump::loads(text).unwrap_or_else(|| Value::Object(Default::default()))
}

/// `settings.image_min_interval`：库里存过后台改的值就是整数（旧版 `_apply_runtime_limits` 写的 int），否则是环境变量的浮点数。
async fn image_min_interval_value(state: &AppState) -> Result<Value, KernelError> {
    let raw = state.db.get_setting("runtime_image_min_interval").await?;
    let saved = raw
        .and_then(|s| pyc::parse_int(&s))
        .filter(|v| (15..=120).contains(v));
    Ok(match saved {
        Some(v) => Value::from(v),
        None => pyval::float(state.env.image_min_interval),
    })
}

async fn site_daily(state: &AppState) -> i64 {
    let tokens = usable_token_ids(state).await.len();
    gm::site_daily(state.guard.with(|g| g.values["account_daily_cap"]), tokens)
}

/// 面板上各模块「关键参数」的当前值（顺序同 `MODULES[..].params`）；读不出来的写「读取失败：…」。
pub(super) async fn param_values(state: &AppState, name: &str) -> Vec<Value> {
    match name {
        "capacity" => {
            let (daily, burst, hourly) = state.guard.with(|g| {
                (
                    g.values["account_daily_cap"],
                    g.values["daily_burst"],
                    g.values["account_hourly_cap"],
                )
            });
            vec![
                Value::from(daily),
                Value::String(gm::capacity_burst_text(Some(burst), daily)),
                Value::from(hourly),
                image_min_interval_value(state).await.unwrap_or_else(failed),
                pyval::float(quota::V5_IMAGES_PER_PERCENT),
            ]
        }
        "allocation" => {
            let ceiling = match state.db.get_setting(quota::STATE_KEY).await {
                Err(e) => failed(e),
                Ok(raw) => match setting_json(raw.as_deref()) {
                    Value::Object(m) => m.get("ceiling").cloned().unwrap_or(Value::Null),
                    other => failed(format!(
                        "'{}' object has no attribute 'get'",
                        py_type_name(&other)
                    )),
                },
            };
            vec![
                ceiling,
                Value::String(gm::allocation_rule_text()),
                Value::String(gm::allocation_range_text()),
                Value::String(gm::allocation_v5_range_text()),
            ]
        }
        "autopilot" => vec![
            Value::String(gm::AUTOPILOT_BREAKER_TEXT.into()),
            Value::String(gm::AUTOPILOT_KEY_GUARD_TEXT.into()),
            Value::String(gm::autopilot_slots_text()),
        ],
        "registration" => vec![
            Value::from(state.env.default_expires_days),
            Value::String(gm::idle_recycle_text(state.env.key_inactivity_delete_days)),
        ],
        "registration_ready" => {
            let hourly = match state.db.get_setting(ISSUE_HOURLY_CAP.key).await {
                Ok(raw) => Value::String(gm::hourly_text(gm::issue_hourly_cap(raw.as_deref()))),
                Err(e) => failed(e),
            };
            let seats = seats_text(state).await;
            vec![hourly, Value::String(seats)]
        }
        _ => Vec::new(),
    }
}

/// 全站日容量 ÷ 每人额度 = 能拿满额度的人数（注册名额的现实上限）。出任何错都显示「—」。
async fn seats_text(state: &AppState) -> String {
    let reg = state.kernel.seen_registrar();
    let per = match &reg {
        None => 0,
        Some(r) => match r.settings().await {
            Err(_) => return "—".to_string(),
            Ok(cfg) if cfg.daily_images != 0 => cfg.daily_images,
            Ok(_) => r.env_defaults().0,
        },
    };
    gm::seats_text(per, site_daily(state).await)
}
