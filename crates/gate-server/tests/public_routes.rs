//! 公开页面、实时看板（JSON + SSE）、状态、客户端兼容视图、网页报错上报、问卷、首页首屏数据。
//! 期望值来自旧版 Python 网关实际跑出来的响应（2.x 的演示环境）与旧 pytest 用例的断言（都在标签 py-final 里）。

mod common;
use axum::body::{Body, BodyDataStream};
use axum::http::{HeaderMap, Request, StatusCode};
use common::*;
use futures::StreamExt;
use gate_db::repo::usage::LogFilter;
use gate_server::AppState;
use gate_server::env::EnvConfig;
use gate_server::key::Key;
use gate_server::session;
use gate_server::web::Assets;
use gate_testkit::MockUpstream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tower::ServiceExt;

const MEMBER_ID: &str = "900000000000000001";

/// 配了 Discord 注册服务（member_state 才会去查领取的 Key）+ 站点地址 + 邀请链接。
fn reg_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("DISCORD_CLIENT_ID", "1"),
        ("DISCORD_CLIENT_SECRET", "synthetic-client-secret"),
        ("DISCORD_BOT_TOKEN", "synthetic-bot-token"),
        (
            "REGISTRATION_BRIDGE_SECRET",
            "dev-bridge-secret-0123456789abcdefghij",
        ),
        ("DISCORD_GUILD_ID", "1"),
        ("SITE_URL", "https://gate.example.top"),
        ("DISCORD_INVITE_URL", "https://discord.gg/abc"),
    ]
}

/// 把测试 Key 登记成 Discord 成员，返回 `owl_member=...` Cookie。
async fn register_member(state: &AppState) -> String {
    state
        .db
        .write(|tx| {
            tx.execute(
                "INSERT INTO discord_registrations (discord_id, key_id, created_at, username, display_name, avatar) \
                 VALUES (?,?,?,?,?,?)",
                [MEMBER_ID, "1", "1.0", "member_one", "成员甲", "avatarhash"],
            )
        })
        .await
        .unwrap();
    let row = state.db.get_key_by_token(TOKEN).await.unwrap().unwrap();
    let tag = session::key_tag(Some(&Key(row)));
    let exp = state.now() as i64 + 86400;
    format!(
        "owl_member={}",
        session::sign_member(state, MEMBER_ID, exp, &tag)
    )
}

fn admin_cookie(state: &AppState) -> String {
    format!("nai_gate_admin={}", session::make_admin_cookie(state))
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| panic!("不是 JSON：{}", String::from_utf8_lossy(body)))
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

/// 任意方法 / 任意请求体。
async fn raw(
    state: &AppState,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let resp = gate_server::app(state.clone())
        .oneshot(b.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

async fn get_json(
    state: &AppState,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let (s, h, b) = raw(state, "GET", path, headers, b"").await;
    (s, h, json_of(&b))
}

/// SSE 读取器：按空行切事件块。
struct Sse {
    stream: BodyDataStream,
    buf: String,
}

impl Sse {
    async fn open(
        state: &AppState,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Sse) {
        let mut b = Request::builder().method("GET").uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = gate_server::app(state.clone())
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        (
            status,
            headers,
            Sse {
                stream: resp.into_body().into_data_stream(),
                buf: String::new(),
            },
        )
    }

    /// 下一个事件块（`data: ...` 或保活注释 `: ...`）；超时或流结束返回 None。
    async fn next(&mut self, within: Duration) -> Option<String> {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let block = self.buf[..i].to_string();
                self.buf.drain(..i + 2);
                return Some(block);
            }
            match tokio::time::timeout(within, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                _ => return None,
            }
        }
    }

    /// 下一个 `data:` 事件的 JSON（跳过保活注释）。
    async fn next_data(&mut self, within: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let block = self.next(left).await?;
            if let Some(d) = block.strip_prefix("data: ") {
                return Some(json_of(d.as_bytes()));
            }
        }
    }
}

fn without_t(mut v: Value) -> Value {
    v.as_object_mut().unwrap().shift_remove("t");
    v
}

// ------------------------------------------------------------------ 条款 / 隐私

#[tokio::test]
async fn terms_and_privacy_pages() {
    let e = setup(&[("SITE_URL", "https://gate.example.top/")]).await;
    for (path, title, discord_only) in [
        (
            "/terms",
            "服务条款",
            "网页登录只支持 Discord 账号（OAuth），不提供其他登录方式。",
        ),
        (
            "/privacy",
            "隐私政策",
            "网页登录只有 Discord 一种方式，本站不收集你的 Discord 密码。",
        ),
    ] {
        let (status, headers, body) = Call::new("GET", path).send(&e.state).await;
        assert_eq!(status, 200, "{path}");
        let html = String::from_utf8(body).unwrap();
        // 与旧版 `_DOC_HEADERS` 逐项一致
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(
            headers["content-security-policy"],
            "default-src 'none'; style-src 'unsafe-inline'; sandbox; frame-ancestors 'none'"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["cache-control"], "public, max-age=3600");
        assert!(headers.get("x-request-id").is_some());
        assert!(
            html.starts_with("<!doctype html><html lang=\"zh-CN\">"),
            "{html}"
        );
        assert!(html.contains(&format!("<title>{title} · 猫头鹰公益站</title>")));
        assert!(html.contains(&format!("<h1>{title}</h1>")));
        assert!(html.contains("猫头鹰公益站（gate.example.top） · 最近更新：2026-10-15"));
        assert!(html.contains(discord_only), "{path} 缺 Discord-only 说明");
        assert!(!html.contains("<script"), "文档页不能有脚本");
        // 3.0：不再有「记录证据」「系统记录」的说法
        for gone in ["记录证据", "证据", "系统记录"] {
            assert!(!html.contains(gone), "{path} 还有「{gone}」");
        }
    }
    let terms = String::from_utf8(Call::new("GET", "/terms").send(&e.state).await.2).unwrap();
    for kept in [
        "「猫头鹰公益站」（gate.example.top，下称「本站」）是由个人运营的非商业公益服务",
        "多人共用或转售会被回收 Key。",
        "<h2>责任限制</h2>",
        "<h2>联系</h2>",
    ] {
        assert!(terms.contains(kept), "条款缺：{kept}");
    }
    let privacy = String::from_utf8(Call::new("GET", "/privacy").send(&e.state).await.2).unwrap();
    for kept in [
        "<h2>我们收集什么</h2>",
        "只申请 identify（身份识别）权限",
        "<h2>你的选择</h2>",
    ] {
        assert!(privacy.contains(kept), "隐私政策缺：{kept}");
    }
    // 图片保留期与线上一致（168 小时 = 7 天）；机器人不私信、也不做「防多人共用」的行为统计（docs/v3-deviations.md #79）
    assert!(privacy.contains("生成的图片 7 天后自动删除"));
    for gone in ["防多人共用", "行为统计", "私信通知", "默认保留数天"] {
        assert!(!privacy.contains(gone), "隐私政策还有「{gone}」");
    }
}

#[tokio::test]
async fn doc_pages_without_site_url_have_no_domain() {
    let e = setup(&[]).await;
    let terms = String::from_utf8(Call::new("GET", "/terms").send(&e.state).await.2).unwrap();
    assert!(terms.contains("「猫头鹰公益站」（下称「本站」）是由个人运营"));
    assert!(terms.contains("<p class=\"upd\">猫头鹰公益站 · 最近更新：2026-10-15</p>"));
}

// ------------------------------------------------------------------ 公告

#[tokio::test]
async fn announcement_is_sandboxed_and_404_when_empty() {
    let e = setup(&[]).await;
    let (status, _, body) = Call::new("GET", "/announcement").send(&e.state).await;
    assert_eq!(status, 404);
    assert_eq!(json_of(&body), json!({"detail": "没有公告"}));

    let path = e.state.env.announcement_path();
    std::fs::write(&path, "  \n\t ").unwrap();
    assert_eq!(
        Call::new("GET", "/announcement").send(&e.state).await.0,
        404,
        "只有空白也算没有公告"
    );

    std::fs::write(&path, "<h1>hi</h1><script>alert(1)</script>").unwrap();
    let (status, headers, body) = Call::new("GET", "/announcement").send(&e.state).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        headers["content-security-policy"],
        "sandbox allow-popups allow-popups-to-escape-sandbox"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(body, b"<h1>hi</h1><script>alert(1)</script>");

    e.state.live.invalidate();
    let (_, _, status_body) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(status_body["has_announcement"], true);
    let (_, _, live) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(live["has_announcement"], true);
}

// ------------------------------------------------------------------ /public/live

#[tokio::test]
async fn public_live_shape_and_key_order() {
    let e = setup(&reg_env()).await;
    e.state.live_note("ok");
    e.state.live_note("rejected");
    let (status, headers, body) = raw(&e.state, "GET", "/public/live", &[], b"").await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["content-type"], "application/json");
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(text.starts_with("{\"t\":"), "{text}");
    assert!(text.ends_with(",\"me\":null}"), "{text}");
    let v = json_of(&body);
    // 键顺序与旧版实际响应一致：live.build → meta → me
    assert_eq!(
        keys(&v),
        [
            "t",
            "mode",
            "auth",
            "quota",
            "queue",
            "guard",
            "accounts",
            "members",
            "interval",
            "rules",
            "site",
            "discord_invite",
            "has_announcement",
            "algo_notice",
            "key_inactivity_delete_days",
            "web_login",
            "home_join_open",
            "audit_notice",
            "registration",
            "me"
        ]
    );
    assert_eq!(v["mode"], "fifo");
    assert_eq!(v["auth"], json!({"requests": 2, "rejected": 1}));
    assert_eq!(
        keys(&v["quota"]),
        // 节约模式折算的 units_today 已删除（docs/v3-deviations.md #67）
        ["images_today", "images_cap", "v5_today", "v5_cap"]
    );
    assert_eq!(v["quota"]["images_today"], 0);
    assert_eq!(v["quota"]["images_cap"], 1000);
    assert_eq!(v["queue"], json!({"waiting": 0, "running": 0}));
    // 10 分钟突发窗口已删除：没有 burst_cap / burst_wait（docs/v3-deviations.md #64）
    assert_eq!(
        keys(&v["guard"]),
        [
            "hour", "hour_cap", "quiet", "next_in", "cooldown", "blocked"
        ]
    );
    assert_eq!(v["guard"]["hour_cap"], 150);
    assert_eq!(v["guard"]["quiet"], false);
    assert_eq!(v["guard"]["next_in"], json!(0.0));
    assert_eq!(v["guard"]["blocked"], Value::Null);
    assert_eq!(v["accounts"], json!([{"usable": true, "cooling": 0}]));
    // 注册服务已配置：members 是 {max, active, open}
    assert_eq!(v["members"], json!({"max": 10, "active": 0, "open": false}));
    // 测试环境 IMAGE_MIN_INTERVAL=0：`float(x or 15)` → 15；interval = 15 + 抖动 5 / 2
    assert_eq!(v["interval"], json!(17.5));
    // 保底 `base`、空闲借用 `borrow_open`、Anlas 自动分配占位 `anlas` 已删除（docs/v3-deviations.md #61）
    assert_eq!(
        keys(&v["rules"]),
        [
            "key_queue",
            "queue_cap",
            "hourly_cap",
            "daily_cap",
            "quiet_start",
            "quiet_end",
            "quiet_cap",
            "min_interval",
            "jitter",
            "quota"
        ]
    );
    let rules = &v["rules"];
    assert_eq!(rules["queue_cap"], 5);
    assert_eq!(rules["daily_cap"], 1000);
    assert_eq!(rules["quiet_cap"], 20);
    assert_eq!(rules["min_interval"], json!(15.0));
    assert!(rules["min_interval"].is_f64());
    // 还没有动态额度结果：键都在，值全是 null
    assert_eq!(
        rules["quota"],
        json!({"ceiling": null, "v5_each": null, "v5_global": null, "v5_rate": null, "v5_percent": null})
    );
    assert_eq!(v["site"], "https://gate.example.top");
    assert_eq!(v["discord_invite"], "https://discord.gg/abc");
    assert_eq!(v["has_announcement"], false);
    assert_eq!(v["key_inactivity_delete_days"], 3);
    assert_eq!(v["web_login"], false, "默认网页登录暂停（fail-closed）");
    assert_eq!(v["home_join_open"], true);
    assert_eq!(v["audit_notice"], "");
    assert_eq!(
        v["registration"],
        json!({"configured": true, "open": false, "slots_left": 10})
    );
    assert_eq!(v["me"], Value::Null);
    // 不泄露任何标识
    for secret in [TOKEN, "pst-synthetic-upstream-token", "token_id"] {
        assert!(!text.contains(secret), "看板里不能有 {secret}");
    }
}

#[tokio::test]
async fn public_live_without_registration_service() {
    let e = setup(&[]).await;
    let (_, _, v) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(v["members"], json!({}));
    assert_eq!(
        v["registration"],
        json!({"configured": false, "open": false})
    );
    let (_, _, s) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(
        s["registration"],
        json!({"open": false, "slots_left": null})
    );
}

#[tokio::test]
async fn public_live_reflects_site_settings_after_meta_cache() {
    let e = setup(&reg_env()).await;
    let long = "额".repeat(400);
    for (k, v) in [
        ("web_login_paused", "0"),
        ("home_join_open", "0"),
        ("algo_notice", long.as_str()),
        ("register_open", "1"),
        ("register_max_users", "3"),
    ] {
        e.state.db.set_setting(k, v).await.unwrap();
    }
    e.state.live.invalidate();
    let (_, _, v) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(v["web_login"], true);
    assert_eq!(v["home_join_open"], false);
    assert_eq!(v["algo_notice"].as_str().unwrap().chars().count(), 300);
    assert_eq!(
        v["registration"],
        json!({"configured": true, "open": true, "slots_left": 3})
    );
    assert_eq!(v["members"], json!({"max": 3, "active": 0, "open": true}));

    // 30 秒子缓存：改了设置，不 invalidate 就还是旧值
    e.state
        .db
        .set_setting("web_login_paused", "1")
        .await
        .unwrap();
    let (_, _, again) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(again["web_login"], true);
    e.state.live.invalidate();
    let (_, _, fresh) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(fresh["web_login"], false);
}

#[tokio::test]
async fn public_live_blocked_reason_and_queue() {
    let e = setup(&[]).await;
    let now = e.state.now();
    // 全站熔断：所有可用账号都被站点级规则拦下，首页要说清原因
    e.state
        .guard
        .with(|g| g.trip_breaker(300.0, "", now))
        .unwrap();
    // 有人在排队
    e.state.guard.with(|g| g.admit_image(77, 1, now));
    e.state.live.invalidate();
    let (_, _, v) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(v["queue"], json!({"waiting": 1, "running": 0}));
    assert_eq!(
        v["guard"]["blocked"],
        json!({"code": "breaker", "text": "上游连续出错，已暂停出图 300 秒，到点自动恢复"})
    );
    // 没登录的人看不到别人的排队位置
    assert_eq!(v["me"], Value::Null);
}

#[tokio::test]
async fn public_live_me_for_logged_in_member_only() {
    let e = setup(&reg_env()).await;
    let cookie = register_member(&e.state).await;
    let now = e.state.now();
    e.state.guard.with(|g| g.admit_image(77, 1, now - 5.0)); // 别人先到
    e.state.guard.with(|g| g.admit_image(1, 1, now)); // 测试成员排第 2
    e.state.live.invalidate();

    let (_, _, guest) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(guest["me"], Value::Null);
    assert_eq!(guest["queue"], json!({"waiting": 2, "running": 0}));

    let (status, headers, v) = get_json(&e.state, "/public/live", &[("cookie", &cookie)]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    // 排第 2、约 2 × 15 秒（IMAGE_MIN_INTERVAL=0 → 15）
    assert_eq!(
        v["me"],
        json!({"mine": [{"state": "waiting", "position": 2, "eta": 30}], "waiting": 2, "running": 0})
    );
    // 共享快照没有被某个成员的 me 污染（旧版红线）
    let (_, _, guest_again) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(guest_again["me"], Value::Null);

    // Key 被停用：不再显示「我的排队」
    let fields = json!({"enabled": false});
    e.state
        .db
        .update_key(1, fields.as_object().unwrap())
        .await
        .unwrap();
    let (_, _, off) = get_json(&e.state, "/public/live", &[("cookie", &cookie)]).await;
    assert_eq!(off["me"], Value::Null);
}

/// `/v1/live/me` 已删除（docs/v3-deviations.md #103）：首页一直用 `/public/live` 里的 `me`。
#[tokio::test]
async fn live_me_route_is_gone() {
    let e = setup(&[]).await;
    let (status, _, v) = get_json(
        &e.state,
        "/v1/live/me",
        &[("authorization", &format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!((status.as_u16(), v), (404, json!({"detail": "Not Found"})));
}

// ------------------------------------------------------------------ /public/status

#[tokio::test]
async fn public_status_exposes_only_safe_fields() {
    let e = setup(&reg_env()).await;
    let (status, headers, v) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    // 键集合与键顺序都与旧版实际响应一致
    assert_eq!(
        keys(&v),
        [
            "site",
            "upstream",
            "registration",
            "default_features",
            "audit_notice",
            "web_login",
            "algo_notice",
            "discord_invite",
            "key_inactivity_delete_days",
            "limits",
            "image_jobs",
            "stability",
            "has_announcement"
        ]
    );
    assert_eq!(v["site"], "https://gate.example.top");
    assert_eq!(v["discord_invite"], "https://discord.gg/abc");
    assert_eq!(
        v["upstream"],
        json!({"status": "idle", "image_cooldown_seconds": 0})
    );
    assert_eq!(v["registration"], json!({"open": false, "slots_left": 10}));
    assert_eq!(
        v["default_features"],
        json!([{"id": "image", "label": "文生图"}])
    );
    assert_eq!(
        v["limits"],
        // 保底 `base_daily_images` 已删除（docs/v3-deviations.md #61）
        json!({"key_image_queue": 1, "quiet_start": 0, "quiet_end": 0, "quiet": false})
    );
    assert_eq!(v["image_jobs"], 0);
    assert_eq!(v["stability"], json!({"ok": 0, "error": 0}));
    assert_eq!(v["web_login"], false);
    assert_eq!(v["has_announcement"], false);
    assert_eq!(v["key_inactivity_delete_days"], 3);

    // 8 秒缓存；invalidate 后看到新值
    for (k, val) in [
        ("register_features", "*"),
        ("register_open", "1"),
        ("register_max_users", "5"),
    ] {
        e.state.db.set_setting(k, val).await.unwrap();
    }
    let (_, _, cached) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(cached, v);
    e.state.live.invalidate();
    let _cookie = register_member(&e.state).await; // 占 1 个名额
    let (_, _, v2) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(v2["registration"], json!({"open": true, "slots_left": 4}));
    let ids: Vec<&str> = v2["default_features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["image", "upscale", "augment", "vibe", "tags"]);

    // 上游状态
    for _ in 0..4 {
        e.state.record_upstream(false);
    }
    e.state.cooldown.block(30.0, e.state.now());
    e.state.live.invalidate();
    let (_, _, v3) = get_json(&e.state, "/public/status", &[]).await;
    assert_eq!(v3["upstream"]["status"], "degraded");
    assert!(v3["upstream"]["image_cooldown_seconds"].as_i64().unwrap() > 0);
    // 只有 status 和冷却秒数，没有次数
    assert_eq!(keys(&v3["upstream"]), ["status", "image_cooldown_seconds"]);
}

// ------------------------------------------------------------------ /queue-status

#[tokio::test]
async fn queue_status_shape() {
    let e = setup(&[]).await;
    let (status, _, body) = raw(&e.state, "GET", "/queue-status", &[], b"").await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"global":{"active":0,"waiting":0,"concurrency":1},"image_next_slot_in":0.0,"image_cooldown_remaining":0,"queue_timeout":5,"image_min_interval":0.0}"#
    );
    // 上游 429 之后冷却秒数出现
    e.state.cooldown.block(30.0, e.state.now());
    let (_, _, v) = get_json(&e.state, "/queue-status", &[]).await;
    assert!(v["image_cooldown_remaining"].as_i64().unwrap() > 0);
}

// ------------------------------------------------------------------ 客户端兼容视图

#[tokio::test]
async fn user_compat_routes_require_key_and_keep_panel_schema() {
    let e = setup(&[]).await;
    let now = e.state.now();
    for prefix in ["", "/ai", "/v1"] {
        for leaf in [
            "subscription",
            "data",
            "priority",
            "keystore",
            "information",
        ] {
            if prefix == "/v1" && !matches!(leaf, "subscription" | "information") {
                continue;
            }
            let path = format!("{prefix}/user/{leaf}");
            let (status, _, body) = Call::new("GET", &path).send(&e.state).await;
            assert_eq!(status, 401, "{path}");
            assert_eq!(
                json_of(&body)["message"],
                "猫头鹰公益站提醒：缺少 API Key",
                "{path}"
            );
            let (status, _, body) = Call::new("GET", &path)
                .bearer("nai-wrong")
                .send(&e.state)
                .await;
            assert_eq!(status, 401, "{path}");
            assert_eq!(
                json_of(&body)["error"]["message"],
                "猫头鹰公益站提醒：无效的 API Key"
            );
        }
    }
    let get = |path: &'static str| {
        let state = e.state.clone();
        async move {
            let (s, _, b) = Call::new("GET", path).bearer(TOKEN).send(&state).await;
            (s, b)
        }
    };

    for path in [
        "/user/subscription",
        "/ai/user/subscription",
        "/v1/user/subscription",
    ] {
        let (status, body) = get(path).await;
        assert_eq!(status, 200, "{path}");
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(!text.contains(TOKEN), "订阅视图不能回显 Key");
        let v = json_of(&body);
        assert_eq!(
            keys(&v),
            [
                "tier",
                "active",
                "expiresAt",
                "trainingStepsLeft",
                "perks",
                "naiGate"
            ]
        );
        assert_eq!(v["tier"], 3);
        assert_eq!(v["active"], true);
        let expires = v["expiresAt"].as_f64().unwrap();
        assert!(
            (expires - (now + 3650.0 * 86400.0)).abs() < 86400.0,
            "没有到期时间 = 十年后"
        );
        assert_eq!(
            v["trainingStepsLeft"],
            json!({"fixedTrainingStepsLeft": 0, "purchasedTrainingSteps": 0})
        );
        assert_eq!(
            v["perks"],
            json!({"imageGeneration": true, "unlimitedImageGeneration": false, "voiceGeneration": false, "contextTokens": 0})
        );
        assert_eq!(
            v["naiGate"],
            json!({
                "imageModelScope": "legacy", "anlasEnabled": false, "anlasLeft": 0, "anlasMonthlyLimit": 0,
                "v5LeftToday": 10, "v5DailyLimit": 10, "v5Unlimited": false, "isAdmin": false, "account": null
            })
        );
    }

    // 用掉的 V5 体现在 v5LeftToday 里
    common::add_usage(&e.state.db, 1, &e.state.day(), 3, 0.0).await;
    let (_, body) = get("/user/subscription").await;
    assert_eq!(json_of(&body)["naiGate"]["v5LeftToday"], 7);

    for path in ["/user/data", "/ai/user/data"] {
        let (status, body) = get(path).await;
        assert_eq!(status, 200, "{path}");
        let v = json_of(&body);
        assert_eq!(
            keys(&v),
            ["subscription", "priority", "trainingStepsLeft", "anlas"]
        );
        assert_eq!(
            keys(&v["subscription"]),
            ["tier", "active", "expiresAt", "trainingStepsLeft", "perks"],
            "naiGate 不暴露在 data 里"
        );
        assert_eq!(
            keys(&v["priority"]),
            ["maxPriorityActions", "nextRefillAt", "taskPriority"]
        );
        assert_eq!(
            v["trainingStepsLeft"],
            v["subscription"]["trainingStepsLeft"]
        );
        assert_eq!(v["anlas"], 0);
    }

    for path in ["/user/priority", "/ai/user/priority"] {
        let (status, body) = get(path).await;
        assert_eq!(status, 200, "{path}");
        let v = json_of(&body);
        assert_eq!(v["maxPriorityActions"], 0);
        assert_eq!(v["taskPriority"], 10);
        let next = v["nextRefillAt"].as_i64().unwrap();
        assert_eq!(next % 3600, 0, "下个整点刷新");
        assert!(next as f64 > now && next as f64 <= now + 3601.0);
    }

    for path in [
        "/user/information",
        "/ai/user/information",
        "/v1/user/information",
    ] {
        let (status, body) = get(path).await;
        assert_eq!(status, 200, "{path}");
        let v = json_of(&body);
        assert_eq!(keys(&v), ["tier", "active", "username", "expiresAt"]);
        assert_eq!(v["username"], "测试成员");
        assert_eq!(v["tier"], 3);
        assert!(v["expiresAt"].is_i64());
    }

    // 密钥库：照官方对持久令牌的说法回 403
    for path in ["/user/keystore", "/ai/user/keystore"] {
        let (status, body) = get(path).await;
        assert_eq!(status, 403, "{path}");
        let v = json_of(&body);
        assert_eq!(v["statusCode"], 403);
        assert_eq!(
            v["message"],
            "猫头鹰公益站提醒：该项目仅限浏览器会话访问（NovelAI 官方对持久令牌同样返回 403）"
        );
        assert_eq!(v["error"]["status"], 403);
        assert_eq!(v["error"]["message"], v["message"]);
    }
    // 403 被记成一条「rejected」（旧版 GateError 处理器的统一记账）
    settle().await;
    let rows = e
        .state
        .db
        .list_logs(50, 0, &LogFilter::default())
        .await
        .unwrap();
    assert!(
        rows.iter()
            .any(|r| r["status"] == "rejected" && r["kind"] == "account"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn user_information_uses_key_expiry_and_admin_is_unlimited() {
    let e = setup(&[]).await;
    let fields = json!({"expires_at": 1_900_000_000.9});
    e.state
        .db
        .update_key(1, fields.as_object().unwrap())
        .await
        .unwrap();
    // is_admin 不在 update_key 的白名单里（后台改不了谁是站长）：直接改库
    e.state
        .db
        .write(|tx| tx.execute("UPDATE api_keys SET is_admin=1 WHERE id=1", []))
        .await
        .unwrap();
    let (_, _, body) = Call::new("GET", "/user/information")
        .bearer(TOKEN)
        .send(&e.state)
        .await;
    assert_eq!(json_of(&body)["expiresAt"], 1_900_000_000);
    let (_, _, body) = Call::new("GET", "/user/subscription")
        .bearer(TOKEN)
        .send(&e.state)
        .await;
    let v = json_of(&body);
    assert_eq!(v["expiresAt"], 1_900_000_000);
    assert_eq!(v["naiGate"]["isAdmin"], true);
    assert_eq!(v["naiGate"]["v5Unlimited"], true);
    assert_eq!(v["naiGate"]["imageModelScope"], "all");
}

// ------------------------------------------------------------------ 网页报错上报

const CLIENT_ERR: &[u8] = br#"{"page":"landing","msg":"TypeError: x is undefined","line":3}"#;

async fn web_bugs(state: &AppState) -> Vec<Value> {
    settle().await;
    state
        .db
        .list_error_events(true, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r["source"].as_str().is_some_and(|s| s.starts_with("web:")))
        .collect()
}

#[tokio::test]
async fn client_error_report_is_bounded() {
    let e = setup(&[]).await;
    for i in 0..15 {
        let (status, _, body) = raw(
            &e.state,
            "POST",
            "/public/client-error",
            &[
                ("host", "gate.example.top"),
                ("origin", "https://gate.example.top"),
            ],
            CLIENT_ERR,
        )
        .await;
        assert_eq!(status, 204, "第 {i} 次");
        assert!(body.is_empty());
    }
    // 不是 JSON 的也 204
    let (status, _, _) = raw(&e.state, "POST", "/public/client-error", &[], b"not json").await;
    assert_eq!(status, 204);
    e.state.bugs.idle().await; // 报错是后台任务异步入库的：等写完再读，不靠固定睡眠
    let bugs = web_bugs(&e.state).await;
    assert_eq!(bugs.len(), 1, "同一个报错归并成一行：{bugs:?}");
    assert_eq!(bugs[0]["source"], "web:landing");
    assert_eq!(bugs[0]["level"], "warn");
    assert!(bugs[0]["title"].as_str().unwrap().starts_with("TypeError"));
    assert_eq!(bugs[0]["count"], 10, "每 IP 每 10 分钟只收 10 条");
    let detail = bugs[0]["detail"].as_str().unwrap();
    assert!(detail.starts_with(":3\n"), "{detail:?}"); // `{src}:{line}`，没有来源
}

#[tokio::test]
async fn client_error_filters() {
    let e = setup(&[]).await;
    let post = |headers: Vec<(&'static str, &'static str)>, body: &'static [u8]| {
        let state = e.state.clone();
        async move {
            let (s, _, b) = raw(&state, "POST", "/public/client-error", &headers, body).await;
            assert_eq!(s, 204);
            assert!(b.is_empty());
        }
    };
    // 别的网站的页面：丢弃，且不占限流名额
    for _ in 0..12 {
        post(
            vec![
                ("host", "gate.example.top"),
                ("origin", "https://evil.example"),
            ],
            CLIENT_ERR,
        )
        .await;
    }
    post(
        vec![
            ("host", "gate.example.top"),
            ("referer", "https://evil.example/page"),
        ],
        CLIENT_ERR,
    )
    .await;
    assert!(web_bugs(&e.state).await.is_empty());

    // 浏览器 / 插件注入的脚本：堆栈里没有一帧来自本站
    post(
        vec![("host", "gate.example.top")],
        br#"{"page":"landing","msg":"boom","stack":"Error: boom\n    at hit_test (<anonymous>:1:2)\n    at native_call (chrome-extension://abc/x.js:3:4)"}"#,
    )
    .await;
    // 没有来源的「Script error.」
    post(
        vec![("host", "gate.example.top")],
        br#"{"page":"admin","msg":"Script error.","line":0}"#,
    )
    .await;
    // 不是对象
    post(vec![], b"[1,2]").await;
    assert!(web_bugs(&e.state).await.is_empty());

    // 来自本站文件的报错才记；页面名只认 landing / admin，其余归 other；来源与堆栈截断
    post(
        vec![("host", "gate.example.top"), ("user-agent", "Mozilla/5.0 (synthetic)")],
        "{\"page\":\"survey\",\"msg\":\"ReferenceError: y\",\"src\":\"/_owl/assets/app-abc.js\",\"line\":7,\
          \"stack\":\"ReferenceError: y\\n    at f (https://gate.example.top/_owl/assets/app-abc.js:7:1)\"}"
            .as_bytes(),
    )
    .await;
    let bugs = web_bugs(&e.state).await;
    assert_eq!(bugs.len(), 1, "{bugs:?}");
    assert_eq!(bugs[0]["source"], "web:other");
    assert_eq!(bugs[0]["title"], "ReferenceError: y");
    let detail = bugs[0]["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("/_owl/assets/app-abc.js:7\nMozilla/5.0 (synthetic)\nReferenceError: y"),
        "{detail:?}"
    );
}

// ------------------------------------------------------------------ 问卷

fn survey_body() -> Value {
    json!({"client": "柏宝绘 1.2", "understand": "基本理解", "improve": ["稳定性", "报错提示更清楚"],
           "improve_other": "深夜偶尔 502", "donate": "愿意捐V4.5"})
}

#[tokio::test]
async fn member_survey_errors_and_upsert() {
    let e = setup(&[]).await;
    let post = |auth: Option<&'static str>, body: Vec<u8>| {
        let state = e.state.clone();
        async move {
            let mut h = vec![("content-type", "application/json")];
            let bearer;
            if let Some(a) = auth {
                bearer = format!("Bearer {a}");
                h.push(("authorization", bearer.as_str()));
            }
            let (s, _, b) = raw(&state, "POST", "/member/survey", &h, &body).await;
            (s, json_of(&b))
        }
    };
    let j = |v: Value| serde_json::to_vec(&v).unwrap();

    // 旧版 `HTTPException(exc.status, exc.message)`：{"detail": 原始文案}，没有「提醒」前缀
    assert_eq!(
        post(None, j(survey_body())).await,
        (StatusCode::UNAUTHORIZED, json!({"detail": "缺少 API Key"}))
    );
    assert_eq!(
        post(Some("nai-wrong"), j(survey_body())).await,
        (
            StatusCode::UNAUTHORIZED,
            json!({"detail": "无效的 API Key"})
        )
    );
    assert_eq!(
        post(Some(TOKEN), b"nope".to_vec()).await,
        (
            StatusCode::BAD_REQUEST,
            json!({"detail": "请求体不是合法 JSON"})
        )
    );
    assert_eq!(
        post(Some(TOKEN), b"[]".to_vec()).await,
        (StatusCode::BAD_REQUEST, json!({"detail": "请求体格式无效"}))
    );
    let missing = json!({"detail": "还有必选题没填：限额理解、捐赠意愿"});
    assert_eq!(
        post(Some(TOKEN), j(json!({"understand": "x"}))).await,
        (StatusCode::UNPROCESSABLE_ENTITY, missing.clone())
    );
    assert_eq!(
        post(Some(TOKEN), j(json!({"understand": "  ", "donate": "y"}))).await,
        (StatusCode::UNPROCESSABLE_ENTITY, missing.clone())
    );
    assert_eq!(
        post(Some(TOKEN), Vec::new()).await,
        (StatusCode::UNPROCESSABLE_ENTITY, missing)
    );
    assert_eq!(
        post(
            Some(TOKEN),
            j(json!({"understand": "x", "donate": "y", "improve": "abc"}))
        )
        .await,
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"detail": "多选项格式无效"})
        )
    );
    assert!(
        e.state.db.list_survey().await.unwrap().is_empty(),
        "校验失败不落库"
    );

    let (status, ok) = post(Some(TOKEN), j(survey_body())).await;
    assert_eq!((status, ok), (StatusCode::OK, json!({"ok": true})));
    let rows = e.state.db.list_survey().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key_id"], 1);
    assert_eq!(rows[0]["client"], "柏宝绘 1.2");
    assert_eq!(rows[0]["understand"], "基本理解");
    assert_eq!(rows[0]["improve"], "稳定性、报错提示更清楚");
    assert_eq!(rows[0]["improve_other"], "深夜偶尔 502");
    assert_eq!(rows[0]["donate"], "愿意捐V4.5");

    // 重复提交覆盖而不是新增；多选项逐项截 40 个字符、空项丢掉、非字符串按 str() 处理
    let again = json!({"client": " 换成 Aaalice 了 ", "understand": "x", "donate": "y",
                       "improve": ["排队和等待体验", "", "  ", 7, "长".repeat(60)]});
    assert_eq!(post(Some(TOKEN), j(again)).await.0, StatusCode::OK);
    let rows = e.state.db.list_survey().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["client"], "换成 Aaalice 了");
    assert_eq!(
        rows[0]["improve"],
        format!("排队和等待体验、7、{}", "长".repeat(40))
    );

    // passive：不算「使用」、不写用量日志
    settle().await;
    assert!(
        e.state
            .db
            .list_logs(50, 0, &LogFilter::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn member_survey_identifies_by_discord_session() {
    let e = setup(&reg_env()).await;
    let cookie = register_member(&e.state).await;
    let body = serde_json::to_vec(&survey_body()).unwrap();
    // 只带成员会话、不带 Bearer：新问卷页的路径
    let (status, _, b) = raw(
        &e.state,
        "POST",
        "/member/survey",
        &[("content-type", "application/json"), ("cookie", &cookie)],
        &body,
    )
    .await;
    assert_eq!((status, json_of(&b)), (StatusCode::OK, json!({"ok": true})));
    let rows = e.state.db.list_survey().await.unwrap();
    assert_eq!(
        (
            rows.len(),
            rows[0]["key_id"].clone(),
            rows[0]["discord_id"].clone()
        ),
        (1, json!(1), json!(MEMBER_ID))
    );

    // 会话 Cookie 对应的 Key 被停用：和旧版 authenticate 一样 403
    let fields = json!({"enabled": false});
    e.state
        .db
        .update_key(1, fields.as_object().unwrap())
        .await
        .unwrap();
    let (status, _, b) = raw(
        &e.state,
        "POST",
        "/member/survey",
        &[("cookie", &cookie)],
        &body,
    )
    .await;
    assert_eq!(
        (status, json_of(&b)),
        (
            StatusCode::FORBIDDEN,
            json!({"detail": "该 Key 已被站长停用，有疑问请联系站长。"})
        )
    );

    // 伪造 / 过期的会话不被认，也没有 Bearer：401
    let (status, _, b) = raw(
        &e.state,
        "POST",
        "/member/survey",
        &[("cookie", "owl_member=900000000000000001:1:-.deadbeef")],
        &body,
    )
    .await;
    assert_eq!(
        (status, json_of(&b)),
        (StatusCode::UNAUTHORIZED, json!({"detail": "缺少 API Key"}))
    );
}

// ------------------------------------------------------------------ SSE

#[tokio::test]
async fn live_stream_first_event_changes_and_keepalive() {
    let e = setup(&[]).await;
    e.state.live.set_keepalive(Duration::from_millis(150));
    let (status, headers, mut sse) = Sse::open(&e.state, "/public/live/stream", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert!(headers.get("x-request-id").is_some());
    assert_eq!(
        e.state
            .live
            .open_streams(gate_server::live::StreamKind::Public),
        1
    );

    // 连上立刻推第一条：和 /public/live 同一份数据（t 除外）
    let first = sse
        .next_data(Duration::from_secs(5))
        .await
        .expect("第一条事件");
    let (_, _, polled) = get_json(&e.state, "/public/live", &[]).await;
    assert_eq!(keys(&first), keys(&polled));
    assert_eq!(first["me"], Value::Null);
    assert_eq!(without_t(first.clone()), without_t(polled));

    // 内容没变就不推，只有保活注释（`: keep-alive`）
    let block = sse
        .next(Duration::from_secs(3))
        .await
        .expect("没等到保活注释");
    assert_eq!(block, ": keep-alive", "内容没变就不该推事件");

    // 有变化：下一秒的快照推过来
    e.state.live_note("rejected");
    let changed = sse
        .next_data(Duration::from_secs(5))
        .await
        .expect("变化后的事件");
    assert_eq!(changed["auth"], json!({"requests": 1, "rejected": 1}));

    // 关停：流结束，后台任务退出
    e.state.shutdown.cancel();
    loop {
        match sse.next(Duration::from_secs(3)).await {
            None => break,
            Some(_) => continue, // 在途的保活 / 事件
        }
    }
    e.state.tasks.close();
    tokio::time::timeout(Duration::from_secs(5), e.state.tasks.wait())
        .await
        .expect("后台任务应随 shutdown 退出");
    drop(sse);
    assert_eq!(
        e.state
            .live
            .open_streams(gate_server::live::StreamKind::Public),
        0
    );
}

#[tokio::test]
async fn live_stream_carries_my_queue_per_connection() {
    let e = setup(&reg_env()).await;
    let cookie = register_member(&e.state).await;
    let (_, _, mut member) =
        Sse::open(&e.state, "/public/live/stream", &[("cookie", &cookie)]).await;
    let (_, _, mut guest) = Sse::open(&e.state, "/public/live/stream", &[]).await;
    let m1 = member.next_data(Duration::from_secs(5)).await.unwrap();
    let g1 = guest.next_data(Duration::from_secs(5)).await.unwrap();
    assert_eq!(m1["me"], json!({"mine": [], "waiting": 0, "running": 0}));
    assert_eq!(g1["me"], Value::Null);

    let now = e.state.now();
    e.state.guard.with(|g| g.admit_image(1, 1, now));
    let m2 = member.next_data(Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        m2["me"],
        json!({"mine": [{"state": "waiting", "position": 1, "eta": 15}], "waiting": 1, "running": 0})
    );
    // 访客那条流看到的是全站数字，没有任何人的位置
    let g2 = guest.next_data(Duration::from_secs(5)).await.unwrap();
    assert_eq!(g2["queue"], json!({"waiting": 1, "running": 0}));
    assert_eq!(g2["me"], Value::Null);
}

#[tokio::test]
async fn admin_events_requires_admin_session() {
    let e = setup(&[]).await;
    let (status, _, body) = raw(&e.state, "GET", "/admin/api/events", &[], b"").await;
    assert_eq!(status, 401);
    assert_eq!(json_of(&body), json!({"detail": "未登录或会话已过期"}));
    let (status, _, _) = raw(
        &e.state,
        "GET",
        "/admin/api/events",
        &[("cookie", "nai_gate_admin=1.deadbeef")],
        b"",
    )
    .await;
    assert_eq!(status, 401);
    // 成员 / Key 都不是后台会话
    let (status, _, _) = raw(
        &e.state,
        "GET",
        "/admin/api/events",
        &[("authorization", &format!("Bearer {TOKEN}"))],
        b"",
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(
        e.state
            .live
            .open_streams(gate_server::live::StreamKind::Admin),
        0
    );
}

#[tokio::test]
async fn admin_events_stream_snapshot_and_badges() {
    let e = setup(&[]).await;
    e.state.live.set_keepalive(Duration::from_millis(150));
    let cookie = admin_cookie(&e.state);
    let (status, headers, mut sse) =
        Sse::open(&e.state, "/admin/api/events", &[("cookie", &cookie)]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert_eq!(headers["x-content-type-options"], "nosniff");

    let first = sse
        .next_data(Duration::from_secs(5))
        .await
        .expect("第一条事件");
    assert_eq!(keys(&first), ["live", "badges"]);
    assert_eq!(first["badges"], json!({"bugs": 0, "donations": 0}));
    // live 是同一份看板快照（不含任何成员的 me）
    let (_, _, polled) = get_json(&e.state, "/public/live", &[]).await;
    let live = without_t(first["live"].clone());
    assert!(live.get("me").is_none());
    let mut want = without_t(polled);
    want.as_object_mut().unwrap().shift_remove("me");
    assert_eq!(live, want);

    // 内容没变：只有保活注释，不会让后台页面白白刷新
    match sse.next(Duration::from_secs(2)).await {
        Some(b) => assert!(b.starts_with(':'), "没有变化却推了：{b}"),
        None => panic!("应该有保活"),
    }

    // 真有变化（队列里多了一张图）才推，隔至少 2 秒
    let t0 = tokio::time::Instant::now();
    let now = e.state.now();
    e.state.guard.with(|g| g.admit_image(9, 1, now));
    let second = sse
        .next_data(Duration::from_secs(6))
        .await
        .expect("变化后的事件");
    assert_eq!(second["live"]["queue"], json!({"waiting": 1, "running": 0}));
    assert!(t0.elapsed() < Duration::from_secs(6));

    // 退出登录 / 改密码使会话失效：流结束，重连会拿到 401
    e.state
        .db
        .set_setting("admin_password_hash", "x")
        .await
        .unwrap();
    *e.state.admin_pw_hash.write().unwrap() = Some(session::hash_password("another-password"));
    loop {
        match sse.next(Duration::from_secs(4)).await {
            None => break,
            Some(_) => continue,
        }
    }
    assert_eq!(
        e.state
            .live
            .open_streams(gate_server::live::StreamKind::Admin),
        0
    );
}

// ------------------------------------------------------------------ 首页首屏数据

struct Page {
    state: AppState,
    _mock: MockUpstream,
    _dir: tempfile::TempDir,
}

/// 带一份最小 landing.html（含 `<!--owl-boot-->` 占位）的网关：`web/dist` 在测试里不一定构建过。
async fn setup_page(extra: &[(&str, &str)]) -> Page {
    let mock = MockUpstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let static_dir = dir.path().join("static");
    std::fs::create_dir_all(&static_dir).unwrap();
    std::fs::write(
        static_dir.join("landing.html"),
        "<!doctype html><html><head><!--owl-boot--></head><body><div id=\"app\"></div></body></html>",
    )
    .unwrap();
    let mut vars: HashMap<String, String> = [
        (
            "DATA_DIR",
            dir.path().join("data").to_str().unwrap().to_string(),
        ),
        ("NAI_TOKENS", "pst-synthetic-upstream-token".into()),
        ("NAI_IMAGE_HOST", mock.url()),
        ("IMAGE_MIN_INTERVAL", "0".into()),
        (
            "SECRET_KEY",
            "test-secret-key-test-secret-key-0123456789".into(),
        ),
        ("ADMIN_PASSWORD", ADMIN_PASSWORD.into()),
        ("UPSTREAM_HTTP2", "0".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    let state = AppState::build(EnvConfig::load(&vars), Assets::new(Some(static_dir)))
        .await
        .unwrap();
    let key: gate_db::repo::keys::NewKey = serde_json::from_value(json!({
        "name": "测试成员", "token": TOKEN, "daily_images": 100, "daily_v5": 10,
        "monthly_anlas": 0.0, "daily_text_tokens": 1000, "rpm": 60,
    }))
    .unwrap();
    state.db.create_key(&key, state.now()).await.unwrap();
    Page {
        state,
        _mock: mock,
        _dir: dir,
    }
}

fn boot_of(html: &str) -> Value {
    let open = "<script type=\"application/json\" id=\"owl-boot\">";
    let start = html.find(open).expect("没有注入 owl-boot") + open.len();
    let end = html[start..].find("</script>").unwrap() + start;
    // 不能出现能提前闭合标签的字符
    let raw = &html[start..end];
    assert!(!raw.contains('<') && !raw.contains('>') && !raw.contains('&'));
    json_of(raw.as_bytes())
}

#[tokio::test]
async fn landing_boot_for_guest() {
    let p = setup_page(&reg_env()).await;
    let (status, headers, body) = raw(&p.state, "GET", "/", &[], b"").await;
    assert_eq!(status, 200);
    let html = String::from_utf8(body).unwrap();
    // 页面头与不注入时一样：严格 CSP、按登录态区分
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert_eq!(headers["vary"], "Cookie");
    assert_eq!(headers["cache-control"], "no-cache");
    let boot = boot_of(&html);
    assert_eq!(keys(&boot), ["me", "live", "status"]);
    assert_eq!(boot["me"], json!({"logged_in": false}));
    // live / status 与对应接口同形状
    let (_, _, live) = get_json(&p.state, "/public/live", &[]).await;
    assert_eq!(keys(&boot["live"]), keys(&live));
    assert_eq!(boot["live"]["me"], Value::Null);
    assert_eq!(boot["live"]["site"], "https://gate.example.top");
    assert_eq!(boot["live"]["quota"]["images_cap"], 1000);
    assert_eq!(boot["status"]["site"], "https://gate.example.top");
    assert_eq!(boot["status"]["web_login"], false);
    assert_eq!(
        boot["status"]["registration"],
        json!({"open": false, "slots_left": 10})
    );
}

#[tokio::test]
async fn landing_boot_for_member_with_key() {
    let p = setup_page(&reg_env()).await;
    let cookie = register_member(&p.state).await;
    let now = p.state.now();
    p.state.guard.with(|g| g.admit_image(1, 1, now));
    let (status, _, body) = raw(&p.state, "GET", "/", &[("cookie", &cookie)], b"").await;
    assert_eq!(status, 200);
    let boot = boot_of(&String::from_utf8(body).unwrap());
    let me = &boot["me"];
    assert_eq!(
        keys(me),
        [
            "logged_in",
            "discord",
            "has_key",
            "key",
            "name",
            "expires_at",
            "image_scope",
            "today",
            "queue",
            "coupons",
            "timing"
        ]
    );
    assert_eq!(me["logged_in"], true);
    assert_eq!(
        me["discord"],
        json!({"id": MEMBER_ID, "username": "member_one", "display_name": "成员甲", "avatar": "avatarhash"})
    );
    assert_eq!(me["has_key"], true);
    assert_eq!(me["key"], TOKEN);
    assert_eq!(me["name"], "测试成员");
    assert_eq!(me["expires_at"], Value::Null);
    assert_eq!(me["image_scope"], "legacy");
    assert_eq!(
        me["today"],
        json!({
            "images": 0, "v5": 0, "legacy": 0, "daily_images": 100, "daily_v5": 10,
            "anlas": 0, "daily_anlas": 50, "reset_hour": 7
        })
    );
    assert_eq!(me["queue"], json!([{"state": "waiting", "position": 1}]));
    assert_eq!(me["coupons"], json!([]));
    assert_eq!(
        me["timing"],
        json!({"count": 0, "avg_dur_s": null, "avg_wait_s": null, "site_avg_dur_s": 9.0})
    );
    // 看板里带我的排队
    assert_eq!(
        boot["live"]["me"],
        json!({"mine": [{"state": "waiting", "position": 1, "eta": 15}], "waiting": 1, "running": 0})
    );
}

#[tokio::test]
async fn landing_boot_for_member_without_key_and_bad_session() {
    let p = setup_page(&reg_env()).await;
    // 登录了但没有领 Key：会话里的 Key 指纹是 "-"
    let exp = p.state.now() as i64 + 86400;
    let cookie = format!(
        "owl_member={}",
        session::sign_member(&p.state, MEMBER_ID, exp, "-")
    );
    let (_, _, body) = raw(&p.state, "GET", "/", &[("cookie", &cookie)], b"").await;
    let boot = boot_of(&String::from_utf8(body).unwrap());
    assert_eq!(
        boot["me"],
        json!({"logged_in": true, "discord": {"id": MEMBER_ID}, "has_key": false})
    );
    assert_eq!(boot["live"]["me"], Value::Null);

    // 伪造的会话 = 访客
    let (_, _, body) = raw(
        &p.state,
        "GET",
        "/",
        &[("cookie", "owl_member=900000000000000001:1:-.deadbeef")],
        b"",
    )
    .await;
    assert_eq!(
        boot_of(&String::from_utf8(body).unwrap())["me"],
        json!({"logged_in": false})
    );
}

#[tokio::test]
async fn landing_boot_escapes_markup() {
    let p = setup_page(&[]).await;
    p.state
        .db
        .set_setting("algo_notice", "</script><script>alert(1)</script> & <b>")
        .await
        .unwrap();
    p.state.live.invalidate();
    let (_, _, body) = raw(&p.state, "GET", "/", &[], b"").await;
    let html = String::from_utf8(body).unwrap();
    assert!(!html.contains("<script>alert"));
    let boot = boot_of(&html);
    assert_eq!(
        boot["status"]["algo_notice"],
        "</script><script>alert(1)</script> & <b>"
    );
}
