//! 后台「上游」页的每账号统计：最近 1 小时 / 24 小时的请求成败与等待、上游错误码、今日张数、
//! 每小时计数（来自账号保护）、负载与余额占比，以及由对账快照推算的 Anlas 余额与续费倒计时。
//! 只读，不向上游发任何请求；成员接口看不到这里的任何数据。

use super::upstream::pool_status;
use crate::error::{ApiResult, ok_json};
use crate::session::AdminSession;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use gate_core::anlas_estimate::{self, Projection, SnapshotAccount, Usage};
use gate_core::pyval;
use gate_db::repo::account_stats::AccountLogRow;
use serde_json::{Value, json};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/admin/api/upstream-stats", get(upstream_stats))
}

const HOUR: f64 = 3600.0;
const DAY: f64 = 86400.0;

/// 一个时间窗内某个账号的请求统计。
fn window_stats(rows: &[&AccountLogRow]) -> Value {
    let count = |s: &str| rows.iter().filter(|r| r.status == s).count() as i64;
    let up = |code: i64| rows.iter().filter(|r| r.up_status == code).count() as i64;
    let mut waits: Vec<i64> = rows
        .iter()
        .filter(|r| r.status == "ok")
        .map(|r| r.wait_ms)
        .collect();
    waits.sort_unstable();
    let avg = (!waits.is_empty()).then(|| waits.iter().sum::<i64>() / waits.len() as i64);
    // 最近排名法：第 ceil(0.9 n) 小的
    let p90 = (!waits.is_empty()).then(|| waits[((waits.len() * 9).div_ceil(10)).max(1) - 1]);
    json!({
        "ok": count("ok"),
        "error": count("error"),
        "rejected": count("rejected"),
        "avg_wait_ms": avg,
        "p90_wait_ms": p90,
        "up_429": up(429),
        "up_403": up(403),
        "up_401": up(401),
    })
}

fn pct(part: f64, total: f64) -> Option<i64> {
    (total > 0.0).then(|| (part / total * 100.0).round() as i64)
}

fn estimate_json(e: &anlas_estimate::Estimate, unattributed: f64) -> Value {
    let (kind, value) = match e.projection {
        Projection::Unknown => ("unknown", Value::Null),
        Projection::RunsOut { days } => ("runs_out", json!(days)),
        Projection::Waste { anlas } => ("waste", json!(anlas)),
    };
    json!({
        "cycle_rolled": e.cycle_rolled,
        "balance": e.balance,
        "fixed": e.fixed,
        "purchased": e.purchased,
        "expires_at": e.expires_at,
        "days_left": e.days_left,
        "avg_daily": e.avg_daily,
        "projection": kind,
        "projection_value": value,
        "snapshot_age_days": e.snapshot_age_days,
        "stale": e.stale,
        "approximate": e.approximate,
        "unattributed_anlas": unattributed,
    })
}

async fn upstream_stats(State(state): State<AppState>, _s: AdminSession) -> ApiResult {
    let now = state.now();
    let pool = pool_status(&state).await?;
    let ids: Vec<String> = pool
        .iter()
        .map(|p| p["token_id"].as_str().unwrap_or_default().to_string())
        .collect();
    let single = ids.len() == 1;

    let rows_24h = state.db.account_log_rows(now - DAY).await?;
    let ok_24h_total = rows_24h
        .iter()
        .filter(|r| r.token_id.is_some() && r.status == "ok")
        .count() as f64;
    let unattributed_24h = rows_24h.iter().filter(|r| r.token_id.is_none()).count();

    // 最近一次真实核对：每个账号取最新一份含它的快照
    let history = state.db.reconciliation_history(20).await?;
    let used_3d = state.db.anlas_by_token(0, now - 3.0 * DAY).await?;
    let sum_of = |rows: &[(Option<String>, f64)], id: Option<&str>| -> f64 {
        rows.iter()
            .filter(|(t, _)| t.as_deref() == id)
            .map(|(_, a)| *a)
            .sum()
    };
    let untagged_3d = sum_of(&used_3d, None);

    let mut accounts: Vec<Value> = Vec::new();
    let mut balances: Vec<Option<f64>> = Vec::new();
    for (i, p) in pool.iter().enumerate() {
        let id = ids[i].as_str();
        let all_24h: Vec<&AccountLogRow> = rows_24h
            .iter()
            .filter(|r| r.token_id.as_deref() == Some(id))
            .collect();
        let last_1h: Vec<&AccountLogRow> = all_24h
            .iter()
            .copied()
            .filter(|r| r.ts > now - HOUR)
            .collect();
        let ok_24h = all_24h.iter().filter(|r| r.status == "ok").count() as f64;
        let hour_count = state.guard.with(|g| g.hour_count(id, now));

        let snapshot = history.iter().find_map(|h| {
            let a = h.get("accounts")?.get(id)?;
            let acct = SnapshotAccount {
                checked_at: h.get("checked_at").and_then(Value::as_f64)?,
                fixed: a.get("fixed").and_then(Value::as_f64)?,
                purchased: a.get("purchased").and_then(Value::as_f64)?,
                expires_at: a.get("expires_at").and_then(Value::as_f64),
            };
            let last_id = h
                .get("ledger")
                .and_then(|l| l.get("last_log_id"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            Some((acct, last_id))
        });
        let (balance, est) = match snapshot {
            None => (Value::Null, None),
            Some((acct, last_id)) => {
                let after = state.db.anlas_by_token(last_id, 0.0).await?;
                let untagged_after = sum_of(&after, None);
                let tagged_after = sum_of(&after, Some(id));
                let tagged_3d = sum_of(&used_3d, Some(id));
                let usage = Usage {
                    since_snapshot: tagged_after + if single { untagged_after } else { 0.0 },
                    last_3d: tagged_3d + if single { untagged_3d } else { 0.0 },
                    approximate: !single && (untagged_after > 0.0 || untagged_3d > 0.0),
                };
                let e = anlas_estimate::estimate(now, acct, usage);
                let un = if single { 0.0 } else { untagged_after };
                (estimate_json(&e, un), Some(e))
            }
        };
        balances.push(est.as_ref().and_then(|e| e.balance));
        accounts.push(json!({
            "token_id": id,
            "position": p["position"],
            "usable": p["usable"],
            "images_today": p["images_today"],
            "hour_count": hour_count,
            "w1h": window_stats(&last_1h),
            "w24h": window_stats(&all_24h),
            "load_share_24h": pct(ok_24h, ok_24h_total),
            "balance": balance,
        }));
    }
    // 余额占比：只在每个账号都有可用推算时才给
    let total_balance: f64 = balances.iter().flatten().sum();
    let all_known = !balances.is_empty() && balances.iter().all(Option::is_some);
    for (a, b) in accounts.iter_mut().zip(&balances) {
        a["balance_share"] = match (all_known, b) {
            (true, Some(b)) => pct(*b, total_balance).map_or(Value::Null, |v| json!(v)),
            _ => Value::Null,
        };
    }
    Ok(ok_json(&json!({
        "generated_at": pyval::float(now),
        "accounts": accounts,
        "unattributed_24h": unattributed_24h,
    })))
}
