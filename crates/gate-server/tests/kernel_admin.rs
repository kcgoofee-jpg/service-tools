//! 内核的后台接口：`quota-algo`（GET / PUT）、`autopilot`、`modules`（GET / PUT {name} / POST run）。
//! 形状、状态码、文案、写进库的设置与操作日志对照旧版 `app/admin.py`；数值算法本身由 `kernel_parity` 对拍。
//! 公开仓库：全部是合成数据；上游是本地假服务。

mod common;
use common::*;
use futures::future::BoxFuture;
use gate_db::repo::keys::NewKey;
use gate_server::kernel::{KernelRegistrar, RegistrarSettings};
use serde_json::{Value, json};
use std::sync::Arc;

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

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
        let url = format!("/admin/api{path}");
        let mut call = Call::new(method, &url).header("cookie", self.cookie.clone());
        if let Some(b) = body {
            let len = serde_json::to_vec(&b).unwrap().len();
            call = call.header("content-length", len.to_string()).json(b);
        }
        let (status, _, bytes) = call.send(&self.env.state).await;
        (status.as_u16(), String::from_utf8(bytes).unwrap())
    }

    async fn json(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let (s, text) = self.call(method, path, body).await;
        (s, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

async fn member(env: &Env, id: i64) {
    let key: NewKey = serde_json::from_value(json!({
        "name": format!("成员{id:02}"), "token": format!("nai-synthetic-{id:04}"), "daily_images": 100,
        "daily_v5": 10, "monthly_anlas": 0.0, "daily_text_tokens": 1000, "rpm": 60,
    }))
    .unwrap();
    env.state
        .db
        .create_key(&key, env.state.now())
        .await
        .unwrap();
}

async fn setting(env: &Env, key: &str) -> Option<String> {
    env.state.db.get_setting(key).await.unwrap()
}

#[tokio::test]
async fn kernel_routes_need_admin_login() {
    let e = setup(&[]).await;
    for (method, path) in [
        ("GET", "/admin/api/quota-algo"),
        ("PUT", "/admin/api/quota-algo"),
        ("GET", "/admin/api/autopilot"),
        ("GET", "/admin/api/modules"),
        ("PUT", "/admin/api/modules/capacity"),
        ("POST", "/admin/api/modules/run"),
    ] {
        let (status, _, body) = Call::new(method, path).json(json!({})).send(&e.state).await;
        assert_eq!(status, 401, "{method} {path}");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"detail": "未登录或会话已过期"})
        );
    }
}

#[tokio::test]
async fn quota_algo_get_shape_and_put_validation() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;

    // 什么都没存过：params 是代码里的起点值（整数）；last 是 {}，history 是 []
    // （保底的 quota_base / quota_base_min / quota_base_step 已删除，docs/v3-deviations.md #61；
    // V5 的 quota_v5_min / quota_v5_max 随 3～30 夹取删除，#63）
    let (status, text) = api.call("GET", "/quota-algo", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        text,
        r#"{"last":{},"history":[],"params":{"quota_auto_enabled":1,"quota_target_avg":150,"quota_ceiling_max":300,"quota_base_min_fair":10,"quota_step":25}}"#
    );

    // 校验失败：422 + 旧版文案，不写任何东西
    for (body, msg) in [
        (json!({"quota_step": null}), "quota_step 不能为空"),
        (json!({"quota_step": true}), "quota_step 不能为空"),
        (json!({"quota_step": "  "}), "quota_step 不能为空"),
        (json!({"quota_step": "abc"}), "quota_step 必须是有效数字"),
        (json!({"quota_step": 2.5}), "quota_step 必须是整数"),
        (json!({"quota_step": 0}), "quota_step 必须在 1～500 之间"),
        (
            json!({"quota_auto_enabled": 2}),
            "quota_auto_enabled 必须在 0～1 之间",
        ),
        (
            json!({"quota_v5_max": 100, "quota_target_avg": 5}),
            "quota_target_avg 必须在 10～2000 之间",
        ),
    ] {
        let (status, v) = api.json("PUT", "/quota-algo", Some(body.clone())).await;
        assert_eq!((status, v), (422, json!({"detail": msg})), "{body}");
    }
    assert_eq!(
        setting(&e, "quota_v5_max").await,
        None,
        "校验失败不应写入任何参数"
    );
    assert_eq!(setting(&e, "quota_algo_last").await, None);
}

#[tokio::test]
async fn quota_algo_put_saves_and_recomputes() {
    let e = setup(&[]).await;
    member(&e, 5).await;
    let api = Api::login(&e).await;

    // 不认识的键被忽略；字符串数字、整数值的小数都接受
    let (status, v) = api
        .json(
            "PUT",
            "/quota-algo",
            Some(json!({"quota_target_avg": "200", "quota_step": 30.0, "nope": 1})),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["last"]["enabled"], true);
    // 初始值同步成今天的 N（保底已删除，不再写 quota_base_now，docs/v3-deviations.md #61）
    assert_eq!(
        setting(&e, "quota_target_avg").await.as_deref(),
        Some("200")
    );
    assert_eq!(setting(&e, "quota_step").await.as_deref(), Some("30"));
    assert_eq!(setting(&e, "quota_ceiling").await.as_deref(), Some("200"));
    assert_eq!(setting(&e, "quota_base_now").await, None);
    assert_eq!(setting(&e, "nope").await, None);
    // 重算已经执行：额度下发给算法管理的成员 Key，结果写进 quota_algo_last
    let key = e
        .state
        .db
        .get_key_by_token("nai-synthetic-0005")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key["daily_images"], 200);
    assert!(
        setting(&e, "quota_algo_last")
            .await
            .unwrap()
            .contains("\"ceiling\": 200")
    );
    assert!(
        setting(&e, "algo_notice")
            .await
            .unwrap()
            .contains("V4.5 每人 200 张/天")
    );
    // 后台操作日志：谁改了什么
    settle().await;
    let rows = e.state.db.list_admin_actions(10, 0).await.unwrap();
    assert!(
        rows.iter().any(|r| r["action"] == "修改动态额度参数"
            && r["target"] == "动态额度参数"
            && r["ok"] == 1),
        "{rows:?}"
    );

    // GET 现在能看到结果与参数（库里存过的是字符串）
    let (_, g) = api.json("GET", "/quota-algo", None).await;
    assert_eq!(g["last"]["ceiling"], 200);
    assert_eq!(g["params"]["quota_target_avg"], "200");
    assert_eq!(g["params"].get("quota_base"), None);

    // 总开关关掉：重算直接返回 {"enabled": false}
    let (status, v) = api
        .json("PUT", "/quota-algo", Some(json!({"quota_auto_enabled": 0})))
        .await;
    assert_eq!(
        (status, v["last"].clone()),
        (200, json!({"enabled": false}))
    );
}

#[tokio::test]
async fn autopilot_get_before_and_after_a_run() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    let (status, v) = api.json("GET", "/autopilot", None).await;
    assert_eq!((status, v), (200, json!({"last": {}, "history": []})));

    // 先跑一轮内核（含自动驾驶）
    let (status, _) = api.json("POST", "/modules/run", None).await;
    assert_eq!(status, 200);
    let (_, v) = api.json("GET", "/autopilot", None).await;
    let rules = &v["last"]["rules"];
    // 名额 +5 与观察 / 关闭模式已删除（docs/v3-deviations.md #68）：只有两条规则，一律执行
    let names: Vec<&String> = rules.as_object().unwrap().keys().collect();
    assert_eq!(names, ["breaker", "key_guard"]);
    assert_eq!(rules["breaker"]["mode"], "enforce");
    assert_eq!(rules["key_guard"]["mode"], "enforce");
    assert_eq!(rules["breaker"]["value"], false);
    assert_eq!(rules["key_guard"]["why"], "没有 Key 触发");
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["history"][0]["key_guard"], 0);
    assert!(v["history"][0].get("slots").is_none());
}

/// 模式设置不再读（#68）：写成 observe / off 也照样执行——单 Key 限流触发就暂停。
#[tokio::test]
async fn autopilot_ignores_old_mode_settings() {
    let e = setup(&[]).await;
    member(&e, 5).await;
    for (k, v) in [
        ("autopilot_breaker", "off"),
        ("autopilot_key_guard", "observe"),
        ("autopilot_slots", "enforce"),
    ] {
        e.state.db.set_setting(k, v).await.unwrap();
    }
    // 这把 Key 1 小时内被拒 60 次（个人额度用完，算成员自己的拒绝）
    let key = e
        .state
        .db
        .get_key_by_token("nai-synthetic-0005")
        .await
        .unwrap()
        .unwrap();
    let kid = key["id"].as_i64().unwrap();
    for i in 0..60 {
        e.state
            .db
            .add_log(
                &gate_db::repo::usage::NewLog {
                    key_id: Some(kid),
                    kind: "image".into(),
                    status: "rejected".into(),
                    reason: "member_quota".into(),
                    ..Default::default()
                },
                e.state.now() - 60.0 + f64::from(i),
            )
            .await
            .unwrap();
    }
    let api = Api::login(&e).await;
    api.json("POST", "/modules/run", None).await;
    let (_, v) = api.json("GET", "/autopilot", None).await;
    let kg = &v["last"]["rules"]["key_guard"];
    assert_eq!(kg["mode"], "enforce");
    assert_eq!(kg["value"][0]["key"], kid);
    assert_eq!(kg["value"][0]["applied"], true);
    assert!(
        e.state.pauses.paused(kid, e.state.now()).is_some(),
        "被暂停了"
    );
    assert_eq!(
        e.state.db.get_setting("register_max_users").await.unwrap(),
        None,
        "不再改名额"
    );
}

#[tokio::test]
async fn modules_list_toggle_and_run() {
    let e = setup(&[]).await;
    member(&e, 5).await;
    let api = Api::login(&e).await;

    // 还没跑过：六个模块按登记顺序，面板参数是实时读的
    let (status, v) = api.json("GET", "/modules", None).await;
    assert_eq!(status, 200);
    let mods = v["modules"].as_array().unwrap();
    let names: Vec<&str> = mods.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "observation",
            "capacity",
            "allocation",
            "autopilot",
            "risk",
            "registration"
        ]
    );
    let cap = &mods[1];
    assert_eq!(cap["title"], "容量");
    assert_eq!(cap["periodic"], true);
    assert_eq!(cap["last"], json!({}));
    assert_eq!(
        cap["params"][0],
        json!({"name": "每天", "value": 1000, "note": ""})
    );
    assert_eq!(
        cap["params"][1]["value"],
        "服务日起点先放 150 张，之后每小时补 42 张"
    );
    assert_eq!(mods[0]["periodic"], false);
    assert_eq!(
        mods[5]["enabled"], false,
        "领 Key 的开关就是 register_open，默认关"
    );

    // 手动跑一轮：启用的模块都有运行记录，校验结果挂在各自名下
    let (status, v) = api.json("POST", "/modules/run", None).await;
    assert_eq!(status, 200);
    assert_eq!(v["ok"], true);
    let mods = v["modules"].as_array().unwrap();
    for name in ["capacity", "allocation", "autopilot"] {
        let m = mods.iter().find(|m| m["name"] == name).unwrap();
        assert_eq!(m["last"]["ok"], true, "{name}: {}", m["last"]);
        assert_eq!(m["last"]["error"], "");
        assert_eq!(m["last"]["runs"].as_array().unwrap().len(), 1);
    }
    let cap = mods.iter().find(|m| m["name"] == "capacity").unwrap();
    assert!(
        cap["last"]["summary"]
            .as_str()
            .unwrap()
            .starts_with("每小时上限 ")
    );
    assert!(cap["last"]["checks"].is_array());
    assert!(
        setting(&e, "kernel_last")
            .await
            .unwrap()
            .contains("\"capacity\"")
    );
    // 分配模块的 tick 跑了动态额度：成员 Key 拿到了算法给的额度
    assert!(setting(&e, "quota_algo_last").await.is_some());

    // 关闭分配：写 module_allocation=0，立即再跑一轮，面板里 enabled=false 且没有新的运行记录
    let (status, v) = api
        .json(
            "PUT",
            "/modules/allocation",
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(setting(&e, "module_allocation").await.as_deref(), Some("0"));
    let m = v["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "allocation")
        .unwrap()
        .clone();
    assert_eq!(m["enabled"], false);
    assert_eq!(
        m["last"]["runs"].as_array().unwrap().len(),
        1,
        "关着的模块不记运行条"
    );
    let (status, v) = api
        .json("PUT", "/modules/allocation", Some(json!({"enabled": true})))
        .await;
    assert_eq!(status, 200);
    assert_eq!(setting(&e, "module_allocation").await.as_deref(), Some("1"));
    let m = v["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "allocation")
        .unwrap()
        .clone();
    assert_eq!(m["last"]["runs"].as_array().unwrap().len(), 2);

    // 领 Key 模块的开关对接 register_open
    let (status, v) = api
        .json(
            "PUT",
            "/modules/registration",
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(setting(&e, "register_open").await.as_deref(), Some("1"));
    assert_eq!(setting(&e, "module_registration").await, None);
    let m = v["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "registration")
        .unwrap()
        .clone();
    assert_eq!(m["enabled"], true);

    // 操作日志：「模块 容量」这种对象名
    settle().await;
    let rows = e.state.db.list_admin_actions(20, 0).await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r["action"] == "启用 / 关闭模块" && r["target"] == "模块 分配"),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r["action"] == "手动执行模块校验" && r["target"] == "全部模块"),
        "{rows:?}"
    );
}

/// 名额 1、已领 5：「已领人数 ≤ 名额」不通过。
struct Overbooked;

impl KernelRegistrar for Overbooked {
    fn settings(&self) -> BoxFuture<'_, Result<RegistrarSettings, String>> {
        Box::pin(async { Ok(RegistrarSettings { max_users: 1 }) })
    }
    fn count_active(&self) -> BoxFuture<'_, Result<i64, String>> {
        Box::pin(async { Ok(5) })
    }
}

async fn bug_sources(env: &Env) -> Vec<String> {
    settle().await;
    env.state
        .db
        .list_error_events(true, 100)
        .await
        .unwrap()
        .iter()
        .map(|r| r["source"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn failing_checks_are_shown_in_the_modules_page_but_not_filed_as_bugs() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    e.state.kernel.set_registrar(Some(Arc::new(Overbooked)));
    let (status, _) = api
        .json(
            "PUT",
            "/modules/registration",
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, 200);
    let (status, v) = api.json("POST", "/modules/run", None).await;
    assert_eq!(status, 200);
    let reg = v["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "registration")
        .unwrap();
    let check = &reg["last"]["checks"][0];
    assert_eq!(check["name"], "已领人数 ≤ 名额");
    assert_eq!(
        (&check["ok"], &check["level"]),
        (&json!(false), &json!("bad"))
    );
    assert_eq!(check["detail"], "5 / 1");
    // 后台「模块」页和总览提醒已经标出来了：不再另记一条永远处理不完的 Bug（docs/v3-deviations.md #44）
    let sources = bug_sources(&e).await;
    assert!(sources.is_empty(), "校验不通过不该记 Bug：{sources:?}");
}

#[tokio::test]
async fn a_check_that_itself_errors_is_still_a_bug() {
    let e = setup(&[]).await;
    // 算法状态被写坏：分配模块的校验函数读它时抛 AttributeError（模块代码出错，不是「校验没通过」）
    e.state
        .db
        .set_setting("quota_algo_last", "5")
        .await
        .unwrap();
    let mut last = serde_json::Map::new();
    let bad = e.state.kernel.check_all(&e.state, &mut last).await.unwrap();
    assert!(
        bad.iter()
            .any(|(m, c)| m == "allocation" && c.name == "校验本身出错"),
        "{bad:?}"
    );
    assert_eq!(
        last["allocation"]["checks"][0]["name"],
        json!("校验本身出错")
    );
    assert_eq!(bug_sources(&e).await, ["module:allocation:checks"]);
}

#[tokio::test]
async fn modules_put_errors() {
    let e = setup(&[]).await;
    let api = Api::login(&e).await;
    // 名字不存在：先 404（还没读请求体）
    let (status, v) = api
        .json("PUT", "/modules/nope", Some(json!({"enabled": true})))
        .await;
    assert_eq!((status, v), (404, json!({"detail": "没有这个模块"})));
    // 旧版 FastAPI：`PUT /modules/run` 落到 `/modules/{name}`，name="run"
    let (status, v) = api
        .json("PUT", "/modules/run", Some(json!({"enabled": true})))
        .await;
    assert_eq!((status, v), (404, json!({"detail": "没有这个模块"})));
    // enabled 必须是布尔
    for body in [
        json!({}),
        json!({"enabled": 1}),
        json!({"enabled": "true"}),
        json!({"enabled": null}),
    ] {
        let (status, v) = api
            .json("PUT", "/modules/capacity", Some(body.clone()))
            .await;
        assert_eq!(
            (status, v),
            (422, json!({"detail": "enabled 必须是 true / false"})),
            "{body}"
        );
    }
    assert_eq!(setting(&e, "module_capacity").await, None);
}

#[tokio::test]
async fn service_registrar_reads_settings_and_env_defaults() {
    use gate_server::kernel::{KernelRegistrar, ServiceRegistrar};
    let e = setup(&[
        ("DISCORD_CLIENT_ID", "100000000"),
        ("DISCORD_CLIENT_SECRET", "synthetic-secret"),
        ("DISCORD_BOT_TOKEN", "synthetic-bot-token"),
        (
            "REGISTRATION_BRIDGE_SECRET",
            "0123456789abcdef0123456789abcdef",
        ),
        ("DISCORD_GUILD_ID", "9000000000000000099"),
        ("SITE_URL", "https://owl.example"),
        ("REGISTER_MAX_USERS", "20"),
        ("REGISTER_DAILY_IMAGES", "80"),
        ("REGISTER_DAILY_V5", "7"),
    ])
    .await;
    let reg = ServiceRegistrar(e.state.registration.clone().expect("注册服务配齐了"));
    let s = reg.settings().await.unwrap();
    assert_eq!(s.max_users, 20, "没有后台设置时用环境默认值");
    assert_eq!(reg.count_active().await.unwrap(), 0);
    // 后台保存的优先
    e.state
        .db
        .set_setting("register_max_users", "25")
        .await
        .unwrap();
    assert_eq!(reg.settings().await.unwrap().max_users, 25);

    // 接进内核后跑一轮：自动驾驶的「名额 +5」已删除（docs/v3-deviations.md #68），不看也不改名额
    e.state.kernel.set_registrar(Some(std::sync::Arc::new(reg)));
    let api = Api::login(&e).await;
    api.json("POST", "/modules/run", None).await;
    let (_, v) = api.json("GET", "/autopilot", None).await;
    assert!(v["last"]["rules"].get("slots").is_none());
    assert_eq!(
        e.state
            .db
            .get_setting("register_max_users")
            .await
            .unwrap()
            .as_deref(),
        Some("25")
    );
}
