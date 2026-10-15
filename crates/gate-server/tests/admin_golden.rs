//! 后台「成员 / 记录」接口与旧 Python 的路由级对拍。
//!
//! 夹具由 `tools/parity/golden/admin_routes.py` 在旧网关上生成：同一份合成种子（`admin_routes_seed*.sql`）、
//! 同一个冻结时间，按顺序发同样的请求，记录每一步的状态码、响应原文、下载响应头、zip 内容、（假）注册服务收到的调用，
//! 以及写操作之后关键表的内容。这里用同样的种子和时间重放，逐步比较——响应原文逐字节一致（含 JSON 键顺序、浮点写法）。
//!
//! 时间是进程级冻结的，所以这个文件里只放这一个测试（独占一个测试进程）。
//! 不比的东西：`api_keys.anlas_auto` / `v5_pinned` 两列（3.0 已删除功能，见 docs/v3-deviations.md）、
//! 客户端 / 设备识别的痕迹（用量日志每行的 `client`、成员页的 `last_client`、导出 zip 里的 `user_agent` /
//! `client_kinds` 两列：docs/v3-deviations.md #50）、
//! 新建 Key 的随机串（统一成 nai-<rand>）、缩略图字节（编码器不同，只比是不是 JPEG）。

mod admin_util;
mod common;
use admin_util::{ADMIN_PASSWORD, MockDiscord, registration_env};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request};
use common::setup;
use gate_db::Db;
use gate_server::AppState;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use tower::ServiceExt;

const M: &str = env!("CARGO_MANIFEST_DIR");

fn fixture_lines() -> Vec<Value> {
    let path = Path::new(M).join("tests/fixtures/admin_routes.jsonl.gz");
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("读夹具"))
        .read_to_string(&mut text)
        .expect("解压夹具");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("夹具每行是 JSON"))
        .collect()
}

fn seed_sql(variant: &str) -> String {
    let name = if variant == "main" || variant == "reg" {
        "admin_routes_seed.sql".to_string()
    } else {
        format!("admin_routes_seed_{variant}.sql")
    };
    std::fs::read_to_string(Path::new(M).join("tests/fixtures").join(name)).expect("读种子")
}

/// 新建 Key 的随机串统一成 nai-<rand>（`nai-` + 32 位 urlsafe 字符，后面不再连着 urlsafe 字符）。
fn norm_text(s: &str) -> String {
    let is_url = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i..].starts_with(&['n', 'a', 'i', '-']) {
            let run = chars[i + 4..].iter().take_while(|c| is_url(**c)).count();
            if run == 32 {
                out.push_str("nai-<rand>");
                i += 4 + run;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ------------------------------------------------------------------ 重放
async fn send(state: &AppState, step: &Value, cookie: &str) -> (u16, HeaderMap, Vec<u8>) {
    let method = step["method"].as_str().unwrap();
    let path = step["path"].as_str().unwrap();
    // 步骤里的头覆盖默认头（Python 端是 {**默认, **步骤}）
    let mut hdrs: Vec<(String, String)> = vec![
        ("host".into(), "owl.example".into()),
        ("origin".into(), "https://owl.example".into()),
    ];
    if !step["anon"].as_bool().unwrap_or(false) {
        hdrs.push(("cookie".into(), format!("nai_gate_admin={cookie}")));
    }
    if let Some(h) = step["headers"].as_object() {
        for (k, v) in h {
            hdrs.retain(|(x, _)| x != k);
            hdrs.push((k.clone(), v.as_str().unwrap().to_string()));
        }
    }
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in &hdrs {
        b = b.header(k.as_str(), v.as_str());
    }
    let (body, ctype): (Vec<u8>, Option<&str>) = if !step["json"].is_null() {
        (
            serde_json::to_vec(&step["json"]).unwrap(),
            Some("application/json"),
        )
    } else if let Some(raw) = step["raw"].as_str() {
        (raw.as_bytes().to_vec(), None)
    } else {
        (Vec::new(), None)
    };
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    if method != "GET" {
        b = b.header("content-length", body.len().to_string());
    }
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 123))));
    let resp = gate_server::app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

const SNAP_TABLES: [&str; 11] = [
    "api_keys",
    "key_tags",
    "discord_registrations",
    "discord_bans",
    "coupons",
    "admin_actions",
    "share_state",
    "site_settings",
    "daily_quota_offsets",
    "counters",
    "key_donations",
];
const ERRORS_TABLE: &str = "error_events";

async fn snapshot(db: &Db, errors: bool) -> Value {
    let mut out = serde_json::Map::new();
    let tables = SNAP_TABLES
        .iter()
        .copied()
        .chain(errors.then_some(ERRORS_TABLE));
    for t in tables {
        let rows = db
            .execute_fetchall_compat(&format!("SELECT * FROM {t} ORDER BY rowid"), &[])
            .await
            .unwrap();
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|mut r| {
                if t == "api_keys"
                    && let Some(o) = r.as_object_mut()
                {
                    o.shift_remove("anlas_auto");
                    o.shift_remove("v5_pinned");
                    // 3.0 新增：停用原因（docs/v3-deviations.md #47）
                    o.shift_remove("disabled_reason");
                }
                // 3.0 新增的列（docs/v3-deviations.md #45）：旧版没有，比较时去掉
                if t == ERRORS_TABLE
                    && let Some(o) = r.as_object_mut()
                {
                    o.shift_remove("auto_resolved");
                }
                r
            })
            .collect();
        out.insert(t.to_string(), Value::Array(rows));
    }
    let text = norm_text(&serde_json::to_string(&Value::Object(out)).unwrap());
    serde_json::from_str(&text).unwrap()
}

fn zip_view(blob: &[u8]) -> Value {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(blob)).expect("是 zip");
    let mut files = serde_json::Map::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let name = f.name().map(|n| n.to_string()).unwrap_or_default();
        let mut data = Vec::new();
        f.read_to_end(&mut data).unwrap();
        let v = if name.ends_with(".csv") || name.ends_with(".txt") {
            Value::String(norm_text(&String::from_utf8(data).unwrap()))
        } else {
            json!({"sha256": hex::encode(Sha256::digest(&data)), "len": data.len()})
        };
        files.insert(name, v);
    }
    Value::Object(files)
}

/// `/admin/api/errors` 的每一行在 3.0 多了 `auto_resolved`（自动处理的原因码，docs/v3-deviations.md #45）：
/// 旧版没有这个字段，比较时从新版结果里去掉；与成员页一样经 serde 重新序列化后比结构、键顺序和数值类型。
fn drop_rust_added_error_fields(text: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if let Some(rows) = v.get_mut("errors").and_then(Value::as_array_mut) {
        for r in rows {
            if let Some(o) = r.as_object_mut() {
                o.shift_remove("auto_resolved");
            }
        }
    }
    v.to_string()
}

/// 旧版成员页里 3.0 已删除功能的字段（自动分配 Anlas、V5 基础值、最近一次的客户端）：新版不再返回，比较时从旧版结果里去掉。
fn drop_deleted_member_fields(text: &str) -> String {
    let mut v: Value = serde_json::from_str(text).expect("旧版成员页是 JSON");
    if let Some(members) = v["members"].as_array_mut() {
        for m in members {
            if let Some(o) = m.as_object_mut() {
                // last_client：3.0 删除了客户端 / 设备识别（docs/v3-deviations.md #50）
                for k in ["anlas_auto", "anlas_mode", "v5_pinned", "last_client"] {
                    o.shift_remove(k);
                }
            }
        }
    }
    v.to_string()
}

/// 旧版用量日志列表每行的 `client`（UA 摘要）：3.0 不再记录也不再返回（docs/v3-deviations.md #50），
/// 比较时从旧版结果里去掉；与成员页一样经 serde 重新序列化后比结构、键顺序和数值类型。
fn drop_legacy_log_client(text: &str) -> String {
    let mut v: Value = serde_json::from_str(text).expect("旧版日志列表是 JSON");
    if let Some(rows) = v.get_mut("logs").and_then(Value::as_array_mut) {
        for r in rows {
            if let Some(o) = r.as_object_mut() {
                o.shift_remove("client");
            }
        }
    }
    v.to_string()
}

/// 从成员页每一行里去掉某个字段（只在有意差异的场景用）。
fn drop_member_field(text: &str, field: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if let Some(members) = v["members"].as_array_mut() {
        for m in members {
            if let Some(o) = m.as_object_mut() {
                o.shift_remove(field);
            }
        }
    }
    v.to_string()
}

fn first_diff(a: &str, b: &str) -> String {
    let n = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    let cut = |s: &str| -> String { s.chars().skip(n.saturating_sub(40)).take(120).collect() };
    format!(
        "第 {n} 个字符起不同\n  旧版: …{}\n  新版: …{}",
        cut(a),
        cut(b)
    )
}

async fn run_variant(lines: &[Value], variant: &str, diffs: &mut Vec<String>) -> usize {
    let start = lines
        .iter()
        .position(|l| l["kind"] == "meta" && l["variant"] == variant)
        .unwrap_or_else(|| panic!("夹具里没有场景 {variant}"));
    let now = lines[start]["now"].as_f64().unwrap();
    gate_server::time::freeze(Some(now));
    // 有注册服务的场景：真实的 RegistrationService，Discord REST 指向本机假服务
    let discord = if variant == "reg" {
        Some(MockDiscord::start().await)
    } else {
        None
    };
    let vars = discord
        .as_ref()
        .map(|d| registration_env(&d.url()))
        .unwrap_or_default();
    let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let env = setup(&refs).await;
    let state = env.state.clone();
    let sql = seed_sql(variant);
    state
        .db
        .write(move |tx| tx.execute_batch(&sql))
        .await
        .expect("灌种子");
    // 登录（旧版生成夹具时也登录过一次：会留下一条「登录后台」的操作记录）
    let (status, headers, _) = send(
        &state,
        &json!({"method": "POST", "path": "/admin/api/login", "json": {"password": ADMIN_PASSWORD},
                "anon": true, "headers": {}}),
        "",
    )
    .await;
    assert_eq!(status, 200);
    let set = headers["set-cookie"].to_str().unwrap().to_string();
    let cookie = set
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("nai_gate_admin=")
        .to_string();
    let mut steps = 0;
    for l in &lines[start + 1..] {
        match l["kind"].as_str().unwrap() {
            "meta" => break,
            "final" => {
                let got = snapshot(&state.db, true).await;
                compare_snap(&format!("{variant}: 最终数据库"), &l["db"], &got, diffs);
                break;
            }
            _ => {}
        }
        steps += 1;
        let name = format!("{variant}: {}", l["name"].as_str().unwrap());
        if let Some(sql) = l["sql"].as_str() {
            let sql = sql.to_string();
            state
                .db
                .write(move |tx| tx.execute_batch(&sql))
                .await
                .expect("步骤前的 SQL");
        }
        if let Some(at) = l["at"].as_f64() {
            gate_server::time::freeze(Some(at));
        }
        if let Some(p) = l["path"]
            .as_str()
            .filter(|p| p.starts_with(REMOVED_CLIENT_EXPORT))
        {
            if l["status"] == 200 {
                // 旧版：days 缺省 30，0 当 90，夹到 1..=90；操作者是后台登录的打码 IP
                let days = p
                    .split_once("days=")
                    .and_then(|(_, d)| d.split('&').next()?.parse::<i64>().ok())
                    .unwrap_or(30);
                let days = if days == 0 { 90 } else { days.clamp(1, 90) };
                state
                    .db
                    .add_admin_action(
                        "后台 *.*.0.1",
                        "导出客户端日志",
                        &format!("{days}天"),
                        "",
                        true,
                        state.now(),
                    )
                    .await
                    .unwrap();
            }
            continue;
        }
        if let Some(d) = &discord {
            d.set_closed(l["registrar"]["dm"] == "closed");
            d.clear();
        }
        let (status, headers, body) = send(&state, l, &cookie).await;
        let mut bad = |what: &str, want: String, got: String| {
            diffs.push(format!("[{name}] {what}\n{}", first_diff(&want, &got)));
        };
        if u64::from(status) != l["status"].as_u64().unwrap() {
            bad("状态码", l["status"].to_string(), status.to_string());
        }
        let ctype = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if l["internal"] != json!(true) {
            if l["ctype"].as_str() != Some(ctype.as_str()) {
                bad("Content-Type", l["ctype"].to_string(), ctype.clone());
            }
            let disp = headers
                .get("content-disposition")
                .and_then(|v| v.to_str().ok());
            if l["disposition"].as_str() != disp {
                bad(
                    "Content-Disposition",
                    l["disposition"].to_string(),
                    format!("{disp:?}"),
                );
            }
        }
        if let Some(want) = l["text"].as_str() {
            let got = norm_text(&String::from_utf8_lossy(&body));
            let is_errors_list = l["path"]
                .as_str()
                .is_some_and(|p| p == "/admin/api/errors" || p.starts_with("/admin/api/errors?"));
            let is_logs_list = l["path"]
                .as_str()
                .is_some_and(|p| p == "/admin/api/logs" || p.starts_with("/admin/api/logs?"));
            let want = if l["path"] == "/admin/api/members" {
                drop_deleted_member_fields(want)
            } else if is_logs_list {
                drop_legacy_log_client(want)
            } else if is_errors_list {
                serde_json::from_str::<Value>(want).map_or(want.to_string(), |v| v.to_string())
            } else {
                want.to_string()
            };
            // 「early」场景是凌晨 0–7 点：旧版把「今日重置」的起点算成当天 7 点（在未来），一律返回 null；
            // 3.0 按当前服务日起算（docs/v3-deviations.md #46），这一个字段两边都不比。
            let (want, got) = if variant == "early" && l["path"] == "/admin/api/members" {
                (
                    drop_member_field(&want, "reset_today"),
                    drop_member_field(&got, "reset_today"),
                )
            } else {
                (want, got)
            };
            let got = if l["path"] == "/admin/api/members" || is_logs_list {
                // 与上面同样经 serde 重新序列化，只比结构、键顺序和数值类型
                serde_json::from_str::<Value>(&got).map_or(got, |v| v.to_string())
            } else if is_errors_list {
                drop_rust_added_error_fields(&got)
            } else {
                got
            };
            if got != want {
                bad("响应原文", want, got);
            }
        } else if l["zip"].is_object() {
            let got = zip_view(&body);
            if got != l["zip"] {
                bad("zip 内容", l["zip"].to_string(), got.to_string());
            }
        } else if l["image"].is_object() {
            let got = if body.starts_with(&[0xff, 0xd8]) && body.len() >= 40 {
                json!({"jpeg": true})
            } else {
                json!({"sha256": hex::encode(Sha256::digest(&body)), "len": body.len()})
            };
            if got != l["image"] {
                bad("图片", l["image"].to_string(), got.to_string());
            }
        }
        if let Some(d) = &discord {
            let got = Value::Array(d.calls());
            let got: Value = serde_json::from_str(&norm_text(&got.to_string())).unwrap();
            // 私信里的欢迎语在 3.0 改过几句（docs/v3-deviations.md #48）
            let want: Value =
                serde_json::from_str(&common::v3_welcome(&l["calls"].to_string())).unwrap();
            if got != want {
                bad(
                    "发给 Discord 的请求",
                    l["calls"].to_string(),
                    got.to_string(),
                );
            }
        }
        if l["db"].is_object() {
            let got = snapshot(&state.db, l["db"].get(ERRORS_TABLE).is_some()).await;
            compare_snap(&name, &l["db"], &got, diffs);
        }
    }
    steps
}

/// 3.0 删了「客户端日志导出」（docs/v3-deviations.md #53）：夹具里的这几步不发请求，
/// 只按旧版的写法补记成功导出时的那条操作日志，后面的操作日志编号、列表才能对上。
const REMOVED_CLIENT_EXPORT: &str = "/admin/api/logs/export";

fn compare_snap(name: &str, want: &Value, got: &Value, diffs: &mut Vec<String>) {
    for t in SNAP_TABLES.iter().copied().chain([ERRORS_TABLE]) {
        if want.get(t).is_none() {
            continue;
        }
        // 按序列化文本比：键顺序（列顺序）和数字类型（整数 / 浮点）都要一致
        let (w, g) = (want[t].to_string(), got[t].to_string());
        if w != g {
            diffs.push(format!("[{name}] 表 {t} 内容不同\n{}", first_diff(&w, &g)));
        }
    }
}

#[tokio::test]
async fn admin_routes_match_legacy() {
    let lines = fixture_lines();
    let mut diffs = Vec::new();
    let main = run_variant(&lines, "main", &mut diffs).await;
    let reg = run_variant(&lines, "reg", &mut diffs).await;
    let merge = run_variant(&lines, "merge", &mut diffs).await;
    let early = run_variant(&lines, "early", &mut diffs).await;
    gate_server::time::freeze(None);
    // 夹具没被意外截短
    assert!(
        main >= 220 && reg >= 35 && merge == 1 && early == 2,
        "{main} {reg} {merge} {early}"
    );
    assert!(
        diffs.is_empty(),
        "与旧版不一致 {} 处（只列前 12 处）：\n{}",
        diffs.len(),
        diffs
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
