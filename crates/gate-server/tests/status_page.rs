//! 公开状态页与后台事件接口（旧版 `app/status_page.py`）。
//!
//! 对拍夹具 `tests/fixtures/status_page.jsonl` 由 `tools/parity/golden/status_page.py` 在旧 Python 上生成
//! （合成事件 / 小时计数 / 上游请求 / 官方状态 → 旧版的 `/status.json`、Atom、页面文本、后台列表）：
//! - `/status.json`、`/status/feed.xml`、`/admin/api/incidents`：与旧版逐字节相同；
//! - `/status` HTML：3.0 改了样式 / 脚本的加载方式，不比整页——旧页渲染出的每一个可见字符串（文本节点按顺序，
//!   title / aria-label / href / role / lang 属性）新页都必须包含。
//!
//! 时钟是全进程共享的（`time::freeze`），所以每个测试先拿 [`clock`] 的锁。

mod common;
use common::*;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

const M: &str = env!("CARGO_MANIFEST_DIR");

/// 2026-10-14 12:00:00 +08:00
const NOW: f64 = 1_791_950_400.0;

static CLOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 串行化用到时钟的测试；`at` 为 None 时用真实时间。
async fn clock(at: Option<f64>) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = CLOCK.lock().await;
    gate_server::time::freeze(at);
    guard
}

fn fixture() -> Vec<Value> {
    let dir = Path::new(M).join("tests").join("fixtures");
    let plain = dir.join("status_page.jsonl");
    let gz = dir.join("status_page.jsonl.gz");
    let text = if gz.exists() {
        let mut t = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&gz).unwrap())
            .read_to_string(&mut t)
            .unwrap();
        t
    } else {
        std::fs::read_to_string(&plain).unwrap()
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn text(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).unwrap()
}

async fn get(e: &Env, path: &str) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let (s, h, b) = Call::new("GET", path).send(&e.state).await;
    (s.as_u16(), h, b)
}

async fn login(e: &Env) -> String {
    let (status, headers, _) = Call::new("POST", "/admin/api/login")
        .json(json!({"password": ADMIN_PASSWORD}))
        .send(&e.state)
        .await;
    assert_eq!(status, 200);
    headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn post(e: &Env, cookie: &str, path: &str, body: Value) -> (u16, Value) {
    // 操作日志中间件只缓存声明了 Content-Length 的小请求体（真实客户端都会带）
    let len = serde_json::to_vec(&body).unwrap().len().to_string();
    let (s, _, b) = Call::new("POST", path)
        .header("cookie", cookie.to_string())
        .header("content-length", len)
        .json(body)
        .send(&e.state)
        .await;
    (s.as_u16(), serde_json::from_slice(&b).unwrap())
}

/// 把夹具里的输入写进库：事件走 gate-db 的写入接口，其余直接落表。
async fn load_inputs(e: &Env, sc: &Value) {
    for spec in sc["incidents"].as_array().unwrap() {
        let comps: Vec<String> = spec["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        let ups = spec["updates"].as_array().unwrap();
        let first = &ups[0];
        let id = e
            .state
            .db
            .create_incident(
                spec["title"].as_str().unwrap(),
                spec["impact"].as_str().unwrap(),
                &comps,
                first[0].as_str().unwrap(),
                first[1].as_str().unwrap(),
                first[2].as_f64().unwrap(),
            )
            .await
            .unwrap();
        for u in &ups[1..] {
            let ok = e
                .state
                .db
                .add_incident_update(
                    id,
                    u[0].as_str().unwrap(),
                    u[1].as_str().unwrap(),
                    u[2].as_f64().unwrap(),
                )
                .await
                .unwrap();
            assert!(ok);
        }
    }
    let mut sql = String::new();
    for r in sc["status_hourly"].as_array().unwrap() {
        sql += &format!(
            "INSERT INTO status_hourly(hour, code, n) VALUES ({}, {}, {});\n",
            r[0].as_i64().unwrap(),
            r[1].as_i64().unwrap(),
            r[2].as_i64().unwrap()
        );
    }
    for r in sc["usage"].as_array().unwrap() {
        let up = r[2].as_i64().unwrap();
        sql += &format!(
            "INSERT INTO usage_log(ts, key_id, kind, status, up_status) VALUES ({:?}, {}, 'image', '{}', {up});\n",
            r[0].as_f64().unwrap(),
            r[1].as_i64().unwrap(),
            if up == 200 { "ok" } else { "error" },
        );
    }
    insert_sql(e, sql).await;
    if let Some(off) = sc["official"].as_object() {
        // json.dumps(.., ensure_ascii=False)，维护循环写入的就是这种格式
        let raw = gate_core::pyjson::dumps_with(
            &Value::Object(off.clone()),
            gate_core::pyjson::Style {
                compact: false,
                ensure_ascii: false,
            },
        );
        e.state
            .db
            .set_setting("novelai_official_status", &String::from_utf8(raw).unwrap())
            .await
            .unwrap();
    }
}

async fn insert_sql(e: &Env, sql: String) {
    if sql.is_empty() {
        return;
    }
    e.state
        .db
        .write(move |tx| tx.execute_batch(&sql))
        .await
        .unwrap();
}

/// 合成上游请求：`ok` 个成功（来自 Key 1–5 轮流）+ 若干 `(up_status, key_id)` 失败，都在最近一小时内。
async fn add_usage(e: &Env, ok: usize, failures: &[(i64, i64)]) {
    let mut sql = String::new();
    let base = NOW - 3000.0;
    for i in 0..ok {
        sql += &format!(
            "INSERT INTO usage_log(ts, key_id, kind, status, up_status) VALUES ({:?}, {}, 'image', 'ok', 200);\n",
            base + i as f64 * 20.0,
            (i % 5) + 1
        );
    }
    for (j, (code, key)) in failures.iter().enumerate() {
        sql += &format!(
            "INSERT INTO usage_log(ts, key_id, kind, status, up_status) VALUES ({:?}, {key}, 'image', 'error', {code});\n",
            base + 2000.0 + j as f64
        );
    }
    insert_sql(e, sql).await;
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 旧页的可见字符串都在新页里，且文本节点的先后顺序不变。
#[track_caller]
fn assert_old_strings_present(name: &str, html: &str, expect: &Value) {
    let mut cursor = 0;
    for t in expect["html_texts"].as_array().unwrap() {
        let needle = escape(t.as_str().unwrap());
        match html[cursor..].find(&needle) {
            Some(i) => cursor += i + needle.len(),
            None => panic!(
                "{name}：新页缺少（或顺序不对）旧页文本 {needle:?}\n（从偏移 {cursor} 起找）"
            ),
        }
    }
    for a in expect["html_attrs"].as_array().unwrap() {
        let needle = format!(
            "{}=\"{}\"",
            a[0].as_str().unwrap(),
            escape(a[1].as_str().unwrap())
        );
        assert!(html.contains(&needle), "{name}：新页缺少旧页属性 {needle}");
    }
}

#[tokio::test]
async fn golden_outputs_match_python() {
    let _g = clock(None).await;
    for sc in fixture() {
        let name = sc["scenario"].as_str().unwrap().to_string();
        gate_server::time::freeze(Some(sc["now"].as_f64().unwrap()));
        let discord = &sc["discord"];
        let (guild, chan) = (
            discord["guild"].as_str().unwrap().to_string(),
            discord["channel"].as_str().unwrap().to_string(),
        );
        let extra: Vec<(&str, &str)> = if guild.is_empty() {
            vec![]
        } else {
            vec![("DISCORD_GUILD_ID", &guild), ("ANNOUNCE_CHANNEL_ID", &chan)]
        };
        let e = setup(&extra).await;
        load_inputs(&e, &sc).await;
        let expect = &sc["expect"];

        // 写入路径：后台列表（全部事件，含不上公开页的）与旧版逐字节相同；components 列的原文也相同
        let cookie = login(&e).await;
        let (s, _, b) = Call::new("GET", "/admin/api/incidents")
            .header("cookie", cookie)
            .send(&e.state)
            .await;
        assert_eq!(s, 200, "{name}");
        assert_eq!(
            text(&b),
            expect["admin_list"].as_str().unwrap(),
            "{name}：后台列表"
        );
        let cols: Vec<String> = e
            .state
            .db
            .read(|c| {
                let mut st = c.prepare("SELECT components FROM incidents ORDER BY id")?;
                st.query_map([], |r| r.get::<_, String>(0))?.collect()
            })
            .await
            .unwrap();
        let want: Vec<String> = expect["components_column"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(cols, want, "{name}：components 列");

        // JSON
        let (s, h, b) = get(&e, "/status.json").await;
        assert_eq!(s, 200, "{name}");
        assert_eq!(h["content-type"], "application/json", "{name}");
        assert_eq!(h["cache-control"], "no-cache", "{name}");
        assert_eq!(
            text(&b),
            expect["json"].as_str().unwrap(),
            "{name}：/status.json"
        );

        // Atom
        let (s, h, b) = get(&e, "/status/feed.xml").await;
        assert_eq!(s, 200, "{name}");
        assert_eq!(h["content-type"], "application/atom+xml", "{name}");
        assert_eq!(h["cache-control"], "no-cache", "{name}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{name}");
        if let Some(atom) = expect["atom"].as_str() {
            assert_eq!(text(&b), atom, "{name}：/status/feed.xml");
        }

        // HTML
        let (s, h, b) = get(&e, "/status").await;
        assert_eq!(s, 200, "{name}");
        assert_eq!(h["content-type"], "text/html; charset=utf-8", "{name}");
        let html = text(&b);
        assert_old_strings_present(&name, &html, expect);
        assert!(
            html.contains(&format!(
                "href=\"{}\"",
                expect["subscribe_url"].as_str().unwrap()
            )),
            "{name}：订阅地址"
        );
        // 当前状态
        let statuses = expect["statuses"].as_object().unwrap();
        let json: Value = serde_json::from_str(expect["json"].as_str().unwrap()).unwrap();
        for c in json["components"].as_array().unwrap() {
            assert_eq!(c["status"], statuses[c["key"].as_str().unwrap()], "{name}");
        }
    }
    gate_server::time::freeze(None);
}

#[tokio::test]
async fn html_is_hardened_and_links_work() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let sc = fixture()
        .into_iter()
        .find(|s| s["scenario"] == "history_mix")
        .unwrap();
    load_inputs(&e, &sc).await;
    let (s, h, b) = get(&e, "/status").await;
    assert_eq!(s, 200);
    let html = text(&b);

    // 安全头
    let csp = h["content-security-policy"].to_str().unwrap();
    assert!(
        !csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"),
        "{csp}"
    );
    for need in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "form-action 'none'",
    ] {
        assert!(csp.contains(need), "CSP 缺 {need}：{csp}");
    }
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["cache-control"], "no-cache");
    assert_eq!(
        h["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        b.len()
    );

    // 页面里没有内联样式 / 脚本 / 事件处理器
    assert!(!html.contains(" style="), "不应有 style 属性");
    assert!(!html.contains("<style"), "不应有 <style>");
    assert!(!html.contains(" onclick=") && !html.contains("javascript:"));
    for chunk in html.split("<script").skip(1) {
        let tag = chunk.split('>').next().unwrap();
        assert!(
            tag.contains("src=\"/_owl/status-page.js?v="),
            "内联脚本：{tag}"
        );
        assert!(
            chunk.starts_with(&format!("{tag}></script>")),
            "脚本标签里不应有内容"
        );
    }

    // 过去事件的标题是真正的链接，指向同页锚点；锚点存在
    let links: Vec<&str> = html
        .split("<a class=\"whitespace-pre-wrap\" href=\"")
        .skip(1)
        .map(|r| r.split('"').next().unwrap())
        .collect();
    assert!(links.len() >= 10, "过去事件链接太少：{links:?}");
    for l in links {
        assert!(l.starts_with("#incident-"), "{l}");
        assert!(
            html.contains(&format!("id=\"{}\"", &l[1..])),
            "缺少锚点 {l}"
        );
    }
    // 返回首页
    assert!(html.contains("<a class=\"home-link\" href=\"/\">返回首页</a>"));

    // 样式与脚本由本站提供，地址带内容哈希，长期缓存
    let css_href = html
        .split("<link rel=\"stylesheet\" href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert!(
        css_href.starts_with("/_owl/status-page.css?v="),
        "{css_href}"
    );
    let (s, h, b) = get(&e, &css_href).await;
    assert_eq!(s, 200);
    assert_eq!(h["content-type"], "text/css; charset=utf-8");
    assert_eq!(h["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(text(&b).contains("--bg:#FAF9F5"));
    let js_href = html
        .split("<script src=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let (s, h, b) = get(&e, &js_href).await;
    assert_eq!(s, 200);
    assert_eq!(h["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(h["cache-control"], "public, max-age=31536000, immutable");
    assert!(text(&b).contains("owl-theme"));
    // 地址里的哈希对不上（旧页面缓存）：不长期缓存
    let (s, h, _) = get(&e, "/_owl/status-page.css?v=stale").await;
    assert_eq!((s, h["cache-control"].to_str().unwrap()), (200, "no-cache"));
    // 其它 /_owl/ 资源仍走原来的静态资源处理（测试里没有前端构建产物 → 404）
    let (s, _, _) = get(&e, "/_owl/no-such-file.js").await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn head_returns_headers_without_body() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let (s, h, b) = get(&e, "/status").await;
    assert_eq!(s, 200);
    let (hs, hh, hb) = Call::new("HEAD", "/status").send(&e.state).await;
    assert_eq!(hs, 200);
    assert!(hb.is_empty());
    assert_eq!(hh["content-type"], h["content-type"]);
    assert_eq!(hh["content-security-policy"], h["content-security-policy"]);
    assert_eq!(hh["content-length"], h["content-length"]);
    assert_eq!(
        hh["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        b.len()
    );
    // 方法不对
    let (s, _, _) = Call::new("POST", "/status").send(&e.state).await;
    assert_eq!(s, 405);
}

/// 3.0 新规则：「现在」的上游状态要求失败来自至少 2 把 Key 且至少 3 次；每天的色条仍按旧版的占比。
#[tokio::test]
async fn upstream_now_needs_failures_from_two_keys() {
    let _g = clock(Some(NOW)).await;
    let status_of = |body: &[u8]| -> (String, String) {
        let v: Value = serde_json::from_slice(body).unwrap();
        let s = |i: usize| v["components"][i]["status"].as_str().unwrap().to_string();
        (s(0), s(1))
    };

    // 同一把 Key 失败 5 次、100 次成功 → 运行正常
    let e = setup(&[]).await;
    add_usage(&e, 100, &[(500, 4); 5]).await;
    let (_, _, b) = get(&e, "/status.json").await;
    assert_eq!(status_of(&b), ("运行正常".into(), "运行正常".into()));
    let (_, _, h) = get(&e, "/status").await;
    let html = text(&h);
    assert!(html.contains("所有系统运行正常"));
    // 今天的色条仍然按当天占比（4.8% > 1%）变黄
    let bars = html
        .split("<svg")
        .nth(1)
        .unwrap()
        .split("</svg>")
        .next()
        .unwrap();
    let last_rect = bars.rsplit("<rect").next().unwrap();
    assert!(last_rect.contains("fill=\"#FAA72A\""), "{last_rect}");

    // 两把 Key 各失败 → 性能下降（页首横幅也变）
    let e = setup(&[]).await;
    add_usage(&e, 100, &[(500, 1), (500, 1), (429, 2)]).await;
    let (_, _, b) = get(&e, "/status.json").await;
    assert_eq!(status_of(&b), ("性能下降".into(), "运行正常".into()));
    let (_, _, h) = get(&e, "/status").await;
    assert!(text(&h).contains("部分系统性能下降"));

    // 两把 Key 但只失败 2 次 → 不算
    let e = setup(&[]).await;
    add_usage(&e, 100, &[(500, 1), (500, 2)]).await;
    let (_, _, b) = get(&e, "/status.json").await;
    assert_eq!(status_of(&b), ("运行正常".into(), "运行正常".into()));

    // 请求不到 10 次不下结论（与旧版一致）
    let e = setup(&[]).await;
    add_usage(&e, 3, &[(500, 1), (500, 2), (500, 3)]).await;
    let (_, _, b) = get(&e, "/status.json").await;
    assert_eq!(status_of(&b), ("运行正常".into(), "运行正常".into()));

    // 官方状态与进行中的事件照旧生效（不受新规则影响）
    let e = setup(&[]).await;
    e.state
        .db
        .set_setting(
            "novelai_official_status",
            &format!(r#"{{"image_code": 300, "fetched_at": {NOW}}}"#),
        )
        .await
        .unwrap();
    let (_, _, b) = get(&e, "/status.json").await;
    assert_eq!(status_of(&b), ("性能下降".into(), "运行正常".into()));
}

/// 快照缓存 60 秒：期间新发布的事件还看不到（与旧版一致），过了就看到。
#[tokio::test]
async fn snapshot_is_cached_for_a_minute() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    // 缓存挂在路由上：同一个 app 才共享（线上 app() 只组装一次）
    let app = gate_server::app(e.state.clone());
    macro_rules! fetch {
        () => {{
            let req = axum::http::Request::builder()
                .uri("/status.json")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }};
    }
    assert_eq!(fetch!()["incidents"], json!([]));
    e.state
        .db
        .create_incident(
            "出图接口请求超时",
            "minor",
            &["api".to_string()],
            "investigating",
            "x",
            NOW,
        )
        .await
        .unwrap();
    gate_server::time::freeze(Some(NOW + 30.0));
    assert_eq!(fetch!()["incidents"], json!([]), "60 秒内复用缓存");
    gate_server::time::freeze(Some(NOW + 61.0));
    let v = fetch!();
    assert_eq!(v["incidents"][0]["title"], "出图接口请求超时");
    assert_eq!(v["active"].as_array().unwrap().len(), 1);
    gate_server::time::freeze(None);
}

// ------------------------------------------------------------------ 后台接口

const NEED_LOGIN: &str = r#"{"detail":"未登录或会话已过期"}"#;

#[tokio::test]
async fn admin_incident_endpoints_require_login() {
    let _g = clock(None).await;
    let e = setup(&[]).await;
    for (m, p) in [
        ("GET", "/admin/api/incidents"),
        ("POST", "/admin/api/incidents"),
        ("POST", "/admin/api/incidents/1/updates"),
    ] {
        let mut c = Call::new(m, p);
        if m == "POST" {
            c = c.json(json!({"status": "investigating", "title": "x"}));
        }
        let (s, _, b) = c.send(&e.state).await;
        assert_eq!(
            (s.as_u16(), text(&b)),
            (401, NEED_LOGIN.to_string()),
            "{m} {p}"
        );
    }
    // 路径参数先于登录校验（与 FastAPI 一致）：不是整数一律 422
    let (s, _, b) = Call::new("POST", "/admin/api/incidents/abc/updates")
        .json(json!({"status": "update"}))
        .send(&e.state)
        .await;
    assert_eq!(s, 422);
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        v,
        json!({"detail": [{
            "type": "int_parsing",
            "loc": ["path", "incident_id"],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": "abc",
        }]})
    );
    // 跨站写请求被拒
    let cookie = login(&e).await;
    let (s, _, b) = Call::new("POST", "/admin/api/incidents")
        .header("host", "owl.example")
        .header("origin", "https://evil.example")
        .header("cookie", cookie)
        .json(json!({"status": "investigating", "title": "x"}))
        .send(&e.state)
        .await;
    assert_eq!(
        (s.as_u16(), text(&b)),
        (403, r#"{"detail":"来源校验失败"}"#.to_string())
    );
    assert!(e.state.db.list_incidents(0.0).await.unwrap().is_empty());
}

#[tokio::test]
async fn admin_create_validation() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let cookie = login(&e).await;
    let create = |body: Value| post(&e, &cookie, "/admin/api/incidents", body);
    let status_msg = "status 必须是 investigating / identified / update / monitoring / resolved";
    let cases: Vec<(Value, &str)> = vec![
        (json!({"title": "x"}), status_msg),
        (json!({"status": "nope", "title": "x"}), status_msg),
        (json!({"status": 5, "title": "x"}), status_msg),
        (json!({"status": "investigating"}), "需要 title"),
        (
            json!({"status": "investigating", "title": "  \n "}),
            "需要 title",
        ),
        (
            json!({"status": "investigating", "title": "x", "impact": "huge"}),
            "impact 必须是 none / minor / major / critical",
        ),
        (
            json!({"status": "investigating", "title": "x", "impact": 3}),
            "impact 必须是 none / minor / major / critical",
        ),
        (
            json!({"status": "investigating", "title": "x", "at": "now"}),
            "at 必须是秒级时间戳",
        ),
        (
            json!({"status": "investigating", "title": "x", "at": true}),
            "at 必须是秒级时间戳",
        ),
        (
            json!({"status": "investigating", "title": "x", "at": (NOW * 1000.0) as i64}),
            "at 超出范围（要秒，不是毫秒）",
        ),
        (
            json!({"status": "investigating", "title": "x", "at": NOW - 401.0 * 86400.0}),
            "at 超出范围（要秒，不是毫秒）",
        ),
        (
            json!({"status": "investigating", "title": "x", "at": NOW + 86401.0}),
            "at 超出范围（要秒，不是毫秒）",
        ),
    ];
    for (body, msg) in cases {
        let (s, v) = create(body.clone()).await;
        assert_eq!((s, v), (422, json!({"detail": msg})), "{body}");
    }
    // 不是 JSON 对象 / 不是合法 JSON
    let (s, v) = create(json!([1, 2])).await;
    assert_eq!((s, v), (400, json!({"detail": "请求体必须是 JSON 对象"})));
    assert!(
        e.state.db.list_incidents(0.0).await.unwrap().is_empty(),
        "校验失败不写库"
    );

    // 边界上的 at 是合法的
    for at in [NOW - 400.0 * 86400.0, NOW + 86400.0] {
        let (s, _) = create(json!({"status": "investigating", "title": "x", "at": at})).await;
        assert_eq!(s, 200, "{at}");
    }
}

#[tokio::test]
async fn admin_create_update_list_roundtrip() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let cookie = login(&e).await;

    // 发布：正文留空 = 标准句式；未知组件丢弃；标题去空白
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "investigating", "title": "  出图接口请求超时 \n", "impact": "major",
               "components": ["upstream", "nope", 7, "api", "upstream"]}),
    )
    .await;
    assert_eq!((s, v), (200, json!({"id": 1})));
    // 没给 impact / components：minor + ["api"]；正文去空白
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "monitoring", "title": "部分 V5 出图画质下降", "body": "  已观察  ", "at": NOW - 600.5,
               "components": "not-a-list"}),
    )
    .await;
    assert_eq!((s, v), (200, json!({"id": 2})));
    // 直接以 resolved 发布：resolved_at = at
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "resolved", "title": "例行维护", "impact": "none", "components": ["web"], "at": NOW - 3600.0}),
    )
    .await;
    assert_eq!((s, v), (200, json!({"id": 3})));

    let list = |cookie: &str| {
        let cookie = cookie.to_string();
        let state = e.state.clone();
        async move {
            let (s, _, b) = Call::new("GET", "/admin/api/incidents")
                .header("cookie", cookie)
                .send(&state)
                .await;
            assert_eq!(s, 200);
            serde_json::from_slice::<Value>(&b).unwrap()["incidents"].clone()
        }
    };
    let before = list(&cookie).await;
    let by_id = |v: &Value, id: i64| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()
            .clone()
    };
    let one = by_id(&before, 1);
    assert_eq!(one["title"], "出图接口请求超时");
    assert_eq!(one["impact"], "major");
    assert_eq!(one["components"], json!(["upstream", "api", "upstream"]));
    assert_eq!(one["started_at"], NOW);
    assert_eq!(one["resolved_at"], Value::Null);
    assert_eq!(one["updates"][0]["body"], "我们正在调查此问题。");
    let two = by_id(&before, 2);
    assert_eq!(
        (&two["impact"], &two["components"]),
        (&json!("minor"), &json!(["api"]))
    );
    assert_eq!(two["updates"][0]["body"], "已观察");
    assert_eq!(two["started_at"], NOW - 600.5);
    let three = by_id(&before, 3);
    assert_eq!(three["resolved_at"], NOW - 3600.0);
    // 开始时间新的在前
    let ids: Vec<i64> = before
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [1, 2, 3]);
    // 3 号只影响官网：后台列表有，公开页没有
    let (_, _, b) = get(&e, "/status.json").await;
    let public: Value = serde_json::from_slice(&b).unwrap();
    let public_ids: Vec<i64> = public["incidents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(public_ids, [1, 2]);
    assert_eq!(public["active"].as_array().unwrap().len(), 2);

    // 追加更新：留空用标准句式；非 resolved 保持未解决
    let up = |body: Value| post(&e, &cookie, "/admin/api/incidents/1/updates", body);
    let (s, v) = up(json!({"status": "identified", "at": NOW + 10.0})).await;
    assert_eq!((s, v), (200, json!({"ok": true})));
    let one = by_id(&list(&cookie).await, 1);
    assert_eq!(one["updates"][0]["status"], "identified");
    assert_eq!(one["updates"][0]["body"], "问题已定位，正在实施修复。");
    assert_eq!(one["resolved_at"], Value::Null);
    // 解决：resolved_at = 更新时间
    let (s, _) =
        up(json!({"status": "resolved", "body": "影响时段 11:00–11:40。", "at": NOW + 20.0})).await;
    assert_eq!(s, 200);
    let one = by_id(&list(&cookie).await, 1);
    assert_eq!(one["resolved_at"], NOW + 20.0);
    assert_eq!(one["updates"].as_array().unwrap().len(), 3);
    // 再来一条非 resolved：重新打开
    let (s, _) = up(json!({"status": "update", "at": NOW + 30.0})).await;
    assert_eq!(s, 200);
    assert_eq!(by_id(&list(&cookie).await, 1)["resolved_at"], Value::Null);
    // 不给 at：用当前（冻结）时间
    let (s, _) = up(json!({"status": "monitoring"})).await;
    assert_eq!(s, 200);
    let updates = by_id(&list(&cookie).await, 1)["updates"].clone();
    assert!(
        updates
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["status"] == "monitoring" && u["at"] == NOW),
        "{updates}"
    );

    // 校验错误与 404
    let (s, v) = up(json!({"status": "nope"})).await;
    assert_eq!(
        (
            s,
            v["detail"].as_str().unwrap().starts_with("status 必须是")
        ),
        (422, true)
    );
    let (s, v) = up(json!({"status": "update", "at": "x"})).await;
    assert_eq!((s, v), (422, json!({"detail": "at 必须是秒级时间戳"})));
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents/99/updates",
        json!({"status": "update"}),
    )
    .await;
    assert_eq!((s, v), (404, json!({"detail": "事件不存在"})));
    // 404 先于 at 校验（与旧版的检查顺序一致）
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents/99/updates",
        json!({"status": "update", "at": "x"}),
    )
    .await;
    assert_eq!((s, v), (404, json!({"detail": "事件不存在"})));
    let (s, v) = post(
        &e,
        &cookie,
        "/admin/api/incidents/99999999999999999999/updates",
        json!({"status": "update"}),
    )
    .await;
    assert_eq!((s, v), (404, json!({"detail": "事件不存在"})));
    assert!(
        e.state
            .db
            .list_incidents(0.0)
            .await
            .unwrap()
            .iter()
            .all(|i| i.id <= 3),
        "没有孤儿更新"
    );
}

#[tokio::test]
async fn admin_field_limits() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let cookie = login(&e).await;
    let title = "题".repeat(250);
    let body = "文".repeat(2500);
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "update", "title": title, "body": body}),
    )
    .await;
    assert_eq!(s, 200);
    let inc = &e.state.db.list_incidents(0.0).await.unwrap()[0];
    assert_eq!(inc.title.chars().count(), 200);
    assert_eq!(inc.updates[0].body.chars().count(), 2000);
}

/// 后台操作日志：发布 / 更新记成功，校验失败记失败（带原因）；未登录的不记。
#[tokio::test]
async fn admin_actions_are_logged() {
    let _g = clock(None).await;
    let e = setup(&[]).await;
    let cookie = login(&e).await;
    // 未登录：不记录
    let _ = Call::new("POST", "/admin/api/incidents")
        .json(json!({"status": "investigating", "title": "不该被记录"}))
        .send(&e.state)
        .await;
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "investigating", "title": "出图接口请求超时", "impact": "major"}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents/1/updates",
        json!({"status": "resolved"}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "bad", "title": "x"}),
    )
    .await;
    assert_eq!(s, 422);
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents/42/updates",
        json!({"status": "update"}),
    )
    .await;
    assert_eq!(s, 404);
    // 读取不记录
    let _ = Call::new("GET", "/admin/api/incidents")
        .header("cookie", cookie.clone())
        .send(&e.state)
        .await;
    settle().await;

    let rows = e.state.db.list_admin_actions(50, 0).await.unwrap();
    let mut got: Vec<(String, String, bool, String)> = rows
        .iter()
        .filter(|r| r["action"] != "登录后台")
        .map(|r| {
            (
                r["action"].as_str().unwrap().to_string(),
                r["target"].as_str().unwrap().to_string(),
                r["ok"].as_i64() == Some(1),
                r["detail"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    got.reverse(); // 旧的在前
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0].0, "发布状态页事件");
    assert!(got[0].2);
    assert!(
        got[0].3.contains("出图接口请求超时") && got[0].3.contains("major"),
        "{:?}",
        got[0]
    );
    assert_eq!(
        (got[1].0.as_str(), got[1].1.as_str(), got[1].2),
        ("更新状态页事件", "事件 #1", true)
    );
    assert!(got[1].3.contains("resolved"));
    assert_eq!((got[2].0.as_str(), got[2].2), ("发布状态页事件", false));
    assert!(
        got[2].3.starts_with("失败（422）：status 必须是"),
        "{}",
        got[2].3
    );
    assert_eq!(
        (got[3].0.as_str(), got[3].1.as_str(), got[3].2),
        ("更新状态页事件", "事件 #42", false)
    );
    assert_eq!(got[3].3, "失败（404）：事件不存在");
    assert!(
        rows.iter()
            .all(|r| r["actor"].as_str().unwrap().starts_with("后台 "))
    );
}

/// 新发布的事件按 Statuspage 标准句式写，公开页和订阅源里都看得到，且没有内部细节。
#[tokio::test]
async fn published_incident_shows_on_public_pages() {
    let _g = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let cookie = login(&e).await;
    let (s, _) = post(
        &e,
        &cookie,
        "/admin/api/incidents",
        json!({"status": "investigating", "title": "出图接口请求超时", "impact": "major", "at": NOW - 120.0}),
    )
    .await;
    assert_eq!(s, 200);
    let (_, _, b) = get(&e, "/status").await;
    let html = text(&b);
    assert!(html.contains("出图接口请求超时") && html.contains("我们正在调查此问题。"));
    assert!(html.contains("2026年10月14日 11:58 UTC+8"));
    assert!(html.contains("部分中断"));
    // 有未解决事件时页首是事件条而不是「所有系统运行正常」
    assert!(!html.contains("所有系统运行正常"));
    let (_, _, b) = get(&e, "/status/feed.xml").await;
    let atom = text(&b);
    assert!(atom.contains("<title>出图接口请求超时 · 调查中</title>"));
    assert!(atom.contains("<updated>2026-10-14T11:58:00+08:00</updated>"));
}
