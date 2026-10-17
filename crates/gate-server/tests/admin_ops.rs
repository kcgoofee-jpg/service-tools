//! 后台「运营 / 监控」接口：预算、排队与冷却、账号保护、领 Key / 功能 / 记录开关、公告与告警、
//! Discord 机器人、上游 Token 增删改、Anlas 核对、总览与监控。
//!
//! 期望值来自旧 Python 网关（`app/admin.py`）在同样输入下的真实输出：状态码、`detail` 文案、JSON 键顺序、
//! 数字是 int 还是 float、写进 `site_settings` 的字符串、操作日志行都逐条对过。
//! 公开仓库：全部是合成数据；不会连 Discord / NovelAI（上游是本地假服务）。

mod common;
use common::*;
use gate_db::repo::usage::{NewLog, Success};
use gate_testkit::{MockUpstream, Reply};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

// ------------------------------------------------------------------ 小工具

struct Api<'a> {
    env: &'a Env,
    cookie: String,
}

impl<'a> Api<'a> {
    async fn login(env: &'a Env) -> Self {
        let (status, headers, _) = Call::new("POST", "/admin/api/login")
            .json(json!({"password": ADMIN_PASSWORD}))
            .send(&env.state)
            .await;
        assert_eq!(status, 200);
        let cookie = headers["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        Self { env, cookie }
    }

    /// (状态码, 响应头, 响应原文)
    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> (u16, axum::http::HeaderMap, String) {
        let url = format!("/admin/api{path}");
        let mut call = Call::new(method, &url).header("cookie", self.cookie.clone());
        for (k, v) in extra {
            call = call.header(k, *v);
        }
        if let Some(b) = body {
            // 真实客户端都会带 Content-Length；操作日志中间件靠它决定要不要缓存请求体来做参数摘要
            let len = serde_json::to_vec(&b).expect("JSON").len();
            call = call.header("content-length", len.to_string()).json(b);
        }
        let (status, headers, bytes) = call.send(&self.env.state).await;
        (
            status.as_u16(),
            headers,
            String::from_utf8(bytes).expect("响应是 UTF-8"),
        )
    }

    /// 响应原文（用来逐字节比对 JSON 的键顺序与数字写法）。
    async fn text(&self, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
        let (s, _, t) = self.raw(method, path, body, &[]).await;
        (s, t)
    }

    async fn json(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let (s, t) = self.text(method, path, body).await;
        (s, serde_json::from_str(&t).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        let (s, v) = self.json("GET", path, None).await;
        assert_eq!(s, 200, "GET {path}: {v}");
        v
    }

    /// 期望失败：状态码与 `{"detail": ...}` 逐字一致。
    async fn fails(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        status: u16,
        detail: &str,
    ) {
        let (s, t) = self.text(method, path, body.clone()).await;
        assert_eq!(
            (s, t.as_str()),
            (status, json!({"detail": detail}).to_string().as_str()),
            "{method} {path} {body:?}"
        );
    }
}

/// `site_settings` 全部行。
async fn settings(e: &Env) -> BTreeMap<String, String> {
    common::query(&e.state.db, "SELECT key, value FROM site_settings")
        .await
        .into_iter()
        .map(|r| {
            (
                r["key"].as_str().unwrap().to_string(),
                r["value"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// 只留下与 `before` 不同（新增 / 改动）的行。
fn changed(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 操作日志（旧→新）：(操作者, 操作, 对象, 详情, 成功?)。
async fn actions(e: &Env) -> Vec<(String, String, String, String, bool)> {
    let mut rows = common::admin_actions(&e.state.db, 200).await;
    rows.reverse();
    rows.iter()
        .map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (
                s("actor"),
                s("action"),
                s("target"),
                s("detail"),
                r["ok"].as_i64() == Some(1),
            )
        })
        .collect()
}

fn last_action(rows: &[(String, String, String, String, bool)]) -> (&str, &str, &str, bool) {
    let r = rows.last().expect("至少有一条操作日志");
    (r.1.as_str(), r.2.as_str(), r.3.as_str(), r.4)
}

// ------------------------------------------------------------------ 鉴权

/// 本组的每个接口：方法 + 路径（路径参数用占位值）。
const ENDPOINTS: &[(&str, &str)] = &[
    ("GET", "/status-codes"),
    ("GET", "/overview"),
    ("GET", "/status"),
    ("GET", "/perf"),
    ("GET", "/settings"),
    ("PUT", "/settings"),
    ("GET", "/runtime-limits"),
    ("PUT", "/runtime-limits"),
    ("GET", "/allowance"),
    ("POST", "/upstream-tokens"),
    ("PUT", "/upstream-tokens/token-x"),
    ("DELETE", "/upstream-tokens/token-x"),
    ("PUT", "/upstream-tokens/token-x/v5-limit"),
    ("PUT", "/upstream-tokens/token-x/image-concurrency"),
    ("PUT", "/upstream-tokens/token-x/enabled"),
    ("GET", "/announcement"),
    ("PUT", "/announcement"),
    ("POST", "/announcement/send"),
    ("POST", "/alerts/test"),
    ("GET", "/ops"),
    ("PUT", "/ops/registration"),
    ("PUT", "/ops/features"),
    ("PUT", "/ops/audit"),
    ("GET", "/guard"),
    ("PUT", "/guard"),
    ("GET", "/discord-bot"),
    ("GET", "/upstream-stats"),
    ("GET", "/reconciliation"),
    ("POST", "/reconciliation"),
];

#[tokio::test]
async fn every_endpoint_needs_a_session_and_same_origin_writes() {
    let e = setup(&[]).await;
    for (method, path) in ENDPOINTS {
        let url = format!("/admin/api{path}");
        let (status, _, body) = Call::new(method, &url).json(json!({})).send(&e.state).await;
        assert_eq!(status, 401, "{method} {path}");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"detail":"未登录或会话已过期"}"#,
            "{method} {path}"
        );
    }
    // 没登录的请求不进操作日志（防止被刷）
    assert!(actions(&e).await.is_empty());

    // 登录后：跨源的写请求仍被拒
    let api = Api::login(&e).await;
    for (method, path) in ENDPOINTS.iter().filter(|(m, _)| *m != "GET") {
        let (s, _, t) = api
            .raw(
                method,
                path,
                Some(json!({})),
                &[("host", "owl.example"), ("origin", "https://evil.example")],
            )
            .await;
        assert_eq!(
            (s, t.as_str()),
            (403, r#"{"detail":"来源校验失败"}"#),
            "{method} {path}"
        );
    }
    // 同源 / 无 Origin 的读请求照常
    assert_eq!(
        api.raw(
            "GET",
            "/settings",
            None,
            &[("origin", "https://evil.example")]
        )
        .await
        .0,
        200
    );
}

// ------------------------------------------------------------------ 预算

const SETTINGS_DEFAULT: &str = r#"{"global_monthly_anlas":10000.0,"global_daily_v5":150,"global_daily_anlas":200,"member_daily_anlas":50,"v5_alert_threshold":20,"v5_medium_mode":"off","v5_medium_state":{"active":false,"lowest_percent":null,"threshold":20}}"#;

#[tokio::test]
async fn settings_get_put_and_stored_strings() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.text("GET", "/settings", None).await,
        (200, SETTINGS_DEFAULT.to_string())
    );

    // 动态额度开着（默认）：全站 V5 归算法管，带上它就 400、什么都不写
    let before = settings(&e).await;
    assert_eq!(
        api.text(
            "PUT",
            "/settings",
            Some(json!({"global_daily_v5": 200, "member_daily_anlas": 40}))
        )
        .await,
        (
            400,
            r#"{"detail":"global_daily_v5 由动态额度管理：关闭动态额度后才能手动修改"}"#.into()
        )
    );
    assert_eq!(settings(&e).await, before);
    // 以下是动态额度关着时手动改
    e.state
        .db
        .set_setting("quota_auto_enabled", "0")
        .await
        .unwrap();

    // 空请求体：缺省字段保持原值，并且把现值（带类型的字符串）写回库
    let before = settings(&e).await;
    let (s, t) = api.text("PUT", "/settings", Some(json!({}))).await;
    assert_eq!(s, 200);
    assert_eq!(
        t,
        r#"{"ok":true,"global_monthly_anlas":10000.0,"global_daily_v5":150,"global_daily_anlas":200,"member_daily_anlas":50,"v5_alert_threshold":20,"v5_medium_mode":"off"}"#
    );
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("global_daily_anlas", "200"),
            ("global_daily_v5", "150"),
            ("global_monthly_anlas", "10000.0"),
            ("member_daily_anlas", "50"),
            ("v5_alert_threshold", "20"),
            ("v5_medium_mode", "off"),
        ])
    );

    // (请求体, 响应原文, 库里的 global_monthly_anlas / global_daily_v5 / member_daily_anlas / v5_alert_threshold)
    #[allow(clippy::type_complexity)]
    let ok_cases: Vec<(Value, &str, [&str; 4])> = vec![
        (
            json!({"global_monthly_anlas": 5000, "global_daily_v5": 200, "member_daily_anlas": 40, "v5_alert_threshold": 30}),
            r#"{"ok":true,"global_monthly_anlas":5000.0,"global_daily_v5":200,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["5000.0", "200", "40", "30"],
        ),
        // 字符串数字也收（与旧版 float("1e3") 一致）
        (
            json!({"global_monthly_anlas": "1e3"}),
            r#"{"ok":true,"global_monthly_anlas":1000.0,"global_daily_v5":200,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["1000.0", "200", "40", "30"],
        ),
        // 0 被夹到下限：库里是整数 "0"，响应仍是请求值的浮点写法（旧版怪癖）
        (
            json!({"global_monthly_anlas": 0}),
            r#"{"ok":true,"global_monthly_anlas":0.0,"global_daily_v5":200,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["0", "200", "40", "30"],
        ),
        (
            json!({"global_monthly_anlas": 1000000}),
            r#"{"ok":true,"global_monthly_anlas":1000000.0,"global_daily_v5":200,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["1000000", "200", "40", "30"],
        ),
        (
            json!({"global_monthly_anlas": 12.5}),
            r#"{"ok":true,"global_monthly_anlas":12.5,"global_daily_v5":200,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["12.5", "200", "40", "30"],
        ),
        (
            json!({"global_daily_v5": "150"}),
            r#"{"ok":true,"global_monthly_anlas":12.5,"global_daily_v5":150,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["12.5", "150", "40", "30"],
        ),
        // 12.0 作为整数字段：存 "12"
        (
            json!({"global_monthly_anlas": 7.0, "global_daily_v5": 12.0}),
            r#"{"ok":true,"global_monthly_anlas":7.0,"global_daily_v5":12,"global_daily_anlas":200,"member_daily_anlas":40,"v5_alert_threshold":30,"v5_medium_mode":"off"}"#,
            ["7.0", "12", "40", "30"],
        ),
    ];
    for (body, expect, stored) in ok_cases {
        let (s, t) = api.text("PUT", "/settings", Some(body.clone())).await;
        assert_eq!((s, t.as_str()), (200, expect), "{body}");
        let now = settings(&e).await;
        for (key, want) in [
            "global_monthly_anlas",
            "global_daily_v5",
            "member_daily_anlas",
            "v5_alert_threshold",
        ]
        .iter()
        .zip(stored)
        {
            assert_eq!(now[*key], want, "{body} → {key}");
        }
    }

    // 非法输入：422，原文逐字一致，且什么都没写
    let frozen = settings(&e).await;
    for (body, detail) in [
        (
            json!({"global_monthly_anlas": "abc"}),
            "global_monthly_anlas 必须是有效数字",
        ),
        (
            json!({"global_daily_v5": 1.5}),
            "global_daily_v5 必须是整数",
        ),
        (
            json!({"global_daily_v5": -1}),
            "global_daily_v5 必须在 0～100000 之间",
        ),
        (json!({"global_daily_v5": true}), "global_daily_v5 不能为空"),
        (
            json!({"global_monthly_anlas": 2e6}),
            "global_monthly_anlas 必须在 0.0～1000000.0 之间",
        ),
        (
            json!({"v5_alert_threshold": 0}),
            "V5 余量低提示线必须为 1～100 的整数百分比",
        ),
        (
            json!({"v5_alert_threshold": 20.0}),
            "V5 余量低提示线必须为 1～100 的整数百分比",
        ),
        (
            json!({"v5_alert_threshold": "30"}),
            "V5 余量低提示线必须为 1～100 的整数百分比",
        ),
        (
            json!({"global_monthly_anlas": null}),
            "global_monthly_anlas 不能为空",
        ),
        (
            json!({"global_monthly_anlas": " "}),
            "global_monthly_anlas 不能为空",
        ),
        (
            json!({"member_daily_anlas": 100001}),
            "member_daily_anlas 必须在 0～100000 之间",
        ),
        (
            json!({"global_daily_anlas": 100001}),
            "global_daily_anlas 必须在 0～100000 之间",
        ),
        (
            json!({"global_daily_anlas": -1}),
            "global_daily_anlas 必须在 0～100000 之间",
        ),
        (
            json!({"global_daily_anlas": 1.5}),
            "global_daily_anlas 必须是整数",
        ),
        (
            json!({"global_daily_anlas": null}),
            "global_daily_anlas 不能为空",
        ),
        (
            json!({"global_daily_v5": [1]}),
            "global_daily_v5 必须是有效数字",
        ),
    ] {
        api.fails("PUT", "/settings", Some(body), 422, detail).await;
    }
    assert_eq!(settings(&e).await, frozen);

    // 操作日志：成功记「修改预算」（参数摘要），失败记原因
    let rows = actions(&e).await;
    let (action, target, detail, ok) = last_action(&rows);
    assert_eq!(
        (action, target, ok),
        ("修改预算", "Anlas 与 V5 预算", false)
    );
    assert_eq!(detail, "失败（422）：global_daily_v5 必须是有效数字");
    assert!(rows.iter().any(|r| r.1 == "修改预算"
        && r.3 == r#"{"global_monthly_anlas":5000,"global_daily_v5":200,"member_daily_anlas":40,"v5_alert_threshold":30}"#
        && r.4));
}

#[tokio::test]
async fn global_daily_anlas_pool_is_editable() {
    // Vibe 用的全站每日 Anlas 池（以前后台没有写入口，默认值取环境变量）
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.json("GET", "/settings", None).await.1["global_daily_anlas"],
        200
    );
    for (body, shown, stored) in [
        (json!({"global_daily_anlas": 350}), 350, "350"),
        (json!({"global_daily_anlas": "120"}), 120, "120"),
        // 0 = 不开放 Vibe；12.0 作为整数字段存 "12"
        (json!({"global_daily_anlas": 0}), 0, "0"),
        (json!({"global_daily_anlas": 12.0}), 12, "12"),
        (json!({"global_daily_anlas": 100000}), 100000, "100000"),
    ] {
        let (s, v) = api.json("PUT", "/settings", Some(body.clone())).await;
        assert_eq!(
            (s, &v["global_daily_anlas"]),
            (200, &json!(shown)),
            "{body}"
        );
        assert_eq!(settings(&e).await["global_daily_anlas"], stored, "{body}");
        assert_eq!(
            api.json("GET", "/settings", None).await.1["global_daily_anlas"],
            shown
        );
    }
    // 不带这个字段的保存不会把它改掉
    api.json("PUT", "/settings", Some(json!({"member_daily_anlas": 60})))
        .await;
    assert_eq!(settings(&e).await["global_daily_anlas"], "100000");
    // 准入真的读这个设置：池子是 0 时没有 Anlas 权限的成员用不了 Vibe（见 admission）
    assert_eq!(
        gate_core::site_flags::get_from_raw(
            &gate_core::site_flags::GLOBAL_DAILY_ANLAS,
            settings(&e)
                .await
                .get("global_daily_anlas")
                .map(String::as_str),
            None
        ),
        json!(100000)
    );
}

// ------------------------------------------------------------------ 排队与冷却

#[tokio::test]
async fn runtime_limits_validate_apply_and_persist() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    // 每 Key 出图间隔 key_image_min_interval 已删除：只剩三项，传它算未知设置
    assert_eq!(
        api.text("GET", "/runtime-limits", None).await,
        (
            200,
            r#"{"queue_timeout":5,"image_min_interval":0,"image_429_cooldown_seconds":60}"#.into()
        )
    );
    let before = settings(&e).await;
    for (body, detail) in [
        (json!({}), "包含未知或空的运行限制设置"),
        (json!({"unknown": 15}), "包含未知或空的运行限制设置"),
        (
            json!({"queue_timeout": 0}),
            "queue_timeout 必须是 15～300 的整数",
        ),
        (
            json!({"queue_timeout": true}),
            "queue_timeout 必须是 15～300 的整数",
        ),
        (
            json!({"queue_timeout": 15.0}),
            "queue_timeout 必须是 15～300 的整数",
        ),
        (
            json!({"queue_timeout": "20"}),
            "queue_timeout 必须是 15～300 的整数",
        ),
        (
            json!({"queue_timeout": 301}),
            "queue_timeout 必须是 15～300 的整数",
        ),
        (
            json!({"image_429_cooldown_seconds": 59}),
            "image_429_cooldown_seconds 必须是 60～3600 的整数",
        ),
        (
            json!({"key_image_min_interval": 20}),
            "包含未知或空的运行限制设置",
        ),
        (
            json!({"image_min_interval": 14}),
            "image_min_interval 必须是 15～120 的整数",
        ),
        // 多个字段时，按请求体顺序报第一个非法的；已校验过的也不会写
        (
            json!({"queue_timeout": 20, "image_min_interval": 1}),
            "image_min_interval 必须是 15～120 的整数",
        ),
    ] {
        api.fails("PUT", "/runtime-limits", Some(body), 422, detail)
            .await;
    }
    assert_eq!(settings(&e).await, before);

    let all =
        json!({"queue_timeout": 120, "image_min_interval": 25, "image_429_cooldown_seconds": 90});
    assert_eq!(
        api.text("PUT", "/runtime-limits", Some(all.clone())).await,
        (200, all.to_string())
    );
    let l = e.state.limits();
    assert_eq!(
        (
            l.queue_timeout,
            l.image_min_interval,
            l.image_429_cooldown_seconds
        ),
        (120.0, 25.0, 90.0)
    );
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("runtime_queue_timeout", "120"),
            ("runtime_image_min_interval", "25"),
            ("runtime_image_429_cooldown_seconds", "90"),
        ])
    );
    // 只改一项：响应仍是三项全量
    assert_eq!(
        api.text("PUT", "/runtime-limits", Some(json!({"queue_timeout": 20})))
            .await,
        (
            200,
            r#"{"queue_timeout":20,"image_min_interval":25,"image_429_cooldown_seconds":90}"#
                .into()
        )
    );
    assert_eq!(api.get("/runtime-limits").await["queue_timeout"], 20);

    // 重启后（读回库里保存的值）优先于 .env
    e.state
        .apply_runtime_limits(&[("queue_timeout", 200), ("image_min_interval", 99)])
        .await;
    e.state.load_runtime_limits().await.unwrap();
    assert_eq!(e.state.limits().queue_timeout, 20.0);
    assert_eq!(e.state.limits().image_min_interval, 25.0);

    let rows = actions(&e).await;
    assert!(rows.iter().any(|r| r.1 == "修改排队与冷却"
        && r.2 == "图片排队与冷却"
        && r.3 == all.to_string().replace(' ', "")
        && r.4));
}

// ------------------------------------------------------------------ 账号保护

const GUARD_FIELDS: &str = r#""fields":{"account_daily_cap":{"default":1000,"min":0,"max":20000,"label":"每个上游账号每天最多出图张数"},"daily_burst":{"default":150,"min":0,"max":20000,"label":"每日上限按时段放行：每天重置时（默认 7 点）先放出这么多张，之后每小时补「每日上限 ÷ 24」张，没用完的留到后面；0 = 不分时段（整天先到先得）"},"account_hourly_cap":{"default":150,"min":0,"max":240,"label":"每个上游账号每小时最多出图张数（自动调整：上游 429 减半；前一天顶到过上限且没有 429 才上调一步；下限、上限、步长另有设置）"},"quiet_start":{"default":0,"min":0,"max":23,"label":"安静时段开始（北京时间，整点）；与结束相同 = 不启用（2026-10-10 起不启用：夜里不限速）"},"quiet_end":{"default":0,"min":0,"max":23,"label":"安静时段结束（北京时间，整点；与开始相同表示不设安静时段）"},"quiet_hourly_cap":{"default":20,"min":0,"max":240,"label":"安静时段每小时最多出图张数；0 = 完全暂停出图（2026-10-13 站长：3-7 点停用）"},"interval_jitter":{"default":5,"min":0,"max":30,"label":"两次出图间隔额外随机增加 0～N 秒"},"post_jitter_min":{"default":1,"min":0,"max":30,"label":"出图结束后至少静默几秒才开始下一张（实际静默在最少～最多之间均匀随机，不超过账号出图间隔；与出图间隔取较晚者）"},"post_jitter_max":{"default":3,"min":0,"max":30,"label":"出图结束后最多静默几秒（小于最少时按最少；两项都为 0 = 出图后不静默）"},"key_image_queue":{"default":1,"min":0,"max":3,"label":"每把 Key 在生成中的那张之外，最多再排几张"},"queue_per_account":{"default":5,"min":0,"max":100,"label":"每个上游账号全站最多同时排几张图"},"hourly_min":{"default":100,"min":0,"max":240,"label":"每小时上限自动调整的下限（上游 429 减半后不会低于它）"},"hourly_max":{"default":160,"min":0,"max":240,"label":"每小时上限自动调整的上限（平稳一天后自动上调，最高到这里）"},"hourly_step":{"default":10,"min":0,"max":100,"label":"每小时上限平稳一天后自动上调的步长；0 = 不自动上调"}}"#;

fn guard_json(values: &str, quiet_now: bool, cap: i64, accounts: &str, queued: i64) -> String {
    format!(
        r#"{{"values":{values},{GUARD_FIELDS},"quiet_now":{quiet_now},"hourly_cap_now":{cap},"accounts":{accounts},"queued_images":{queued}}}"#
    )
}

#[tokio::test]
async fn guard_get_put() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let defaults = r#"{"account_daily_cap":1000,"daily_burst":150,"account_hourly_cap":150,"quiet_start":0,"quiet_end":0,"quiet_hourly_cap":20,"interval_jitter":5,"post_jitter_min":1,"post_jitter_max":3,"key_image_queue":1,"queue_per_account":5,"hourly_min":100,"hourly_max":160,"hourly_step":10}"#;
    // 节约模式折算的 today_units 已删除（weight 恒为 1）
    let acct0 = r#"[{"position":1,"usable":true,"today":0,"this_hour":0}]"#;
    assert_eq!(
        api.text("GET", "/guard", None).await,
        (200, guard_json(defaults, false, 150, acct0, 0))
    );

    let before = settings(&e).await;
    let after_cap = r#"{"account_daily_cap":1000,"daily_burst":150,"account_hourly_cap":120,"quiet_start":0,"quiet_end":0,"quiet_hourly_cap":20,"interval_jitter":5,"post_jitter_min":1,"post_jitter_max":3,"key_image_queue":1,"queue_per_account":5,"hourly_min":100,"hourly_max":160,"hourly_step":10}"#;
    assert_eq!(
        api.text("PUT", "/guard", Some(json!({"account_hourly_cap": 120})))
            .await,
        (200, guard_json(after_cap, false, 120, acct0, 0))
    );
    // 不认识的字段忽略；浮点但是整数值（120.0）可以；只写改过的字段
    api.json(
        "PUT",
        "/guard",
        Some(json!({"foo": 1, "quiet_start": 3, "quiet_end": 7, "account_hourly_cap": 120.0})),
    )
    .await;
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("guard_account_hourly_cap", "120"),
            ("guard_quiet_start", "3"),
            ("guard_quiet_end", "7")
        ])
    );
    assert_eq!(e.state.guard.with(|g| g.values["quiet_start"]), 3);

    let label_cap = "每个上游账号每小时最多出图张数（自动调整：上游 429 减半；前一天顶到过上限且没有 429 才上调一步；下限、上限、步长另有设置）";
    let frozen = settings(&e).await;
    for (body, detail) in [
        (
            json!({"account_hourly_cap": 500}),
            format!("{label_cap} 必须在 0～240 之间"),
        ),
        (
            json!({"account_hourly_cap": 1.5}),
            format!("{label_cap} 必须是整数"),
        ),
        (
            json!({"account_hourly_cap": true}),
            format!("{label_cap} 必须是整数"),
        ),
        (
            json!({"account_hourly_cap": "5"}),
            format!("{label_cap} 必须是整数"),
        ),
        (
            json!({"account_daily_cap": -1}),
            "每个上游账号每天最多出图张数 必须在 0～20000 之间".to_string(),
        ),
    ] {
        api.fails("PUT", "/guard", Some(body), 422, &detail).await;
    }
    assert_eq!(settings(&e).await, frozen);

    let rows = actions(&e).await;
    assert!(rows.iter().any(|r| r.1 == "修改账号保护与排队设置"
        && r.2 == "账号保护与排队"
        && r.3 == r#"{"account_hourly_cap":120}"#
        && r.4));
}

/// 出图后静默：后台可调，0～30 秒，保存后上游客户端立即按新值静默。
#[tokio::test]
async fn guard_post_jitter_is_adjustable() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        e.state.guard.post_jitter(),
        (1.0, 3.0),
        "默认与原来的环境变量一致"
    );
    let before = settings(&e).await;
    api.json(
        "PUT",
        "/guard",
        Some(json!({"post_jitter_min": 2, "post_jitter_max": 6})),
    )
    .await;
    assert_eq!(e.state.guard.post_jitter(), (2.0, 6.0));
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("guard_post_jitter_min", "2"),
            ("guard_post_jitter_max", "6")
        ])
    );
    api.fails(
        "PUT",
        "/guard",
        Some(json!({"post_jitter_max": 31})),
        422,
        "出图结束后最多静默几秒（小于最少时按最少；两项都为 0 = 出图后不静默） 必须在 0～30 之间",
    )
    .await;
    // 重启后从库里读回
    e.state.guard.with(|g| {
        g.values.insert("post_jitter_min".into(), 1);
        g.values.insert("post_jitter_max".into(), 3);
    });
    e.state.guard.load().await.unwrap();
    assert_eq!(e.state.guard.post_jitter(), (2.0, 6.0));
}

#[tokio::test]
async fn guard_hourly_adjust_bounds_are_settings() {
    // 每小时上限自动调整的下限 / 上限 / 步长：以前是私密参数文件 private_params.json 的 capacity.hourly_*，
    // 现在和别的保护字段一样在后台改、存 site_settings
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let v = api.get("/guard").await;
    assert_eq!(
        (
            &v["values"]["hourly_min"],
            &v["values"]["hourly_max"],
            &v["values"]["hourly_step"]
        ),
        (&json!(100), &json!(160), &json!(10))
    );
    let before = settings(&e).await;
    let (s, v) = api
        .json(
            "PUT",
            "/guard",
            Some(json!({"hourly_min": 90, "hourly_max": 200, "hourly_step": 20})),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        (
            &v["values"]["hourly_min"],
            &v["values"]["hourly_max"],
            &v["values"]["hourly_step"]
        ),
        (&json!(90), &json!(200), &json!(20))
    );
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("guard_hourly_min", "90"),
            ("guard_hourly_max", "200"),
            ("guard_hourly_step", "20"),
        ])
    );
    // 上游 429：150 减半是 75，被下限 90 托住
    let change = e.state.guard.on_upstream_429(e.state.now()).await.unwrap();
    assert_eq!(change, Some((150, 90)));
    // 下限不能高于上限（只改一边也按现值比）；范围越界照旧 422；什么都没写
    let frozen = settings(&e).await;
    api.fails(
        "PUT",
        "/guard",
        Some(json!({"hourly_min": 230})),
        422,
        "每小时上限自动调整的下限不能高于上限",
    )
    .await;
    api.fails(
        "PUT",
        "/guard",
        Some(json!({"hourly_step": 101})),
        422,
        "每小时上限平稳一天后自动上调的步长；0 = 不自动上调 必须在 0～100 之间",
    )
    .await;
    assert_eq!(settings(&e).await, frozen);
}

#[tokio::test]
async fn guard_accounts_show_usage() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let id = e.state.nai.tokens().await[0].token_id.clone();
    e.state
        .db
        .bump_upstream_image_counter(&id, &e.state.day(), 3)
        .await
        .unwrap();
    e.state.guard.record_start(&id, 2, e.state.now());
    e.state.guard.with(|g| g.image_inflight.insert(7, 2));
    let v = api.get("/guard").await;
    assert_eq!(
        v["accounts"],
        json!([{"position": 1, "usable": true, "today": 3, "this_hour": 2}])
    );
    assert_eq!(v["queued_images"], 2);
}

// ------------------------------------------------------------------ Discord 机器人

#[tokio::test]
async fn discord_bot_corrupt_store_is_a_server_error() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    // 库里的心跳不是合法 JSON：旧版 json.loads 抛异常没人接，500
    e.state
        .db
        .set_setting(gate_core::bot_config::STATUS_KEY, "not json")
        .await
        .unwrap();
    assert_eq!(api.text("GET", "/discord-bot", None).await.0, 500);
    // 是 JSON 但不是对象
    e.state
        .db
        .set_setting(gate_core::bot_config::STATUS_KEY, "[1]")
        .await
        .unwrap();
    assert_eq!(api.text("GET", "/discord-bot", None).await.0, 500);
}

/// 后台只剩「机器人状态」：跑图分享设置和最近动作删了（机器人从未实现），
/// 库里残留的 `bot_gallery_*` / `bot_events` 行不再被读（哪怕是坏的），也没有写设置的接口。
#[tokio::test]
async fn discord_bot_status_only() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.text("GET", "/discord-bot", None).await,
        (200, r#"{"status":{"online":false}}"#.to_string())
    );

    for (key, value) in [
        ("bot_gallery_like", "1"),
        ("bot_gallery_forum", "作品"),
        ("bot_events", "not json"),
    ] {
        e.state.db.set_setting(key, value).await.unwrap();
    }
    assert_eq!(
        api.text("GET", "/discord-bot", None).await,
        (200, r#"{"status":{"online":false}}"#.to_string())
    );

    // 修改设置的接口没有了
    let frozen = settings(&e).await;
    let (s, _) = api
        .text("PUT", "/discord-bot", Some(json!({"gallery_like": 1})))
        .await;
    assert_eq!(s, 405);
    assert_eq!(settings(&e).await, frozen);

    // 机器人心跳：在线判断
    let now = e.state.now();
    let status = gate_core::bot_config::status_to_store(
        json!({"user": "奶妹", "guild": "测试服", "latency_ms": 42})
            .as_object()
            .unwrap(),
        now,
    );
    e.state
        .db
        .set_setting(gate_core::bot_config::STATUS_KEY, &status)
        .await
        .unwrap();
    let v = api.get("/discord-bot").await;
    assert_eq!(v["status"]["online"], true);
    assert_eq!(v["status"]["user"], "奶妹");
    assert_eq!(v.as_object().unwrap().len(), 1, "只有 status：{v}");
    // 3 分钟没心跳 → 离线
    e.state
        .db
        .set_setting(
            gate_core::bot_config::STATUS_KEY,
            &gate_core::bot_config::status_to_store(&Map::new(), now - 600.0),
        )
        .await
        .unwrap();
    assert_eq!(api.get("/discord-bot").await["status"]["online"], false);
}

// ------------------------------------------------------------------ 领 Key / 功能 / 记录

fn ops_fixed(v: &Value) -> Value {
    json!({
        "registration": v["registration"], "features": v["features"], "new_key": v["new_key"],
        "audit": v["audit"],
    })
}

#[tokio::test]
async fn ops_snapshot_defaults() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let v = api.get("/ops").await;
    assert_eq!(
        ops_fixed(&v),
        json!({
            "registration": {"configured": false, "open": false, "max_users": 0, "features": null, "daily_images": 30,
                // 新 Key 默认不过期
                "daily_v5": 0, "image_scope": "legacy", "expires_days": 0, "role_guild": "", "role_id": "", "role_note": "",
                "active": 0},
            "features": {
                "global": {"image": true, "upscale": true, "augment": true, "vibe": true, "tags": true},
                "labels": {"image": "文生图", "upscale": "放大", "augment": "导演工具（线稿、上色等）", "vibe": "Vibe 编码",
                    "tags": "标签补全"},
                "new_key": ["image"],
            },
            // 后台「新建 Key」弹窗里没有数据库设置的默认值
            "new_key": {"rpm": 10, "daily_anlas": 0.0, "monthly_anlas": 2500.0},
            "audit": {"prompts": false, "thumbs": false, "retention_days": 7, "image_cap_mb": 8192,
                "image_bytes": 0, "image_keep_hours": 24, "announce_configured": false},
        })
    );
    assert_eq!(
        v["upstream"],
        json!({"status": "idle", "recent": 0, "failed": 0, "image_cooldown_seconds": 0})
    );
    // 键顺序
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["registration", "features", "new_key", "audit", "upstream"]
    );
    let reg_keys: Vec<&str> = v["registration"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        reg_keys,
        [
            "configured",
            "open",
            "max_users",
            "features",
            "daily_images",
            "daily_v5",
            "image_scope",
            "expires_days",
            "role_guild",
            "role_id",
            "role_note",
            "active",
        ]
    );
}

#[tokio::test]
async fn ops_registration_put() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let reg = |v: &Value| v["registration"].clone();

    let before = settings(&e).await;
    let (s, v) = api
        .json("PUT", "/ops/registration", Some(json!({"open": true})))
        .await;
    assert_eq!((s, reg(&v)["open"].clone()), (200, json!(true)));
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[("register_open", "1")])
    );

    // 动态额度开着（默认）：新 Key 的每日张数 / V5 / 模型范围归算法管，400、什么都不写
    let before = settings(&e).await;
    let (s, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"max_users": 20, "daily_v5": 15, "image_scope": "all"})),
        )
        .await;
    assert_eq!(
        (s, v),
        (
            400,
            json!({"detail": "daily_v5、image_scope 由动态额度管理：关闭动态额度后才能手动修改"})
        )
    );
    assert_eq!(settings(&e).await, before);
    e.state
        .db
        .set_setting("quota_auto_enabled", "0")
        .await
        .unwrap();

    let before = settings(&e).await;
    let (_, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"max_users": 20, "daily_images": 50, "daily_v5": 15, "expires_days": 10, "image_scope": "all"})),
        )
        .await;
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("register_max_users", "20"),
            ("register_daily_images", "50"),
            ("register_daily_v5", "15"),
            ("register_expires_days", "10"),
            ("register_image_scope", "all"),
        ])
    );
    // 名额 20 × 每人 V5 15 = 300 > 全站 V5 日限 150：不再有「超卖」提示，
    // 注册设置和预算接口都不带 v5_capacity
    assert!(reg(&v).get("v5_capacity").is_none());
    assert!(api.get("/settings").await.get("v5_capacity").is_none());

    // 功能清单：列表 / null（全部）/ 空列表（一个都不开）
    let feats = |v: &Value| (reg(v)["features"].clone(), v["features"]["new_key"].clone());
    let (_, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"features": ["tags", "image", "vibe"]})),
        )
        .await;
    assert_eq!(
        feats(&v),
        (
            json!(["image", "vibe", "tags"]),
            json!(["image", "vibe", "tags"])
        )
    );
    assert_eq!(settings(&e).await["register_features"], "image,vibe,tags");
    // 文本生成 / 语音已不是功能：和别的未知名字一样 422，库里的值不变
    api.fails(
        "PUT",
        "/ops/registration",
        Some(json!({"features": ["image", "text"]})),
        422,
        "features 必须是功能名列表",
    )
    .await;
    assert_eq!(settings(&e).await["register_features"], "image,vibe,tags");
    let (_, v) = api
        .json("PUT", "/ops/registration", Some(json!({"features": null})))
        .await;
    assert_eq!(feats(&v), (json!(null), json!(null)));
    assert_eq!(settings(&e).await["register_features"], "*");
    let (_, v) = api
        .json("PUT", "/ops/registration", Some(json!({"features": []})))
        .await;
    assert_eq!(
        feats(&v),
        (json!([]), json!(["image"])),
        "没开任何功能时，新建 Key 弹窗仍默认文生图"
    );
    assert_eq!(settings(&e).await["register_features"], "");

    // 限定身份组：成对填写（合成 ID）
    let (_, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"role_guild": "900000000000000001", "role_id": "900000000000000002", "role_note": " 创作者 "})),
        )
        .await;
    assert_eq!(
        (
            reg(&v)["role_guild"].clone(),
            reg(&v)["role_id"].clone(),
            reg(&v)["role_note"].clone()
        ),
        (
            json!("900000000000000001"),
            json!("900000000000000002"),
            json!("创作者")
        )
    );
    assert_eq!(settings(&e).await["register_role_note"], "创作者");
    // 只改一半：与库里另一半比较
    api.fails(
        "PUT",
        "/ops/registration",
        Some(json!({"role_guild": ""})),
        422,
        "限定身份组的服务器 ID 与身份组 ID 必须同时填写，或全部留空",
    )
    .await;
    let (s, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"role_guild": "", "role_id": ""})),
        )
        .await;
    assert_eq!((s, reg(&v)["role_id"].clone()), (200, json!("")));
    // 备注最多 60 字；image_scope 不是 all 一律 legacy；open 按真假值
    let (_, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"role_note": "x".repeat(100), "image_scope": "foo", "open": 0})),
        )
        .await;
    assert_eq!(reg(&v)["role_note"].as_str().unwrap().len(), 60);
    assert_eq!(
        (reg(&v)["image_scope"].clone(), reg(&v)["open"].clone()),
        (json!("legacy"), json!(false))
    );
    // 数字按 Python int() 取整；bool 是 int
    let (_, v) = api
        .json("PUT", "/ops/registration", Some(json!({"max_users": 7.9})))
        .await;
    assert_eq!(reg(&v)["max_users"], 7);
    let (_, v) = api
        .json(
            "PUT",
            "/ops/registration",
            Some(json!({"max_users": true, "notify": false})),
        )
        .await;
    assert_eq!(reg(&v)["max_users"], 1);

    // 校验失败：文案与旧版一致（含 Python 自己的 int() 报错原文），且什么都没写
    let frozen = settings(&e).await;
    for (body, detail) in [
        (json!({"max_users": 5000}), "max_users 超出范围 0～1000"),
        (
            json!({"daily_images": -1}),
            "daily_images 超出范围 0～100000",
        ),
        (
            json!({"expires_days": 3651}),
            "expires_days 超出范围 0～3650",
        ),
        (
            json!({"max_users": "x"}),
            "invalid literal for int() with base 10: 'x'",
        ),
        (
            json!({"max_users": null}),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
        ),
        (
            json!({"max_users": [1]}),
            "int() argument must be a string, a bytes-like object or a real number, not 'list'",
        ),
        (
            json!({"role_id": "abc"}),
            "role_id 需要是 Discord 的数字 ID",
        ),
        (
            json!({"role_guild": "123"}),
            "role_guild 需要是 Discord 的数字 ID",
        ),
        (json!({"features": ["bogus"]}), "features 必须是功能名列表"),
        (json!({"features": "image"}), "features 必须是功能名列表"),
        (json!({"features": [1]}), "features 必须是功能名列表"),
    ] {
        api.fails("PUT", "/ops/registration", Some(body), 422, detail)
            .await;
    }
    assert_eq!(settings(&e).await, frozen);

    let rows = actions(&e).await;
    let (action, target, detail, ok) = last_action(&rows);
    assert_eq!(
        (action, target, ok),
        ("修改开放注册设置", "自助领 Key 设置", false)
    );
    assert_eq!(detail, "失败（422）：features 必须是功能名列表");
}

#[tokio::test]
async fn ops_registration_defaults_are_constants_not_env() {
    let secret = "x".repeat(32);
    let e = setup(&[
        ("DISCORD_CLIENT_ID", "1"),
        ("DISCORD_CLIENT_SECRET", "synthetic"),
        ("DISCORD_BOT_TOKEN", "synthetic"),
        ("REGISTRATION_BRIDGE_SECRET", &secret),
        ("DISCORD_GUILD_ID", "900000000000000001"),
        ("SITE_URL", "https://owl.example"),
        // 这些环境变量 3.0 已删除（每日张数 / 模型范围归动态额度管；
        // 名额 / 有效期 / 功能只在后台「领 Key」里设）：设了也不读
        ("REGISTER_MAX_USERS", "12"),
        ("REGISTER_DAILY_IMAGES", "40"),
        ("REGISTER_DAILY_V5", "5"),
        ("REGISTER_IMAGE_SCOPE", "all"),
        ("REGISTER_EXPIRES_DAYS", "14"),
        ("REGISTER_FEATURES", "image,vibe"),
    ])
    .await;
    let api = Api::login(&e).await;
    // 登记两个已领取的成员（一个 Key 未过期）→ 占用名额 2
    for i in 1..=2_i64 {
        let key = e
            .state
            .db
            .create_key(
                &serde_json::from_value(
                    json!({"name": format!("m{i}"), "token": format!("nai-synthetic-{i:0>30}"),
                    "daily_images": 5, "monthly_anlas": 0.0, "daily_text_tokens": 0, "rpm": 5}),
                )
                .unwrap(),
                e.state.now(),
            )
            .await
            .unwrap();
        let kid = key["id"].as_i64().unwrap();
        e.state
            .db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO discord_registrations(discord_id, key_id, created_at, username, display_name) VALUES (?,?,?,?,?)",
                    (format!("90000000000000010{i}"), kid, 1.0_f64, "u", "d"),
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let v = api.get("/ops").await;
    assert_eq!(
        v["registration"],
        json!({"configured": true, "open": false, "max_users": 10, "features": ["image"], "daily_images": 30,
               "daily_v5": 0, "image_scope": "legacy", "expires_days": 0, "role_guild": "", "role_id": "", "role_note": "",
               "active": 2})
    );
    // 站长在后台改过的值优先于 defaults 里的常量
    api.json("PUT", "/ops/registration", Some(json!({"max_users": 3})))
        .await;
    let v = api.get("/ops").await;
    assert_eq!(
        (
            v["registration"]["max_users"].clone(),
            v["registration"]["daily_images"].clone()
        ),
        (json!(3), json!(30))
    );
}

#[tokio::test]
async fn ops_features_put() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let (s, v) = api
        .json(
            "PUT",
            "/ops/features",
            Some(json!({"flags": {"upscale": false}})),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["features"]["global"]["upscale"], false);
    assert_eq!(
        settings(&e).await["features_global"],
        r#"{"image": true, "upscale": false, "augment": true, "vibe": true, "tags": true}"#
    );
    // 多项一起改，库里始终按功能顺序、Python 的默认分隔符
    api.json(
        "PUT",
        "/ops/features",
        Some(json!({"flags": {"tags": false, "image": true}})),
    )
    .await;
    assert_eq!(
        settings(&e).await["features_global"],
        r#"{"image": true, "upscale": false, "augment": true, "vibe": true, "tags": false}"#
    );
    let frozen = settings(&e).await;
    for body in [
        json!({"flags": {}}),
        json!({"flags": {"bogus": true}}),
        json!({"flags": {"upscale": 1}}),
        // 文本生成 / 语音已不是功能
        json!({"flags": {"text": false}}),
        json!({"flags": {"voice": true}}),
        json!({"flags": [1]}),
        json!({}),
    ] {
        api.fails(
            "PUT",
            "/ops/features",
            Some(body),
            422,
            "flags 必须是 {功能名: true/false}",
        )
        .await;
    }
    assert_eq!(settings(&e).await, frozen);
    let rows = actions(&e).await;
    assert!(rows.iter().any(|r| r.1 == "修改全局功能开关"
        && r.2 == "全站功能开关"
        && r.3 == r#"{"flags":{"upscale":false}}"#
        && r.4));
}

#[tokio::test]
async fn ops_features_corrupt_store_is_a_server_error() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    e.state
        .db
        .set_setting("features_global", "[1]")
        .await
        .unwrap();
    let (s, _) = api.text("GET", "/ops", None).await;
    assert_eq!(s, 500);
}

#[tokio::test]
async fn ops_audit_put() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let audit = |v: &Value| v["audit"].clone();

    let before = settings(&e).await;
    let (s, v) = api
        .json("PUT", "/ops/audit", Some(json!({"prompts": true})))
        .await;
    assert_eq!((s, audit(&v)["prompts"].clone()), (200, json!(true)));
    // 每次都把记录相关的全部开关写回（含默认值）
    assert_eq!(
        changed(&before, &settings(&e).await),
        map(&[
            ("audit_prompts", "1"),
            ("audit_thumbs", "0"),
            ("audit_retention_days", "7"),
            ("audit_image_cap_mb", "8192"),
            ("audit_image_keep_hours", "24"),
        ])
    );
    // 夹到范围内；浮点取整
    let (_, v) = api
        .json("PUT", "/ops/audit", Some(json!({"thumbs": 1, "retention_days": 99999, "image_cap_mb": -5, "image_keep_hours": 100000})))
        .await;
    assert_eq!(
        audit(&v),
        json!({"prompts": true, "thumbs": true, "retention_days": 3650, "image_cap_mb": 0,
               "image_bytes": 0, "image_keep_hours": 8760, "announce_configured": false})
    );
    let s = settings(&e).await;
    assert_eq!(
        [
            &s["audit_retention_days"],
            &s["audit_image_cap_mb"],
            &s["audit_image_keep_hours"],
        ],
        ["3650", "0", "8760"]
    );
    api.json(
        "PUT",
        "/ops/audit",
        Some(json!({"image_cap_mb": 10.9, "prompts": 0})),
    )
    .await;
    let s = settings(&e).await;
    assert_eq!(
        (
            s["audit_image_cap_mb"].as_str(),
            s["audit_prompts"].as_str()
        ),
        ("10", "0")
    );

    let frozen = settings(&e).await;
    for body in [
        json!({"retention_days": "x"}),
        json!({"retention_days": null}),
        json!({"image_keep_hours": [1]}),
    ] {
        api.fails("PUT", "/ops/audit", Some(body), 422, "参数无效")
            .await;
    }
    assert_eq!(settings(&e).await, frozen);
    // 空请求体：不改任何值，操作日志对象为空（旧版没有为 /ops/audit 登记对象名）
    api.json("PUT", "/ops/audit", Some(json!({}))).await;
    let rows = actions(&e).await;
    let (action, target, detail, ok) = last_action(&rows);
    assert_eq!(
        (action, target, detail, ok),
        ("修改生成记录设置", "", "", true)
    );
}

// ------------------------------------------------------------------ 首页加入按钮（已删除）

#[tokio::test]
async fn home_join_switch_is_gone() {
    // 首页「加入 Discord」按钮跟着领 Key 开关（registration.open），不再有独立开关
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    for method in ["GET", "POST"] {
        let (s, _, _) = api
            .raw(method, "/home/join", Some(json!({"open": false})), &[])
            .await;
        assert!(matches!(s, 404 | 405), "{method} {s}");
    }
    assert!(!settings(&e).await.contains_key("home_join_open"));
}

// ------------------------------------------------------------------ 公告与告警

/// 站长告警私信发到的频道（假 Discord 建私信频道时回这个 id）。
const OWNER_DM: &str = "/channels/900000000000000030/messages";

/// 告警私信接到本地假 Discord：机器人 Token + 站长 ID，建私信频道回固定 id。
fn owner_dm(hook: &MockUpstream) -> Vec<(&'static str, &'static str)> {
    hook.push(
        "/users/@me/channels",
        Reply::json(200, &json!({"id": "900000000000000030"})),
    );
    let base: &'static str = Box::leak(hook.url().into_boxed_str());
    vec![
        ("DISCORD_BOT_TOKEN", "synthetic-bot"),
        ("DISCORD_API_BASE", base),
        ("ALERT_USER_ID", "900000000000000031"),
    ]
}

#[tokio::test]
async fn announcement_file_roundtrip() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.text("GET", "/announcement", None).await,
        (200, r#"{"html":""}"#.into())
    );
    assert_eq!(
        api.text(
            "PUT",
            "/announcement",
            Some(json!({"html": "<p>你好\r\n世界</p>"}))
        )
        .await,
        (200, r#"{"ok":true}"#.into())
    );
    // 文件里是原样；读出来按通用换行（与 Python read_text 一致）
    assert_eq!(
        std::fs::read_to_string(e.state.env.announcement_path()).unwrap(),
        "<p>你好\r\n世界</p>"
    );
    assert_eq!(
        api.text("GET", "/announcement", None).await,
        (200, r#"{"html":"<p>你好\n世界</p>"}"#.into())
    );
    // 非字符串按 str() 存；缺省存空串；超过 20000 字符截断
    api.json("PUT", "/announcement", Some(json!({"html": 12345})))
        .await;
    assert_eq!(api.get("/announcement").await["html"], "12345");
    api.json("PUT", "/announcement", Some(json!({}))).await;
    assert_eq!(api.get("/announcement").await["html"], "");
    api.json(
        "PUT",
        "/announcement",
        Some(json!({"html": "猫".repeat(25000)})),
    )
    .await;
    assert_eq!(
        api.get("/announcement").await["html"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        20000
    );
    // 操作日志不记公告正文
    let rows = actions(&e).await;
    assert!(
        rows.iter().any(|r| r.1 == "修改站点公告"
            && r.2 == "站点公告"
            && r.3 == r#"{"html":"（已隐去）"}"#)
    );
}

#[tokio::test]
async fn announcement_send_validation_and_unconfigured() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    api.fails(
        "POST",
        "/announcement/send",
        Some(json!({"text": "  "})),
        422,
        "公告内容不能为空",
    )
    .await;
    api.fails(
        "POST",
        "/announcement/send",
        Some(json!({})),
        422,
        "公告内容不能为空",
    )
    .await;
    api.fails(
        "POST",
        "/announcement/send",
        Some(json!({"text": "x".repeat(1801)})),
        422,
        "公告太长（Discord 单条 2000 字符，留了余量）",
    )
    .await;
    // 恰好 1800 字符过长度检查；测试环境没配公告频道（也没有机器人 Token）→ 503，不会发给 Discord
    api.fails(
        "POST",
        "/announcement/send",
        Some(json!({"text": "猫".repeat(1800)})),
        503,
        "公告频道未配置（.env 的 ANNOUNCE_CHANNEL_ID）",
    )
    .await;
    let rows = actions(&e).await;
    let (action, _, detail, ok) = last_action(&rows);
    assert_eq!((action, ok), ("发送公告", false));
    assert_eq!(
        detail,
        "失败（503）：公告频道未配置（.env 的 ANNOUNCE_CHANNEL_ID）"
    );
}

#[tokio::test]
async fn alerts_test_unconfigured_then_throttled() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    api.fails(
        "POST",
        "/alerts/test",
        None,
        409,
        "尚未配置告警私信（DISCORD_BOT_TOKEN 与 ALERT_USER_ID）",
    )
    .await;
    // 配了私信（Discord 指向本地假服务）：发一条，10 秒内再发被节流
    let hook = MockUpstream::start().await;
    let e = setup(&owner_dm(&hook)).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.text("POST", "/alerts/test", None).await,
        (200, r#"{"ok":true}"#.into())
    );
    settle().await;
    let sent = hook.requests_to(OWNER_DM);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].json(),
        json!({"content": "⚠ **猫头鹰公益站告警**（test）\n这是一条测试告警，收到说明告警渠道正常。",
               "allowed_mentions": {"parse": []}})
    );
    let (s, t) = api.text("POST", "/alerts/test", None).await;
    assert_eq!(s, 429);
    assert!(
        t == r#"{"detail":"测试告警发送过于频繁，请等待 9 秒后再试"}"#
            || t == r#"{"detail":"测试告警发送过于频繁，请等待 10 秒后再试"}"#,
        "{t}"
    );
    settle().await;
    assert_eq!(hook.requests_to(OWNER_DM).len(), 1, "被节流的请求不会再发");
    // 操作日志里是「发送测试告警」
    let rows = actions(&e).await;
    assert_eq!(
        rows.iter()
            .filter(|r| r.1 == "发送测试告警")
            .map(|r| r.4)
            .collect::<Vec<_>>(),
        [true, false]
    );
}

// ------------------------------------------------------------------ 上游 Token

const SUBSCRIPTION_OK: &str = "/user/subscription";

fn pool_ids(v: &Value) -> Vec<String> {
    v["pool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["token_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn upstream_token_settings_endpoints() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let id = e.state.nai.tokens().await[0].token_id.clone();
    assert_eq!(id, "token-12d24769535a51a7", "token_id 与旧版同一算法");
    let v5 = format!("/upstream-tokens/{id}/v5-limit");
    let conc = format!("/upstream-tokens/{id}/image-concurrency");
    let en = format!("/upstream-tokens/{id}/enabled");

    let msg5 = "上游 V5 日限额必须是 0～100000 的整数（0 为不限）";
    for bad in [
        json!(-1),
        json!(100001),
        json!(1.5),
        json!("12"),
        json!(true),
        json!(null),
    ] {
        api.fails("PUT", &v5, Some(json!({"v5_daily_limit": bad})), 422, msg5)
            .await;
    }
    api.fails("PUT", &v5, Some(json!({})), 422, msg5).await;
    api.fails(
        "PUT",
        "/upstream-tokens/token-unknown/v5-limit",
        Some(json!({"v5_daily_limit": 12})),
        404,
        "上游 Token 不存在",
    )
    .await;
    // 路径里的百分号编码会被解码（操作日志的对象名也是解码后的）
    api.fails(
        "PUT",
        "/upstream-tokens/%E4%BD%A0%E5%A5%BD/v5-limit",
        Some(json!({"v5_daily_limit": 12})),
        404,
        "上游 Token 不存在",
    )
    .await;
    assert!(actions(&e).await.iter().any(|r| r.2 == "上游 你好" && !r.4));
    assert_eq!(
        api.text("PUT", &v5, Some(json!({"v5_daily_limit": 12})))
            .await,
        (200, r#"{"ok":true,"v5_daily_limit":12}"#.into())
    );
    assert_eq!(e.state.nai.tokens().await[0].fields().v5_daily_limit, 12);
    assert_eq!(
        e.state.db.get_upstream_token_limits().await.unwrap(),
        vec![(id.clone(), 12)]
    );

    let msgc = "上游图片并发必须是 1～4 的整数";
    for bad in [json!(0), json!(5), json!(1.5), json!("2"), json!(true)] {
        api.fails(
            "PUT",
            &conc,
            Some(json!({"image_concurrency": bad})),
            422,
            msgc,
        )
        .await;
    }
    api.fails(
        "PUT",
        "/upstream-tokens/token-unknown/image-concurrency",
        Some(json!({"image_concurrency": 2})),
        404,
        "上游 Token 不存在",
    )
    .await;
    assert_eq!(
        api.text("PUT", &conc, Some(json!({"image_concurrency": 2})))
            .await,
        (200, r#"{"ok":true,"image_concurrency":2}"#.into())
    );
    assert_eq!(e.state.nai.tokens().await[0].image_slots.limit(), 2);
    assert_eq!(
        e.state
            .db
            .get_upstream_token_image_concurrency()
            .await
            .unwrap(),
        vec![(id.clone(), 2)]
    );

    for bad in [json!(0), json!(1), json!("false"), json!(null)] {
        api.fails(
            "PUT",
            &en,
            Some(json!({"enabled": bad})),
            422,
            "enabled 必须是布尔值",
        )
        .await;
    }
    api.fails(
        "PUT",
        "/upstream-tokens/token-unknown/enabled",
        Some(json!({"enabled": false})),
        404,
        "上游 Token 不存在",
    )
    .await;
    assert_eq!(
        api.text("PUT", &en, Some(json!({"enabled": false}))).await,
        (200, r#"{"ok":true,"enabled":false}"#.into())
    );
    assert!(!e.state.nai.tokens().await[0].fields().admin_enabled);
    assert_eq!(
        e.state.db.get_upstream_token_enabled().await.unwrap(),
        vec![(id.clone(), false)]
    );
    // 令牌池状态反映这些设置（总览里）
    let ov = api.get("/overview").await;
    assert_eq!(
        ov["pool"],
        json!([{"token_id": id, "position": 1, "token": "pst-sy…oken", "usable": false, "admin_enabled": false,
                "disabled": false, "fails": 0, "blocked_for": 0, "last_ok": 0.0, "allow_anlas": true,
                "image_concurrency": 2, "image_active": 0, "v5_daily_limit": 12, "v5_used": 0, "images_today": 0}])
    );
    assert_eq!(
        api.text("PUT", &en, Some(json!({"enabled": true}))).await.0,
        200
    );
    assert!(e.state.nai.tokens().await[0].fields().admin_enabled);

    let rows = actions(&e).await;
    let find = |a: &str, ok: bool| {
        rows.iter()
            .any(|r| r.1 == a && r.2 == format!("上游 {id}") && r.4 == ok)
    };
    assert!(find("修改上游 Token V5 日限", true) && find("修改上游 Token V5 日限", false));
    assert!(find("修改上游 Token 图片并发", true) && find("启用/停用上游 Token", true));
    assert!(rows.iter().any(|r| r.1 == "修改上游 Token V5 日限"
        && r.2 == "上游 token-unknown"
        && r.3 == "失败（404）：上游 Token 不存在"));
}

#[tokio::test]
async fn upstream_token_add_replace_remove() {
    let hook = MockUpstream::start().await;
    let e = setup(&owner_dm(&hook)).await;
    let api = Api::login(&e).await;
    let first = e.state.nai.tokens().await[0].token_id.clone();
    let file = e.state.env.upstream_tokens_path();
    assert!(
        !file.exists(),
        "后台没接管过：令牌池来自 NAI_TOKENS，没有 Token 文件"
    );

    // 格式不对：还没向 NovelAI 验证就拒绝
    let fmt = "Token 格式不对：应以 pst- 开头（在 NovelAI 的 账号设置 → Get Persistent API Token 里获取）";
    for body in [
        json!({}),
        json!({"token": "abc"}),
        json!({"token": "pst-short"}),
        json!({"token": 5}),
        json!({"token": null}),
    ] {
        api.fails("POST", "/upstream-tokens", Some(body), 422, fmt)
            .await;
    }
    assert!(e.mock.requests_to(SUBSCRIPTION_OK).is_empty());
    // 官方说无效
    e.mock.push(
        SUBSCRIPTION_OK,
        Reply::json(401, &json!({"statusCode": 401})),
    );
    api.fails(
        "POST",
        "/upstream-tokens",
        Some(json!({"token": "pst-bad-token-value-0001"})),
        422,
        "NovelAI 拒绝了这把 Token（无效，或已被重置）",
    )
    .await;
    // 订阅无效 / 官方 5xx / 看不懂
    e.mock.push(
        SUBSCRIPTION_OK,
        Reply::json(200, &json!({"tier": 3, "active": false})),
    );
    api.fails(
        "POST",
        "/upstream-tokens",
        Some(json!({"token": "pst-bad-token-value-0001"})),
        422,
        "这把 Token 对应的账号没有有效订阅",
    )
    .await;
    e.mock.push(SUBSCRIPTION_OK, Reply::status(503, "down"));
    api.fails(
        "POST",
        "/upstream-tokens",
        Some(json!({"token": "pst-bad-token-value-0001"})),
        422,
        "NovelAI 返回 503，暂时无法验证",
    )
    .await;
    // 已在池里
    api.fails(
        "POST",
        "/upstream-tokens",
        Some(json!({"token": "pst-synthetic-upstream-token"})),
        409,
        "这把 Token 已经在令牌池里",
    )
    .await;
    assert_eq!(e.state.nai.tokens().await.len(), 1);
    assert!(!file.exists());

    // 添加（首尾空白被去掉）
    e.mock.clear();
    let (s, t) = api
        .text(
            "POST",
            "/upstream-tokens",
            Some(json!({"token": " pst-test-added-token-0001 ", "allow_anlas": false})),
        )
        .await;
    assert_eq!(s, 200);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(
        (v["ok"].clone(), v["tier"].clone()),
        (json!(true), json!(3))
    );
    assert!(
        !t.contains("pst-test-added-token-0001") && !t.contains("pst-synthetic-upstream-token"),
        "响应里没有完整 Token"
    );
    let ids = pool_ids(&v);
    assert_eq!(ids.len(), 2);
    assert_eq!(v["pool"][1]["token"], "pst-te…0001");
    assert_eq!(
        (
            v["pool"][1]["position"].clone(),
            v["pool"][1]["allow_anlas"].clone()
        ),
        (json!(2), json!(false))
    );
    // 验证请求带着这把 Token
    let seen = e.mock.requests_to(SUBSCRIPTION_OK);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].header("authorization"),
        Some("Bearer pst-test-added-token-0001")
    );
    // Token 文件：权限 600，完整 Token 只在这里
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(&file).unwrap()).unwrap(),
        json!({"tokens": [{"token": "pst-synthetic-upstream-token", "allow_anlas": true},
                          {"token": "pst-test-added-token-0001", "allow_anlas": false}]})
    );
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // 站长私信：变更提醒
    settle().await;
    let alerts: Vec<String> = hook
        .requests_to(OWNER_DM)
        .iter()
        .map(|r| r.json()["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains("（upstream_token_changed）\n后台添加了一把上游 Token（订阅等级 3）。如果这不是你操作的，请立刻更换后台密码。"), "{alerts:?}");

    // 替换第二把：V5 日限 / 并发 / 启停 / 当天计数延续到新标识
    let second = ids[1].clone();
    api.json(
        "PUT",
        &format!("/upstream-tokens/{second}/v5-limit"),
        Some(json!({"v5_daily_limit": 7})),
    )
    .await;
    api.json(
        "PUT",
        &format!("/upstream-tokens/{second}/enabled"),
        Some(json!({"enabled": false})),
    )
    .await;
    e.state
        .db
        .bump_upstream_image_counter(&second, &e.state.day(), 2)
        .await
        .unwrap();
    let (s, t) = api
        .text(
            "PUT",
            &format!("/upstream-tokens/{second}"),
            Some(json!({"token": "pst-test-replaced-token-0002"})),
        )
        .await;
    assert_eq!(s, 200);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert!(!t.contains("pst-test-replaced-token-0002"));
    let ids2 = pool_ids(&v);
    assert_eq!(
        (ids2[0].as_str(), ids2[1].as_str() != second),
        (first.as_str(), true)
    );
    assert_eq!(v["pool"][1]["token"], "pst-te…0002");
    assert_eq!(
        (
            v["pool"][1]["v5_daily_limit"].clone(),
            v["pool"][1]["admin_enabled"].clone(),
            v["pool"][1]["images_today"].clone(),
            v["pool"][1]["allow_anlas"].clone()
        ),
        (json!(7), json!(false), json!(2), json!(false))
    );
    assert_eq!(
        e.state.db.get_upstream_token_limits().await.unwrap(),
        vec![(ids2[1].clone(), 7)],
        "库里的设置从旧标识搬到新标识"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(&file).unwrap()).unwrap()["tokens"]
            [1],
        json!({"token": "pst-test-replaced-token-0002", "allow_anlas": false})
    );
    // 旧标识已不存在 → 404；和池里另一把重复 → 409；官方不认 → 422
    api.fails(
        "PUT",
        &format!("/upstream-tokens/{second}"),
        Some(json!({"token": "pst-test-replaced-token-0003"})),
        404,
        "上游 Token 不存在",
    )
    .await;
    api.fails(
        "PUT",
        "/upstream-tokens/token-unknown",
        Some(json!({"token": "pst-test-replaced-token-0003"})),
        404,
        "上游 Token 不存在",
    )
    .await;
    api.fails(
        "PUT",
        &format!("/upstream-tokens/{first}"),
        Some(json!({"token": "pst-test-replaced-token-0002"})),
        409,
        "这把 Token 已经在令牌池的另一个位置",
    )
    .await;
    e.mock.push(SUBSCRIPTION_OK, Reply::json(403, &json!({})));
    api.fails(
        "PUT",
        &format!("/upstream-tokens/{first}"),
        Some(json!({"token": "pst-bad-token-value-0001"})),
        422,
        "NovelAI 拒绝了这把 Token（无效，或已被重置）",
    )
    .await;

    // 删除
    api.fails(
        "DELETE",
        "/upstream-tokens/token-unknown",
        None,
        404,
        "上游 Token 不存在",
    )
    .await;
    let (s, t) = api
        .text("DELETE", &format!("/upstream-tokens/{}", ids2[1]), None)
        .await;
    assert_eq!(s, 200);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(
        (v["ok"].clone(), pool_ids(&v)),
        (json!(true), vec![first.clone()])
    );
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(&file).unwrap()).unwrap(),
        json!({"tokens": [{"token": "pst-synthetic-upstream-token", "allow_anlas": true}]})
    );
    // 至少保留一把（即使要删的不存在，也是这个提示——与旧版一致）
    api.fails(
        "DELETE",
        &format!("/upstream-tokens/{first}"),
        None,
        409,
        "至少需要保留一把上游 Token",
    )
    .await;
    api.fails(
        "DELETE",
        "/upstream-tokens/token-unknown",
        None,
        409,
        "至少需要保留一把上游 Token",
    )
    .await;

    // 操作日志：完整 Token 不会出现；对象是「上游 <标识前 24 位>」
    let rows = actions(&e).await;
    for r in &rows {
        assert!(
            !format!("{r:?}").contains("pst-test-"),
            "操作日志里不能有 Token：{r:?}"
        );
    }
    assert!(rows.iter().any(|r| r.1 == "添加上游 Token"
        && r.2.is_empty()
        && r.3 == r#"{"token":"（已隐去）","allow_anlas":false}"#
        && r.4));
    assert!(rows.iter().any(|r| r.1 == "替换上游 Token"
        && r.2 == format!("上游 {second}")
        && r.3 == r#"{"token":"（已隐去）"}"#
        && r.4));
    assert!(rows.iter().any(|r| r.1 == "删除上游 Token"
        && r.2 == format!("上游 {}", ids2[1])
        && r.3.is_empty()
        && r.4));
    assert!(rows.iter().any(|r| r.1 == "删除上游 Token"
        && r.3 == "失败（409）：至少需要保留一把上游 Token"
        && !r.4));
}

#[tokio::test]
async fn upstream_token_body_is_limited_to_4k() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    api.fails(
        "POST",
        "/upstream-tokens",
        Some(json!({"token": "p".repeat(5000)})),
        413,
        "请求体过大",
    )
    .await;
}

// ------------------------------------------------------------------ Anlas 核对

async fn csrf(api: &Api<'_>) -> String {
    api.get("/reconciliation").await["csrf_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn reconciliation_requests_are_guarded() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let (s, h, t) = api.raw("GET", "/reconciliation", None, &[]).await;
    assert_eq!(s, 200);
    assert_eq!(h["cache-control"], "no-store");
    let v: Value = serde_json::from_str(&t).unwrap();
    let token = v["csrf_token"].as_str().unwrap().to_string();
    assert_eq!(
        (
            v["history"].clone(),
            v["retry_after"].clone(),
            v["running"].clone(),
            token.len()
        ),
        (json!([]), json!(0), json!(false), 64)
    );
    assert!(
        t.rfind("csrf_token").unwrap() > t.find("running").unwrap(),
        "键顺序：history, retry_after, running, csrf_token"
    );
    // 同一会话令牌稳定；换一个会话就变
    assert_eq!(csrf(&api).await, token);
    let other = Api::login(&e).await;
    if other.cookie != api.cookie {
        assert_ne!(csrf(&other).await, token);
    }

    // 缺 / 错的 CSRF 头：403，什么也没查
    api.fails(
        "POST",
        "/reconciliation",
        Some(json!({})),
        403,
        "会话校验失败，请刷新后台后重试",
    )
    .await;
    api.fails(
        "POST",
        "/reconciliation",
        Some(json!({})),
        403,
        "会话校验失败，请刷新后台后重试",
    )
    .await;
    let wrong = "0".repeat(64);
    let (s, _, t) = api
        .raw(
            "POST",
            "/reconciliation",
            Some(json!({})),
            &[("x-nai-admin-csrf", wrong.as_str())],
        )
        .await;
    assert_eq!(
        (s, t.as_str()),
        (403, r#"{"detail":"会话校验失败，请刷新后台后重试"}"#)
    );
    // 另一个会话的令牌不能拿来用
    if other.cookie != api.cookie {
        let theirs = csrf(&other).await;
        let (s, _, _) = api
            .raw(
                "POST",
                "/reconciliation",
                Some(json!({})),
                &[("x-nai-admin-csrf", theirs.as_str())],
            )
            .await;
        assert_eq!(s, 403);
    }
    let h = [("x-nai-admin-csrf", token.as_str())];
    // 内容类型必须是 JSON
    let (s, _, t) = api
        .raw(
            "POST",
            "/reconciliation",
            Some(json!({})),
            &[h[0], ("content-type", "text/plain")],
        )
        .await;
    assert_eq!(
        (s, t.as_str()),
        (415, r#"{"detail":"请使用 JSON 请求"}"#),
        "多个 content-type 头时取第一个"
    );
    // 不接受任何自定义内容
    let (s, _, t) = api
        .raw(
            "POST",
            "/reconciliation",
            Some(json!({"url": "https://untrusted.invalid"})),
            &h,
        )
        .await;
    assert_eq!(
        (s, t.as_str()),
        (400, r#"{"detail":"本操作不接受自定义账号或地址"}"#)
    );
    let (s, _, t) = api
        .raw("POST", "/reconciliation", Some(json!([])), &h)
        .await;
    assert_eq!(
        (s, t.as_str()),
        (400, r#"{"detail":"请求体必须是 JSON 对象"}"#)
    );
    let (s, _, t) = api
        .raw(
            "POST",
            "/reconciliation",
            Some(json!({"big": "a".repeat(1100)})),
            &h,
        )
        .await;
    assert_eq!((s, t.as_str()), (413, r#"{"detail":"请求体过大"}"#));
    let (s, _, t) = api
        .raw(
            "POST",
            "/reconciliation",
            None,
            &[h[0], ("content-type", "application/json")],
        )
        .await;
    assert_eq!(
        (s, t.as_str()),
        (400, r#"{"detail":"请求体不是合法 JSON"}"#)
    );
    let (s, _, t) = api.raw("POST", "/reconciliation", None, &h).await;
    assert_eq!(
        (s, t.as_str()),
        (415, r#"{"detail":"请使用 JSON 请求"}"#),
        "没有 Content-Type 也是 415"
    );
    assert!(
        e.mock.requests_to(SUBSCRIPTION_OK).is_empty(),
        "校验没过之前不能去查官方"
    );
    assert_eq!(
        api.get("/reconciliation").await["retry_after"],
        0,
        "也没有写冷却"
    );

    let rows = actions(&e).await;
    assert!(
        rows.iter()
            .all(|r| r.1 == "Anlas 核对" || r.1 == "登录后台")
    );
    let forbidden = rows
        .iter()
        .filter(|r| r.1 == "Anlas 核对" && r.3 == "失败（403）：会话校验失败，请刷新后台后重试")
        .count();
    assert_eq!(forbidden, 3 + usize::from(other.cookie != api.cookie));
}

#[tokio::test]
async fn reconciliation_compares_balances() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let token = csrf(&api).await;
    let h = [("x-nai-admin-csrf", token.as_str())];
    let id = e.state.nai.tokens().await[0].token_id.clone();

    let (s, hd, t) = api
        .raw("POST", "/reconciliation", Some(json!({})), &h)
        .await;
    assert_eq!(s, 200);
    assert_eq!(hd["cache-control"], "no-store");
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["running"], false);
    assert_eq!(v["csrf_token"], token);
    let (retry, first) = (v["retry_after"].as_i64().unwrap(), v["history"][0].clone());
    assert!(
        (59..=60).contains(&retry),
        "每次核对先写 60 秒冷却：{retry}"
    );
    assert_eq!(first["id"], 1);
    assert_eq!(
        first["accounts"],
        json!({ id.clone(): {"fixed": 10000, "purchased": 0, "expires_at": 1893456000} })
    );
    assert_eq!(
        first["ledger"],
        json!({"anlas": 0, "unconfirmed_anlas": 0, "unconfirmed_requests": 0, "last_log_id": 0})
    );
    assert_eq!(
        (first["balance"].clone(), first["comparison"].clone()),
        (json!(10000), json!({"reason": "first"}))
    );
    // 键顺序与旧版快照一致
    let keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "checked_at",
            "accounts",
            "ledger",
            "balance",
            "comparison",
            "id"
        ]
    );
    assert!(first["checked_at"].is_f64());
    // 查询带的是这把上游 Token
    let q = e.mock.requests_to(SUBSCRIPTION_OK);
    assert_eq!(q.len(), 1);
    assert_eq!(
        q[0].header("authorization"),
        Some("Bearer pst-synthetic-upstream-token")
    );
    // 冷却中：429 + Retry-After，不再查官方
    let (s, hd, t) = api
        .raw("POST", "/reconciliation", Some(json!({})), &h)
        .await;
    assert_eq!(s, 429);
    let secs: i64 = hd["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&secs));
    assert_eq!(
        t,
        json!({"detail": format!("请在 {secs} 秒后重试")}).to_string()
    );
    assert_eq!(e.mock.requests_to(SUBSCRIPTION_OK).len(), 1);

    // 冷却过后：账上记了 7 Anlas、官方余额少了 10 → 差 3；未确认 3 笔里 1 个请求
    e.state
        .db
        .set_setting(gate_server::reconciliation::COOLDOWN_SETTING, "0")
        .await
        .unwrap();
    let key = e.state.db.get_key_by_token(TOKEN).await.unwrap().unwrap();
    let kid = key["id"].as_i64().unwrap();
    e.state
        .db
        .record_success(
            &Success {
                key_id: kid,
                key_name: "测试成员".into(),
                kind: "image".into(),
                model: "nai-diffusion-4-5-full".into(),
                day: e.state.day(),
                images: 1,
                anlas: 7.0,
                ..Default::default()
            },
            e.state.now(),
        )
        .await
        .unwrap();
    e.state
        .db
        .add_log(
            &NewLog {
                key_id: Some(kid),
                key_name: "测试成员".into(),
                kind: "image".into(),
                status: "error".into(),
                unconfirmed_anlas: 3.0,
                ..Default::default()
            },
            e.state.now(),
        )
        .await
        .unwrap();
    // 官方：余额减少 10
    e.mock.push(
        SUBSCRIPTION_OK,
        Reply::json(200, &json!({"active": true, "expiresAt": 1893456000, "trainingStepsLeft": {"fixedTrainingStepsLeft": 9990, "purchasedTrainingSteps": 0}})),
    );
    let (s, _, t) = api
        .raw("POST", "/reconciliation", Some(json!({})), &h)
        .await;
    assert_eq!(s, 200, "{t}");
    let v: Value = serde_json::from_str(&t).unwrap();
    let second = v["history"][0].clone();
    assert_eq!(v["history"][1]["id"], 1);
    assert_eq!(
        second["comparison"],
        json!({"reason": null, "since": first["checked_at"], "balance_decrease": 10, "recorded": 7.0, "difference": 3.0,
               "unconfirmed_anlas": 3.0, "unconfirmed_requests": 1})
    );
    // 整数仍是整数、浮点仍是浮点（页面按原样显示）
    assert!(
        t.contains(r#""balance_decrease":10,"recorded":7.0,"difference":3.0,"#),
        "{t}"
    );
    // 完整 Token 不在任何响应里
    assert!(!t.contains("pst-synthetic-upstream-token"));

    // 官方出错：不推进基线，不泄露响应；失败的尝试也进了冷却
    for (reply, why) in [
        (Reply::json(401, &json!({"token": "leak-me"})), "401"),
        (Reply::status(500, "leak-me"), "500"),
        (Reply::status(200, "not-json"), "not-json"),
        (
            Reply::json(
                200,
                &json!({"active": true, "trainingStepsLeft": {"fixedTrainingStepsLeft": -1, "purchasedTrainingSteps": 0}}),
            ),
            "负余额",
        ),
        (Reply::json(200, &json!({"active": false})), "未订阅"),
    ] {
        e.state
            .db
            .set_setting(gate_server::reconciliation::COOLDOWN_SETTING, "0")
            .await
            .unwrap();
        e.mock.push(SUBSCRIPTION_OK, reply);
        let (s, _, t) = api
            .raw("POST", "/reconciliation", Some(json!({})), &h)
            .await;
        assert_eq!(
            (s, t.as_str()),
            (503, r#"{"detail":"官方余额未能完整读取；上次结果已保留"}"#),
            "{why}"
        );
        let st = api.get("/reconciliation").await;
        assert_eq!(st["history"].as_array().unwrap().len(), 2, "{why}");
        assert!(st["retry_after"].as_i64().unwrap() > 0, "{why}");
    }
}

#[tokio::test]
async fn reconciliation_official_rate_limit_is_remembered() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let token = csrf(&api).await;
    let h = [("x-nai-admin-csrf", token.as_str())];
    for (header, want) in [
        ("600", 600),
        ("5", 300),
        ("999999", 86400),
        ("invalid", 300),
    ] {
        e.state
            .db
            .set_setting(gate_server::reconciliation::COOLDOWN_SETTING, "0")
            .await
            .unwrap();
        e.mock.push(SUBSCRIPTION_OK, Reply::rate_limited(header));
        let (s, hd, t) = api
            .raw("POST", "/reconciliation", Some(json!({})), &h)
            .await;
        assert_eq!(s, 429, "{header}");
        assert_eq!(hd["retry-after"], want.to_string().as_str(), "{header}");
        assert_eq!(
            t,
            r#"{"detail":"官方查询限流，请稍后重试；上次结果已保留"}"#
        );
        let st = api.get("/reconciliation").await;
        assert!(
            (want - 2..=want).contains(&st["retry_after"].as_i64().unwrap()),
            "{header}: {st}"
        );
        assert_eq!(st["history"], json!([]));
    }
}

#[tokio::test]
async fn reconciliation_refuses_when_account_is_unusable() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let token = csrf(&api).await;
    let h = [("x-nai-admin-csrf", token.as_str())];
    // 账号被上游 401 判了失效 → 不查
    let t = e.state.nai.tokens().await[0].clone();
    e.state.nai.mark_unauthorized(&t);
    let (s, _, body) = api
        .raw("POST", "/reconciliation", Some(json!({})), &h)
        .await;
    assert_eq!(
        (s, body.as_str()),
        (
            503,
            r#"{"detail":"有上游账号已失效，请先检查令牌池；尚未查询官方"}"#
        )
    );
    assert!(e.mock.requests_to(SUBSCRIPTION_OK).is_empty());
    assert_eq!(api.get("/reconciliation").await["retry_after"], 0);
}

#[tokio::test]
async fn reconciliation_waits_for_inflight_reservations_and_blocks_admission() {
    use gate_server::state::Reservation;
    let e = setup(&[]).await;
    e.state.budget.lock().await.reservations.insert(
        1,
        Reservation {
            key_id: 1,
            exclude_global_v5: false,
            anlas: 0.0,
            v5: 0,
            legacy: 0,
        },
    );
    // 预留还在：等不到 → 409，也没有查官方、没有写冷却
    let busy =
        gate_server::reconciliation::run_with(&e.state, std::time::Duration::from_millis(150))
            .await;
    match busy {
        Err(gate_server::reconciliation::RunError::Refused(r)) => {
            assert_eq!(
                (r.status, r.message.as_str()),
                (409, "图片任务仍在处理，请稍后核对；尚未查询官方")
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(e.mock.requests_to(SUBSCRIPTION_OK).is_empty());
    assert_eq!(
        gate_server::reconciliation::retry_after(&e.state)
            .await
            .unwrap(),
        0
    );

    // 预留清空后立刻继续（不用等满超时）；运行期间 running = true，预算锁被持着
    let state = e.state.clone();
    let task = tokio::spawn(async move {
        gate_server::reconciliation::run_with(&state, std::time::Duration::from_secs(10)).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!task.is_finished());
    assert!(e.mock.requests_to(SUBSCRIPTION_OK).is_empty());
    {
        let mut b = e.state.budget.lock().await;
        b.reservations.clear();
        e.state.budget_idle.notify_waiters();
    }
    let v = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(v["history"][0]["comparison"], json!({"reason": "first"}));
    assert_eq!(v["running"], false);
    assert_eq!(e.mock.requests_to(SUBSCRIPTION_OK).len(), 1);
}

#[tokio::test]
async fn reconciliation_only_one_at_a_time() {
    let e = setup(&[]).await;
    e.mock.push(SUBSCRIPTION_OK, Reply::Hang);
    let state = e.state.clone();
    let first = tokio::spawn(async move { gate_server::reconciliation::run(&state).await });
    for _ in 0..50 {
        if !e.mock.requests_to(SUBSCRIPTION_OK).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(e.mock.requests_to(SUBSCRIPTION_OK).len(), 1);
    // 正在核对：状态里 running = true；再点一次 → 409
    let st = gate_server::reconciliation::status(&e.state).await.unwrap();
    assert_eq!(st["running"], true);
    match gate_server::reconciliation::run(&e.state).await {
        Err(gate_server::reconciliation::RunError::Refused(r)) => {
            assert_eq!(
                (r.status, r.message.as_str()),
                (409, "已有核对正在进行，请稍后查看结果")
            );
        }
        other => panic!("{other:?}"),
    }
    // 取消（调用方断开）：锁都释放，历史里没有半条
    first.abort();
    let _ = first.await;
    let st = gate_server::reconciliation::status(&e.state).await.unwrap();
    assert_eq!(
        (st["running"].clone(), st["history"].clone()),
        (json!(false), json!([]))
    );
    assert!(e.state.budget.try_lock().is_ok());
    assert!(st["retry_after"].as_i64().unwrap() > 0);
}

// ------------------------------------------------------------------ 总览 / 状态 / 监控

/// 往库里灌一批合成数据：成员出图、文本（文本接口删除前留下的旧记录）、拒绝、上游 429，外加内存里的状态码计数。
async fn seed_usage(e: &Env) -> i64 {
    let db = &e.state.db;
    let key = db.get_key_by_token(TOKEN).await.unwrap().unwrap();
    let kid = key["id"].as_i64().unwrap();
    let now = e.state.now();
    let day = e.state.day();
    let ok = |kind: &str, model: &str, images: i64, v5: i64, tokens: i64, dur: i64| Success {
        key_id: kid,
        key_name: "测试成员".into(),
        kind: kind.into(),
        model: model.into(),
        day: day.clone(),
        images,
        v5,
        tokens,
        wait_ms: 1000,
        dur_ms: dur,
        up_status: 200,
        ..Default::default()
    };
    db.record_success(&ok("image", "nai-diffusion-4-5-full", 2, 0, 0, 8000), now)
        .await
        .unwrap();
    db.record_success(&ok("image", "nai-diffusion-5-full", 1, 1, 0, 9000), now)
        .await
        .unwrap();
    db.record_success(&ok("text", "kayra", 0, 0, 120, 0), now)
        .await
        .unwrap();
    let log = |status: &str, up: i64, detail: &str, reason: &str| NewLog {
        key_id: Some(kid),
        key_name: "测试成员".into(),
        kind: "image".into(),
        model: "nai-diffusion-4-5-full".into(),
        status: status.into(),
        up_status: up,
        detail: detail.into(),
        reason: reason.into(),
        ..Default::default()
    };
    db.add_log(&log("rejected", 0, "额度用完", "daily_quota"), now)
        .await
        .unwrap();
    db.add_log(&log("error", 429, "上游 429", ""), now)
        .await
        .unwrap();
    for code in [200, 200, 200, 429, 500] {
        e.state.status_stats.record(code, now);
    }
    kid
}

#[tokio::test]
async fn overview_reports_today_week_pool_and_feature_usage() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let empty = api.get("/overview").await;
    assert_eq!(empty["log_since"], Value::Null);
    assert_eq!(empty["pool_configured"], true);
    let keys: Vec<&str> = empty
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "today",
            "active_members",
            "members_total",
            "keys_total",
            "keys_active",
            "week",
            "month",
            "pool",
            "pool_configured",
            "anlas_budget",
            "v5_limit",
            "feature_usage",
            "log_since"
        ]
    );
    seed_usage(&e).await;
    let v = api.get("/overview").await;
    assert_eq!(
        v["today"],
        json!({"images": 3, "member_images": 3, "anlas": 0.0, "text_tokens": 120, "requests": 3, "v5": 1,
               "unconfirmed_requests": 0, "unconfirmed_anlas": 0.0})
    );
    assert_eq!(
        (
            v["keys_total"].clone(),
            v["keys_active"].clone(),
            v["members_total"].clone(),
            v["active_members"].clone()
        ),
        (json!(1), json!(1), json!(1), json!(1))
    );
    let week = v["week"].as_array().unwrap();
    assert_eq!(week.len(), 7);
    assert_eq!(week[6]["day"], e.state.day().as_str());
    assert_eq!(
        (week[6]["images"].clone(), week[6]["requests"].clone()),
        (json!(3), json!(3))
    );
    assert_eq!(week[0]["images"], 0);
    assert_eq!(
        (v["anlas_budget"].clone(), v["v5_limit"].clone()),
        (json!(10000.0), json!(150))
    );
    assert!(v["log_since"].is_f64());
    let usage = |id: &str| {
        v["feature_usage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        usage("image"),
        json!({"id": "image", "label": "文生图",
               "today": {"ok": 2, "error": 1, "rejected": 1, "images": 3, "tokens": 0, "anlas": 0.0},
               "week": {"ok": 2, "error": 1, "rejected": 1, "images": 3, "tokens": 0, "anlas": 0.0}})
    );
    // 旧的文本记录不再归到任何功能下（文本生成 / 语音已删），总览的今日合计里照旧有
    let ids: Vec<&str> = v["feature_usage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["image", "upscale", "augment", "vibe", "tags"]);
    // 全站预算改了（全站 V5 要先关掉动态额度才能手改），总览跟着变
    e.state
        .db
        .set_setting("quota_auto_enabled", "0")
        .await
        .unwrap();
    api.json(
        "PUT",
        "/settings",
        Some(json!({"global_monthly_anlas": 4000, "global_daily_v5": 99})),
    )
    .await;
    let v = api.get("/overview").await;
    assert_eq!(
        (v["anlas_budget"].clone(), v["v5_limit"].clone()),
        (json!(4000.0), json!(99))
    );
}

#[tokio::test]
async fn status_codes_distribution() {
    async fn sc(api: &Api<'_>, q: &str) -> Value {
        api.get(&format!("/status-codes{q}")).await
    }
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    seed_usage(&e).await;
    let v = api.get("/status-codes").await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "hours",
            "since",
            "total",
            "codes",
            "upstream",
            "upstream_total",
            "reasons",
            "reasons_total"
        ]
    );
    assert_eq!(
        v["codes"],
        json!([
            {"code": 200, "n": 3, "pct": 60.0, "meaning": "成功"},
            {"code": 429, "n": 1, "pct": 20.0, "meaning": "太频繁 / 排队满 / 每小时上限"},
            {"code": 500, "n": 1, "pct": 20.0, "meaning": "服务器返回 500（NovelAI 返回 500 时网关原样转给成员，不一定是网关自己出错）"},
        ])
    );
    assert_eq!(v["total"], 5);
    // 上游实际返回的状态码：3 次成功日志记的是 200，拒绝的是 0，错误的是 429（次数相同的先后顺序不作要求）
    let up = v["upstream"].as_array().unwrap();
    assert_eq!(
        up[0],
        json!({"code": 200, "n": 3, "pct": 60.0, "meaning": "NovelAI 正常返回"})
    );
    let mut rest = up[1..].to_vec();
    rest.sort_by_key(|r| r["code"].as_i64());
    assert_eq!(
        rest,
        vec![
            json!({"code": 0, "n": 1, "pct": 20.0, "meaning": "没发到 NovelAI（网关自己拦下、排队超时或客户端断开）"}),
            json!({"code": 429, "n": 1, "pct": 20.0, "meaning": "NovelAI 限流"}),
        ]
    );
    assert_eq!(v["upstream_total"], 5);
    // 拒绝与失败原因：认识的码用码的短说明；没有码、或码已不认识（旧机制留下的）的按详情首句归类
    assert_eq!(v["reasons_total"], 2);
    let mut rs: Vec<(String, String, i64)> = v["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["label"].as_str().unwrap().to_string(),
                r["status"].as_str().unwrap().to_string(),
                r["n"].as_i64().unwrap(),
            )
        })
        .collect();
    rs.sort();
    assert_eq!(
        rs,
        vec![
            ("额度用完".to_string(), "rejected".to_string(), 1),
            ("上游 429".to_string(), "error".to_string(), 1),
        ]
    );
    // 范围参数
    let h = |q: &'static str| sc(&api, q);
    let today = h("").await;
    let hours = today["hours"].as_i64().unwrap();
    assert!(
        (1..=48).contains(&hours),
        "今日：从服务日当天本地 0 点起算的小时数 {hours}"
    );
    assert_eq!(today["since"].as_i64().unwrap() % 3600, 0);
    assert_eq!(h("?hours=today").await["since"], today["since"]);
    assert_eq!(h("?hours=0").await["since"], today["since"]);
    assert_eq!(h("?hours=abc").await["hours"], 24, "读不懂 → 24");
    assert_eq!(h("?hours=99999").await["hours"], 720, "最多 30 天");
    assert_eq!(h("?hours=-5").await["hours"], 1, "至少 1");
    let one = h("?hours=%2012%20").await;
    assert_eq!(one["hours"], 12, "带空白的数字也认");
    assert_eq!(one["since"].as_i64().unwrap() % 3600, 0);
    // 1 小时窗口只含当前整点之后的计数
    assert_eq!(h("?hours=1").await["total"], 5);
    // 落库后的计数也算上（不重复计）
    let drained = e.state.status_stats.drain();
    e.state.db.flush_status_hourly(&drained).await.unwrap();
    assert_eq!(
        h("?hours=24").await["codes"][0],
        json!({"code": 200, "n": 3, "pct": 60.0, "meaning": "成功"})
    );
    e.state.status_stats.record(404, e.state.now());
    assert_eq!(h("?hours=24").await["total"], 6);
}

#[tokio::test]
async fn server_status_reports_alerts_audit_protection_disk() {
    // 防爆破的几个数是代码常量（原来是环境变量）：环境里设了也不读
    let e = setup(&[("AUTH_FAIL_MAX", "7"), ("LOGIN_MAX_ATTEMPTS", "9")]).await;
    let api = Api::login(&e).await;
    let v = api.get("/status").await;
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["alerts", "audit", "upstream", "protection", "disk"]);
    assert_eq!(v["alerts"], json!({"configured": false, "sent": 0}));
    assert_eq!(
        v["audit"],
        json!({"prompts": false, "thumbs": false, "retention_days": 7, "image_cap_mb": 8192,
               "image_bytes": 0, "image_keep_hours": 24})
    );
    assert_eq!(
        v["upstream"],
        json!({"status": "idle", "recent": 0, "failed": 0, "image_cooldown_seconds": 0})
    );
    assert_eq!(
        v["protection"],
        json!({"auth_fail_max": 20, "auth_fail_window": 600, "auth_block_seconds": 900, "login_max_attempts": 5})
    );
    let (free, total) = (
        v["disk"]["free_mb"].as_i64().unwrap(),
        v["disk"]["total_mb"].as_i64().unwrap(),
    );
    assert!(0 < free && free <= total, "{v}");
    // 上游近况与冷却
    for _ in 0..4 {
        e.state.record_upstream(false);
    }
    e.state.block_image_generation(90.0).await;
    let v = api.get("/status").await;
    assert_eq!(
        (
            v["upstream"]["status"].clone(),
            v["upstream"]["failed"].clone(),
            v["upstream"]["recent"].clone()
        ),
        (json!("degraded"), json!(4), json!(4))
    );
    assert!((88..=90).contains(&v["upstream"]["image_cooldown_seconds"].as_i64().unwrap()));
    // 记录开关改了，状态里跟着变
    api.json("PUT", "/ops/audit", Some(json!({"prompts": true})))
        .await;
    let v = api.get("/status").await;
    assert_eq!(v["audit"]["prompts"], json!(true));
}

#[tokio::test]
async fn perf_report_from_usage_log() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let empty = api.get("/perf").await;
    assert_eq!(empty["families"], json!({}));
    assert_eq!(empty["flags"], json!([]));
    assert_eq!(
        (
            empty["slots"].clone(),
            empty["site_interval"].clone(),
            empty["key_interval"].clone()
        ),
        (json!(1), json!(0.0), json!(0.0))
    );
    assert!(empty["generated_at"].is_f64());
    seed_usage(&e).await;
    let v = api.get("/perf").await;
    let fams: Vec<&str> = v["families"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fams, ["V4.5", "V5"]);
    let h1 = &v["families"]["V4.5"]["h1"];
    // V4.5：一次成功（2 张）+ 一次上游 429（拒绝的那条不算：只统计 ok / error）
    assert_eq!(
        (
            h1["requests"].clone(),
            h1["ok"].clone(),
            h1["images"].clone(),
            h1["rate_limited"].clone(),
            h1["samples"].clone(),
            h1["p50_ms"].clone()
        ),
        (
            json!(2),
            json!(1),
            json!(2),
            json!(1),
            json!(1),
            json!(8000)
        )
    );
    assert_eq!(v["families"]["V5"]["h1"]["images"], 1);
    assert_eq!(v["families"]["V4.5"]["baseline_ready"], false);
    let hourly = v["families"]["V4.5"]["hourly"].as_array().unwrap();
    assert_eq!(hourly.len(), 24);
    assert_eq!(
        (
            hourly[23]["ok"].clone(),
            hourly[23]["err"].clone(),
            hourly[23]["r429"].clone(),
            hourly[23]["images"].clone()
        ),
        (json!(1), json!(1), json!(1), json!(2))
    );
    // 产能按「全站每小时上限 × 可用账号」封顶（默认每小时 150）
    assert_eq!(h1["capacity_per_hour"], 150);
}

#[tokio::test]
async fn allowance_snapshot_is_read_only() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.text("GET", "/allowance", None).await,
        (
            200,
            r#"{"threshold":20,"accounts":[{"account":1,"percent":null,"recharge_per_day":null,"is_negative":null,"checked_at":null,"low":false,"uncertain":true,"error":null,"stale":true}],"mode":"on_demand"}"#.into()
        )
    );
    // 查看额度不会去问 NovelAI（只有真实出图会刷新缓存）
    assert!(e.mock.requests_to(SUBSCRIPTION_OK).is_empty());
    // 告警阈值来自预算设置
    api.json("PUT", "/settings", Some(json!({"v5_alert_threshold": 35})))
        .await;
    assert_eq!(api.get("/allowance").await["threshold"], 35);
}

#[tokio::test]
async fn v5_medium_mode_is_validated_stored_and_audited() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    assert_eq!(
        api.json("GET", "/settings", None).await.1["v5_medium_mode"],
        "off"
    );
    // 三个合法值；写进库里的就是原文
    for mode in ["on", "auto", "off"] {
        let (s, v) = api
            .json("PUT", "/settings", Some(json!({"v5_medium_mode": mode})))
            .await;
        assert_eq!((s, &v["v5_medium_mode"]), (200, &json!(mode)), "{v}");
        assert_eq!(settings(&e).await["v5_medium_mode"], mode);
        assert_eq!(
            api.json("GET", "/settings", None).await.1["v5_medium_mode"],
            mode
        );
    }
    // 只改别的字段：模式保持原值
    api.json("PUT", "/settings", Some(json!({"v5_medium_mode": "auto"})))
        .await;
    let (s, v) = api
        .json("PUT", "/settings", Some(json!({"member_daily_anlas": 41})))
        .await;
    assert_eq!((s, &v["v5_medium_mode"]), (200, &json!("auto")));
    // 非法值：422，什么都不写（包括同一个请求里别的合法字段）
    let frozen = settings(&e).await;
    for bad in [
        json!("ON"),
        json!("Auto"),
        json!(" on"),
        json!(""),
        json!("medium"),
        json!(true),
        json!(1),
        json!(null),
        json!(["on"]),
    ] {
        api.fails(
            "PUT",
            "/settings",
            Some(json!({"v5_medium_mode": bad, "member_daily_anlas": 7})),
            422,
            "v5_medium_mode 只能是 off / on / auto",
        )
        .await;
    }
    assert_eq!(settings(&e).await, frozen);
    // 库里被写坏的值按 off 显示（fail-closed）
    e.state
        .db
        .set_setting("v5_medium_mode", "garbage")
        .await
        .unwrap();
    assert_eq!(
        api.json("GET", "/settings", None).await.1["v5_medium_mode"],
        "off"
    );
    // 操作日志：照常记「修改预算」，摘要里带着模式
    let rows = actions(&e).await;
    assert!(
        rows.iter()
            .any(|r| r.1 == "修改预算" && r.3 == r#"{"v5_medium_mode":"on"}"# && r.4)
    );
}

#[tokio::test]
async fn v5_medium_state_reports_lowest_percent_for_auto() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    // 还没有读数：不生效，最低余量是 null
    api.json("PUT", "/settings", Some(json!({"v5_medium_mode": "auto"})))
        .await;
    let st = api.json("GET", "/settings", None).await.1["v5_medium_state"].clone();
    assert_eq!(
        st,
        json!({"active": false, "lowest_percent": null, "threshold": 20})
    );
    // 手动开：直接生效
    api.json("PUT", "/settings", Some(json!({"v5_medium_mode": "on"})))
        .await;
    let st = api.json("GET", "/settings", None).await.1["v5_medium_state"].clone();
    assert_eq!(st["active"], true);
}
