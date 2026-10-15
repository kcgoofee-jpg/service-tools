//! 账号保护与排队的纯逻辑（对应 `app/guard.py` 的 `Guard`）：每个上游账号的每日 / 每小时出图上限、
//! 10 分钟突发窗口、上游 429 触发的 AIMD 调整、安静时段、请求间隔抖动、每把 Key 的排队数、全站排队上限。
//!
//! [`GuardCore`] 持有 Python `Guard` 的全部内存状态；所有依赖时间的方法都显式传 `now`（Unix 秒），
//! 抖动用的随机数由调用方注入（`random.uniform(0, n)` = `0 + n * r`，r 为 [0,1) 的均匀随机数）。
//!
//! 注意：很多看似「只读」的方法在旧版里有副作用——`hourly_cap` 会往 `_cap_marks` 记一笔，`_window`
//! 会把 3 小时前的时间戳弹出。这些副作用被后面的判定依赖（`hourly_cap_ceiling`），所以这里的对应方法都是 `&mut self`，
//! 调用顺序也与旧版一致。
//!
//! 不在这里的 IO（服务层负责，键名都在 `site_settings` 表）：
//! - `load()`：对每个字段 `get_setting("guard_<字段>", None)`，读到的键值交给 [`GuardCore::load_settings`]；
//!   `save(body)`：先 [`GuardCore::validate_save`]，再 `set_settings_bulk(guard_<字段> → str(int))`，
//!   写成功之后才 [`GuardCore::apply_updates`]（与旧版「先写库后改内存」同序）；`to_settings` 给出全部字段的当前串；
//! - `seed_hour(db, token_ids)`：`SELECT ts, images FROM usage_log WHERE ts>now-10800 AND status='ok' AND kind LIKE 'image%'
//!   AND images>0 ORDER BY ts`，行交给 [`stamps_from_rows`] + [`GuardCore::seed_hour`]；
//! - `token_block_reason(db, token_id, day)` 里的 `db.get_upstream_counter(token_id, day)`
//!   （`upstream_token_counters` 表的 `units`，缺 `units` 用 `images`，没有记录是 0）：服务层取出 `used` 传进来
//!   （旧版 `daily == 0` 或熔断 / 安静时段已经拦下时不会去读，多读一次没有可观察的差别）；
//! - `on_upstream_429`：读 `guard_429_cap_at`，写 `guard_account_hourly_cap` / `guard_hourly_adapted_at` /
//!   `guard_last_upstream_429` / `guard_429_cap_at`（方法返回要写的键值，旧版 `db is None` 时不写）；
//! - `adapt_daily`：读 `guard_last_upstream_429` / `guard_hourly_adapted_at` / `guard_429_cap_at`，查 `usage_log`
//!   （`SELECT 1 FROM usage_log u LEFT JOIN api_keys k ON k.id=u.key_id WHERE u.ts>? AND COALESCE(k.is_test,0)=0
//!   AND COALESCE(k.is_admin,0)=0 AND u.reason=? LIMIT 1` 参数 `(now-86400, "hourly_cap")`，以及 `SELECT MIN(ts) FROM usage_log`），
//!   写 `guard_hourly_adapted_at` / `guard_account_hourly_cap`；
//! - 私密参数 `P(...)`：用 [`crate::params::Params`] 传入。
//!
//! 与旧版一致的怪癖（照抄，别「修」）：
//! - `guard_429_cap_at` 写的是 `min(prev, old)` 这个**对象**：前值是 float，`old` 是 int，`min` 取到谁写谁的 `str`，
//!   所以可能写 `"125.0"` 也可能写 `"125"`；
//! - 每小时上限的 `_cap_marks` 只在上限变化时记，最多 64 条；
//! - `_window` 只从队首弹 3 小时前的时间戳，乱序追加的时间戳不会被整理。
//!
//! 已删除功能的残留不移植：3 小时上限（`WINDOW_3H` 只剩队列保留期）、10 分钟旋钮、每日熔断。
//!
//! 3.0 删除的（docs/v3-deviations.md #61）：「V4.5 每人保底 + 全站空闲时借用到每日上限」整层——
//! 设置 `base_daily_images`、`site_idle`、`paced_headroom`。额度是 PVP：每人只有一个上限，
//! 全站按时段放行（`paced`）对所有人一视同仁。

use crate::clock::Clock;
use crate::params::Params;
use crate::pyc;
use crate::reasons::{self, Reason};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};

pub const HOUR: i64 = 3600;
/// 队列保留期（旧版的 3 小时上限已删，只剩这个）
pub const WINDOW_3H: i64 = 3 * HOUR;
/// 突发窗口：一小时的额度挤在十分钟里发完，上游看到的就是 6 倍速率
pub const WINDOW_10M: i64 = 600;
/// 每 10 分钟上限 = 每小时上限 ÷ 4
pub const BURST_DIVISOR: i64 = 4;
/// 每小时上限的自动调整范围（加性增、乘性减）
pub const HOURLY_MIN: i64 = 100;
pub const HOURLY_MAX: i64 = 160;
pub const HOURLY_STEP: i64 = 10;
/// `_cap_marks` 的容量（`deque(maxlen=64)`）
pub const CAP_MARKS_MAX: usize = 64;

/// 一个设置字段：名称、默认值、最小、最大、说明。
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub default: i64,
    pub min: i64,
    pub max: i64,
    pub label: &'static str,
}

const fn field(name: &'static str, default: i64, min: i64, max: i64, label: &'static str) -> Field {
    Field {
        name,
        default,
        min,
        max,
        label,
    }
}

/// 所有设置（存 `site_settings` 的键是 `guard_<名称>`）；0 表示关闭该项。
pub const FIELDS: [Field; 9] = [
    field(
        "account_daily_cap",
        1000,
        0,
        20000,
        "每个上游账号每天最多出图张数",
    ),
    field(
        "daily_burst",
        150,
        0,
        20000,
        "每日上限按时段放行：每天重置时（默认 7 点）先放出这么多张，之后每小时补「每日上限 ÷ 24」张，没用完的留到后面；0 = 不分时段（整天先到先得）",
    ),
    field(
        "account_hourly_cap",
        150,
        0,
        240,
        "每个上游账号每小时最多出图张数（自动调整：上游 429 减半；前一天顶到过上限且没有 429 才 +10，范围 100～160）",
    ),
    field(
        "quiet_start",
        0,
        0,
        23,
        "安静时段开始（北京时间，整点）；与结束相同 = 不启用（2026-10-10 起不启用：夜里不限速）",
    ),
    field(
        "quiet_end",
        0,
        0,
        23,
        "安静时段结束（北京时间，整点；与开始相同表示不设安静时段）",
    ),
    field(
        "quiet_hourly_cap",
        20,
        0,
        240,
        "安静时段每小时最多出图张数；0 = 完全暂停出图（2026-10-13 站长：3-7 点停用）",
    ),
    field(
        "interval_jitter",
        5,
        0,
        30,
        "两次出图间隔额外随机增加 0～N 秒",
    ),
    field(
        "key_image_queue",
        1,
        0,
        3,
        "每把 Key 在生成中的那张之外，最多再排几张",
    ),
    field(
        "queue_per_account",
        5,
        0,
        100,
        "每个上游账号全站最多同时排几张图",
    ),
];

/// `site_settings` 里的键名。
pub fn setting_key(name: &str) -> String {
    format!("guard_{name}")
}

/// Python 异常的类型（golden 只对拍类型）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("ValueError: {0}")]
    Value(String),
    #[error("OverflowError: {0}")]
    Overflow(String),
}

/// Python `int(float)`：NaN → ValueError，无穷 → OverflowError，其余向零截断（超出 i64 饱和）。
fn py_int(x: f64) -> Result<i64, GuardError> {
    if x.is_nan() {
        Err(GuardError::Value(
            "cannot convert float NaN to integer".into(),
        ))
    } else if x.is_infinite() {
        Err(GuardError::Overflow(
            "cannot convert float infinity to integer".into(),
        ))
    } else {
        Ok(x.trunc() as i64)
    }
}

/// 设置表里 `float(await get_setting(k, 0) or 0)` 的读法：没有 / 空串 → 0.0，读不懂 → ValueError。
pub fn parse_setting_float(raw: Option<&str>) -> Result<f64, GuardError> {
    match raw {
        None | Some("") => Ok(0.0),
        Some(s) => pyc::parse_float(s)
            .ok_or_else(|| GuardError::Value(format!("could not convert string to float: {s:?}"))),
    }
}

/// 进行中的出图（排队 / 生成），按进入时间排序。
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub key: i64,
    pub since: f64,
    pub running: bool,
}

/// `on_upstream_429` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream429 {
    /// `(旧, 新)`；没变化 None
    pub change: Option<(i64, i64)>,
    /// 要按顺序写进 `site_settings` 的键值（旧版 `db is None` 时不写）
    pub writes: Vec<(String, String)>,
}

/// `adapt_daily` 的第一步结论。
#[derive(Debug, Clone, PartialEq)]
pub enum AdaptPlan {
    /// 不用再查库了：按 [`Adapted`] 写库并返回
    Done(Adapted),
    /// 去查 `usage_log` 再调 [`GuardCore::adapt_daily_finish`]
    NeedUsage,
}

/// `adapt_daily` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Adapted {
    pub change: Option<(i64, i64)>,
    pub writes: Vec<(String, String)>,
}

/// `save(body)` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub updates: Vec<(String, i64)>,
    /// `set_settings_bulk` 要写的键值；`updates` 为空时旧版不写库
    pub writes: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct GuardCore {
    pub clock: Clock,
    /// 服务日起点（与 `state.day()` 同一口径）
    pub reset_hour: i64,
    pub values: BTreeMap<String, i64>,
    /// token_id → 最近的出图开始时间戳（保留 3 小时）
    pub starts: BTreeMap<String, VecDeque<f64>>,
    pub image_inflight: BTreeMap<i64, i64>,
    pub entries: Vec<Entry>,
    pub seq: i64,
    /// 全站熔断到期时间（内存态，重启即清）
    pub breaker_until: f64,
    pub breaker_reason: String,
    /// (时间, 该时刻的每小时上限)：上限变过就记一笔。
    /// 别在构造时记初始值：构造发生在 load 之前，那时是字段默认值（150），会在重启后的第一个小时把上限抬成 150。
    pub cap_marks: VecDeque<(f64, i64)>,
}

impl GuardCore {
    /// `Guard(db, tz, reset_hour)`（旧版默认 `reset_hour=0`，线上用 `DAY_RESET_HOUR`=7）。
    pub fn new(tz: &str, reset_hour: i64) -> Self {
        let reset_hour = reset_hour.clamp(0, 23);
        Self {
            clock: Clock::new(tz, reset_hour),
            reset_hour,
            values: FIELDS
                .iter()
                .map(|f| (f.name.to_string(), f.default))
                .collect(),
            starts: BTreeMap::new(),
            image_inflight: BTreeMap::new(),
            entries: Vec::new(),
            seq: 0,
            breaker_until: 0.0,
            breaker_reason: String::new(),
            cap_marks: VecDeque::new(),
        }
    }

    /// 构造并按库里的键值加载设置（见 [`load_settings`](Self::load_settings)）。
    pub fn from_settings(
        tz: &str,
        reset_hour: i64,
        stored: &BTreeMap<String, String>,
    ) -> Result<Self, GuardError> {
        let mut g = Self::new(tz, reset_hour);
        g.load_settings(stored)?;
        Ok(g)
    }

    fn v(&self, name: &str) -> i64 {
        self.values[name]
    }

    // ---------- 设置 ----------

    /// `load()` 的解析：`stored` 是库里 `guard_*` 键的原文（没有的键 = 没存，保持现值）。
    /// 每个字段 `max(low, min(high, int(float(raw))))`；读不懂（ValueError / TypeError）回到字段默认值；
    /// `inf` 在旧版是 OverflowError 且没被捕获——这里同样返回 `Err`，前面已处理的字段保持已更新（与旧版一致）。
    pub fn load_settings(&mut self, stored: &BTreeMap<String, String>) -> Result<(), GuardError> {
        for f in FIELDS {
            let Some(raw) = stored.get(&setting_key(f.name)) else {
                continue;
            };
            let v = match pyc::parse_float(raw) {
                None => f.default,
                Some(x) if x.is_nan() => f.default,
                Some(x) => py_int(x)?.clamp(f.min, f.max),
            };
            self.values.insert(f.name.to_string(), v);
        }
        Ok(())
    }

    /// 所有字段的当前值，写库用的串（`str(int)`，不带 `.0`），顺序同 `FIELDS`。
    pub fn to_settings(&self) -> Vec<(String, String)> {
        FIELDS
            .iter()
            .map(|f| (setting_key(f.name), self.values[f.name].to_string()))
            .collect()
    }

    /// `dict(self.values)`，顺序同 `FIELDS`。
    pub fn values_map(&self) -> Map<String, Value> {
        FIELDS
            .iter()
            .map(|f| (f.name.to_string(), Value::from(self.values[f.name])))
            .collect()
    }

    /// `save(body)` 的校验：不认识的字段忽略；每个值必须是整数（bool、字符串、小数都不行）且在范围内。
    /// 返回按请求体顺序的更新列表；不改内存。
    pub fn validate_save(body: &Map<String, Value>) -> Result<Vec<(String, i64)>, GuardError> {
        let mut updates = Vec::new();
        for (name, value) in body {
            let Some(f) = FIELDS.iter().find(|f| f.name == name) else {
                continue;
            };
            let int: Option<i128> = match value {
                Value::Number(n) if !n.is_f64() => n
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| n.as_u64().map(i128::from)),
                // value != int(value)：有小数部分的浮点不是整数
                Value::Number(n) => n
                    .as_f64()
                    .filter(|x| x.is_finite() && *x == x.trunc())
                    .map(|x| x as i128),
                _ => None,
            };
            let Some(int) = int else {
                return Err(GuardError::Value(format!("{} 必须是整数", f.label)));
            };
            if !(i128::from(f.min) <= int && int <= i128::from(f.max)) {
                return Err(GuardError::Value(format!(
                    "{} 必须在 {}～{} 之间",
                    f.label, f.min, f.max
                )));
            }
            updates.push((name.clone(), int as i64));
        }
        Ok(updates)
    }

    /// `self.values.update(updates)`。
    pub fn apply_updates(&mut self, updates: &[(String, i64)]) {
        for (k, v) in updates {
            self.values.insert(k.clone(), *v);
        }
    }

    /// `save(body)` 整体：校验 + 改内存，并给出要写库的键值。
    /// 想保持旧版「先写库后改内存」的顺序时，改用 `validate_save` + 写库 + `apply_updates`。
    pub fn save(&mut self, body: &Map<String, Value>) -> Result<Saved, GuardError> {
        let updates = Self::validate_save(body)?;
        let writes = updates
            .iter()
            .map(|(k, v)| (setting_key(k), v.to_string()))
            .collect();
        self.apply_updates(&updates);
        Ok(Saved { updates, writes })
    }

    /// `describe()`：后台「账号保护」页的设置 + 当前是否安静时段 / 当前每小时上限。
    pub fn describe(&mut self, now: f64) -> Value {
        let fields: Map<String, Value> = FIELDS
            .iter()
            .map(|f| {
                let mut m = Map::new();
                m.insert("default".into(), Value::from(f.default));
                m.insert("min".into(), Value::from(f.min));
                m.insert("max".into(), Value::from(f.max));
                m.insert("label".into(), Value::String(f.label.into()));
                (f.name.to_string(), Value::Object(m))
            })
            .collect();
        let mut out = Map::new();
        out.insert("values".into(), Value::Object(self.values_map()));
        out.insert("fields".into(), Value::Object(fields));
        out.insert("quiet_now".into(), Value::Bool(self.in_quiet(now)));
        out.insert("hourly_cap_now".into(), Value::from(self.hourly_cap(now)));
        Value::Object(out)
    }

    // ---------- 安静时段与每小时上限 ----------

    pub fn in_quiet(&self, now: f64) -> bool {
        let (start, end) = (self.v("quiet_start"), self.v("quiet_end"));
        if start == end {
            return false;
        }
        let hour = self.clock.local_hour(now);
        if start < end {
            start <= hour && hour < end
        } else {
            hour >= start || hour < end
        }
    }

    /// 当前每小时上限。安静时段 `quiet_hourly_cap=0` 表示完全暂停（不是不限）。
    /// 上限变了（429 减半 / 每日 +10 / 进出安静时段）就在 `cap_marks` 记一笔。
    pub fn hourly_cap(&mut self, now: f64) -> i64 {
        let mut cap = self.v("account_hourly_cap");
        if self.in_quiet(now) {
            cap = self.v("quiet_hourly_cap");
            if cap <= 0 {
                return 0;
            }
        }
        if self.cap_marks.back().is_none_or(|m| m.1 != cap) {
            if self.cap_marks.len() >= CAP_MARKS_MAX {
                self.cap_marks.pop_front();
            }
            self.cap_marks.push_back((now, cap));
        }
        cap
    }

    /// 最近 60 分钟窗口里**出现过的最高上限**（限流器用滚动 60 分钟窗口，而上限本身会变）。
    pub fn hourly_cap_ceiling(&mut self, now: f64) -> i64 {
        let current = self.hourly_cap(now);
        self.cap_marks
            .iter()
            .filter(|(ts, _)| now - ts <= HOUR as f64)
            .map(|(_, cap)| *cap)
            .fold(current, i64::max)
    }

    /// 记一次「开始发往上游」。`images` = 这批实际会出几张（n_samples）：按张数记，批量请求不能绕过窗口。
    pub fn record_start(&mut self, token_id: &str, now: f64, images: i64) {
        let q = self.starts.entry(token_id.to_string()).or_default();
        for _ in 0..images.max(1) {
            q.push_back(now);
        }
    }

    /// 启动时从用量日志补回最近 3 小时的出图（`stamps` 来自 [`stamps_from_rows`]）。
    /// 日志里没有记是哪个上游账号，所以每个账号都按全站数量计（偏保守）。返回补回的张数。
    pub fn seed_hour(&mut self, token_ids: &[String], stamps: &[f64]) -> usize {
        for tid in token_ids {
            let q = self.starts.entry(tid.clone()).or_default();
            let merged: Vec<f64> = if q.is_empty() {
                stamps.to_vec()
            } else {
                // sorted(set(q) | set(stamps))
                let mut all: Vec<f64> = q.iter().chain(stamps.iter()).copied().collect();
                all.sort_by(|a, b| a.total_cmp(b));
                all.dedup();
                all
            };
            self.starts.insert(tid.clone(), merged.into());
        }
        stamps.len()
    }

    /// `_window`：队首弹掉 3 小时前的时间戳（副作用），返回 `span` 秒内的。
    fn window(&mut self, token_id: &str, now: f64, span: i64) -> Vec<f64> {
        let Some(q) = self.starts.get_mut(token_id) else {
            return Vec::new();
        };
        if q.is_empty() {
            return Vec::new();
        }
        while q.front().is_some_and(|t| *t <= now - WINDOW_3H as f64) {
            q.pop_front();
        }
        q.iter()
            .copied()
            .filter(|t| *t > now - span as f64)
            .collect()
    }

    pub fn hour_count(&mut self, token_id: &str, now: f64) -> i64 {
        self.window(token_id, now, HOUR).len() as i64
    }

    pub fn count_10m(&mut self, token_id: &str, now: f64) -> i64 {
        self.window(token_id, now, WINDOW_10M).len() as i64
    }

    /// 每 10 分钟最多出几张：每小时上限 ÷ 4（不减少总量，只抹平突发）。
    pub fn burst_cap(&mut self, now: f64) -> i64 {
        let cap = self.hourly_cap(now);
        if cap != 0 {
            ((cap as f64 / BURST_DIVISOR as f64).ceil() as i64).max(1)
        } else {
            0
        }
    }

    /// 突发窗口是否被拦：0 = 放行；>0 = 还要等几分钟。放行判定和首页状态必须用它。
    ///
    /// 两档：① 真·突发：10 分钟内已出到 1.5 倍额度；② 小时额度快见底：剩余不足一个突发窗口。
    pub fn burst_blocked(&mut self, token_id: &str, now: f64) -> i64 {
        let burst = self.burst_cap(now);
        if burst == 0 {
            return 0;
        }
        let n10 = self.count_10m(token_id, now);
        let cap = self.hourly_cap(now);
        let left = if cap != 0 {
            cap - self.hour_count(token_id, now)
        } else {
            burst + 1
        };
        let hard = (burst + 1).max((burst as f64 * 1.5).ceil() as i64);
        if n10 >= hard || (n10 >= burst && left <= burst) {
            return self.minutes_until_free_10m(token_id, now);
        }
        0
    }

    /// 到 10 分钟窗口降回上限以下要等多久（同 `minutes_until_free` 的口径）。
    pub fn minutes_until_free_10m(&mut self, token_id: &str, now: f64) -> i64 {
        let q = self.window(token_id, now, WINDOW_10M);
        if q.is_empty() {
            return 1;
        }
        let cap = self.burst_cap(now);
        let idx = slide_index(q.len(), cap);
        ((q[idx] + WINDOW_10M as f64 - now) / 60.0).ceil().max(1.0) as i64
    }

    /// 到计数降回上限以下要等多久：超了 n 张就要等第 n+1 早的那张滑出 60 分钟窗口（不是最早那一张）。
    pub fn minutes_until_free(&mut self, token_id: &str, now: f64) -> i64 {
        let q = self.window(token_id, now, HOUR);
        if q.is_empty() {
            return 1;
        }
        let cap = self.hourly_cap(now);
        let idx = slide_index(q.len(), cap);
        ((q[idx] + HOUR as f64 - now) / 60.0).ceil().max(1.0) as i64
    }

    // ---------- 每小时上限自动调整（AIMD：上游 429 减半，平稳一天 +10）----------

    /// 上游限流是我们唯一能拿到的「红线」信号：立刻把每小时上限减半（不低于 100）。
    /// `prev_cap_at` = 设置 `guard_429_cap_at` 的现值（[`parse_setting_float`]）。
    pub fn on_upstream_429(
        &mut self,
        now: f64,
        prev_cap_at: f64,
        p: &Params,
    ) -> Result<Upstream429, GuardError> {
        let old = self.v("account_hourly_cap");
        let floor = p.int("capacity.hourly_min", HOURLY_MIN);
        // min(old, …)：上限已经被手动调到下限以下时，429 只能维持或降低，绝不能反而把它抬回下限
        let halved = py_int(old as f64 * p.float("capacity.hourly_decrease", 0.5))?;
        let new = old.min(floor.max(halved));
        let mut writes = self.set_adaptive(new, now, Some(now));
        // 记下「挨限流时的上限」（只在真的降档时记：站长手动压过的低值不该污染这个天花板）。
        // 取历次里更小的那个；已经在下限时也不记，否则加窗天花板会被永久冻在下限。
        if old > floor {
            // min(prev, old) 返回的是对象本身：old < prev 才是 int，否则（含相等）是 float prev
            let value = if prev_cap_at != 0.0 {
                if (old as f64) < prev_cap_at {
                    old.to_string()
                } else {
                    pyc::float_repr(prev_cap_at)
                }
            } else {
                old.to_string()
            };
            writes.push(("guard_429_cap_at".into(), value));
        }
        Ok(Upstream429 {
            change: (new != old).then_some((old, new)),
            writes,
        })
    }

    /// `adapt_daily` 第一步：`last_429` / `last_step` = 设置 `guard_last_upstream_429` /
    /// `guard_hourly_adapted_at` 的现值（[`parse_setting_float`]）。
    /// 第一次运行只开始计时（上线当天不能算「平稳了一天」）；24 小时内调整过或挨过限流不动。
    pub fn adapt_daily_begin(now: f64, last_429: f64, last_step: f64) -> AdaptPlan {
        if last_step == 0.0 {
            return AdaptPlan::Done(Adapted {
                change: None,
                writes: vec![("guard_hourly_adapted_at".into(), pyc::float_repr(now))],
            });
        }
        if now - last_step < 86400.0 || now - last_429 < 86400.0 {
            return AdaptPlan::Done(Adapted {
                change: None,
                writes: vec![],
            });
        }
        AdaptPlan::NeedUsage
    }

    /// `adapt_daily` 第二步（过去 24 小时没有 429 → +10，不超过 `HOURLY_MAX`）。
    /// `has_hit`：过去 24 小时非测试、非管理员 Key 的日志里有没有 `reason='hourly_cap'` 的拒绝（只有上限真的「顶到过」才有信息）；
    /// `first_ts`：`SELECT MIN(ts) FROM usage_log`（空表 = None）；`seen_at`：设置 `guard_429_cap_at` 的现值。
    pub fn adapt_daily_finish(
        &mut self,
        now: f64,
        has_hit: bool,
        first_ts: Option<f64>,
        seen_at: f64,
        p: &Params,
    ) -> Result<Adapted, GuardError> {
        // float(first or now)：0 和 None 都当「没有」
        let first = first_ts.filter(|t| *t != 0.0).unwrap_or(now);
        let start = (now - 86400.0).max(first);
        let covered = (now - start) / 3600.0;
        if !has_hit || covered < 20.0 {
            // 没顶到过上限（没信息），或过去 24 小时数据不完整：不加
            return Ok(Adapted {
                change: None,
                writes: vec![("guard_hourly_adapted_at".into(), pyc::float_repr(now))],
            });
        }
        let old = self.v("account_hourly_cap");
        let mut wanted = p
            .int("capacity.hourly_max", HOURLY_MAX)
            .min(old + p.int("capacity.hourly_step", HOURLY_STEP));
        // 挨过 429 的话，加窗的天花板压到「当时上限的 80%」：AIMD 的乘性减是 /2，
        // 但加性增会一路加回原来的水平，于是每过一两天又撞同一个红线。
        if seen_at != 0.0 {
            let ceiling = p
                .int("capacity.hourly_min", HOURLY_MIN)
                .max(py_int(seen_at * 0.8)?);
            wanted = wanted.min(ceiling);
        }
        let new = old.max(wanted); // 加窗只升不降：天花板低于当前值时也不能反向降档
        let writes = self.set_adaptive(new, now, None);
        Ok(Adapted {
            change: (new != old).then_some((old, new)),
            writes,
        })
    }

    /// `_set_adaptive`：改内存，返回要写库的键值（旧版 `db is None` 时不写）。
    fn set_adaptive(
        &mut self,
        value: i64,
        now: f64,
        last_429: Option<f64>,
    ) -> Vec<(String, String)> {
        self.values.insert("account_hourly_cap".into(), value);
        let mut writes = vec![
            (setting_key("account_hourly_cap"), value.to_string()),
            ("guard_hourly_adapted_at".into(), pyc::float_repr(now)),
        ];
        if let Some(t) = last_429 {
            writes.push(("guard_last_upstream_429".into(), pyc::float_repr(t)));
        }
        writes
    }

    /// `jitter()`：两次出图间隔额外随机增加 0～N 秒。N=0 时不消耗随机数。
    pub fn jitter(&self, rand: &mut dyn FnMut() -> f64) -> f64 {
        let span = self.v("interval_jitter");
        if span != 0 {
            // random.uniform(0, span) = 0 + (span - 0) * random()
            0.0 + (span as f64) * rand()
        } else {
            0.0
        }
    }

    /// 全站熔断：在 `seconds` 秒内，所有上游账号都拒接新的出图任务。到点自动恢复。
    pub fn trip_breaker(&mut self, seconds: f64, reason: &str, now: f64) -> Result<(), GuardError> {
        self.breaker_until = now + if seconds > 0.0 { seconds } else { 0.0 };
        self.breaker_reason = if reason.is_empty() {
            format!(
                "上游连续出错，已暂停出图 {} 秒，到点自动恢复",
                py_int(seconds)?
            )
        } else {
            reason.to_string()
        };
        Ok(())
    }

    // ---------- 每日上限按时段放行（令牌桶：服务日起点有 burst 张，每小时补 daily/24，当天总量不超过 daily）----------

    /// 距离服务日起点（每天 `reset_hour` 点）过去了多少秒。
    pub fn day_elapsed(&self, now: f64) -> f64 {
        now - self.clock.day_start(now)
    }

    /// 到现在为止，这个账号今天最多可以出多少张。
    pub fn paced_allowance(&self, now: f64) -> f64 {
        let (daily, burst) = (self.v("account_daily_cap"), self.v("daily_burst"));
        let x = burst as f64 + (daily as f64 * self.day_elapsed(now)) / 86400.0;
        let cap = daily as f64;
        if x < cap { x } else { cap }
    }

    pub fn minutes_until_paced(&self, used: f64, now: f64) -> i64 {
        let (daily, burst) = (self.v("account_daily_cap"), self.v("daily_burst"));
        let at = if daily != 0 {
            (used + 1.0 - burst as f64) * 86400.0 / daily as f64
        } else {
            0.0
        };
        ((at - self.day_elapsed(now)) / 60.0).ceil().max(1.0) as i64
    }

    /// 这个上游账号现在不能再接新的出图任务时，返回给成员看的原因。
    /// `used` = 该账号今天已用（`upstream_token_counters` 的 `units`，缺则 `images`；节约模式 14 步的图算半张）。
    pub fn token_block_reason(&mut self, token_id: &str, used: f64, now: f64) -> Option<Reason> {
        if self.breaker_until > now {
            let text = if self.breaker_reason.is_empty() {
                format!(
                    "上游连续出错，已暂停出图，约 {} 秒后自动恢复",
                    (self.breaker_until - now) as i64
                )
            } else {
                self.breaker_reason.clone()
            };
            return Some(Reason::new(text, reasons::BREAKER));
        }
        let (qs, qe) = (self.v("quiet_start"), self.v("quiet_end"));
        // 安静时段完全暂停（quiet_hourly_cap=0，2026-10-13 站长：3-7 点硬停用）：
        // hourly_cap 的 0 是 falsy 会跳过每小时检查，必须在这里显式拦截
        if self.in_quiet(now) && self.v("quiet_hourly_cap") <= 0 {
            return Some(Reason::new(
                format!("现在是安静时段（{qs}:00–{qe}:00），暂停出图，{qe} 点恢复"),
                reasons::QUIET_CAP,
            ));
        }
        let daily = self.v("account_daily_cap");
        if daily != 0 {
            if used >= daily as f64 {
                return Some(Reason::new(
                    format!(
                        "本站今天的出图总量已达上限（每个账号 {daily} 张/天，按算力折算，用来保护上游账号），明天 {} 点恢复",
                        self.reset_hour
                    ),
                    reasons::DAILY_CAP,
                ));
            }
            if self.v("daily_burst") != 0 && used >= self.paced_allowance(now) {
                let wait = self.minutes_until_paced(used, now);
                return Some(Reason::new(
                    format!(
                        "全站出图按时段放行（每天 {daily} 张平均分到 24 小时，保证晚来的人也有图出），这个时段的份额已用完，约 {wait} 分钟后有空位"
                    ),
                    reasons::PACED,
                ));
            }
        }
        let cap = self.hourly_cap(now);
        if cap != 0 && self.hour_count(token_id, now) >= cap {
            let wait = self.minutes_until_free(token_id, now);
            if self.in_quiet(now) {
                return Some(Reason::new(
                    format!(
                        "现在是安静时段（{qs}:00–{qe}:00），出图放慢到每小时 {cap} 张，约 {wait} 分钟后有空位"
                    ),
                    reasons::QUIET_CAP,
                ));
            }
            return Some(Reason::new(
                format!(
                    "最近 60 分钟的出图量已达上限（滚动窗口，全站每小时 {cap} 张，不是整点重置），约 {wait} 分钟后随最早的图滑出窗口逐张恢复"
                ),
                reasons::HOURLY_CAP,
            ));
        }
        // 突发保护（2026-10-13 第一次被上游 429 之后加）：小时的额度可以被挤在十分钟里一口气发完，
        // 上游看到的就是 6 倍速率 —— 那正是 429 的触发方式。
        let blocked = self.burst_blocked(token_id, now);
        if blocked != 0 {
            return Some(Reason::new(
                format!(
                    "本时段出图已达上限（每 10 分钟 {} 张，用来抹平突发、保护上游账号），约 {blocked} 分钟后可继续",
                    self.burst_cap(now)
                ),
                reasons::BURST_CAP,
            ));
        }
        None
    }

    // ---------- 排队（P1） ----------

    /// 每把 Key 同时生成 1 张、最多再排 `key_image_queue` 张；全站排队总数有上限。返回拒绝原因或 None。
    /// 通过时登记一个排队条目（编号 = `self.seq`，取 `entries.last()` 可得）；`now` 是 `time.time()`。
    pub fn admit_image(&mut self, key_id: i64, accounts: i64, now: f64) -> Option<Reason> {
        let mine = self.image_inflight.get(&key_id).copied().unwrap_or(0);
        let extra = self.v("key_image_queue");
        if mine > extra {
            // 即 mine >= 1 + extra：每把 Key 同时生成 1 张，再排 extra 张
            let more = if extra != 0 {
                format!("、最多再排 {extra} 张")
            } else {
                String::new()
            };
            return Some(Reason::new(
                format!(
                    "你的上一张图还没出完：每把 Key 同时只生成 1 张{more}，请等前面的完成后再发"
                ),
                reasons::KEY_BUSY,
            ));
        }
        let per = self.v("queue_per_account");
        let total: i64 = self.image_inflight.values().sum();
        if per != 0 && total >= per * accounts.max(1) {
            return Some(Reason::new(
                format!(
                    "当前排队的人太多（全站最多同时排 {} 张），请稍后再试",
                    per * accounts.max(1)
                ),
                reasons::QUEUE_FULL,
            ));
        }
        self.image_inflight.insert(key_id, mine + 1);
        self.seq += 1;
        self.entries.push(Entry {
            id: self.seq,
            key: key_id,
            since: now,
            running: false,
        });
        None
    }

    /// 这把 Key 的一张排队中的图开始发往上游；给了 `entry_id` 就标记那一张。
    pub fn mark_running(&mut self, key_id: i64, entry_id: Option<i64>) {
        for e in &mut self.entries {
            if e.key == key_id && !e.running && entry_id.is_none_or(|id| e.id == id) {
                e.running = true;
                return;
            }
        }
    }

    /// 当前排队中（未开始生成）的 key_id，按到达先后。
    pub fn waiting_keys(&self) -> Vec<i64> {
        self.waiting_sorted().iter().map(|e| e.key).collect()
    }

    fn waiting_sorted(&self) -> Vec<&Entry> {
        let mut w: Vec<&Entry> = self.entries.iter().filter(|e| !e.running).collect();
        w.sort_by(|a, b| a.since.total_cmp(&b.since)); // 稳定排序
        w
    }

    pub fn release_image(&mut self, key_id: i64, entry_id: Option<i64>) {
        let n = self.image_inflight.get(&key_id).copied().unwrap_or(0) - 1;
        if n > 0 {
            self.image_inflight.insert(key_id, n);
        } else {
            self.image_inflight.remove(&key_id);
        }
        let mine: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.key == key_id)
            .map(|(i, _)| i)
            .collect();
        let done = match entry_id {
            // 按条目编号移除：排队中的那张被取消时，不能误删正在生成的那张
            Some(id) => mine.iter().copied().find(|&i| self.entries[i].id == id),
            // 旧调用：先移除正在生成的那张，否则移除最早的
            None => mine
                .iter()
                .copied()
                .find(|&i| self.entries[i].running)
                .or_else(|| mine.first().copied()),
        };
        if let Some(i) = done {
            self.entries.remove(i);
        }
    }

    /// 匿名的排队视图：waiting / running 数量；给定 key_id 时附上这把 Key 的图所处位置（1 起）。
    pub fn queue_view(&self, key_id: Option<i64>) -> Value {
        let waiting = self.waiting_sorted();
        let running: Vec<&Entry> = self.entries.iter().filter(|e| e.running).collect();
        let mut out = Map::new();
        out.insert("waiting".into(), Value::from(waiting.len()));
        out.insert("running".into(), Value::from(running.len()));
        if let Some(k) = key_id {
            let mut mine = Vec::new();
            for _ in running.iter().filter(|e| e.key == k) {
                let mut m = Map::new();
                m.insert("state".into(), Value::String("running".into()));
                mine.push(Value::Object(m));
            }
            for (i, e) in waiting.iter().enumerate() {
                if e.key == k {
                    let mut m = Map::new();
                    m.insert("state".into(), Value::String("waiting".into()));
                    m.insert("position".into(), Value::from(i + 1));
                    mine.push(Value::Object(m));
                }
            }
            out.insert("mine".into(), Value::Array(mine));
        }
        Value::Object(out)
    }
}

/// `max(0, min(len(q) - 1, len(q) - cap)) if cap else 0`
fn slide_index(len: usize, cap: i64) -> usize {
    if cap == 0 {
        return 0;
    }
    let len = len as i64;
    (len - 1).min(len - cap).max(0) as usize
}

/// `[float(ts) for ts, n in rows for _ in range(int(n))]`：用量日志行（ts, images）展开成每张图一个时间戳。
pub fn stamps_from_rows(rows: &[(f64, i64)]) -> Vec<f64> {
    rows.iter()
        .flat_map(|(ts, n)| std::iter::repeat_n(*ts, (*n).max(0) as usize))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-14 12:00:00 +08:00
    const NOON: f64 = 1_791_950_400.0;

    fn guard() -> GuardCore {
        let mut g = GuardCore::new("Asia/Shanghai", 7);
        g.values.insert("quiet_start".into(), 0);
        g.values.insert("quiet_end".into(), 0);
        g
    }

    #[test]
    fn burst_cap_is_quarter_of_hourly() {
        let mut g = guard();
        g.values.insert("account_hourly_cap".into(), 100);
        assert_eq!(g.burst_cap(NOON), 25);
    }

    #[test]
    fn batch_requests_count_per_image() {
        let mut g = guard();
        g.record_start("t", NOON, 4);
        assert_eq!(g.count_10m("t", NOON), 4);
        assert_eq!(g.hour_count("t", NOON), 4);
    }

    #[test]
    fn aimd_halves_and_remembers_cap() {
        let mut g = guard();
        let p = Params::empty();
        let r = g.on_upstream_429(NOON, 0.0, &p).unwrap();
        assert_eq!(r.change, Some((150, 100)));
        assert_eq!(g.v("account_hourly_cap"), 100);
        assert!(
            r.writes
                .contains(&("guard_429_cap_at".into(), "150".into()))
        );
    }

    #[test]
    fn save_rejects_non_integers() {
        let body = serde_json::json!({"quiet_start": 1.5});
        assert!(GuardCore::validate_save(body.as_object().unwrap()).is_err());
        let body = serde_json::json!({"quiet_start": 3.0, "nope": "x"});
        assert_eq!(
            GuardCore::validate_save(body.as_object().unwrap()).unwrap(),
            vec![("quiet_start".to_string(), 3)]
        );
    }
}
