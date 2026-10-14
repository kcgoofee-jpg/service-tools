//! 模块登记里的纯校验与文案（旧 `app/modules.py`）。
//!
//! 每个模块「每 10 分钟跑 tick、再用独立来源交叉核对」。这里只移植**纯的部分**：
//! 校验函数（输入是已查好的数字，输出 [`Check`] 列表，文案与旧版逐字节一致）、tick 的摘要文案、
//! 面板参数的文案格式化、模块静态元数据 [`MODULES`]。取数（SQL / 内存状态）由服务层完成，下面逐模块列出。
//!
//! 没有移植：
//! - `risk` 模块的取数（`risk_check.collect`，不在本次范围）；这里只有 [`risk_checks`]（把它的条目映射成 `Check`）；
//! - 孤立标题「③ 公平调度（DRR）」（该功能已删除，只剩一行注释）；
//! - `Kernel`（`app/kernel.py`）的调度 / 汇总逻辑；`Check.level` 为空时由内核补成 `ok` / `bad`，这里保持旧版 `Check` 的原始值（空串）。
//!
//! # 服务层要实现的查询 / 读取
//!
//! ## capacity（容量）
//! - tick：`SELECT MAX(ts) FROM upstream_snapshots` → [`snapshot_due`]；需要记录时先 `quota_algo.refresh_allowance`、再 `quota_algo._allowance`，
//!   `INSERT INTO upstream_snapshots(ts, v5_percent, v5_rate, anlas, fresh) VALUES (?,?,?,?,?)` 参数 `(now, pct, rate, 0, 1 if fresh else 0)`；
//!   然后 `guard.adapt_daily()`（返回 `(旧, 新)` 或无变化），有变化写 `log_action("系统", "自动调整：每小时上限", "", [`capacity_adjust_log_detail`])`；
//!   tick 摘要 [`capacity_tick_summary`]。
//! - checks（[`CapacityInputs`]）：
//!   `logged` = `SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'` 参数 `(now-3600)`；
//!   `tokens` = 池里 `usable`（缺省按可用）的上游账号数；`mem` = 这些账号 `guard.hour_count(token_id, now)` 的最大值；
//!   `cap_ceiling` = `guard.hourly_cap_ceiling(now)`；
//!   `snaps` = `SELECT ts, v5_percent, v5_rate FROM upstream_snapshots WHERE ts>? AND v5_percent IS NOT NULL ORDER BY ts` 参数 `(now-86400)`；
//!   `counted` = `SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND ts<=? AND status='ok' AND model LIKE '%diffusion-5%'`，
//!   参数是 [`capacity_v5_count_window`] 给出的 `(首条快照 ts, 末条快照 ts)`（返回 `None` 时旧版不查）。
//!
//! ## allocation（分配）
//! - tick：`quota_algo.run(state)`，摘要 [`allocation_tick_summary`]。
//! - checks：先 [`allocation_query`] 解析 `quota_algo_last`；`keys` = `SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 AND enabled=1`；
//!   `drift` = 同一条再加 `AND (daily_images<>? OR daily_v5<>?)`，参数 `(a, each)`（`a` 为 NULL 时 SQL 比较恒不成立 → 0）；
//!   `ack_oversell` = `bool(get_setting("ack_v5_oversell", ""))`（非空字符串即真，包括 `"0"`）；
//!   `base` = `guard.values.get("base_daily_images")`（没有 Guard 传 `Some(0)`）。
//!
//! ## autopilot（自动驾驶）
//! - tick：`autopilot.run`，摘要 [`autopilot_tick_summary`]。
//!
//! ## registration / registration_ready（领 Key / 开注册就绪度）
//! - `RegistrarInfo`：`registrar.settings()` 里的 `max_users` / `daily_images` / `daily_v5`（均 `or 0`）、`registrar.count_active()`、
//!   以及注册服务对象上的环境变量值 `key_daily_images` / `key_daily_v5` / `max_users`（缺省 0）；没有注册服务传 `None`。
//! - `open_now` = `site_flags.get(REGISTER_OPEN)`；`hourly` = `site_flags.get(ISSUE_HOURLY_CAP)`（可用 [`issue_hourly_cap`] 解析原始设置值）；
//!   `ack` = `bool(get_setting("ack_registration_ready", ""))`；`account_daily_cap` = `guard.values["account_daily_cap"]`；`tokens` = 可用上游账号数。
//!
//! ## observation（观测）
//! - 起点 [`observation_start`]（**本地 0 点**，不是服务日起点——与旧版一致）；
//!   `logged` = `SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>=? AND status='ok' AND kind LIKE 'image%'` 参数 `(start)`；
//!   `audited` = `SELECT COUNT(*) FROM generation_audit WHERE ts>=? AND status='ok'` 参数 `(start)`；
//!   `quota_state` = 设置 `quota_algo_last` 的原始字符串。

use crate::clock::Clock;
use crate::quota::{
    DecideError, PyNum, V5_FALLBACK_RATE, fmt_prec, hget, loads_setting, num, site_int,
};
use crate::{autopilot, pyc, pyval, quota};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `kernel.Check`：`level` 为空表示「按 ok 判断」，`warn` / `bad` / `ok` / `na` 是显式级别。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub level: String,
    pub evidence: Vec<String>,
}

impl Check {
    pub fn new(name: &str, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            ok,
            detail: detail.into(),
            level: String::new(),
            evidence: Vec::new(),
        }
    }

    fn warn(name: &str, ok: bool, detail: impl Into<String>) -> Self {
        let mut c = Self::new(name, ok, detail);
        c.level = "warn".to_string();
        c
    }
}

// ---------------------------------------------------------------------------
// 静态元数据（后台「模块」页显示的文案）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMeta {
    pub name: &'static str,
    pub title: &'static str,
    pub question: &'static str,
    pub hard_note: &'static str,
    pub default_enabled: bool,
    pub report_bugs: bool,
    /// 有周期任务（`tick`）。
    pub periodic: bool,
    /// 有交叉校验（`checks`）。
    pub has_checks: bool,
    /// 开关接到已有设置（`get_enabled` / `set_enabled`），而不是 `module_<名字>`。
    pub custom_enabled: bool,
    /// 面板参数 `(名称, 说明)`；参数值是实时读的，见各 `*_text` 函数。
    pub params: &'static [(&'static str, &'static str)],
}

/// 按 `modules.build()` 的登记顺序。
pub const MODULES: [ModuleMeta; 7] = [
    ModuleMeta {
        name: "observation",
        title: "观测",
        question: "各处记录的数字对不对得上",
        hard_note: "关闭后不再核对；日志照常记录",
        default_enabled: true,
        report_bugs: true,
        periodic: false,
        has_checks: true,
        custom_enabled: false,
        params: &[],
    },
    ModuleMeta {
        name: "capacity",
        title: "容量",
        question: "每个上游账号一小时、一天能出多少张",
        hard_note: "关闭后不再自动调每小时上限；各项上限照常生效",
        default_enabled: true,
        report_bugs: true,
        periodic: true,
        has_checks: true,
        custom_enabled: false,
        params: &[
            ("每天", ""),
            ("按时段放行", ""),
            ("每小时", "连续 24 小时没有 429 且顶到过上限时 +10"),
            ("出图间隔（秒）", "另加随机 0～5 秒"),
            ("V5 每 1% ≈ 张", ""),
        ],
    },
    ModuleMeta {
        name: "allocation",
        title: "分配",
        question: "每人每天能出多少张（所有人一样）",
        hard_note: "关闭后额度冻结在当前值",
        default_enabled: true,
        report_bugs: true,
        periodic: true,
        has_checks: true,
        custom_enabled: false,
        params: &[
            ("今天每人 V4.5", "每天服务日起点后按昨天的用量重算"),
            ("规则", ""),
            ("范围", ""),
            ("V5 每人范围", "按账号 V5 剩余和恢复速度分"),
        ],
    },
    ModuleMeta {
        name: "autopilot",
        title: "自动驾驶",
        question: "名额、全站熔断、单个 Key 限流",
        hard_note: "关闭后这些设置保持当前值",
        default_enabled: true,
        report_bugs: true,
        periodic: true,
        has_checks: false,
        custom_enabled: false,
        params: &[("全站熔断", ""), ("单个 Key 限流", ""), ("名额", "")],
    },
    ModuleMeta {
        name: "risk",
        title: "风险检查",
        question: "出站请求像不像正常浏览器，有没有滥用迹象",
        hard_note: "关闭后不再体检；不影响任何处罚",
        default_enabled: true,
        report_bugs: false,
        periodic: false,
        has_checks: true,
        custom_enabled: false,
        params: &[],
    },
    ModuleMeta {
        name: "registration",
        title: "领 Key",
        question: "新成员能不能领 Key",
        hard_note: "关闭 = 暂停领取；已领的成员和邀请 Key 绑定不受影响",
        default_enabled: true,
        report_bugs: true,
        periodic: false,
        has_checks: true,
        custom_enabled: true,
        params: &[("Key 有效天数", ""), ("闲置回收", "")],
    },
    ModuleMeta {
        name: "registration_ready",
        title: "开注册就绪度",
        question: "把领 Key 打开会发生什么：名额能兑现吗、链路通吗",
        hard_note: "只读体检；不影响领 Key 开关、不改任何额度",
        default_enabled: true,
        report_bugs: true,
        periodic: false,
        has_checks: true,
        custom_enabled: false,
        params: &[
            ("每小时最多发放", ""),
            (
                "容量能兑现的名额",
                "全站日容量 ÷ 每人额度：开注册前先看这一条",
            ),
        ],
    },
];

// ---------------------------------------------------------------------------
// ① 容量
// ---------------------------------------------------------------------------

/// tick：距离上次快照 ≥ 1 小时（或从未记录）才记一次。`last` = `SELECT MAX(ts) FROM upstream_snapshots`。
pub fn snapshot_due(last: Option<f64>, now: f64) -> bool {
    match last {
        // `not last`：None 与 0.0 都算没有
        None => true,
        Some(0.0) => true,
        Some(t) => now - t >= 3600.0,
    }
}

/// 快照表一行的参数：`(ts, v5_percent, v5_rate, anlas, fresh)`（`anlas` 恒为 0；`fresh` = 这次是否主动刚读过订阅信息）。
pub fn snapshot_row(
    now: f64,
    pct: Option<f64>,
    rate: Option<f64>,
    fresh: bool,
) -> (f64, Option<f64>, Option<f64>, i64, i64) {
    (now, pct, rate, 0, i64::from(fresh))
}

/// tick 摘要：`每小时上限 N`（有调整时追加 `（旧 → 新）`）。
pub fn capacity_tick_summary(hourly_cap: i64, changed: Option<(i64, i64)>) -> String {
    let tail = changed.map_or(String::new(), |(a, b)| format!("（{a} → {b}）"));
    format!("每小时上限 {hourly_cap}{tail}")
}

/// 每小时上限自动调整时的操作日志 `detail`。
pub fn capacity_adjust_log_detail(changed: (i64, i64)) -> String {
    format!("一天没有上游限流：{} → {}", changed.0, changed.1)
}

/// 面板参数「按时段放行」。`daily_burst` 为 0 / 缺失时显示「关」。
pub fn capacity_burst_text(daily_burst: Option<i64>, account_daily_cap: i64) -> String {
    match daily_burst {
        Some(b) if b != 0 => format!(
            "服务日起点先放 {b} 张，之后每小时补 {} 张",
            pyc::round0(account_daily_cap as f64 / 24.0)
        ),
        _ => "关".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapRow {
    pub ts: f64,
    pub percent: f64,
    /// `v5_rate`（实测恢复 %/天，可能为 NULL）。
    pub rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityInputs {
    pub tokens: usize,
    pub logged: i64,
    pub mem: i64,
    pub cap_ceiling: i64,
    pub snaps: Vec<SnapRow>,
    /// 仅在 [`capacity_v5_count_window`] 返回 `Some` 时有意义。
    pub counted: i64,
}

/// 「上游 V5 消耗 ↔ 日志 V5 张数」需要日志张数时，返回要查的窗口 `(首条快照 ts, 末条快照 ts)`。
pub fn capacity_v5_count_window(snaps: &[SnapRow]) -> Option<(f64, f64)> {
    let (first, last) = (snaps.first()?, snaps.last()?);
    if snaps.len() >= 2 && last.ts - first.ts >= 6.0 * 3600.0 && last.percent < 95.0 {
        Some((first.ts, last.ts))
    } else {
        None
    }
}

pub fn capacity_checks(inp: &CapacityInputs) -> Vec<Check> {
    if inp.tokens == 0 {
        return vec![Check::new(
            "有可用的上游账号",
            false,
            "没有可用的上游账号，无法核对每小时计数",
        )];
    }
    let cap = inp.cap_ceiling * inp.tokens as i64;
    // 独立来源：上游报告的 V5 剩余变化 ↔ 我们日志里数到的 V5 张数（同时校准「每 1% ≈ 14.2 张」）
    let mut v5_check = Check::new(
        "上游 V5 消耗 ↔ 日志 V5 张数",
        true,
        "数据不足（需要 ≥ 6 小时、账号剩余 < 95%）",
    );
    if let Some((t0, t1)) = capacity_v5_count_window(&inp.snaps) {
        let (first, last) = (&inp.snaps[0], &inp.snaps[inp.snaps.len() - 1]);
        let hours = (t1 - t0) / 3600.0;
        let rate = match last.rate {
            Some(r) if r != 0.0 => r,
            _ => V5_FALLBACK_RATE,
        };
        let used_pct = first.percent - last.percent + rate * hours / 24.0;
        let counted = inp.counted;
        if used_pct > 0.5 {
            let per = counted as f64 / used_pct;
            v5_check = Check::new(
                "上游 V5 消耗 ↔ 日志 V5 张数",
                (7.0..=28.0).contains(&per),
                format!(
                    "{} 小时内上游用掉 {}%，日志 {counted} 张 → 每 1% ≈ {} 张（参数 14.2）",
                    fmt_prec(hours, 0),
                    fmt_prec(used_pct, 1),
                    fmt_prec(per, 1)
                ),
            );
        }
    }
    let mem_ok = inp.mem >= inp.logged - 2;
    vec![
        v5_check,
        Check::new(
            "内存每小时计数 ↔ 用量日志",
            mem_ok,
            format!("内存 {} 张，日志 {} 张", inp.mem, inp.logged)
                + if mem_ok {
                    ""
                } else {
                    "：内存计数偏少，上限可能被绕过（重启清零？）"
                },
        ),
        Check::new(
            "实际每小时出图 ≤ 上限",
            inp.logged as f64 <= cap as f64 * 1.05 + 2.0,
            format!("最近 60 分钟 {} 张，上限 {cap}", inp.logged),
        ),
    ]
}

// ---------------------------------------------------------------------------
// ② 分配
// ---------------------------------------------------------------------------

/// tick 摘要。`r` = `quota_algo.run` 的返回值（`{"enabled": False}` 也是真值）；空 / None 返回空串。
pub fn allocation_tick_summary(r: Option<&Value>) -> String {
    let Some(r) = r.filter(|v| pyval::truthy(v)) else {
        return String::new();
    };
    let v5 = pyval::get(r, "v5").filter(|v| pyval::truthy(v));
    let get = |v: Option<&Value>, k: &str| {
        v.and_then(|x| pyval::get(x, k))
            .map_or("None".to_string(), pyval::py_str)
    };
    format!(
        "每人 V4.5 {} 张 · V5 {} 张（全站 V5 {}）",
        get(Some(r), "ceiling"),
        get(v5, "each"),
        get(v5, "global")
    )
}

/// 面板参数「规则」（读的是代码里的 `DEFAULTS`，不是实时设置——与旧版一致）。
pub fn allocation_rule_text() -> String {
    format!(
        "需求接近总量 → 按需求平分；有余量且有人用满 → +{}；否则不变",
        quota::QuotaCfg::default().quota_step
    )
}

/// 面板参数「范围」。
pub fn allocation_range_text() -> String {
    let c = quota::QuotaCfg::default();
    format!("{}～{} 张", c.quota_base_min_fair, c.quota_ceiling_max)
}

/// 面板参数「V5 每人范围」。
pub fn allocation_v5_range_text() -> String {
    let c = quota::QuotaCfg::default();
    format!("{}～{} 张", c.quota_v5_min, c.quota_v5_max)
}

/// `quota_algo_last` 解析结果里服务层要用的查询参数。
#[derive(Debug, Clone, PartialEq)]
pub struct AllocationQuery {
    /// `last.get("ceiling")`（SQL 参数 `a`；NULL 时 `drift` 恒为 0）。
    pub a: Value,
    /// `v5.get("each") or 0`（SQL 参数）。
    pub each: PyNum,
}

fn state_json(raw: Option<&str>) -> Value {
    // `_setting_json`：解析失败（TypeError / ValueError）回到 {}
    loads_setting(raw, "{}").unwrap_or_else(|_| serde_json::json!({}))
}

fn num_or_zero(v: Option<&Value>) -> Result<PyNum, DecideError> {
    match v {
        Some(x) if pyval::truthy(x) => {
            PyNum::from_value(x).ok_or_else(|| DecideError::Type(format!("不是数字: {x}")))
        }
        _ => Ok(PyNum::Int(0)),
    }
}

/// 解析 `quota_algo_last`；返回 `None` 表示旧版直接返回空校验列表（没有数据 / 算法被关）。
pub fn allocation_query(raw: Option<&str>) -> Result<Option<AllocationQuery>, DecideError> {
    let last = state_json(raw);
    if !pyval::truthy(&last) || hget(&last, "enabled")? == Some(&Value::Bool(false)) {
        return Ok(None);
    }
    let v5 = hget(&last, "v5")?.filter(|v| pyval::truthy(v));
    let each = match v5 {
        Some(v) => num_or_zero(hget(v, "each")?)?,
        None => PyNum::Int(0),
    };
    let a = hget(&last, "ceiling")?.cloned().unwrap_or(Value::Null);
    Ok(Some(AllocationQuery { a, each }))
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationInputs {
    /// `quota_algo_last` 设置的原始字符串。
    pub last_raw: Option<String>,
    pub keys: i64,
    pub drift: i64,
    pub ack_oversell: bool,
    /// `guard.values.get("base_daily_images")`（没有 Guard 时传 `Some(0)`）。
    pub base: Option<i64>,
}

pub fn allocation_checks(inp: &AllocationInputs) -> Result<Vec<Check>, DecideError> {
    let Some(_q) = allocation_query(inp.last_raw.as_deref())? else {
        return Ok(Vec::new());
    };
    let last = state_json(inp.last_raw.as_deref());
    let v5 = hget(&last, "v5")?.filter(|v| pyval::truthy(v));
    let (each, global_v5) = match v5 {
        Some(v) => (
            num_or_zero(hget(v, "each")?)?,
            num_or_zero(hget(v, "global")?)?,
        ),
        None => (PyNum::Int(0), PyNum::Int(0)),
    };
    let a = hget(&last, "ceiling")?.cloned().unwrap_or(Value::Null);
    let sold = each.mul_int(inp.keys);
    let name = "V5：每人额度 × 人数 ≤ 全站上限";
    let oversell = if inp.ack_oversell {
        // 超卖是「先到先得」的有意设计（站长已在后台确认）：转绿，数字仍显示
        Check::new(
            name,
            true,
            format!(
                "站长已确认先到先得：每人 {} × {} 人 = {} 张，全站上限 {} 张",
                each.py_str(),
                inp.keys,
                sold.py_str(),
                global_v5.py_str()
            ),
        )
    } else {
        let over = global_v5.truthy() && sold.as_f64() > global_v5.as_f64();
        let mut detail = format!(
            "每人 {} × {} 人 = {} 张，全站上限 {} 张",
            each.py_str(),
            inp.keys,
            sold.py_str(),
            global_v5.py_str()
        );
        if over {
            detail.push_str(&format!(
                "：超卖 {} 张，先到先得，后到的成员拿不到「每人 {} 张」",
                sold.minus(global_v5).py_str(),
                each.py_str()
            ));
        }
        Check::new(name, !over, detail)
    };
    // V4.5：保底层只在 保底 < 每日上限 时存在；算法把两者设成同一个数后这一层就没了
    let base = inp.base;
    let a_truthy = pyval::truthy(&a);
    let base_ok = match base {
        None | Some(0) => true,
        Some(_) if !a_truthy => true,
        Some(b) => {
            let af = pyval::number_like(&a)
                .ok_or_else(|| DecideError::Type(format!("不能比较: {a}")))?;
            (b as f64) <= af
        }
    };
    let base_check = Check::new(
        "V4.5：保底 ≤ 每日上限",
        base_ok,
        format!(
            "保底 {} · 每人上限 {}（两者相等 = 没有保底/空闲借用两层，只有每日上限）",
            base.map_or("None".to_string(), |b| b.to_string()),
            pyval::py_str(&a)
        ),
    );
    Ok(vec![
        Check::new(
            "算法管理的 Key 都已同步",
            inp.drift == 0,
            format!("{} 把 Key 的额度和算法结果不一致", inp.drift),
        ),
        oversell,
        base_check,
    ])
}

// ---------------------------------------------------------------------------
// 自动驾驶
// ---------------------------------------------------------------------------

/// tick 摘要。`r` = `autopilot.run` 的返回值。
pub fn autopilot_tick_summary(r: &Value) -> String {
    let rules = pyval::get(r, "rules").filter(|v| pyval::truthy(v));
    let breaker = rules
        .and_then(|x| pyval::get(x, "breaker"))
        .filter(|v| pyval::truthy(v));
    let tripped = breaker
        .and_then(|b| pyval::get(b, "value"))
        .is_some_and(pyval::truthy);
    let guarded = rules
        .and_then(|x| pyval::get(x, "key_guard"))
        .filter(|v| pyval::truthy(v))
        .and_then(|k| pyval::get(k, "value"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    format!(
        "全站熔断：{} · 被限流的 Key：{guarded} 把",
        if tripped { "触发" } else { "没有" }
    )
}

pub const AUTOPILOT_BREAKER_TEXT: &str = "15 分钟上游失败 ≥ 5 次且 ≥ 30% → 暂停出图 5 分钟";
pub const AUTOPILOT_KEY_GUARD_TEXT: &str = "1 小时被拒 ≥ 60 次 → 暂停 1 小时";

/// 面板参数「名额」。
pub fn autopilot_slots_text() -> String {
    format!(
        "快满且有余量时 +{}，最多 {}（名额不限时不动）",
        autopilot::SLOTS_STEP,
        autopilot::SLOTS_MAX
    )
}

// ---------------------------------------------------------------------------
// 领 Key
// ---------------------------------------------------------------------------

/// 注册服务给出的数字（`registrar.settings()` 各项已 `or 0`；`env_*` 是服务对象上的环境变量值）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrarInfo {
    pub max_users: i64,
    pub daily_images: i64,
    pub daily_v5: i64,
    pub active: i64,
    pub env_daily_images: i64,
    pub env_daily_v5: i64,
    pub env_max_users: i64,
}

/// 「领 Key」模块校验。没有注册服务返回空列表。
pub fn registration_checks(reg: Option<&RegistrarInfo>) -> Vec<Check> {
    let Some(r) = reg else { return Vec::new() };
    let cap = r.max_users;
    let shown = if cap != 0 {
        cap.to_string()
    } else {
        "不限".to_string()
    };
    vec![Check::new(
        "已领人数 ≤ 名额",
        cap == 0 || r.active <= cap,
        format!("{} / {shown}", r.active),
    )]
}

/// 面板参数「闲置回收」。
pub fn idle_recycle_text(days: i64) -> String {
    if days != 0 {
        format!("{days} 天")
    } else {
        "关".to_string()
    }
}

// ---------------------------------------------------------------------------
// 开注册就绪度
// ---------------------------------------------------------------------------

/// `site_flags.get(ISSUE_HOURLY_CAP)`：int，默认 12，范围 0～500。
pub fn issue_hourly_cap(raw: Option<&str>) -> i64 {
    site_int(raw, 0.0, 500.0, 12)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationReadyInputs {
    pub open_now: bool,
    pub registrar: Option<RegistrarInfo>,
    pub account_daily_cap: i64,
    pub tokens: usize,
    pub hourly: i64,
    pub ack: bool,
}

/// 全站一天的出图容量 = 上游账号数 × 每账号日上限（至少按 1 个账号算）。
pub fn site_daily(account_daily_cap: i64, tokens: usize) -> i64 {
    account_daily_cap * std::cmp::max(1, tokens) as i64
}

pub fn registration_ready_checks(inp: &RegistrationReadyInputs) -> Vec<Check> {
    let Some(reg) = &inp.registrar else {
        // 服务没配起来：开关是关的就没事；开着的话成员会看到「领 Key 暂不可用」
        return vec![Check::new(
            "注册链路已配置",
            !inp.open_now,
            "领 Key 开关是打开的，但网关没有注册服务配置（DISCORD_CLIENT_ID / \
             DISCORD_CLIENT_SECRET / DISCORD_BOT_TOKEN / REGISTRATION_BRIDGE_SECRET / \
             DISCORD_GUILD_ID / SITE_URL 缺项）：成员用 /register 只会看到「暂不可用」",
        )];
    };
    let (cap, per, v5_per, active) = (reg.max_users, reg.daily_images, reg.daily_v5, reg.active);
    let daily = site_daily(inp.account_daily_cap, inp.tokens);
    let mut out = vec![Check::new(
        "注册链路已配置",
        true,
        format!(
            "{}；配置齐备（网关能签发 Key，Key 通过临时消息发给本人）",
            if inp.open_now {
                "开放中"
            } else {
                "当前关闭"
            }
        ),
    )];
    // ① 每人承诺 × 人数 vs 全站日容量
    if per != 0 && daily != 0 {
        let promise = active * per;
        if inp.ack {
            out.push(Check::new(
                "每人承诺 ≤ 全站日容量",
                true,
                format!(
                    "站长已确认先到先得：每人 {per} 张 × {active} 人 = {promise} 张 vs 全站 {daily} 张/天（{} 个上游账号）",
                    inp.tokens
                ),
            ));
        } else {
            out.push(Check::warn(
                "每人承诺 ≤ 全站日容量",
                promise <= daily,
                format!(
                    "每人 {per} 张 × {active} 人 = {promise} 张 vs 全站 {daily} 张/天（{} 个上游账号）：平分下来每人每天约 {} 张，先到先得；只有前约 {} 个名额能拿满 {per} 张",
                    inp.tokens,
                    pyc::floordiv(daily, std::cmp::max(1, active)),
                    pyc::floordiv(daily, std::cmp::max(1, per))
                ),
            ));
        }
    }
    // ② 领取速度 × 每人额度 = 每小时新增的边际需求，不能超过全站一天的量
    if inp.hourly != 0 && per != 0 && daily != 0 {
        let ramp = inp.hourly * per;
        out.push(Check::new(
            "每小时领取上限 ≤ 全站日容量",
            ramp <= daily,
            format!(
                "每小时最多发 {} 个名额 × 每人 {per} 张 = {ramp} 张/小时的边际需求（全站 {daily} 张/天，约占 {}%）",
                inp.hourly,
                pyc::round0(ramp as f64 / daily as f64 * 100.0)
            ),
        ));
    }
    // ③ 后台额度 vs 环境变量：后台值被清掉时会跳回环境变量（可能是很早前写的大数）
    let mut drift: Vec<String> = Vec::new();
    for (label, db_value, env_value) in [
        ("每人张数", per, reg.env_daily_images),
        ("每人 V5", v5_per, reg.env_daily_v5),
        ("名额上限", cap, reg.env_max_users),
    ] {
        if env_value != 0 && db_value != env_value {
            drift.push(format!("{label} 后台 {db_value} / 环境变量 {env_value}"));
        }
    }
    out.push(Check::warn(
        "后台额度与环境变量一致",
        drift.is_empty(),
        drift.join("；")
            + if drift.is_empty() {
                ""
            } else {
                "（后台值被清掉会跳到环境变量那个数）"
            },
    ));
    out
}

/// 面板参数「每小时最多发放」。
pub fn hourly_text(cap: i64) -> String {
    if cap != 0 {
        format!("{cap} 个名额/小时")
    } else {
        "不限".to_string()
    }
}

/// 面板参数「容量能兑现的名额」：全站日容量 ÷ 每人额度。`per` 为 0 显示「—」。
pub fn seats_text(per: i64, daily: i64) -> String {
    if per != 0 {
        format!(
            "约 {} 人（{daily} 张/天 ÷ 每人 {per} 张）",
            pyc::floordiv(daily, per)
        )
    } else {
        "—".to_string()
    }
}

// ---------------------------------------------------------------------------
// 观测
// ---------------------------------------------------------------------------

/// 观测校验的统计起点：服务日字符串对应的**本地 0 点**（不是服务日起点，与旧版一致）。
pub fn observation_start(clock: &Clock, day: &str) -> Result<f64, DecideError> {
    clock
        .local_midnight(day)
        .ok_or_else(|| DecideError::Value(format!("time data {day:?} does not match format")))
}

/// 观测模块的校验：`audited` 为 0 时只有一条，不含 V5 恢复速度核对（与旧版一致）。
pub fn observation_checks(
    logged: i64,
    audited: i64,
    quota_state_raw: Option<&str>,
) -> Result<Vec<Check>, DecideError> {
    if audited == 0 {
        return Ok(vec![Check::new(
            "生成记录 ↔ 用量日志",
            true,
            "生成记录未开启或今天还没有记录",
        )]);
    }
    // `max(2, logged * 0.005)`
    let tol = quota::py_max(2.0, logged as f64 * 0.005);
    Ok(vec![
        Check::new(
            "生成记录 ↔ 用量日志",
            ((audited - logged).abs() as f64) <= tol,
            format!("生成记录 {audited} 条，日志 {logged} 张"),
        ),
        v5_rate_check(quota_state_raw)?,
    ])
}

/// V5 恢复速度：接口读数和「每天余量变化 + 用掉的量」反推的实测值核对。
pub fn v5_rate_check(quota_state_raw: Option<&str>) -> Result<Check, DecideError> {
    let name = "V5 恢复速度：接口 ↔ 实测";
    let last = state_json(quota_state_raw);
    let v5 = hget(&last, "v5")?.filter(|v| pyval::truthy(v));
    let rc = match v5 {
        Some(v) => hget(v, "rate_check")?.filter(|v| pyval::truthy(v)),
        None => None,
    };
    let Some(rc) = rc else {
        return Ok(Check::new(name, true, "还没有数据"));
    };
    let fmt = |key: &str| -> Result<String, DecideError> {
        match hget(rc, key)? {
            None | Some(Value::Null) => Ok("—".to_string()),
            Some(v) => Ok(format!("{}%/天", fmt_prec(num(v)?, 1))),
        }
    };
    let status = hget(rc, "status")?;
    let status_s = status.map_or("None".to_string(), pyval::py_str);
    let detail = format!(
        "接口 {} · 实测 {} · 采用 {}（{status_s}）",
        fmt("api")?,
        fmt("measured")?,
        fmt("used")?
    );
    let not_mismatch = status != Some(&Value::String("对不上".to_string()));
    Ok(Check::new(name, not_mismatch, detail))
}

// ---------------------------------------------------------------------------
// 风险检查
// ---------------------------------------------------------------------------

/// `risk` 模块：把 `risk_check.collect` 的条目映射成 `Check`（全部 `ok=True`，级别取条目的 `status`）。
pub fn risk_checks(items: &[Value]) -> Result<Vec<Check>, DecideError> {
    items
        .iter()
        .map(|it| {
            let get_s = |k: &str| -> Result<String, DecideError> {
                Ok(pyval::py_str(
                    hget(it, k)?.ok_or_else(|| DecideError::Key(format!("'{k}'")))?,
                ))
            };
            let evidence = hget(it, "evidence")?
                .and_then(Value::as_array)
                .map(|a| a.iter().map(pyval::py_str).collect())
                .unwrap_or_default();
            let mut c = Check::new(&get_s("title")?, true, get_s("detail")?);
            c.level = get_s("status")?;
            c.evidence = evidence;
            Ok(c)
        })
        .collect()
}
