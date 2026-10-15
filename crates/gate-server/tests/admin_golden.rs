//! 后台「成员 / 记录」接口与旧 Python 的路由级对拍。
//!
//! 夹具由旧网关生成（脚本见标签 py-final）：同一份合成种子（`admin_routes_seed*.sql`）、
//! 同一个冻结时间，按顺序发同样的请求，记录每一步的状态码、响应原文、下载响应头、zip 内容、（假）注册服务收到的调用，
//! 以及写操作之后关键表的内容。这里用同样的种子和时间重放，逐步比较——响应原文逐字节一致（含 JSON 键顺序、浮点写法）。
//!
//! 时间是进程级冻结的，所以这个文件里只放这一个测试（独占一个测试进程）。
//! 不比的东西：`api_keys.anlas_auto` / `v5_pinned` 两列（3.0 已删除功能，见 docs/v3-deviations.md）、
//! 客户端 / 设备识别的痕迹（用量日志每行的 `client`、成员页的 `last_client`、导出 zip 里的 `user_agent` /
//! `client_kinds` 两列：docs/v3-deviations.md #50）、
//! 文本生成 / 语音的痕迹（Key 上的 `daily_text_tokens`、今日 / 已用的 `text_tokens`、功能表里的 `text` / `voice`：
//! docs/v3-deviations.md #81；只测它们的两步——`daily_text_tokens` 越界——不发请求，只补记操作日志）、
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
                    // 文本额度不再读写（docs/v3-deviations.md #81）：两边都不比这一列
                    o.shift_remove("daily_text_tokens");
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
                // 今日的文本 token 计数、自动标签「调文本接口」：docs/v3-deviations.md #81
                if let Some(Value::Object(today)) = o.get_mut("today") {
                    today.shift_remove("text_tokens");
                }
                if let Some(Value::Array(tags)) = o.get_mut("auto_tags") {
                    tags.retain(|t| t["tag"] != "调文本接口");
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

// ------------------------------------------------------------------ 文本生成 / 语音（docs/v3-deviations.md #81）
//
// 冻结夹具是旧版的输出：Key 带 `daily_text_tokens`、今日 / 已用带 `text_tokens`、功能表有 `text` / `voice`，
// 功能名校验的提示是「image,upscale,augment,vibe,tags,text,voice」。3.0 都没了，比较时在旧版（期望）一侧改写。

const REMOVED_FEATURES: [&str; 2] = ["text", "voice"];
const LEGACY_FEATURES_HINT: &str = "image,upscale,augment,vibe,tags,text,voice";
const FEATURES_HINT: &str = "image,upscale,augment,vibe,tags";

/// 逗号串（`api_keys.features`）里去掉已删除的功能名。
fn features_csv(s: &str) -> String {
    s.split(',')
        .filter(|n| !REMOVED_FEATURES.contains(n))
        .collect::<Vec<_>>()
        .join(",")
}

/// 旧版的 Key / 成员 JSON 在新版里的样子：去掉 `daily_text_tokens`、`used` / `today` 里的 `text_tokens`，
/// 功能列表（`features`）去掉 `text` / `voice`。
fn drop_removed_text_fields(v: &mut Value) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(drop_removed_text_fields),
        Value::Object(o) => {
            o.shift_remove("daily_text_tokens");
            for k in ["used", "today"] {
                if let Some(Value::Object(inner)) = o.get_mut(k) {
                    inner.shift_remove("text_tokens");
                }
            }
            match o.get_mut("features") {
                Some(Value::Array(a)) => {
                    a.retain(|x| !x.as_str().is_some_and(|n| REMOVED_FEATURES.contains(&n)))
                }
                Some(Value::String(csv)) => *csv = features_csv(csv),
                _ => {}
            }
            o.values_mut().for_each(drop_removed_text_fields);
        }
        _ => {}
    }
}

/// 把旧版响应原文改写成新版的样子（功能名提示、Key / 成员 JSON 里的文本字段）。
fn v3_text(want: &str, path: &str) -> String {
    let want = want.replace(LEGACY_FEATURES_HINT, FEATURES_HINT);
    let key_like = path.starts_with("/admin/api/keys") || path.starts_with("/admin/api/members");
    if !key_like {
        return want;
    }
    match serde_json::from_str::<Value>(&want) {
        Ok(mut v) => {
            drop_removed_text_fields(&mut v);
            v.to_string()
        }
        Err(_) => want,
    }
}

/// 请求体里的 `features` 去掉 `text` / `voice`（旧版接受，3.0 这两个名字已不是功能）。
/// 返回 (改写后的请求体, 旧 / 新两种紧凑写法，用来改写旧版操作日志里记下的请求体)。
fn v3_request(body: &Value) -> (Value, Option<(String, String)>) {
    let mut body = body.clone();
    let Some(Value::Array(list)) = body.get("features").cloned() else {
        return (body, None);
    };
    let kept: Vec<Value> = list
        .iter()
        .filter(|x| !x.as_str().is_some_and(|n| REMOVED_FEATURES.contains(&n)))
        .cloned()
        .collect();
    if kept.len() == list.len() {
        return (body, None);
    }
    let pair = (
        format!("\"features\":{}", Value::Array(list)),
        format!("\"features\":{}", Value::Array(kept.clone())),
    );
    body["features"] = Value::Array(kept);
    (body, Some(pair))
}

/// 新建 Key 没填的字段：旧版取环境变量 `DEFAULT_*`（100 张 / 100 Anlas / 50 张 V5 / 每月 2500 / 每分钟 10 次 / 30 天）；
/// 3.0 删了这些环境变量，改取后台「领 Key」的设置和 `defaults` 里的常量（docs/v3-deviations.md #93 #94）。
/// 重放时把旧默认值明确写进请求，响应和库里的内容才能逐字对上旧版；新默认值另由 admin_members.rs 的
/// `create_key_defaults_come_from_registration_settings` 断言。
/// 返回 (旧版请求体的摘要, 改写后的摘要)，用来改写旧版操作日志里记下的请求体。
fn legacy_new_key_defaults(body: &mut Value) -> Option<(String, String)> {
    let before = gate_server::action_log::summarize(body, 300);
    let obj = body.as_object_mut()?;
    for (k, v) in [
        ("daily_images", json!(100)),
        ("monthly_anlas", json!(2500.0)),
        ("daily_anlas", json!(100.0)),
        ("daily_v5", json!(50)),
        ("rpm", json!(10)),
        ("expires_days", json!(30)),
    ] {
        if !obj.contains_key(k) {
            obj.insert(k.into(), v);
        }
    }
    let after = gate_server::action_log::summarize(body, 300);
    (before != after).then_some((before, after))
}

/// 旧版快照（期望）改写成新版的样子：`api_keys` 去掉 `daily_text_tokens`、功能串去掉 `text` / `voice`，
/// 操作日志里的功能名提示和请求体按 `rewrites` 改。
fn v3_snapshot(want: &Value, rewrites: &[(String, String)]) -> Value {
    let mut out = want.clone();
    if let Some(Value::Array(rows)) = out.get_mut("api_keys") {
        for r in rows {
            if let Some(o) = r.as_object_mut() {
                o.shift_remove("daily_text_tokens");
                if let Some(Value::String(csv)) = o.get_mut("features") {
                    *csv = features_csv(csv);
                }
            }
        }
    }
    if let Some(Value::Array(rows)) = out.get_mut("admin_actions") {
        for r in rows {
            let is_create = r["action"] == "新建 Key";
            if let Some(Value::String(detail)) = r.get_mut("detail") {
                let mut d = detail.replace(LEGACY_FEATURES_HINT, FEATURES_HINT);
                for (old, new) in rewrites {
                    if old.is_empty() {
                        // 旧版什么字段都没填的「新建 Key」：请求体摘要是空串
                        if is_create && d.is_empty() {
                            d = new.clone();
                        }
                    } else {
                        d = d.replace(old, new);
                    }
                }
                *detail = d;
            }
        }
    }
    out
}

/// 3.0 删了成员私信出口（docs/v3-deviations.md #76）：冻结夹具里 reg 场景发给 Discord 的请求全是私信 / 回复消息，
/// 期望一侧把这些请求去掉（新版一个都不发）。
fn drop_dm_calls(calls: &Value) -> Value {
    let kept: Vec<Value> = calls
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| {
            let path = c[1].as_str().unwrap_or("");
            path != "/users/@me/channels" && !path.ends_with("/messages")
        })
        .collect();
    Value::Array(kept)
}

/// 「奖励成员」的响应：旧版新发 Key 时带 `dm_sent`，私信没发出去才带 `dm_block` / `key`，传了 `reply` 才带 `replied`；
/// 3.0 不私信，新发 Key 一律带回 `key`（放在最后），上面三个字段都没有（docs/v3-deviations.md #76）。
fn v3_gift_text(text: &str) -> String {
    let mut v: Value = serde_json::from_str(text).expect("旧版奖励响应是 JSON");
    if let Some(o) = v.as_object_mut() {
        let key = o.shift_remove("key");
        for k in ["dm_sent", "dm_block", "replied"] {
            o.shift_remove(k);
        }
        if o.get("new_key") == Some(&json!(true)) {
            o.insert("key".into(), key.unwrap_or_else(|| json!("nai-<rand>")));
        }
    }
    v.to_string()
}

/// 种子里自带的一条历史「私信成员」记录（旧版留下的老数据，库里原样保留、后台照常显示）。
const SEED_DM_DETAIL: &str = "已私信：你好";

/// 期望一侧的操作日志：去掉旧版跑步骤时新写的「私信成员」记录（种子里的历史行保留；行号顺着往前补）、
/// 「奖励成员」备注里的「已私信」「已回复」。
fn v3_admin_actions(rows: &Value) -> Value {
    let mut out = Vec::new();
    let mut removed = 0;
    for r in rows.as_array().cloned().unwrap_or_default() {
        if r["action"] == "私信成员" && r["detail"] != SEED_DM_DETAIL {
            removed += 1;
            continue;
        }
        let mut r = r;
        let o = r.as_object_mut().expect("操作日志每行是对象");
        if let Some(id) = o.get("id").and_then(Value::as_i64) {
            o.insert("id".into(), json!(id - removed));
        }
        if o.get("action") == Some(&json!("奖励成员")) {
            let detail = o["detail"].as_str().unwrap_or("").to_string();
            o.insert(
                "detail".into(),
                json!(detail.replace(" · 已私信", "").replace(" · 已回复", "")),
            );
        }
        out.push(r);
    }
    Value::Array(out)
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
    // 旧版操作日志里记下的请求体，按 3.0 实际收到的（去掉 text / voice 的功能列表）改写
    let mut rewrites: Vec<(String, String)> = Vec::new();
    for l in &lines[start + 1..] {
        match l["kind"].as_str().unwrap() {
            "meta" => break,
            "final" => {
                let got = snapshot(&state.db, true).await;
                compare_snap(
                    &format!("{variant}: 最终数据库"),
                    &l["db"],
                    &got,
                    &rewrites,
                    diffs,
                );
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
        // 只测「文本额度越界」的两步（docs/v3-deviations.md #81）：3.0 不再校验这个字段，同样不发请求，
        // 只按旧版的写法补记失败的那条操作日志，后面的操作日志编号、列表才能对上。
        if l["status"] == 422 && l["json"].get("daily_text_tokens").is_some() {
            let path = l["path"].as_str().unwrap();
            let (action, target) = if l["method"] == "POST" {
                ("新建 Key", String::new())
            } else {
                let id: i64 = path.rsplit('/').next().unwrap().parse().unwrap();
                let row = state.db.get_key(id).await.unwrap().expect("Key 存在");
                (
                    "修改 Key",
                    format!("Key #{id} {}", row["name"].as_str().unwrap()),
                )
            };
            let why = serde_json::from_str::<Value>(l["text"].as_str().unwrap()).unwrap()["detail"]
                .as_str()
                .unwrap()
                .to_string();
            state
                .db
                .add_admin_action(
                    "后台 *.*.0.1",
                    action,
                    &target,
                    &format!("失败（422）：{why}"),
                    false,
                    state.now(),
                )
                .await
                .unwrap();
            continue;
        }
        if let Some(d) = &discord {
            d.clear();
        }
        // 按已删除的功能筛选用量日志（?feature=text / voice）：3.0 回 422「未知功能」，纯读取、没有副作用，跳过
        if l["method"] == "GET"
            && l["path"].as_str().is_some_and(|p| {
                REMOVED_FEATURES
                    .iter()
                    .any(|f| p.contains(&format!("feature={f}")))
            })
        {
            continue;
        }
        // 功能列表里的 text / voice：3.0 不认这两个名字，请求里去掉再发
        let mut step = l.clone();
        if step["json"].is_object() {
            let (body, rewrite) = v3_request(&l["json"]);
            step["json"] = body;
            rewrites.extend(rewrite);
        }
        if l["method"] == "POST" && l["path"] == "/admin/api/keys" && step["json"].is_object() {
            rewrites.extend(legacy_new_key_defaults(&mut step["json"]));
        }
        // 没配注册服务时的一键邀请 Key：旧版取环境变量默认值（100 张 / 50 张 V5 / 全部模型 / 30 天），
        // 3.0 一律读后台「领 Key」的设置（docs/v3-deviations.md #94）。这一步前临时设成旧默认值，做完立刻删掉
        let legacy_invite = variant == "main"
            && l["method"] == "POST"
            && l["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/keys/invite"));
        // 功能：旧版没配注册服务时不限功能（`register_features` = `*`）；种子里这个设置另有值，做完要还原
        const INVITE_SETTINGS: [(&str, &str); 5] = [
            ("register_daily_images", "100"),
            ("register_daily_v5", "50"),
            ("register_image_scope", "all"),
            ("register_expires_days", "30"),
            ("register_features", "*"),
        ];
        let saved_features = state.db.get_setting("register_features").await.unwrap();
        if legacy_invite {
            for (k, v) in INVITE_SETTINGS {
                state.db.set_setting(k, v).await.unwrap();
            }
        }
        let (status, headers, body) = send(&state, &step, &cookie).await;
        if legacy_invite {
            state
                .db
                .write(|tx| {
                    tx.execute_batch(
                        "DELETE FROM site_settings WHERE key IN \
                         ('register_daily_images','register_daily_v5','register_image_scope','register_expires_days')",
                    )
                })
                .await
                .unwrap();
            match saved_features {
                Some(v) => state.db.set_setting("register_features", &v).await.unwrap(),
                None => state
                    .db
                    .write(|tx| {
                        tx.execute_batch("DELETE FROM site_settings WHERE key='register_features'")
                    })
                    .await
                    .unwrap(),
            }
        }
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
        // /反馈 在 3.0 改成一段文字（docs/v3-deviations.md #52）：冻结的旧版列表带 5 道选择题的 `questions` 和 `answers`、
        // 导出按题目一题一列，响应原文不再相同。这两步照常重放（会留下「导出成员反馈」的操作记录，后面的步骤要用），
        // 但不比响应原文；新格式的断言在 admin_records.rs 的 `feedback_list_and_csv_export`。
        let v3_feedback = l["path"]
            .as_str()
            .is_some_and(|p| p == "/admin/api/feedback" || p == "/admin/api/feedback/export");
        if let Some(want) = l["text"].as_str().filter(|_| !v3_feedback) {
            let got = norm_text(&String::from_utf8_lossy(&body));
            let is_errors_list = l["path"]
                .as_str()
                .is_some_and(|p| p == "/admin/api/errors" || p.starts_with("/admin/api/errors?"));
            let is_logs_list = l["path"]
                .as_str()
                .is_some_and(|p| p == "/admin/api/logs" || p.starts_with("/admin/api/logs?"));
            let is_gift = l["path"] == "/admin/api/gift" && l["status"] == 200;
            let want = if is_gift {
                v3_gift_text(want)
            } else if l["path"] == "/admin/api/members" {
                drop_deleted_member_fields(want)
            } else if is_logs_list {
                drop_legacy_log_client(want)
            } else if is_errors_list {
                serde_json::from_str::<Value>(want).map_or(want.to_string(), |v| v.to_string())
            } else {
                want.to_string()
            };
            let path = l["path"].as_str().unwrap_or("");
            let want = v3_text(&want, path);
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
            let key_like = path.starts_with("/admin/api/keys");
            let got = if l["path"] == "/admin/api/members" || is_logs_list || key_like || is_gift {
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
            // 旧版的 Discord 请求全是私信 / 回复消息，3.0 一个都不发（docs/v3-deviations.md #76）
            let want = drop_dm_calls(&l["calls"]);
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
            compare_snap(&name, &l["db"], &got, &rewrites, diffs);
        }
    }
    steps
}

/// 3.0 删了「客户端日志导出」（docs/v3-deviations.md #53）：夹具里的这几步不发请求，
/// 只按旧版的写法补记成功导出时的那条操作日志，后面的操作日志编号、列表才能对上。
const REMOVED_CLIENT_EXPORT: &str = "/admin/api/logs/export";

fn compare_snap(
    name: &str,
    want: &Value,
    got: &Value,
    rewrites: &[(String, String)],
    diffs: &mut Vec<String>,
) {
    let want = &v3_snapshot(want, rewrites);
    for t in SNAP_TABLES.iter().copied().chain([ERRORS_TABLE]) {
        if want.get(t).is_none() {
            continue;
        }
        // 按序列化文本比：键顺序（列顺序）和数字类型（整数 / 浮点）都要一致
        let want_rows = if t == "admin_actions" {
            v3_admin_actions(&want[t])
        } else {
            want[t].clone()
        };
        let (w, g) = (want_rows.to_string(), got[t].to_string());
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
