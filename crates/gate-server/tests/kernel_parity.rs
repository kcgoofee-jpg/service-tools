//! 内核整轮回放对拍（milestone M6）：夹具由旧 Python 在真实 SQLite 库里生成（脚本见标签 py-final）。
//!
//! 每个场景：新建网关（假上游 + 假告警 webhook）→ 执行同一批 `seed_sql` → 冻结时钟 → 按 `steps` 依次执行
//! （tick_all / quota_run / autopilot_run / snapshot / set_enabled / sql / probe）→ 逐步比较
//! 返回值（或异常类型）、本步 `site_settings` 写入序列、发出的告警；最后比较整库的表行差异。
//!
//! 全进程只有这一个测试函数：`time::freeze` 是进程级全局，场景必须串行。
//!
//! 不逐位比较的地方（夹具生成脚本里同样说明）：
//! - 异常消息 / 堆栈：只比异常类型（`loose_errors` 场景还忽略 Bug 表，特征码里有 Python 堆栈位置）；
//! - risk 模块里「出站请求头」「TLS 指纹」两条（3.0 换了文案，见 docs/v3-deviations.md）：两边都用固定占位；
//! - 3.0 删掉的东西（docs/v3-deviations.md #44）：夹具是冻结的 Python 输出，不重新生成，比较前把这些从期望里去掉——
//!   被删的整个模块、分配模块里被删的那条校验（连带按剩下的校验重算运行条级别），以及校验没通过时记的 Bug 行。
//! - 3.0 的额度改动（见 [`v3`] 模块，逐条对应 docs/v3-deviations.md 的编号）：同样只在期望一侧改写；
//!   操作日志的自增 id 两边都不比（被删掉的日志会让后面的 id 错位）。

use flate2::read::GzDecoder;
use futures::future::BoxFuture;
use gate_core::quota::{AccountAllowance, PyNum};
use gate_db::SettingWrite;
use gate_golden::diff as py_diff;
use gate_server::AppState;
use gate_server::env::EnvConfig;
use gate_server::kernel::{
    AllowanceSource, KernelRegistrar, RegistrarSettings, run_autopilot_checked, run_quota_checked,
};
use gate_server::web::Assets;
use gate_testkit::MockUpstream;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const M: &str = env!("CARGO_MANIFEST_DIR");
const RISK_STUBS: [&str; 2] = ["出站请求头", "TLS 指纹（JA3/JA4）"];

fn load_cases() -> Vec<Value> {
    let dir = Path::new(M).join("tests").join("fixtures");
    let gz = dir.join("kernel_tick.jsonl.gz");
    let plain = dir.join("kernel_tick.jsonl");
    let text = if gz.exists() {
        let mut text = String::new();
        GzDecoder::new(std::fs::File::open(&gz).expect("读夹具"))
            .read_to_string(&mut text)
            .expect("解压夹具");
        text
    } else {
        std::fs::read_to_string(&plain).expect("读夹具")
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("夹具是合法 JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// 假的注册服务与假的账号 V5 缓存（对应夹具生成脚本里的 FakeRegistrar / FakeAllowance）
// ---------------------------------------------------------------------------

struct FakeRegistrar {
    db: gate_db::Db,
    /// 环境变量里的名额上限（后台没存过时用它）
    max_users_env: i64,
    active: i64,
}

impl FakeRegistrar {
    /// `ops.registration_settings` 里的 `to_int`：`max(0, int(float(raw)))`，读不懂用环境默认值。
    async fn int_setting(&self, key: &str, default: i64) -> Result<i64, String> {
        let raw = self.db.get_setting(key).await.map_err(|e| e.to_string())?;
        Ok(match raw.and_then(|s| gate_core::pyc::parse_float(&s)) {
            Some(f) if f.is_finite() => std::cmp::max(0, f.trunc() as i64),
            _ => default,
        })
    }
}

impl KernelRegistrar for FakeRegistrar {
    fn settings(&self) -> BoxFuture<'_, Result<RegistrarSettings, String>> {
        Box::pin(async move {
            Ok(RegistrarSettings {
                max_users: self
                    .int_setting("register_max_users", self.max_users_env)
                    .await?,
            })
        })
    }
    fn count_active(&self) -> BoxFuture<'_, Result<i64, String>> {
        Box::pin(async move { Ok(self.active) })
    }
}

struct FakeAllowance {
    accounts: Vec<Option<(Option<PyNum>, Option<PyNum>)>>,
    cached: bool,
    client: bool,
    fails: bool,
    resolved: AtomicBool,
}

impl FakeAllowance {
    async fn usable(state: &AppState) -> usize {
        let now = state.now();
        state
            .nai
            .tokens()
            .await
            .iter()
            .filter(|t| t.usable(now))
            .count()
    }
}

impl AllowanceSource for FakeAllowance {
    fn snapshot<'a>(
        &'a self,
        state: &'a AppState,
    ) -> BoxFuture<'a, Result<Vec<AccountAllowance>, String>> {
        Box::pin(async move {
            let n = state.nai.tokens().await.len();
            let shown = self.cached || self.resolved.load(Ordering::SeqCst);
            Ok((0..n)
                .map(|i| match self.accounts.get(i).cloned().flatten() {
                    Some((percent, rate)) if shown => AccountAllowance {
                        percent,
                        recharge_per_day: rate,
                    },
                    _ => AccountAllowance {
                        percent: None,
                        recharge_per_day: None,
                    },
                })
                .collect())
        })
    }
    fn resolve_usable<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.client && !self.fails && Self::usable(state).await > 0 {
                self.resolved.store(true, Ordering::SeqCst);
            }
        })
    }
    fn refresh<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let ok = self.client && !self.fails && Self::usable(state).await > 0;
            if ok {
                self.resolved.store(true, Ordering::SeqCst);
            }
            ok
        })
    }
}

fn pynum(v: &Value) -> Option<PyNum> {
    PyNum::from_value(v)
}

// ---------------------------------------------------------------------------
// 比较
// ---------------------------------------------------------------------------

/// 表行里的 `kernel_last` 设置：解析成 JSON 并做和返回值一样的规整（两边的原始字符串里 risk 的占位不同）。
fn normalize_dump(dump: &mut Value, rust_side: bool, loose: bool) {
    let Some(rows) = dump.get_mut("site_settings").and_then(Value::as_array_mut) else {
        return;
    };
    for row in rows {
        if row.get("key").and_then(Value::as_str) != Some("kernel_last") {
            continue;
        }
        let Some(text) = row.get("value").and_then(Value::as_str) else {
            continue;
        };
        let mut v: Value = serde_json::from_str(text).expect("kernel_last 是 JSON");
        if rust_side {
            normalize_kernel_last(&mut v);
        }
        if loose {
            loosen_errors(&mut v);
        }
        row["value"] = v;
    }
}

/// 表行差异：每张表里「seed 后有、结束时没有」与「结束时有、seed 后没有」的行（多重集差；顺序同行序）。
fn table_diff(before: &Value, after: &Value) -> Value {
    let empty = Map::new();
    let (b, a) = (
        before.as_object().unwrap_or(&empty),
        after.as_object().unwrap_or(&empty),
    );
    let mut names: Vec<&String> = b.keys().chain(a.keys()).collect();
    names.sort();
    names.dedup();
    let mut out = Map::new();
    for t in names {
        let rows = |m: &Map<String, Value>| {
            m.get(t)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let (brows, arows) = (rows(b), rows(a));
        let minus = |xs: &[Value], ys: &[Value]| -> Vec<Value> {
            let mut left: HashMap<String, usize> = HashMap::new();
            for y in ys {
                *left.entry(y.to_string()).or_default() += 1;
            }
            let mut out = Vec::new();
            for x in xs {
                match left.get_mut(&x.to_string()) {
                    Some(n) if *n > 0 => *n -= 1,
                    _ => out.push(x.clone()),
                }
            }
            out
        };
        let added = minus(&arows, &brows);
        let removed = minus(&brows, &arows);
        if !added.is_empty() || !removed.is_empty() {
            out.insert(t.clone(), json!({"added": added, "removed": removed}));
        }
    }
    Value::Object(out)
}

/// 异常消息不比：`error` 字段只留冒号前的类型名。
fn loosen_errors(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if k == "error"
                    && let Value::String(s) = x
                {
                    *s = s.split(':').next().unwrap_or("").to_string();
                } else {
                    loosen_errors(x);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(loosen_errors),
        _ => {}
    }
}

fn proxy_env_set() -> bool {
    [
        "UPSTREAM_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ]
    .iter()
    .any(|n| std::env::var(n).is_ok_and(|v| !v.is_empty()))
}

/// risk 模块的两条换了文案的校验换成夹具里的占位；本机有代理环境变量时「代理 / 出口链路」也换成无代理的样子。
fn normalize_kernel_last(v: &mut Value) {
    let Some(checks) = v
        .get_mut("risk")
        .and_then(|r| r.get_mut("checks"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for c in checks {
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if RISK_STUBS.contains(&name.as_str()) {
            c["detail"] = json!("stub");
            c["evidence"] = json!([]);
            c["level"] = json!("ok");
        } else if name == "代理 / 出口链路" && proxy_env_set() {
            c["detail"] = json!("出站链路。");
            c["evidence"] =
                json!(["未配置代理：出站即服务器本机 IP，所有上游 Token 共用同一出口。"]);
        }
    }
}

fn flatten_writes(ws: &[Value]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for w in ws {
        let a = w.as_array().expect("写入是数组");
        match a[0].as_str().expect("写入类型") {
            "set" => out.push((
                a[1].as_str().unwrap().to_string(),
                a[2].as_str().unwrap().to_string(),
            )),
            "bulk" => {
                for kv in a[1].as_array().unwrap() {
                    out.push((
                        kv[0].as_str().unwrap().to_string(),
                        kv[1].as_str().unwrap().to_string(),
                    ));
                }
            }
            other => panic!("未知写入类型 {other}"),
        }
    }
    out
}

fn actual_writes(ws: Vec<SettingWrite>) -> Vec<Value> {
    ws.into_iter()
        .map(|w| match w {
            SettingWrite::Set(k, v) => json!(["set", k, v]),
            SettingWrite::Bulk(p) => json!([
                "bulk",
                p.into_iter()
                    .map(|(k, v)| json!([k, v]))
                    .collect::<Vec<_>>()
            ]),
        })
        .collect()
}

fn compare_writes(
    actual: &[(String, String)],
    expected: &[(String, String)],
    loose: bool,
) -> Option<String> {
    if actual.len() != expected.len() {
        return Some(format!(
            "设置写入条数不同：实际 {} 条 {:?}，期望 {} 条 {:?}",
            actual.len(),
            actual.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            expected.len(),
            expected.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()
        ));
    }
    for (i, ((ak, av), (ek, ev))) in actual.iter().zip(expected).enumerate() {
        if ak != ek {
            return Some(format!("第 {i} 条写入的键不同：实际 {ak}，期望 {ek}"));
        }
        if av == ev {
            continue;
        }
        if ak == "kernel_last" {
            let (mut a, mut e): (Value, Value) = (
                serde_json::from_str(av).expect("实际 kernel_last 是 JSON"),
                serde_json::from_str(ev).expect("期望 kernel_last 是 JSON"),
            );
            normalize_kernel_last(&mut a);
            if loose {
                loosen_errors(&mut a);
                loosen_errors(&mut e);
            }
            if let Some(d) = py_diff(&a, &e, true) {
                return Some(format!("kernel_last 写入不同：{d}"));
            }
        } else if loose {
            // 复盘里不会有异常消息；松的场景里其它键仍须逐字相同
            return Some(format!("{ak} 写入不同：\n实际 {av}\n期望 {ev}"));
        } else {
            return Some(format!("{ak} 写入不同：\n实际 {av}\n期望 {ev}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 3.0 删掉的部分（docs/v3-deviations.md #44）：比较前从冻结夹具的期望里去掉
// ---------------------------------------------------------------------------

/// 被删的模块。
const REMOVED_MODULE: &str = "registration_ready";
/// 分配模块里被删的校验：每人额度 × 人数 vs 全站上限（#44）、保底 ≤ 每日上限（#61）。
const REMOVED_CHECK_MODULE: &str = "allocation";
const REMOVED_CHECKS: [&str; 2] = ["V5：每人额度 × 人数 ≤ 全站上限", "V4.5：保底 ≤ 每日上限"];

/// 已按剩下的校验重算过的运行条级别：`(模块, 运行时刻)` → 级别。后面几步的期望里还带着旧的运行条，要同样改。
type Levels = HashMap<(String, u64), String>;

/// 与 `Kernel::tick_all` 同一套判定：跑挂或有 bad 校验 → bad，有 warn → warn，否则 ok。
fn run_level(entry: &Map<String, Value>, checks: &[Value]) -> &'static str {
    let any = |lv: &str| {
        checks
            .iter()
            .any(|c| c.get("level").and_then(Value::as_str) == Some(lv))
    };
    if entry.get("ok") == Some(&Value::Bool(false)) || any("bad") {
        "bad"
    } else if any("warn") {
        "warn"
    } else {
        "ok"
    }
}

fn run_key(module: &str, run: &Value) -> Option<(String, u64)> {
    Some((module.to_string(), run.get("at")?.as_f64()?.to_bits()))
}

/// 一个模块的运行记录（`kernel_last.<模块>`）：去掉被删的校验，这一轮的运行条级别按剩下的重算。
fn fix_entry(name: &str, entry: &mut Value, levels: &mut Levels) {
    let Some(e) = entry.as_object_mut() else {
        return;
    };
    if name != REMOVED_CHECK_MODULE {
        return;
    }
    let mut this_round = None;
    if let Some(Value::Array(checks)) = e.get_mut("checks") {
        checks.retain(|c| !REMOVED_CHECKS.iter().any(|n| c["name"] == *n));
        let checks = checks.clone();
        this_round = Some(run_level(e, &checks));
    }
    let Some(Value::Array(runs)) = e.get_mut("runs") else {
        return;
    };
    // 带着 checks 的记录里，最后一条运行条就是这一轮的
    if let (Some(level), Some(key)) = (this_round, runs.last().and_then(|r| run_key(name, r))) {
        levels.insert(key, level.to_string());
    }
    for r in runs.iter_mut() {
        if let Some(level) = run_key(name, r).and_then(|k| levels.get(&k)) {
            r["level"] = json!(level);
        }
    }
}

/// 整份 `kernel_last`（模块名 → 运行记录）。
fn fix_kernel_last(last: &mut Value, levels: &mut Levels) {
    let Some(map) = last.as_object_mut() else {
        return;
    };
    map.shift_remove(REMOVED_MODULE);
    for (name, entry) in map.iter_mut() {
        fix_entry(name, entry, levels);
    }
}

fn fix_setting(key: &Value, value: &mut Value, levels: &mut Levels) {
    if key == "kernel_last"
        && let Some(text) = value.as_str()
    {
        let mut parsed: Value = serde_json::from_str(text).expect("kernel_last 是 JSON");
        fix_kernel_last(&mut parsed, levels);
        *value = Value::String(parsed.to_string());
    }
}

/// 一步的期望：返回值（tick_all / snapshot）和设置写入里的 `kernel_last`。
fn fix_expected_step(want: &mut Value, op: &str, levels: &mut Levels, v3: &mut v3::Expect) {
    v3.fix_step(want, op);
    if let Some(result) = want.get_mut("result") {
        match op {
            "tick_all" => fix_kernel_last(result, levels),
            "snapshot" => {
                if let Some(mods) = result.as_array_mut() {
                    mods.retain(|m| m["name"] != REMOVED_MODULE);
                    for m in mods {
                        let name = m["name"].as_str().unwrap_or("").to_string();
                        if let Some(last) = m.get_mut("last") {
                            fix_entry(&name, last, levels);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let Some(writes) = want
        .get_mut("settings_writes")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for w in writes {
        match w[0].as_str() {
            Some("set") => {
                let key = w[1].clone();
                fix_setting(&key, &mut w[2], levels);
            }
            Some("bulk") => {
                for kv in w[1].as_array_mut().into_iter().flatten() {
                    let key = kv[0].clone();
                    fix_setting(&key, &mut kv[1], levels);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 3.0 的额度改动：比较前在期望一侧改写（夹具冻结不改）
// ---------------------------------------------------------------------------

mod v3 {
    use gate_core::jsondump;
    use serde_json::{Map, Value, json};
    use std::collections::HashMap;

    /// 期望一侧的改写；跨步骤记住每个设置最后一次（改写后）的值，用来改写最后的表行差异。
    pub struct Expect {
        last_written: HashMap<String, String>,
        /// 旧版这一步写下的全站 V5 池（`global_daily_v5`）。V5 按新公式算出来的数不同（#63），而第二天的
        /// V4.5 复盘拿它算「可分 = 账号日上限 − 全站 V5」：每步跑完把旧版的值写回库里，让多步场景后面的
        /// 步骤与旧版从同一个起点继续，V4.5 的部分照样逐位对拍。
        resync: Vec<(String, String)>,
        /// 种子里只有原因码 `hourly_cap`、没有「出图量已达上限」文案的拒绝记录的时间戳：
        /// 旧版的复盘按文案 LIKE 数不到它们，3.0 按原因码数得到（#61）
        code_only_hourly: Vec<f64>,
        /// 旧版复盘按节约模式折算过 `used` 的服务日 → 3.0 的 `used`（成员实际张数之和，#67）。
        /// 由 [`raw_used_days`] 在种子写好之后直接查库得到。
        pub raw_used: HashMap<String, i64>,
        /// 旧版的「名额 +5」改过注册名额（#68：3.0 不改，表行差异里也就没有这一行）。
        slots_changed: bool,
        /// 这个场景里写过的自动驾驶每小时快照（`hour`）：这些快照里都没有名额（#68）。
        autopilot_hours: Vec<String>,
    }

    /// 这些服务日里，旧版的复盘会按账号层面「`units` / `images`」折算 `used`（`units < images`）的，
    /// 以及 3.0 应得的实际张数（不含测试号、站长号的成员计数之和）。测试自己写 SQL 算，不用被测代码。
    pub async fn raw_used_days(db: &gate_db::Db, days: &[String]) -> HashMap<String, i64> {
        let mut out = HashMap::new();
        for day in days {
            let up = db
                .execute_fetchall_compat(
                    "SELECT COALESCE(SUM(images),0) AS i, COALESCE(SUM(COALESCE(units, images)),0) AS u \
                     FROM upstream_token_counters WHERE day=?",
                    &[json!(day)],
                )
                .await
                .unwrap();
            let (i, u) = (up[0]["i"].as_f64().unwrap(), up[0]["u"].as_f64().unwrap());
            if i != 0.0 && u < i {
                let used = db
                    .execute_fetchall_compat(
                        "SELECT COALESCE(SUM(c.images),0) AS n FROM counters c JOIN api_keys k ON k.id=c.key_id \
                         WHERE c.day=? AND k.is_test=0 AND k.is_admin=0",
                        &[json!(day)],
                    )
                    .await
                    .unwrap();
                out.insert(day.clone(), used[0]["n"].as_i64().unwrap());
            }
        }
        out
    }

    /// 模块「分配」的开关（#70：旧版写 `module_allocation`）。
    pub const QUOTA_SWITCH: &str = "quota_auto_enabled";

    fn switch_key(key: &str) -> &str {
        if key == "module_allocation" {
            QUOTA_SWITCH
        } else {
            key
        }
    }

    /// 3.0 不再写的设置（#61：保底）。
    const DROPPED_SETTINGS: [&str; 2] = ["guard_base_daily_images", "quota_base_now"];

    /// 整个场景测的是 3.0 已经删掉的规则，没有可比的东西。
    pub const REMOVED_SCENARIOS: [(&str, &str); 10] = [
        (
            "v19_private_params",
            "测的就是私密参数 allocation.min_coverage_hours 把昨天数据覆盖的下限改成 10 小时：\
             私密参数文件已取消，下限固定为 20 小时（docs/v3-deviations.md #91）",
        ),
        (
            "k23_enforce_slots_via_tick",
            "测的就是内核 tick 里「名额 +5」生效（名额 12 → 17）：规则已删除（docs/v3-deviations.md #68）",
        ),
        (
            "k22_module_error_loose",
            "自动驾驶读坏掉的上一轮额度复盘而出错：名额规则删除后自动驾驶不再读它、这一轮不出错\
             （docs/v3-deviations.md #68）；模块出错的记录路径由分配模块的 q15_bad_history_json 等覆盖",
        ),
        (
            "v24_zero_min_people",
            "私密参数把 V5 人数下限设成 0、没人用过 V5 时旧版抛 ZeroDivisionError；\
             3.0 的人数下限固定为 10、不读这个参数（docs/v3-deviations.md #63）",
        ),
        (
            "ap14_breaker_observe",
            "自动驾驶熔断只观察不执行：3.0 一律执行（docs/v3-deviations.md #68）",
        ),
        (
            "ap19_breaker_off",
            "自动驾驶熔断关闭：3.0 一律执行（docs/v3-deviations.md #68）",
        ),
        (
            "ap20_bad_mode_value",
            "模式设置写错按观察处理：3.0 不读模式设置、一律执行（docs/v3-deviations.md #68）",
        ),
        (
            "ap22_keyguard_observe",
            "单 Key 限流只观察不执行：3.0 一律执行（docs/v3-deviations.md #68）",
        ),
        (
            "m04_mixed",
            "熔断按默认的观察模式触发过一次、旧版只记观察日志：3.0 一律执行（docs/v3-deviations.md #68）",
        ),
        (
            "ap30_bad_quota_history",
            "名额规则读上一轮额度复盘出错：名额规则已删除、不再读它（docs/v3-deviations.md #68）",
        ),
    ];

    /// 自动驾驶的状态 / 返回值：没有名额那一条规则；其余规则旧版在这些场景里没有触发过「观察」，
    /// 模式一律写成 `enforce`（#68）。
    fn autopilot_v3(v: &mut Value) {
        if let Some(rules) = v.get_mut("rules").and_then(Value::as_object_mut) {
            rules.shift_remove("slots");
            for r in rules.values_mut() {
                r["mode"] = json!("enforce");
            }
        }
    }

    /// V5 的结果按新公式算（#63），与旧版无从对拍：两边都换成这个占位，V5 本身由 gate-core 的
    /// `quota_golden`（按新规则核对每个夹具用例）和 `quota_v3` 测试。
    const MASK: &str = "<V5：按新公式另行核对>";

    /// 首页文案里的 V5 段（「 · V5 每人 …」到「 · Anlas」或结尾）。
    fn mask_notice(text: &str) -> String {
        let Some(i) = text.find(" · V5 每人 ") else {
            return text.to_string();
        };
        let rest = &text[i..];
        let tail = rest.find(" · Anlas").map_or("", |j| &rest[j..]);
        format!("{} · {MASK}{tail}", &text[..i])
    }

    /// `quota_run` 的返回值 / `quota_algo_last`：`v5` 与「同步了几把 Key」取决于 V5 结果。
    fn mask_quota(v: &mut Value) {
        if let Some(m) = v.as_object_mut()
            && m.contains_key("v5")
        {
            m.insert("v5".into(), json!(MASK));
            m.insert("changed_keys".into(), json!(MASK));
        }
    }

    /// `kernel_last`：分配模块的摘要「每人 V4.5 a 张 · V5 e 张（全站 V5 g）」里的 V5 段；
    /// 风险检查「全站今日 V5 合计 n / 日额度 g（p%）」里的日额度（就是当天的全站 V5 池）。
    /// 夹具里这条检查的级别新旧公式下都一样（都没用到或都超了），级别照比。
    fn mask_kernel_last(v: &mut Value) {
        if let Some(Value::String(s)) = v.get_mut("allocation").and_then(|a| a.get_mut("summary"))
            && let Some(i) = s.find(" · V5 ")
        {
            *s = format!("{} · {MASK}", &s[..i]);
        }
        let checks = v
            .get_mut("risk")
            .and_then(|r| r.get_mut("checks"))
            .and_then(Value::as_array_mut);
        for c in checks.into_iter().flatten() {
            for e in c
                .get_mut("evidence")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if let Value::String(s) = e
                    && let Some(i) = s.find(" / 日额度 ")
                    && s.starts_with("全站今日 V5 合计 ")
                {
                    *s = format!("{} / 日额度 {MASK}", &s[..i]);
                }
            }
        }
    }

    fn mask_setting(key: &str, value: &str) -> String {
        match key {
            "global_daily_v5" | "register_daily_v5" => MASK.to_string(),
            "algo_notice" => mask_notice(value),
            "quota_algo_last" => match jsondump::loads(value) {
                Some(mut v) => {
                    mask_quota(&mut v);
                    jsondump::dumps(&v)
                }
                None => value.to_string(),
            },
            "kernel_last" => {
                let mut v: Value = serde_json::from_str(value).expect("kernel_last 是 JSON");
                mask_kernel_last(&mut v);
                v.to_string()
            }
            _ => value.to_string(),
        }
    }

    /// 两边都做（#63）：设置写入里取决于 V5 结果的部分——当天方案（3.0 只在当天第一次计算时写）去掉，
    /// 全站 V5 / 新 Key 的 V5 默认值 / 首页文案的 V5 段 / `quota_algo_last` 的 V5 换成占位。
    pub fn mask_writes(writes: &mut Vec<Value>) {
        let mut out = Vec::new();
        for w in writes.drain(..) {
            match w[0].as_str() {
                Some("set") => {
                    let key = w[1].as_str().unwrap();
                    if key == "quota_v5_day_plan" {
                        continue;
                    }
                    out.push(json!([
                        "set",
                        key,
                        mask_setting(key, w[2].as_str().unwrap())
                    ]));
                }
                Some("bulk") => {
                    let pairs: Vec<Value> = w[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|kv| kv[0] != "quota_v5_day_plan")
                        .map(|kv| {
                            let key = kv[0].as_str().unwrap();
                            json!([key, mask_setting(key, kv[1].as_str().unwrap())])
                        })
                        .collect();
                    if !pairs.is_empty() {
                        out.push(json!(["bulk", pairs]));
                    }
                }
                _ => out.push(w),
            }
        }
        *writes = out;
    }

    /// 两边都做（#63）：一步的返回值里取决于 V5 结果的部分。
    pub fn mask_result(op: &str, v: &mut Value) {
        match op {
            "quota_run" => mask_quota(v),
            "tick_all" => mask_kernel_last(v),
            "snapshot" => {
                for m in v.as_array_mut().into_iter().flatten() {
                    let name = m["name"].as_str().unwrap_or("").to_string();
                    if let Some(last) = m.get_mut("last") {
                        let mut wrap = json!({ name.as_str(): last.clone() });
                        mask_kernel_last(&mut wrap);
                        *last = wrap[name.as_str()].take();
                    }
                }
            }
            _ => {}
        }
    }

    /// 改写后 added / removed 里完全相同的行互相抵消（多重集差）。
    fn cancel_pairs(t: &mut Value) {
        let (Some(added), Some(removed)) = (
            t.get("added").and_then(Value::as_array).cloned(),
            t.get("removed").and_then(Value::as_array).cloned(),
        ) else {
            return;
        };
        let mut left = removed.clone();
        let mut kept_added = Vec::new();
        for a in added {
            match left.iter().position(|r| *r == a) {
                Some(i) => {
                    left.remove(i);
                }
                None => kept_added.push(a),
            }
        }
        t["added"] = Value::Array(kept_added);
        t["removed"] = Value::Array(left);
    }

    /// 复盘：`from` / `to` 只留每人上限 A，去掉 `base_blocks`（#61）。
    fn review(r: &mut Map<String, Value>) {
        for k in ["from", "to"] {
            if let Some(Value::Array(pair)) = r.get_mut(k) {
                pair.truncate(1);
            }
        }
        r.shift_remove("base_blocks");
    }

    /// `quota_run` 的返回值 / `quota_algo_last`：没有 `base`（#61）。
    pub fn quota_result(v: &mut Value) {
        if let Some(m) = v.as_object_mut() {
            m.shift_remove("base");
            if let Some(Value::Object(r)) = m.get_mut("review") {
                review(r);
            }
        }
    }

    fn reparse(text: &str, f: impl FnOnce(&mut Value)) -> String {
        let mut v = jsondump::loads(text).expect("写进库的 JSON 合法");
        f(&mut v);
        jsondump::dumps(&v)
    }

    /// 首页文案去掉每人注水那一句（#62）：「（每小时自动恢复 4 张，当天最多约 N 张，每日 7 点重置）」
    /// →「（每日 7 点重置）」。
    fn notice(text: &str) -> String {
        let Some(i) = text.find("（每小时自动恢复 ") else {
            return text.to_string();
        };
        let rest = &text[i..];
        let j = rest.find("当天最多约 ").expect("注水文案");
        let k = rest[j..].find(" 张，").expect("注水文案") + j + " 张，".len();
        format!("{}（{}", &text[..i], &rest[k..])
    }

    /// 这一轮的复盘是否只因为保底变了才算「调整」：3.0 的首页文案不再写「算法调整」那一句（#61）。
    fn only_base_changed(history_text: &str) -> bool {
        let v = jsondump::loads(history_text).unwrap();
        let Some(r) = v.as_array().and_then(|a| a.last()) else {
            return false;
        };
        let (from, to) = (&r["from"], &r["to"]);
        from != to && from.get(0) == to.get(0)
    }

    impl Expect {
        pub fn new(seed: &[String]) -> Self {
            let code_only_hourly = seed
                .iter()
                .filter(|s| {
                    s.starts_with("INSERT INTO usage_log") && s.contains("'', 'hourly_cap'")
                })
                .map(|s| {
                    let values = s.split("VALUES (").nth(1).expect("VALUES");
                    values
                        .split(", ")
                        .next()
                        .unwrap()
                        .parse::<f64>()
                        .expect("ts")
                })
                .collect();
            Self {
                last_written: HashMap::new(),
                resync: Vec::new(),
                code_only_hourly,
                raw_used: HashMap::new(),
                slots_changed: false,
                autopilot_hours: Vec::new(),
            }
        }

        pub fn take_resync(&mut self) -> Vec<(String, String)> {
            std::mem::take(&mut self.resync)
        }

        /// 复盘的 `hourly_blocks`：加上旧版按文案数不到、3.0 按原因码数到的小时（#61）。
        fn hourly_blocks(&self, r: &mut Map<String, Value>) {
            if self.code_only_hourly.is_empty() {
                return;
            }
            let Some(day) = r.get("day").and_then(Value::as_str) else {
                return;
            };
            let start = gate_core::clock::Clock::new("Asia/Shanghai", 7)
                .service_day_start(day)
                .expect("复盘日期");
            let mut hours: Vec<i64> = self
                .code_only_hourly
                .iter()
                .filter(|t| start <= **t && **t < start + 86400.0)
                .map(|t| (t / 3600.0).trunc() as i64)
                .collect();
            hours.sort_unstable();
            hours.dedup();
            if hours.is_empty() {
                return;
            }
            // 夹具里这类场景没有带文案的记录，旧版数到 0：并集就是这些小时
            assert_eq!(
                r["hourly_blocks"], 0,
                "夹具假设：只有原因码的记录单独成场景"
            );
            r.insert("hourly_blocks".into(), json!(hours.len()));
        }

        /// 复盘的 `used`：不再按节约模式折算（#67）。
        fn raw_used(&self, r: &mut Map<String, Value>) {
            if let Some(u) = r
                .get("day")
                .and_then(Value::as_str)
                .and_then(|d| self.raw_used.get(d))
            {
                r.insert("used".into(), json!(u));
            }
        }

        fn review(&self, r: &mut Map<String, Value>) {
            review(r);
            self.hourly_blocks(r);
            self.raw_used(r);
        }

        fn quota_result(&self, v: &mut Value) {
            quota_result(v);
            if let Some(Value::Object(r)) = v.get_mut("review") {
                self.hourly_blocks(r);
                self.raw_used(r);
            }
        }

        /// 一个设置的新值；`None` = 3.0 不写这个设置。
        fn setting(&mut self, key: &str, value: &str, drop_note: bool) -> Option<String> {
            if DROPPED_SETTINGS.contains(&key) {
                return None;
            }
            if key == "global_daily_v5" {
                self.resync.push((key.to_string(), value.to_string()));
            }
            // 「名额 +5」的写入（#68）：改注册名额、记加名额的时间
            if key == "register_max_users" {
                self.slots_changed = true;
                return None;
            }
            if key == "autopilot_slots_at" {
                return None;
            }
            let fixed = match key {
                "autopilot_last" => reparse(value, autopilot_v3),
                // 最后一条是这一轮新记的每小时快照；这个场景之前几步记的也一样，都去掉名额
                "autopilot_history" => reparse(value, |v| {
                    let Some(list) = v.as_array_mut() else { return };
                    if let Some(h) = list.last().and_then(|h| h["hour"].as_str()) {
                        self.autopilot_hours.push(h.to_string());
                    }
                    for h in list.iter_mut() {
                        if h["hour"]
                            .as_str()
                            .is_some_and(|x| self.autopilot_hours.iter().any(|y| y == x))
                            && let Some(o) = h.as_object_mut()
                        {
                            o.shift_remove("slots");
                        }
                    }
                }),
                "quota_algo_last" => reparse(value, |v| self.quota_result(v)),
                "quota_algo_history" => reparse(value, |v| {
                    if let Some(Value::Object(r)) = v.as_array_mut().and_then(|a| a.last_mut()) {
                        self.review(r);
                    }
                }),
                "algo_notice" => {
                    let i = if drop_note {
                        value.find("今日额度：").expect("首页文案")
                    } else {
                        0
                    };
                    notice(&value[i..])
                }
                _ => value.to_string(),
            };
            if matches!(
                key,
                "quota_algo_last"
                    | "quota_algo_history"
                    | "algo_notice"
                    | "autopilot_last"
                    | "autopilot_history"
            ) {
                self.last_written.insert(key.to_string(), fixed.clone());
            }
            Some(fixed)
        }

        /// 一步的期望：设置写入、返回值。
        pub fn fix_step(&mut self, want: &mut Value, op: &str) {
            let mut drop_note = false;
            if let Some(writes) = want
                .get_mut("settings_writes")
                .and_then(Value::as_array_mut)
            {
                let mut out = Vec::new();
                for w in writes.iter() {
                    match w[0].as_str() {
                        Some("set") => {
                            let key = switch_key(w[1].as_str().unwrap());
                            if let Some(v) = self.setting(key, w[2].as_str().unwrap(), drop_note) {
                                out.push(json!(["set", key, v]));
                            }
                        }
                        Some("bulk") => {
                            let mut pairs = Vec::new();
                            for kv in w[1].as_array().unwrap() {
                                let key = kv[0].as_str().unwrap();
                                let raw = kv[1].as_str().unwrap();
                                if key == "quota_algo_history" {
                                    drop_note = only_base_changed(raw);
                                }
                                if let Some(v) = self.setting(key, raw, drop_note) {
                                    pairs.push(json!([key, v]));
                                }
                            }
                            if !pairs.is_empty() {
                                out.push(json!(["bulk", pairs]));
                            }
                        }
                        _ => out.push(w.clone()),
                    }
                }
                *writes = out;
                mask_writes(writes);
            }
            if let Some(result) = want.get_mut("result") {
                match op {
                    "quota_run" => self.quota_result(result),
                    "autopilot_run" => autopilot_v3(result),
                    "probe" => {
                        if let Some(g) = result
                            .get_mut("guard_values")
                            .and_then(Value::as_object_mut)
                        {
                            g.shift_remove("base_daily_images");
                            // 每小时上限自动调整的下限 / 上限 / 步长（原来是私密参数，docs/v3-deviations.md #91）
                            g.insert("hourly_min".into(), json!(100));
                            g.insert("hourly_max".into(), json!(160));
                            g.insert("hourly_step".into(), json!(10));
                        }
                    }
                    // 分配模块的面板参数「V5 每人范围 3～30 张」换成新公式的说明（#63）
                    "snapshot" => {
                        for m in result.as_array_mut().into_iter().flatten() {
                            // 自动驾驶没有「名额」了（#68）
                            if m["name"] == "autopilot" {
                                m["question"] = json!("全站熔断、单个 Key 限流");
                                let params = m["params"].as_array_mut().unwrap();
                                assert_eq!(params.pop().unwrap()["name"], "名额");
                            }
                            if m["name"] == "allocation" {
                                let p = &mut m["params"][3];
                                assert_eq!(p["name"], "V5 每人范围");
                                *p = json!({
                                    "name": "V5 每人",
                                    "value": gate_core::modules::allocation_v5_rule_text(),
                                    "note": "每天 7 点按恢复速度定全站池，再按前 3 天用过 V5 的人数分，当天不变",
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        /// 最后的表行差异（期望一侧）：设置行按最后一次写入改写，操作日志按 3.0 的文案改写。
        pub fn fix_diff(&self, diff: &mut Value) {
            if let Some(t) = diff.get_mut("site_settings").and_then(Value::as_object_mut) {
                for side in ["added", "removed"] {
                    if let Some(rows) = t.get_mut(side).and_then(Value::as_array_mut) {
                        rows.retain(|r| !DROPPED_SETTINGS.iter().any(|k| r["key"] == *k));
                        for r in rows.iter_mut() {
                            if r["key"] == "module_allocation" {
                                r["key"] = json!(QUOTA_SWITCH);
                            }
                        }
                        // 名额 +5 删除（#68）：不记加名额的时间，也不改注册名额
                        rows.retain(|r| r["key"] != "autopilot_slots_at");
                        if self.slots_changed {
                            rows.retain(|r| r["key"] != "register_max_users");
                        }
                        if side == "added" {
                            for r in rows.iter_mut() {
                                let key = r["key"].as_str().unwrap_or("").to_string();
                                if let Some(v) = self.last_written.get(&key) {
                                    r["value"] = json!(v);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(rows) = diff
                .get_mut("admin_actions")
                .and_then(|t| t.get_mut("added"))
                .and_then(Value::as_array_mut)
            {
                rows.retain_mut(|r| match r["action"].as_str() {
                    Some("自动驾驶：名额") => false,
                    // 「上限 a→a2 · 保底 b→b2」→「上限 a→a2」；只有保底变了的那条不再记（#61）
                    Some("动态额度调整") => {
                        let target = r["target"].as_str().unwrap();
                        let upper = target.split(" · 保底 ").next().unwrap().to_string();
                        let (from, to) = upper.trim_start_matches("上限 ").split_once('→').unwrap();
                        let keep = from != to;
                        r["target"] = json!(upper);
                        keep
                    }
                    // 「V4.5 上限 a（保底 b）· V5 …」→「V4.5 上限 a · V5 …」（#61）
                    Some("动态额度应用") => {
                        let detail = r["detail"].as_str().unwrap();
                        if let Some((head, rest)) = detail.split_once("（保底 ")
                            && let Some((_, tail)) = rest.split_once("）·")
                        {
                            r["detail"] = json!(format!("{head} ·{tail}"));
                        }
                        true
                    }
                    _ => true,
                });
            }
            drop_empty(diff);
        }
    }

    /// 两边都做：操作日志不比自增 id（期望一侧删掉的日志会让后面的 id 错位）；
    /// 取决于 V5 结果的行换成占位或去掉（#63：成员 Key 的 `daily_v5`、V5 相关设置、「动态额度应用」日志——
    /// 是否下发取决于每人 V5）。
    pub fn normalize_diff(diff: &mut Value) {
        if let Some(t) = diff.get_mut("admin_actions").and_then(Value::as_object_mut) {
            for side in ["added", "removed"] {
                if let Some(rows) = t.get_mut(side).and_then(Value::as_array_mut) {
                    rows.retain(|r| r["action"] != "动态额度应用");
                    for r in rows {
                        if let Some(o) = r.as_object_mut() {
                            o.shift_remove("id");
                        }
                    }
                }
            }
        }
        if let Some(t) = diff.get_mut("api_keys") {
            for side in ["added", "removed"] {
                for r in t
                    .get_mut(side)
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    if r.get("daily_v5").is_some() {
                        r["daily_v5"] = json!(MASK);
                    }
                }
            }
            cancel_pairs(t);
        }
        if let Some(t) = diff.get_mut("site_settings") {
            for side in ["added", "removed"] {
                if let Some(rows) = t.get_mut(side).and_then(Value::as_array_mut) {
                    rows.retain(|r| r["key"] != "quota_v5_day_plan");
                    for r in rows.iter_mut() {
                        let key = r["key"].as_str().unwrap_or("").to_string();
                        match &mut r["value"] {
                            Value::String(s) => *s = mask_setting(&key, s),
                            v if key == "kernel_last" => mask_kernel_last(v),
                            _ => {}
                        }
                    }
                }
            }
            cancel_pairs(t);
        }
        if let Some(t) = diff
            .get_mut("sqlite_sequence")
            .and_then(Value::as_object_mut)
        {
            for side in ["added", "removed"] {
                if let Some(rows) = t.get_mut(side).and_then(Value::as_array_mut) {
                    rows.retain(|r| r["name"] != "admin_actions");
                }
            }
        }
        drop_empty(diff);
    }

    /// 改写后两边都空了的表从差异里去掉。
    fn drop_empty(diff: &mut Value) {
        if let Some(m) = diff.as_object_mut() {
            m.retain(|_, t| {
                ["added", "removed"].iter().any(|s| {
                    t.get(*s)
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty())
                })
            });
        }
    }
}

/// 校验没通过时旧版记的 Bug（来源 `check:…`）：3.0 不再记。模块自己抛异常记的 `module:…` 照旧。
fn drop_check_bugs(diff: &mut Value) {
    let Some(t) = diff.get_mut("error_events").and_then(Value::as_object_mut) else {
        return;
    };
    for side in ["added", "removed"] {
        if let Some(rows) = t.get_mut(side).and_then(Value::as_array_mut) {
            rows.retain(|r| {
                !r["source"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("check:"))
            });
        }
    }
    let empty = |side: &str| {
        t.get(side)
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    };
    if empty("added") && empty("removed") {
        diff.as_object_mut().unwrap().shift_remove("error_events");
    }
}

// ---------------------------------------------------------------------------
// 回放
// ---------------------------------------------------------------------------

async fn settle(state: &AppState, mock: &MockUpstream) -> Vec<String> {
    // 记 Bug 的后台任务写完；告警是 fire-and-forget 的 HTTP，等请求数稳定
    for _ in 0..1000 {
        if state.tasks.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let mut last = usize::MAX;
    for _ in 0..200 {
        let n = mock.requests_to("/alert-hook").len();
        if n == last {
            break;
        }
        last = n;
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    mock.requests_to("/alert-hook")
        .iter()
        .map(|r| {
            r.json()
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn token_id(token: &str) -> String {
    gate_upstream::headers::token_id(token)
}

async fn run_case(case: &Value) -> Result<(), String> {
    let name = case["name"].as_str().unwrap().to_string();
    let env = &case["env"];
    let tokens: Vec<String> = case["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    let first_at = case["steps"][0]["at"].as_f64().unwrap_or(0.0);
    gate_server::time::freeze(Some(first_at));

    let mock = MockUpstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let num = |k: &str| gate_core::pyc::float_repr(env[k].as_f64().unwrap());
    let int = |k: &str| env[k].as_i64().unwrap().to_string();
    let vars: HashMap<String, String> = [
        ("DATA_DIR", dir.path().to_str().unwrap().to_string()),
        ("NAI_TOKENS", tokens.join(",")),
        ("NAI_IMAGE_HOST", mock.url()),
        ("ALERT_WEBHOOK_URL", format!("{}/alert-hook", mock.url())),
        ("TZ", "Asia/Shanghai".into()),
        ("DAY_RESET_HOUR", "7".into()),
        ("IMAGE_MIN_INTERVAL", num("image_min_interval")),
        // 夹具 env 里的 key_image_min_interval：3.0 没有每 Key 出图间隔（docs/v3-deviations.md #64），不再传
        ("DEFAULT_KEY_EXPIRES_DAYS", int("default_expires_days")),
        (
            "KEY_INACTIVITY_DELETE_DAYS",
            int("key_inactivity_delete_days"),
        ),
        // 夹具 env 里的 global_daily_v5：3.0 没有这个环境变量（全站 V5 归动态额度管，docs/v3-deviations.md #70），不再传
        ("POST_REQUEST_JITTER_MIN", num("post_jitter_min")),
        ("POST_REQUEST_JITTER_MAX", num("post_jitter_max")),
        (
            "SECRET_KEY",
            "test-secret-key-test-secret-key-0123456789".into(),
        ),
        ("ADMIN_PASSWORD", "dev-admin-password-123".into()),
        ("UPSTREAM_HTTP2", "0".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let state = AppState::build(EnvConfig::load(&vars), Assets::new(None))
        .await
        .map_err(|e| format!("{name}: 建网关失败 {e}"))?;
    state
        .kernel
        .set_official_api(Some("http://127.0.0.1:9/never".into()));

    // 上游账号状态
    let pool = state.nai.tokens().await;
    if let Some(ts) = case["token_state"].as_object() {
        for (idx, spec) in ts {
            let t = &pool[idx.parse::<usize>().unwrap()];
            if spec["admin_disabled"].as_bool() == Some(true) {
                state.nai.set_admin_enabled(&t.token_id, false).await;
            }
            if let Some(secs) = spec["blocked_for"].as_f64() {
                state.nai.mark_rate_limited(t, secs);
            }
        }
    }

    // 种子数据
    // 模块「分配」的开关 3.0 就是动态额度的 quota_auto_enabled（docs/v3-deviations.md #70）：种子里的
    // module_allocation 按同一个开关写进去
    let seed: Vec<String> = case["seed_sql"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            s.as_str()
                .unwrap()
                .replace("'module_allocation'", &format!("'{}'", v3::QUOTA_SWITCH))
        })
        .collect();
    if !seed.is_empty() {
        state
            .db
            .run_script(seed.clone())
            .await
            .map_err(|e| format!("{name}: 种子数据失败 {e}"))?;
    }
    state.guard.load().await.map_err(|e| e.to_string())?;
    state.pauses.load(
        state
            .db
            .active_pauses(first_at)
            .await
            .map_err(|e| e.to_string())?,
    );
    for st in case["guard_starts"].as_array().unwrap() {
        let st = st.as_array().unwrap();
        state.guard.record_start(
            &token_id(&tokens[st[0].as_u64().unwrap() as usize]),
            st[2].as_i64().unwrap(),
            st[1].as_f64().unwrap(),
        );
    }
    if let Some(r) = case["registrar"].as_object() {
        state.kernel.set_registrar(Some(Arc::new(FakeRegistrar {
            db: state.db.clone(),
            max_users_env: r["max_users_env"].as_i64().unwrap(),
            active: r["active"].as_i64().unwrap(),
        })));
    }
    let al = &case["allowance"];
    state
        .kernel
        .set_allowance_source(Some(Arc::new(FakeAllowance {
            accounts: al["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| {
                    a.as_object().map(|o| {
                        (
                            o.get("percent").and_then(pynum),
                            o.get("recharge_per_day").and_then(pynum),
                        )
                    })
                })
                .collect(),
            cached: al["cached"].as_bool().unwrap(),
            client: al["client"].as_bool().unwrap(),
            fails: al["resolve_fails"].as_bool().unwrap(),
            resolved: AtomicBool::new(false),
        })));
    let loose = case["loose_errors"].as_bool().unwrap_or(false);

    let before = state.db.dump_tables().await.map_err(|e| e.to_string())?;
    state.db.record_setting_writes(true);
    state.kernel.load(&state).await.map_err(|e| e.to_string())?;

    let steps = case["steps"].as_array().unwrap();
    let expected_steps = case["expect"]["steps"].as_array().unwrap();
    let mut seen_alerts = 0usize;
    let mut levels = Levels::new();
    let mut v3_expect = v3::Expect::new(&seed);
    let review_days: Vec<String> = steps
        .iter()
        .map(|s| {
            gate_core::clock::Clock::new("Asia/Shanghai", 7)
                .day(s["at"].as_f64().unwrap() - 86400.0)
        })
        .collect();
    v3_expect.raw_used = v3::raw_used_days(&state.db, &review_days).await;
    for (i, (step, want)) in steps.iter().zip(expected_steps).enumerate() {
        let at = step["at"].as_f64().unwrap();
        gate_server::time::freeze(Some(at));
        let _ = state.db.take_setting_writes();
        let op = step["op"].as_str().unwrap();
        let mut want = want.clone();
        fix_expected_step(&mut want, op, &mut levels, &mut v3_expect);
        let want = &want;
        let mut got = Map::new();
        let outcome: Result<Option<Value>, String> = match op {
            "kernel_load" => state
                .kernel
                .load(&state)
                .await
                .map(|_| None)
                .map_err(|e| e.ty),
            "tick_all" => state
                .kernel
                .tick_all(&state)
                .await
                .map(|m| Some(Value::Object(m)))
                .map_err(|e| e.ty),
            "quota_run" => run_quota_checked(&state).await.map(Some).map_err(|e| e.ty),
            "autopilot_run" => run_autopilot_checked(&state)
                .await
                .map(Some)
                .map_err(|e| e.ty),
            "snapshot" => state
                .kernel
                .snapshot(&state)
                .await
                .map(Some)
                .map_err(|e| e.ty),
            "set_enabled" => state
                .kernel
                .set_enabled(
                    &state,
                    step["name"].as_str().unwrap(),
                    step["on"].as_bool().unwrap(),
                )
                .await
                .map(|_| None)
                .map_err(|e| e.ty),
            "sql" => {
                let sql = step["sql"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect();
                state
                    .db
                    .run_script(sql)
                    .await
                    .map(|_| None)
                    .map_err(|e| e.to_string())
            }
            "guard_load" => state
                .guard
                .load()
                .await
                .map(|_| None)
                .map_err(|e| e.to_string()),
            "trip_breaker" => state
                .guard
                .with(|g| g.trip_breaker(step["seconds"].as_f64().unwrap(), "synthetic", at))
                .map(|_| None)
                .map_err(|e| format!("{e:?}")),
            "probe" => Ok(Some(json!({
                "breaker_until": gate_core::pyval::float(state.guard.with(|g| g.breaker_until)),
                "paused": state.pauses.snapshot().into_iter()
                    .map(|(k, (until, _))| (k.to_string(), gate_core::pyval::float(until))).collect::<Map<String, Value>>(),
                "guard_values": Value::Object(state.guard.with(|g| g.values_map())),
            }))),
            other => return Err(format!("{name}: 未知步骤 {other}")),
        };
        let alerts = settle(&state, &mock).await;
        let new_alerts: Vec<String> = alerts[seen_alerts.min(alerts.len())..].to_vec();
        seen_alerts = alerts.len();
        let mut raw_writes = actual_writes(state.db.take_setting_writes());
        v3::mask_writes(&mut raw_writes);
        let shape = |ws: &[Value]| -> Vec<String> {
            ws.iter()
                .map(|w| {
                    let kind = w[0].as_str().unwrap_or("");
                    let n = if kind == "bulk" {
                        w[1].as_array().map_or(0, Vec::len)
                    } else {
                        1
                    };
                    format!("{kind}:{n}")
                })
                .collect()
        };
        let expected_shape = shape(want["settings_writes"].as_array().unwrap());
        if shape(&raw_writes) != expected_shape {
            return Err(format!(
                "{name} 第 {} 步 {op}: 设置写入的事务划分不同：实际 {:?}，期望 {:?}",
                i + 1,
                shape(&raw_writes),
                expected_shape
            ));
        }
        let writes = flatten_writes(&raw_writes);
        let ctx = format!("{name} 第 {} 步 {op}", i + 1);

        // 异常类型
        match (&outcome, want.get("error").and_then(Value::as_str)) {
            (Err(ty), Some(want_ty)) if ty == want_ty => {}
            (Err(ty), want_ty) => {
                return Err(format!("{ctx}: 异常类型不同：实际 {ty}，期望 {want_ty:?}"));
            }
            (Ok(_), Some(want_ty)) => return Err(format!("{ctx}: 应当抛 {want_ty}，实际成功")),
            (Ok(_), None) => {}
        }
        // 返回值
        if let Ok(Some(mut v)) = outcome {
            if op == "tick_all" {
                normalize_kernel_last(&mut v);
            }
            if op == "snapshot" {
                // 面板里每个模块的 last 也是运行记录
                if let Some(a) = v.as_array_mut() {
                    for m in a {
                        let mname = m["name"].as_str().unwrap().to_string();
                        if let Some(last) = m.get_mut("last") {
                            let mut wrap = json!({ mname.clone(): last.clone() });
                            normalize_kernel_last(&mut wrap);
                            *last = wrap[mname.as_str()].clone();
                        }
                    }
                }
            }
            let mut expected = want.get("result").cloned().unwrap_or(Value::Null);
            v3::mask_result(op, &mut v);
            v3::mask_result(op, &mut expected);
            if loose {
                loosen_errors(&mut v);
                loosen_errors(&mut expected);
            }
            if let Some(d) = py_diff(&v, &expected, true) {
                return Err(format!("{ctx}: 返回值不同：{d}"));
            }
            got.insert("result".into(), v);
        }
        // 设置写入
        let expected_writes = flatten_writes(want["settings_writes"].as_array().unwrap());
        if let Some(d) = compare_writes(&writes, &expected_writes, loose) {
            return Err(format!("{ctx}: {d}"));
        }
        // 告警
        // 3.0 只私信重大事故（docs/v3-deviations.md）：旧版会私信、现在只写日志的从期望里去掉
        let expected_alerts: Vec<Value> = want["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| alert_pages(a.as_str().unwrap_or("")))
            .cloned()
            .collect();
        // 松的场景：告警里有 Bug 特征码（含 Python 堆栈位置）和异常消息，只比条数
        let same_alerts = if loose {
            new_alerts.len() == expected_alerts.len()
        } else {
            json!(new_alerts) == json!(expected_alerts)
        };
        if !same_alerts {
            return Err(format!(
                "{ctx}: 告警不同：实际 {new_alerts:?}，期望 {expected_alerts:?}"
            ));
        }
        // 把旧版这一步写下的全站 V5 池写回库里，后面的步骤从同一个起点继续（见 v3::Expect::resync）
        for (k, v) in v3_expect.take_resync() {
            state
                .db
                .set_setting(&k, &v)
                .await
                .map_err(|e| e.to_string())?;
        }
    }

    // 表行差异
    let after = state.db.dump_tables().await.map_err(|e| e.to_string())?;
    let (mut before, mut after) = (before, after);
    normalize_dump(&mut before, true, loose);
    normalize_dump(&mut after, true, loose);
    let mut actual_diff = table_diff(&before, &after);
    let mut want_diff = case["expect"]["tables_diff"].clone();
    // 3.0 给 error_events 多加了一列 `auto_resolved`（Bug 自动处理，docs/v3-deviations.md #45）；
    // Python 夹具里的行没有它，补成默认值 NULL（新增的行恒为 NULL）
    // 同理：api_keys 多了 `disabled_reason`（停用原因，docs/v3-deviations.md #47），内核不停用 Key，恒为 NULL
    for (table, column) in [
        ("error_events", "auto_resolved"),
        ("api_keys", "disabled_reason"),
    ] {
        for side in ["added", "removed"] {
            if let Some(rows) = want_diff
                .get_mut(table)
                .and_then(|t| t.get_mut(side))
                .and_then(Value::as_array_mut)
            {
                for row in rows {
                    if let Some(o) = row.as_object_mut() {
                        o.insert(column.into(), Value::Null);
                    }
                }
            }
        }
    }
    // 期望里的 kernel_last 行同样解析（Python 侧已经是占位，不需要规整）
    for side in ["added", "removed"] {
        if let Some(rows) = want_diff
            .get_mut("site_settings")
            .and_then(|t| t.get_mut(side))
            .and_then(Value::as_array_mut)
        {
            for row in rows {
                if row.get("key").and_then(Value::as_str) == Some("kernel_last") {
                    let mut v: Value =
                        serde_json::from_str(row["value"].as_str().unwrap()).unwrap();
                    // 「结束时有」的是旧版 tick 写的；「seed 后有」的是种子，两边一样，不动
                    if side == "added" {
                        fix_kernel_last(&mut v, &mut levels);
                    }
                    if loose {
                        loosen_errors(&mut v);
                    }
                    row["value"] = v;
                }
            }
        }
    }
    drop_check_bugs(&mut want_diff);
    v3_expect.fix_diff(&mut want_diff);
    v3::normalize_diff(&mut want_diff);
    v3::normalize_diff(&mut actual_diff);
    if loose {
        // Bug 的特征码 / 堆栈里有 Python 的文件名和函数名：只比数量，不比内容
        let count = |d: &Value| {
            d.get("error_events")
                .and_then(|e| e.get("added"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        if count(&actual_diff) != count(&want_diff) {
            return Err(format!(
                "{name}: 新增 Bug 条数不同：实际 {}，期望 {}",
                count(&actual_diff),
                count(&want_diff)
            ));
        }
        for d in [&mut actual_diff, &mut want_diff] {
            if let Some(o) = d.as_object_mut() {
                o.remove("error_events");
            }
        }
    }
    if let Some(d) = py_diff(&actual_diff, &want_diff, false) {
        return Err(format!("{name}: 表行差异不同：{d}"));
    }
    drop(state);
    drop(mock);
    Ok(())
}

#[tokio::test]
async fn kernel_replays_match_python() {
    let cases = load_cases();
    assert!(cases.len() >= 40, "场景太少：{}", cases.len());
    let only = std::env::var("KERNEL_PARITY_ONLY").ok();
    let mut failures = Vec::new();
    let mut ran = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        if let Some((_, why)) = v3::REMOVED_SCENARIOS.iter().find(|(n, _)| *n == name) {
            eprintln!("[{name}] 跳过：{why}");
            continue;
        }
        ran += 1;
        if let Err(e) = run_case(case).await {
            failures.push(format!("[{name}] {e}"));
        }
    }
    gate_server::time::freeze(None);
    assert!(
        failures.is_empty(),
        "{} / {} 个场景与旧版不一致：\n{}",
        failures.len(),
        ran,
        failures.join("\n\n")
    );
}

/// 告警正文「⚠ **猫头鹰公益站告警**（kind）…」里的 kind 是否还会私信。
fn alert_pages(text: &str) -> bool {
    let kind = text
        .split_once('（')
        .and_then(|(_, rest)| rest.split_once('）'))
        .map_or("", |(k, _)| k);
    gate_server::alerts::pages(kind)
}
