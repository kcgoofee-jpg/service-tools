//! Anlas 手动核对（旧版 `app/reconciliation.py`）：查询官方余额，与本站记账对比。
//!
//! 核对时要拿到「没有进行中的出图、新出图也不能开始」的一刻：先等 `state.budget` 里没有额度预留，
//! 持着预算锁（挡住新的准入）去读官方余额，读完再放。每次核对（含失败）都先写 60 秒冷却，
//! 官方 429 则按 Retry-After 写更长的冷却——冷却存在数据库里，重启不会清零。
//! 官方的响应 / 异常里可能带账号信息，所以失败时只给固定文案，从不回显。

use crate::error::{ApiError, InternalError};
use crate::state::AppState;
use gate_core::pyext;
use gate_core::{pyc, pyval};
use gate_db::DbError;
use gate_upstream::client::Token;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub const COOLDOWN_SETTING: &str = "anlas_reconciliation_retry_at";
/// 等进行中的图片任务结算的最长时间（旧版 `wait_timeout`）。
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 查询全部账号的总时限（旧版 `query_timeout`）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 单个账号的查询时限。
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(8);
/// 官方订阅接口的响应体上限。
const MAX_BODY: usize = 65536;
/// 页面上保留的历史条数。
const HISTORY: i64 = 20;

/// 旧版 `ReconciliationError`：`status` 是回给后台的 HTTP 状态码，`retry_after > 0` 时带 `Retry-After` 头。
#[derive(Debug, Clone)]
pub struct ReconError {
    pub message: String,
    pub status: u16,
    pub retry_after: i64,
}

impl ReconError {
    fn new(message: impl Into<String>, status: u16, retry_after: i64) -> Self {
        Self {
            message: message.into(),
            status,
            retry_after,
        }
    }

    fn unreadable() -> Self {
        Self::new("官方余额未能完整读取；上次结果已保留", 503, 0)
    }
}

/// 核对失败：要么是业务上的拒绝 / 失败（固定文案），要么是内部错误（500）。
#[derive(Debug)]
pub enum RunError {
    Refused(ReconError),
    Internal(ApiError),
}

impl From<ReconError> for RunError {
    fn from(e: ReconError) -> Self {
        RunError::Refused(e)
    }
}

impl From<DbError> for RunError {
    fn from(e: DbError) -> Self {
        RunError::Internal(e.into())
    }
}

fn internal(msg: impl Into<String>) -> RunError {
    RunError::Internal(InternalError(msg.into()).into())
}

// ------------------------------------------------------------------ 数值（保持 int / float 的区别）

/// Python 数字：`int - int` 仍是 int，`round(int, 4)` 仍是 int，一遇到 float 就变 float。
/// 快照里存的类型会原样出现在页面 JSON 里，所以要保持。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Num {
    I(i128),
    F(f64),
}

impl Num {
    fn of(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => match pyext::int_of_number(n) {
                Some(i) => Some(Num::I(i)),
                None => n.as_f64().map(Num::F),
            },
            _ => None,
        }
    }

    fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }

    fn sub(self, o: Self) -> Self {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Num::I(a - b),
            _ => Num::F(self.f() - o.f()),
        }
    }

    fn add(self, o: Self) -> Self {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Num::I(a + b),
            _ => Num::F(self.f() + o.f()),
        }
    }

    /// `round(x, 4)`：int 不变，float 按银行家舍入。
    fn round4(self) -> Self {
        match self {
            Num::I(i) => Num::I(i),
            Num::F(f) => Num::F(pyc::round(f, 4)),
        }
    }

    fn value(self) -> Value {
        match self {
            Num::I(i) => pyext::json_int(i),
            Num::F(f) => pyval::float(f),
        }
    }
}

/// `amount(value)`：必须是有限的 int / float（bool 不算），且在 0～1e12 之间。保持原类型。
fn amount(v: &Value) -> Option<Num> {
    let n = Num::of(v)?;
    let f = n.f();
    (f.is_finite() && (0.0..=1e12).contains(&f)).then_some(n)
}

/// `balance(data)`：官方订阅信息 → `{"fixed", "purchased", "expires_at"}`。读不懂返回 None。
pub fn balance(data: &Value) -> Option<Value> {
    let obj = data.as_object()?;
    if obj.get("active") != Some(&Value::Bool(true)) {
        return None;
    }
    let steps = obj.get("trainingStepsLeft")?.as_object()?;
    let fixed = amount(steps.get("fixedTrainingStepsLeft")?)?;
    let purchased = amount(steps.get("purchasedTrainingSteps")?)?;
    // 用订阅到期时间判断是否进入了新的计费周期
    let expires_at = match obj.get("expiresAt") {
        None | Some(Value::Null) => Value::Null,
        Some(v) => amount(v)?.value(),
    };
    Some(json!({
        "fixed": fixed.value(),
        "purchased": purchased.value(),
        "expires_at": expires_at,
    }))
}

/// Python 的 `a == b`（数字按数值比较，1 == 1.0）。
fn py_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => match (Num::of(x), Num::of(y)) {
            (Some(p), Some(q)) => match (p, q) {
                (Num::I(a), Num::I(b)) => a == b,
                _ => p.f() == q.f(),
            },
            _ => x == y,
        },
        (None, None) => true,
        _ => false,
    }
}

/// 两次快照的对比（旧版 `comparison`）。`previous` 是库里存的上一条；旧快照缺字段时按旧版的 `KeyError` 报错。
pub fn comparison(previous: Option<&Value>, current: &Value) -> Result<Value, String> {
    let Some(previous) = previous else {
        return Ok(json!({"reason": "first"}));
    };
    let missing = |k: &str| format!("KeyError: '{k}'");
    let obj = |v: &Value, k: &str| -> Result<Map<String, Value>, String> {
        v.get(k)
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| missing(k))
    };
    let (old, new) = (obj(previous, "accounts")?, obj(current, "accounts")?);
    if old.len() != new.len() || old.keys().any(|k| !new.contains_key(k)) {
        return Ok(json!({"reason": "accounts_changed"}));
    }
    if old
        .iter()
        .any(|(k, o)| !py_eq(o.get("expires_at"), new[k].get("expires_at")))
    {
        return Ok(json!({"reason": "cycle_changed"}));
    }
    for (k, o) in &old {
        for part in ["fixed", "purchased"] {
            let (Some(a), Some(b)) = (
                new[k].get(part).and_then(Num::of),
                o.get(part).and_then(Num::of),
            ) else {
                return Err(missing(part));
            };
            if a.f() > b.f() {
                return Ok(json!({"reason": "balance_increased"}));
            }
        }
    }
    let (before, after) = (obj(previous, "ledger")?, obj(current, "ledger")?);
    for (k, b) in &before {
        let (Some(a), Some(b)) = (after.get(k).and_then(Num::of), Num::of(b)) else {
            return Err(missing(k));
        };
        if a.f() < b.f() {
            return Ok(json!({"reason": "ledger_reset"}));
        }
    }
    let n = |v: &Value, k: &str| v.get(k).and_then(Num::of).ok_or_else(|| missing(k));
    let ledger =
        |m: &Map<String, Value>, k: &str| m.get(k).and_then(Num::of).ok_or_else(|| missing(k));
    let decrease = n(previous, "balance")?.sub(n(current, "balance")?).round4();
    let recorded = ledger(&after, "anlas")?
        .sub(ledger(&before, "anlas")?)
        .round4();
    let since = previous
        .get("checked_at")
        .cloned()
        .ok_or_else(|| missing("checked_at"))?;
    Ok(json!({
        "reason": null,
        "since": since,
        "balance_decrease": decrease.value(),
        "recorded": recorded.value(),
        "difference": decrease.sub(recorded).round4().value(),
        "unconfirmed_anlas": ledger(&after, "unconfirmed_anlas")?
            .sub(ledger(&before, "unconfirmed_anlas")?)
            .round4()
            .value(),
        "unconfirmed_requests": ledger(&after, "unconfirmed_requests")?
            .sub(ledger(&before, "unconfirmed_requests")?)
            .value(),
    }))
}

// ------------------------------------------------------------------ 官方查询

/// 官方 429 后的冷却秒数：Retry-After（整数秒或 HTTP 日期），至少 5 分钟、至多 1 天。读不懂就 5 分钟。
fn retry_delay(header: &str, now: f64) -> i64 {
    let base = 300_i64;
    let seconds: Option<f64> = if !header.is_empty() && header.chars().all(pyext::is_nd) {
        pyext::py_int_str(header).ok().map(|i| i as f64)
    } else {
        jiff::fmt::rfc2822::parse(header)
            .ok()
            .map(|z| z.timestamp().as_second() as f64 - now)
    };
    match seconds {
        Some(s) if s.is_finite() => base.max(86_400.min(s.ceil() as i64)),
        _ => base,
    }
}

/// 这把 Token 用的浏览器指纹序号（令牌池按序号轮流分配；删除 / 重排后位置会变，指纹序号不变）。
fn profile_index(state: &AppState, token: &Token) -> usize {
    let ua = Some(state.env.upstream_user_agent.as_str()).filter(|s| !s.is_empty());
    (0..3)
        .find(|i| gate_upstream::headers::profile_for(*i, ua) == token.profile)
        .unwrap_or(0)
}

async fn query(state: &AppState, token: &Token) -> Result<Value, RunError> {
    let url = format!("{}/user/subscription", state.nai.image_host());
    let fetched = tokio::time::timeout(
        ACCOUNT_TIMEOUT,
        state.nai.get_json_with(
            &token.token,
            profile_index(state, token),
            &url,
            ACCOUNT_TIMEOUT,
            MAX_BODY,
            false,
        ),
    )
    .await;
    // 官方的响应 / 异常里可能带账号信息或凭据：只给固定文案
    let Ok(Ok((status, headers, body))) = fetched else {
        return Err(ReconError::unreadable().into());
    };
    if status == 429 {
        let header = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let delay = retry_delay(header, state.now());
        state
            .db
            .set_setting(
                COOLDOWN_SETTING,
                &pyc::float_repr(state.now() + delay as f64),
            )
            .await?;
        return Err(ReconError::new("官方查询限流，请稍后重试；上次结果已保留", 429, delay).into());
    }
    if status != 200 {
        return Err(ReconError::unreadable().into());
    }
    let data: Value = serde_json::from_slice(&body).map_err(|_| ReconError::unreadable())?;
    balance(&data).ok_or_else(|| ReconError::unreadable().into())
}

// ------------------------------------------------------------------ 状态与执行

/// 还要等多少秒才能再核对（冷却存在数据库里）。
pub async fn retry_after(state: &AppState) -> Result<i64, RunError> {
    let value = match state.db.get_setting(COOLDOWN_SETTING).await? {
        None => 0.0,
        Some(raw) => pyc::parse_float(&raw)
            .ok_or_else(|| internal(format!("{COOLDOWN_SETTING} 不是数字")))?,
    };
    Ok(((value - state.now()).ceil() as i64).max(0))
}

/// 页面用的状态：最近 20 次结果、冷却剩余秒数、是否正在核对。
pub async fn status(state: &AppState) -> Result<Value, RunError> {
    let history = state.db.reconciliation_history(HISTORY).await?;
    let running = crate::ops::shared(state).reconcile.try_lock().is_err();
    Ok(json!({
        "history": history,
        "retry_after": retry_after(state).await?,
        "running": running,
    }))
}

/// 令牌池里的账号（含已停用的：它们的用量仍在账本里），按 token_id 去重。
async fn accounts(state: &AppState) -> Vec<(String, Arc<Token>)> {
    let mut out: Vec<(String, Arc<Token>)> = Vec::new();
    for t in state.nai.tokens().await {
        match out.iter_mut().find(|(id, _)| *id == t.token_id) {
            Some(slot) => slot.1 = t,
            None => out.push((t.token_id.clone(), t)),
        }
    }
    out
}

/// 执行一次核对，成功返回 [`status`]。
pub async fn run(state: &AppState) -> Result<Value, RunError> {
    run_with(state, WAIT_TIMEOUT).await
}

/// 同 [`run`]，等图片任务结算的最长时间可调（测试用，旧版是类属性 `wait_timeout`）。
pub async fn run_with(state: &AppState, wait_timeout: Duration) -> Result<Value, RunError> {
    {
        let shared = crate::ops::shared(state);
        let Ok(_exclusive) = shared.reconcile.try_lock() else {
            return Err(ReconError::new("已有核对正在进行，请稍后查看结果", 409, 0).into());
        };
        let retry = retry_after(state).await?;
        if retry != 0 {
            return Err(ReconError::new(format!("请在 {retry} 秒后重试"), 429, retry).into());
        }
        // 等所有进行中的图片任务结算完，并持着预算锁（挡住新的准入）读官方余额
        let deadline = Instant::now() + wait_timeout;
        let busy = || ReconError::new("图片任务仍在处理，请稍后核对；尚未查询官方", 409, 0);
        let _budget = loop {
            let guard = tokio::time::timeout_at(deadline, state.budget.lock())
                .await
                .map_err(|_| busy())?;
            if guard.reservations.is_empty() {
                break guard;
            }
            // 先登记等待再放锁：预留清空的通知不会落在两者之间
            let idle = state.budget_idle.notified();
            drop(guard);
            tokio::time::timeout_at(deadline, idle)
                .await
                .map_err(|_| busy())?;
        };
        let pool = accounts(state).await;
        if pool.is_empty() {
            return Err(ReconError::new("尚未配置可查询的上游账号", 503, 0).into());
        }
        if pool.iter().any(|(_, t)| t.fields().disabled) {
            return Err(
                ReconError::new("有上游账号已失效，请先检查令牌池；尚未查询官方", 503, 0).into(),
            );
        }
        let blocked = pool
            .iter()
            .map(|(_, t)| t.fields().blocked_until)
            .fold(f64::NEG_INFINITY, f64::max)
            - state.now();
        if blocked > 0.0 {
            return Err(ReconError::new(
                "上游账号正在冷却，请稍后核对",
                429,
                blocked.ceil() as i64,
            )
            .into());
        }
        // 先把冷却写进库，失败的尝试也算
        state
            .db
            .set_setting(COOLDOWN_SETTING, &pyc::float_repr(state.now() + 60.0))
            .await?;
        let queried = tokio::time::timeout(QUERY_TIMEOUT, async {
            let mut rows = Map::new();
            for (identity, token) in &pool {
                rows.insert(identity.clone(), query(state, token).await?);
            }
            Ok::<_, RunError>(rows)
        })
        .await;
        let rows = match queried {
            Err(_) => return Err(ReconError::new("本次核对超时；上次结果已保留", 503, 0).into()),
            Ok(rows) => rows?,
        };
        let now_ids: Vec<String> = accounts(state)
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if now_ids.len() != pool.len() || pool.iter().any(|(id, _)| !now_ids.contains(id)) {
            return Err(ReconError::new("查询期间账号列表有变化，请重新查询", 503, 0).into());
        }
        let ledger = state.db.reconciliation_totals().await?;
        let mut balance = Num::I(0);
        for row in rows.values() {
            let part = |k: &str| row.get(k).and_then(Num::of).unwrap_or(Num::I(0));
            balance = balance.add(part("fixed").add(part("purchased")));
        }
        let mut current = Map::new();
        current.insert("checked_at".into(), pyval::float(state.now()));
        current.insert("accounts".into(), Value::Object(rows));
        current.insert("ledger".into(), ledger);
        current.insert("balance".into(), balance.value());
        let current = Value::Object(current);
        let previous = state.db.reconciliation_history(1).await?;
        let cmp = comparison(previous.first(), &current).map_err(internal)?;
        let mut snapshot = current;
        if let Some(m) = snapshot.as_object_mut() {
            m.insert("comparison".into(), cmp);
        }
        // 保存要做完：调用方断开也不能留下半条
        let db = state.db.clone();
        let saved = tokio::spawn(async move { db.save_reconciliation(&snapshot).await })
            .await
            .map_err(|e| internal(e.to_string()))?;
        if saved.is_err() {
            return Err(ReconError::new("核对结果未能保存；上次结果已保留", 503, 0).into());
        }
    }
    status(state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_validation_matches_python() {
        let ok = json!({"active": true, "expiresAt": 1900000000,
            "trainingStepsLeft": {"fixedTrainingStepsLeft": 1000, "purchasedTrainingSteps": 100}});
        assert_eq!(
            balance(&ok).unwrap(),
            json!({"fixed": 1000, "purchased": 100, "expires_at": 1900000000})
        );
        // 没有 expiresAt → null；浮点保持浮点
        let f = json!({"active": true, "trainingStepsLeft": {"fixedTrainingStepsLeft": 1.5, "purchasedTrainingSteps": 0}});
        assert_eq!(
            balance(&f).unwrap(),
            json!({"fixed": 1.5, "purchased": 0, "expires_at": null})
        );
        for bad in [
            json!(null),
            json!(-1),
            json!(true),
            json!("100"),
            json!(1e13),
            json!([1]),
        ] {
            let d = json!({"active": true, "trainingStepsLeft": {"fixedTrainingStepsLeft": bad, "purchasedTrainingSteps": 0}});
            assert!(balance(&d).is_none(), "{bad}");
        }
        // active 必须恰好是 true
        let inactive = json!({"active": 1, "trainingStepsLeft": {"fixedTrainingStepsLeft": 1, "purchasedTrainingSteps": 0}});
        assert!(balance(&inactive).is_none());
        assert!(balance(&json!([])).is_none());
    }

    fn snap(fixed: i64, expires: i64, anlas: Value, unconfirmed: Value) -> Value {
        json!({"checked_at": 100.5,
            "accounts": {"token-a": {"fixed": fixed, "purchased": 100, "expires_at": expires}},
            "ledger": {"anlas": anlas, "unconfirmed_anlas": unconfirmed, "unconfirmed_requests": 0, "last_log_id": 5},
            "balance": fixed + 100})
    }

    #[test]
    fn comparison_cases_match_python() {
        let first = snap(1000, 1900000000, json!(0), json!(0));
        assert_eq!(
            comparison(None, &first).unwrap(),
            json!({"reason": "first"})
        );
        // 余额减少 10、账本记 7、未确认 3：与旧版测试一致（int 保持 int）
        let mut next = snap(990, 1900000000, json!(7.0), json!(3.0));
        next["ledger"]["unconfirmed_requests"] = json!(1);
        let c = comparison(Some(&first), &next).unwrap();
        assert_eq!(
            c,
            json!({"reason": null, "since": 100.5, "balance_decrease": 10, "recorded": 7.0,
                   "difference": 3.0, "unconfirmed_anlas": 3.0, "unconfirmed_requests": 1})
        );
        // 序列化时 int 就是 int、float 就是 float
        let text = String::from_utf8(gate_core::pyjson::wire(&c)).unwrap();
        assert!(
            text.contains("\"balance_decrease\":10,") && text.contains("\"recorded\":7.0,"),
            "{text}"
        );
        // 不连续 → 重新记基线
        let cycle = snap(990, 1900086400, json!(0), json!(0));
        assert_eq!(
            comparison(Some(&first), &cycle).unwrap(),
            json!({"reason": "cycle_changed"})
        );
        let credit = snap(1200, 1900000000, json!(0), json!(0));
        assert_eq!(
            comparison(Some(&first), &credit).unwrap(),
            json!({"reason": "balance_increased"})
        );
        let ledger = snap(990, 1900000000, json!(-1), json!(0));
        assert_eq!(
            comparison(Some(&snap(990, 1900000000, json!(5), json!(0))), &ledger).unwrap(),
            json!({"reason": "ledger_reset"})
        );
        let mut other = snap(990, 1900000000, json!(0), json!(0));
        other["accounts"] =
            json!({"token-b": {"fixed": 1, "purchased": 1, "expires_at": 1900000000}});
        assert_eq!(
            comparison(Some(&first), &other).unwrap(),
            json!({"reason": "accounts_changed"})
        );
    }

    #[test]
    fn rate_limit_delay_like_python() {
        assert_eq!(retry_delay("600", 0.0), 600);
        assert_eq!(retry_delay("5", 0.0), 300);
        assert_eq!(retry_delay("999999", 0.0), 86_400);
        assert_eq!(retry_delay("invalid", 0.0), 300);
        assert_eq!(retry_delay("", 0.0), 300);
        // HTTP 日期：2100 年远超一天 → 封顶
        assert_eq!(retry_delay("Fri, 01 Jan 2100 00:00:00 GMT", 0.0), 86_400);
        // 日期在 10 分钟后
        assert_eq!(retry_delay("Thu, 01 Jan 1970 00:10:00 GMT", 0.0), 600);
    }
}
