//! 风险检查（旧版 `app/risk_check.py`，后台只读诊断）：出站请求的样子（请求头、TLS、出口链路）和滥用线索
//! （用量集中度、上游限流信号、当日 V5 消耗）。只读：不写库、不发上游请求、不发消息。
//!
//! 与旧版不同的只有两处（出站指纹在 3.0 换了，旧文案会说谎，见 `docs/v3-deviations.md` #7）：
//! 「出站请求头」里 HTTP/2 一行和状态判断、「TLS 指纹」整条；其余逐字一致。

use super::perf::collect_perf;
use crate::state::AppState;
use gate_core::{pyc, pyval};
use serde_json::{Value, json};

const DAY: f64 = 24.0 * 3600.0;

pub const OK: &str = "ok";
pub const WARN: &str = "warn";
pub const BAD: &str = "bad";
pub const NA: &str = "na";

fn label(status: &str) -> &str {
    match status {
        "ok" => "正常",
        "warn" => "注意",
        "bad" => "风险",
        "na" => "不适用",
        other => other,
    }
}

/// 用量集中度：单把 Key 的成功出图占全站比例达到该值时提示（一把 Key 独吃全站产能，多半是共享 / 转卖）。
const CONCENTRATION_WARN: f64 = 0.5;

fn item(id: &str, title: &str, status: &str, detail: &str, evidence: Vec<String>) -> Value {
    json!({
        "id": id, "title": title, "status": status, "label": label(status),
        "detail": detail, "evidence": evidence,
    })
}

/// 只露后 4 位：上游 Token 是全站最敏感的凭据，后台页面也不必多露。
fn mask_token(token: &str) -> String {
    if pyc::len_chars(token) > 8 {
        format!("…{}", pyc::last_chars(token, 4))
    } else if token.is_empty() {
        String::new()
    } else {
        "…".to_string()
    }
}

/// 代理地址里的账号密码（`scheme://user:pass@host`）打码后再显示。
/// 等价于 `re.sub(r"(://)[^/@\s]+@", r"\1***@", value)`。
fn redact_proxy(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ':' && chars.get(i + 1) == Some(&'/') && chars.get(i + 2) == Some(&'/') {
            let mut j = i + 3;
            while j < chars.len() && !matches!(chars[j], '/' | '@') && !chars[j].is_whitespace() {
                j += 1;
            }
            if j > i + 3 && chars.get(j) == Some(&'@') {
                out.push_str("://***@");
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------- 出站指纹盘点 ----------

/// 如实列出生图请求真正发出去的头（来自 gate-upstream 的 `headers::ordered` + 每把 Token 的指纹档位）。
async fn outbound_headers_item(state: &AppState) -> Value {
    let tokens = state.nai.tokens().await;
    if tokens.is_empty() {
        return item(
            "outbound_headers",
            "出站请求头",
            NA,
            "上游客户端尚未启动，没有 Token。",
            vec![],
        );
    }
    let mut evidence = Vec::new();
    for t in &tokens {
        evidence.push(format!(
            "Token #{}（{}）：",
            t.fields().position,
            mask_token(&t.token)
        ));
        // 旧版列的是 default_browser_headers 字典（不含 Authorization）；Accept-Encoding / Connection 是协议层补的，不列
        for (k, v) in gate_upstream::headers::ordered(&t.profile, None, "application/json", false) {
            if !matches!(k, "Accept-Encoding" | "Connection") {
                evidence.push(format!("  {k}: {v}"));
            }
        }
    }
    evidence.push(format!(
        "HTTP/2：{}",
        if state.env.upstream_http2 {
            "启用"
        } else {
            "关闭"
        }
    ));
    evidence.push(format!(
        "请求后随机间隔：{}~{} 秒；连续 3 次 403 冷却 5 分钟",
        pyc::float_repr(state.env.post_request_jitter_min),
        pyc::float_repr(state.env.post_request_jitter_max)
    ));
    item(
        "outbound_headers",
        "出站请求头",
        OK,
        "所有上游请求使用同一套 Chrome 请求头。",
        evidence,
    )
}

fn tls_fingerprint_item() -> Value {
    item(
        "tls_fingerprint",
        "TLS 指纹（JA3/JA4）",
        OK,
        "TLS / HTTP2 握手由 wreq 模拟 Chrome（129 / 128，随 Token 档位），与请求头一致。",
        vec![],
    )
}

fn egress_ip_item() -> Value {
    let active: Vec<String> = [
        "UPSTREAM_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ]
    .iter()
    .filter_map(|name| {
        let v = std::env::var(name).unwrap_or_default();
        (!v.is_empty()).then(|| format!("{name}={}", redact_proxy(&v)))
    })
    .collect();
    let mut evidence = vec![if active.is_empty() {
        "未配置代理：出站即服务器本机 IP，所有上游 Token 共用同一出口。".to_string()
    } else {
        "检测到代理环境变量：".to_string()
    }];
    evidence.extend(active);
    item(
        "proxy_config",
        "代理 / 出口链路",
        OK,
        "出站链路。",
        evidence,
    )
}

// ---------- 上游行为信号 ----------

async fn upstream_signals_item(state: &AppState, now: f64) -> Value {
    let title = "上游风控信号（429 / 401 / 成功率）";
    let data = match collect_perf(state, now).await {
        Ok(d) => d,
        Err(e) => {
            return item(
                "upstream_signals",
                title,
                NA,
                &format!("统计暂不可用：{e}"),
                vec![],
            );
        }
    };
    let status = if data.flags.iter().any(|f| f.level == "bad") {
        BAD
    } else if !data.flags.is_empty() {
        WARN
    } else {
        OK
    };
    let empty = data.families.0.is_empty();
    item(
        "upstream_signals",
        title,
        if empty && status == OK { NA } else { status },
        "近 1 小时 / 24 小时与 7 天基线对比。",
        data.flags.iter().map(|f| f.text.clone()).collect(),
    )
}

async fn concentration_item(state: &AppState, now: f64) -> Value {
    let title = "用量集中度（24 小时）";
    let (rows, total) = match state.db.usage_concentration(now - DAY).await {
        Ok(v) => v,
        Err(e) => {
            return item(
                "concentration",
                title,
                NA,
                &format!("读取失败：{e}"),
                vec![],
            );
        }
    };
    if rows.is_empty() || total == 0 {
        return item(
            "concentration",
            title,
            OK,
            "近 24 小时没有出图请求。",
            vec![],
        );
    }
    let evidence: Vec<String> = rows
        .iter()
        .map(|(kid, name, c)| {
            let who = if name.is_empty() {
                format!("#{}", kid.map_or("None".to_string(), |k| k.to_string()))
            } else {
                name.clone()
            };
            format!(
                "{who}：{c} 张（占全站 {}%）",
                pyc::round0(*c as f64 / total as f64 * 100.0)
            )
        })
        .collect();
    let top_share = rows[0].2 as f64 / total as f64;
    let status = if top_share >= CONCENTRATION_WARN {
        WARN
    } else {
        OK
    };
    item(
        "concentration",
        title,
        status,
        &format!(
            "单把 Key 占比 ≥ {}% 时提示。",
            pyc::round0(CONCENTRATION_WARN * 100.0)
        ),
        evidence,
    )
}

async fn burn_today_item(state: &AppState, now: f64) -> Value {
    let title = "今日上游额度消耗（V5）";
    let tokens = state.nai.tokens().await;
    if tokens.is_empty() {
        return item("burn_today", title, NA, "令牌池为空。", vec![]);
    }
    let day = state.clock.day(now);
    let mut evidence = Vec::new();
    let mut total_v5 = 0_i64;
    for t in &tokens {
        let counter = match state.db.get_upstream_counter(&t.token_id, &day).await {
            Ok(c) => c,
            Err(e) => return item("burn_today", title, NA, &format!("读取失败：{e}"), vec![]),
        };
        total_v5 += counter.v5;
        let f = t.fields();
        let limit = if f.v5_daily_limit != 0 {
            format!("（本账号日限 {}）", f.v5_daily_limit)
        } else {
            String::new()
        };
        evidence.push(format!(
            "Token #{}：今日图片 {} 张，V5 {} 张{limit}",
            f.position, counter.images, counter.v5
        ));
    }
    // 优先用额度算法当前给的全站 V5 日额度（动态），没有再退回环境变量的静态值
    let from_algo = state
        .db
        .get_setting("quota_algo_last")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .and_then(|s| gate_core::jsondump::loads(&s))
        .and_then(|v| {
            pyval::get(&v, "v5")
                .filter(|x| pyval::truthy(x))
                .and_then(|x| pyval::get(x, "global"))
                .and_then(pyval::number_like)
        })
        .map_or(0, |f| f.trunc() as i64);
    // 没有额度算法的结果时用站点设置里的全站 V5（2.x 退回环境变量 GLOBAL_DAILY_V5，已删除，#70）
    let cap = if from_algo != 0 {
        from_algo
    } else {
        match crate::ops::algo_owned(&state.db).await {
            Ok(o) => o.global_daily_v5,
            Err(e) => return item("burn_today", title, NA, &format!("读取失败：{e}"), vec![]),
        }
    };
    let status = if cap > 0 {
        let ratio = total_v5 as f64 / cap as f64;
        evidence.push(format!(
            "全站今日 V5 合计 {total_v5} / 日额度 {cap}（{}%）",
            pyc::round0(ratio * 100.0)
        ));
        if ratio >= 1.0 {
            BAD
        } else if ratio >= 0.8 {
            WARN
        } else {
            OK
        }
    } else {
        evidence.push(format!("全站今日 V5 合计 {total_v5}（未设置全站日额度）"));
        OK
    };
    item(
        "burn_today",
        title,
        status,
        "今天各账号的出图和 V5 用量。",
        evidence,
    )
}

// ---------- 汇总 ----------

/// 返回 `{items, summary, generated_at}`；全部只读。
pub async fn collect(state: &AppState, now: f64) -> Value {
    let items = vec![
        outbound_headers_item(state).await,
        tls_fingerprint_item(),
        egress_ip_item(),
        upstream_signals_item(state, now).await,
        concentration_item(state, now).await,
        burn_today_item(state, now).await,
    ];
    let count = |s: &str| {
        items
            .iter()
            .filter(|it| it.get("status").and_then(Value::as_str) == Some(s))
            .count()
    };
    let (ok, warn, bad, na) = (count(OK), count(WARN), count(BAD), count(NA));
    let overall = if bad > 0 {
        BAD
    } else if warn > 0 {
        WARN
    } else {
        OK
    };
    json!({
        "items": items,
        "summary": {"ok": ok, "warn": warn, "bad": bad, "na": na, "risk": overall},
        "generated_at": pyval::float(now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_are_redacted() {
        assert_eq!(
            redact_proxy("http://user:pass@10.0.0.1:8080"),
            "http://***@10.0.0.1:8080"
        );
        assert_eq!(
            redact_proxy("socks5://10.0.0.1:1080"),
            "socks5://10.0.0.1:1080"
        );
        // 没有用户信息、@ 出现在路径里：不动
        assert_eq!(redact_proxy("http://h/p@q"), "http://h/p@q");
        assert_eq!(redact_proxy("a://x@b://y@c"), "a://***@b://***@c");
        assert_eq!(redact_proxy("http://@h"), "http://@h");
    }

    #[test]
    fn token_mask_keeps_last_four() {
        assert_eq!(mask_token("pst-abcdefgh1234"), "…1234");
        assert_eq!(mask_token("short"), "…");
        assert_eq!(mask_token(""), "");
    }
}
