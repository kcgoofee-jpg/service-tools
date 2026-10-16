//! 后台「运营 / 监控」接口（旧版 `app/admin.py` 的中间一大段）：
//!
//! - [`monitor`]：状态码分布、总览、运行状态、上游表现、V5 额度；
//! - [`upstream`]：上游 Token 增删改、Anlas 核对；
//! - [`account_stats`]：每个上游账号的请求统计与 Anlas 余额推算（只读）；
//! - [`limits`]：预算、排队与冷却、账号保护；
//! - [`onboarding`]：自助领 Key、全局功能、生成记录、首页加入按钮；
//! - [`system`]：站点公告、测试告警、Discord 机器人。
//!
//! 已删除功能的残留接口不移植：`/quota-shadow`、`/shadow`（调度影子模式）、`/anlas-pool`、`/share-guard`
//! （新前端都不再调用）。动态额度参数和定时任务（`quota-algo`、`jobs`）在 `routes/admin/jobs.rs`。

mod account_stats;
mod limits;
mod monitor;
mod onboarding;
mod system;
mod upstream;

use crate::error::HttpError;
use crate::state::AppState;
use axum::Router;
use axum::http::Uri;
use gate_core::{pyc, pyval};
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(monitor::router())
        .merge(upstream::router())
        .merge(account_stats::router())
        .merge(limits::router())
        .merge(onboarding::router())
        .merge(system::router())
}

/// 查询串里的键值对（已解码）；同名参数以最后一个为准（Starlette 的取法）。
fn query_param(uri: &Uri, name: &str) -> Option<String> {
    let pairs = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default();
    pairs
        .into_iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// 旧版 `st.upstream_health()`：最近 10 分钟上游调用成败 + 全站图片冷却剩余秒数。
fn upstream_health(state: &AppState) -> Value {
    let now = state.now();
    let (status, recent, failed) = state.health.status(now);
    json!({
        "status": status,
        "recent": recent,
        "failed": failed,
        "image_cooldown_seconds": state.cooldown.remaining(now),
    })
}

/// 数字字段的类型（`_num` 的 `kind`）。
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Float,
}

/// 旧版 `_num`：严格解析后台数值。空值、布尔、非数字、越界、小数（整数字段）一律 422。
/// 这些字段里 0 代表「不限」，绝不能把空值或非法值静默变成 0，否则一次误操作就会拆掉额度保护。
/// 整数字段返回 JSON 整数，浮点字段返回 JSON 浮点。
fn strict_num(value: &Value, kind: Kind, lo: f64, hi: f64, name: &str) -> Result<Value, HttpError> {
    let empty = match value {
        Value::Null | Value::Bool(_) => true,
        Value::String(s) => pyc::strip(s).is_empty(),
        _ => false,
    };
    if empty {
        return Err(HttpError::new(422, format!("{name} 不能为空")));
    }
    let invalid = || HttpError::new(422, format!("{name} 必须是有效数字"));
    let v = match value {
        Value::Number(n) => n.as_f64().ok_or_else(invalid)?,
        Value::String(s) => pyc::parse_float(s).ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    if !v.is_finite() {
        return Err(invalid());
    }
    if kind == Kind::Int && v.fract() != 0.0 {
        return Err(HttpError::new(422, format!("{name} 必须是整数")));
    }
    if !(lo <= v && v <= hi) {
        let show = |x: f64| {
            if kind == Kind::Int {
                (x as i64).to_string()
            } else {
                pyc::float_repr(x)
            }
        };
        return Err(HttpError::new(
            422,
            format!("{name} 必须在 {}～{} 之间", show(lo), show(hi)),
        ));
    }
    Ok(match kind {
        Kind::Int => Value::from(v as i64),
        Kind::Float => pyval::float(v),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_num_matches_python_messages() {
        let int = |v: Value| strict_num(&v, Kind::Int, 0.0, 100000.0, "n").map_err(|e| e.detail);
        assert_eq!(int(json!(150)).unwrap(), json!(150));
        assert_eq!(int(json!("150")).unwrap(), json!(150));
        assert_eq!(int(json!(12.0)).unwrap(), json!(12));
        for (bad, msg) in [
            (json!(true), "n 不能为空"),
            (json!(null), "n 不能为空"),
            (json!(" "), "n 不能为空"),
            (json!([1]), "n 必须是有效数字"),
            (json!("abc"), "n 必须是有效数字"),
            (json!(1.5), "n 必须是整数"),
            (json!(-1), "n 必须在 0～100000 之间"),
        ] {
            assert_eq!(int(bad).unwrap_err(), json!(msg));
        }
        let float =
            |v: Value| strict_num(&v, Kind::Float, 0.0, 1_000_000.0, "m").map_err(|e| e.detail);
        assert_eq!(float(json!(5000)).unwrap(), json!(5000.0));
        assert_eq!(
            float(json!(2e6)).unwrap_err(),
            json!("m 必须在 0.0～1000000.0 之间")
        );
    }
}
