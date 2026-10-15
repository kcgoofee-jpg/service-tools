//! 定时任务的后台接口：`GET /admin/api/jobs`，以及动态额度参数 `quota-algo`（GET / PUT）。
//! 内核时代的 `modules*`、`autopilot` 接口已删除（回 404）。公开仓库：全部是合成数据；上游是本地假服务。

mod common;
use common::*;
use serde_json::{Value, json};

/// 2026-10-14 12:00:00 +08:00
const NOW: f64 = 1_791_950_400.0;

const NAMES: [&str; 7] = [
    "quota",
    "upstream_snapshot",
    "hourly_cap",
    "breaker",
    "key_guard",
    "maintenance",
    "idle_recycle",
];

#[tokio::test]
async fn routes_need_admin_login() {
    let e = setup(&[]).await;
    for (method, path) in [
        ("GET", "/admin/api/jobs"),
        ("GET", "/admin/api/quota-algo"),
        ("PUT", "/admin/api/quota-algo"),
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
async fn the_kernel_routes_are_gone() {
    let e = setup(&[]).await;
    let api = Admin::login(&e.state).await;
    for path in ["/modules", "/modules/capacity", "/autopilot"] {
        let (status, v) = api.json("GET", path, None).await;
        assert_eq!((status, v), (404, json!({"detail": "Not Found"})), "{path}");
    }
    // 写操作：`/admin/{*rest}` 是后台前端页面的 GET 路由，别的方法落到它上面是 405（和其他不存在的后台写接口一样）
    for (method, path) in [
        ("PUT", "/modules/capacity"),
        ("PUT", "/modules/x"),
        ("POST", "/modules/run"),
        ("PUT", "/modules/run"),
    ] {
        let (status, v) = api.json(method, path, Some(json!({"enabled": true}))).await;
        assert_eq!(
            (status, v),
            (405, json!({"detail": "Method Not Allowed"})),
            "{method} {path}"
        );
    }
    assert_eq!(setting(&e.state, "module_capacity").await, None);
    assert_eq!(setting(&e.state, "quota_auto_enabled").await, None);
}

#[tokio::test]
async fn jobs_list_before_and_after_a_run() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    e.state
        .jobs
        .set_official_api(Some("http://127.0.0.1:9/never".into()));
    let api = Admin::login(&e.state).await;
    let (status, v) = api.json("GET", "/jobs", None).await;
    assert_eq!(status, 200);
    let jobs = v["jobs"].as_array().unwrap();
    let names: Vec<&str> = jobs.iter().map(|j| j["name"].as_str().unwrap()).collect();
    assert_eq!(names, NAMES, "按登记顺序");
    assert_eq!(
        jobs[0],
        json!({"name": "quota", "title": "动态额度", "every": "每 10 分钟", "every_secs": 600, "last": null})
    );
    assert_eq!(
        (&jobs[5]["every"], &jobs[5]["every_secs"]),
        (&json!("每 5 分钟"), &json!(300))
    );
    assert_eq!(
        (&jobs[6]["every"], &jobs[6]["every_secs"]),
        (&json!("每小时"), &json!(3600))
    );
    assert!(jobs.iter().all(|j| j["last"].is_null()), "还没跑过");

    for name in NAMES {
        run_job(&e.state, name).await.unwrap();
    }
    let (_, v) = api.json("GET", "/jobs", None).await;
    for j in v["jobs"].as_array().unwrap() {
        let last = &j["last"];
        assert_eq!(last["at"], json!(NOW), "{j}");
        assert_eq!(last["ok"], true, "{j}");
        assert!(last["ms"].as_i64().unwrap() >= 0, "{j}");
        assert!(!last["summary"].as_str().unwrap().is_empty(), "{j}");
        let keys: Vec<&String> = last.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["at", "ok", "ms", "summary"]);
    }
    let by = |name: &str| {
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == name)
            .unwrap()["last"]["summary"]
            .clone()
    };
    assert_eq!(by("maintenance"), "全部完成");
    assert_eq!(by("breaker"), "最近 15 分钟上游失败 0/0：正常");
    assert_eq!(by("key_guard"), "没有 Key 触发");
    assert_eq!(by("idle_recycle"), "最近 60 分钟没出图，本轮跳过");
}

#[tokio::test]
async fn one_failing_job_does_not_affect_the_others() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    e.state
        .db
        .set_setting("quota_ceiling", "inf")
        .await
        .unwrap();
    for name in ["quota", "breaker", "key_guard"] {
        let _ = run_job(&e.state, name).await;
    }
    let api = Admin::login(&e.state).await;
    let (_, v) = api.json("GET", "/jobs", None).await;
    let jobs = v["jobs"].as_array().unwrap();
    assert_eq!(jobs[0]["last"]["ok"], false);
    assert!(
        jobs[0]["last"]["summary"]
            .as_str()
            .unwrap()
            .starts_with("OverflowError: "),
        "{}",
        jobs[0]
    );
    assert_eq!(jobs[3]["last"]["ok"], true);
    assert_eq!(jobs[4]["last"]["ok"], true);
    assert_eq!(bug_sources(&e.state).await, ["job:quota"]);
}

#[tokio::test]
async fn the_run_record_survives_a_restart() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    run_job(&e.state, "breaker").await.unwrap();
    let saved: Value = serde_json::from_str(&setting(&e.state, "job_last").await.unwrap()).unwrap();
    assert_eq!(saved["breaker"]["ok"], true);
    // 同一个数据目录重新建一份 AppState，启动时读回
    let s2 = e.restart().await;
    assert!(s2.jobs.last().await.is_empty(), "启动序列之前还是空的");
    s2.jobs.load(&s2.db).await.unwrap();
    assert_eq!(Value::Object(s2.jobs.last().await), saved);
    let api = Admin::login(&s2).await;
    let (_, v) = api.json("GET", "/jobs", None).await;
    assert_eq!(v["jobs"][3]["last"], saved["breaker"]);
    // 读不懂的记录当没有
    s2.db.set_setting("job_last", "坏").await.unwrap();
    s2.jobs.load(&s2.db).await.unwrap();
    assert!(s2.jobs.last().await.is_empty());
}

#[tokio::test]
async fn quota_algo_get_shape_and_put_validation() {
    let e = setup(&[]).await;
    let api = Admin::login(&e.state).await;

    // 什么都没存过：params 是代码里的起点值（整数）；last 是 {}，history 是 []；推算用的「V5 每 1% ≈ 张」
    let (status, text) = api.call("GET", "/quota-algo", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        text,
        r#"{"last":{},"history":[],"params":{"quota_auto_enabled":1,"quota_target_avg":150,"quota_ceiling_max":300,"quota_base_min_fair":10,"quota_step":25},"images_per_percent":14.2}"#
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
        setting(&e.state, "quota_v5_max").await,
        None,
        "校验失败不应写入任何参数"
    );
    assert_eq!(setting(&e.state, "quota_algo_last").await, None);
    assert!(
        e.state.jobs.last().await.is_empty(),
        "校验失败不重算、不记运行"
    );
}

#[tokio::test]
async fn quota_algo_put_saves_recomputes_and_records_the_run() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    let token = "nai-synthetic-0005";
    member_key(&e.state, 5, &[]).await;
    let api = Admin::login(&e.state).await;

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
    // 初始值同步成今天的 N
    assert_eq!(
        setting(&e.state, "quota_target_avg").await.as_deref(),
        Some("200")
    );
    assert_eq!(setting(&e.state, "quota_step").await.as_deref(), Some("30"));
    assert_eq!(
        setting(&e.state, "quota_ceiling").await.as_deref(),
        Some("200")
    );
    assert_eq!(setting(&e.state, "nope").await, None);
    // 重算已经执行：额度下发给算法管理的成员 Key，结果写进 quota_algo_last
    let key = e.state.db.get_key_by_token(token).await.unwrap().unwrap();
    assert_eq!(key["daily_images"], 200);
    assert!(
        setting(&e.state, "quota_algo_last")
            .await
            .unwrap()
            .contains("\"ceiling\": 200")
    );
    assert!(
        setting(&e.state, "algo_notice")
            .await
            .unwrap()
            .contains("V4.5 每人 200 张/天")
    );
    // 这次重算也记进「动态额度」任务的运行记录
    let last = Value::Object(e.state.jobs.last().await);
    assert_eq!(last["quota"]["ok"], true);
    assert_eq!(last["quota"]["at"], json!(NOW));
    assert!(
        last["quota"]["summary"]
            .as_str()
            .unwrap()
            .starts_with("每人 V4.5 200 张"),
        "{last}"
    );
    // 先只在内存里（接口马上看得到），后台接口不顺带多写一行设置；下一次定时任务记结果时一起落库
    assert_eq!(setting(&e.state, "job_last").await, None);
    let (_, j) = api.json("GET", "/jobs", None).await;
    assert_eq!(j["jobs"][0]["last"], last["quota"]);
    run_job(&e.state, "breaker").await.unwrap();
    let saved: Value = serde_json::from_str(&setting(&e.state, "job_last").await.unwrap()).unwrap();
    assert_eq!(saved["quota"], last["quota"]);
    // 后台操作日志：谁改了什么
    settle().await;
    let rows = admin_actions(&e.state.db, 10).await;
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
    assert_eq!(g["images_per_percent"], json!(14.2));

    // 总开关关掉：重算直接返回 {"enabled": false}，运行记录跟着变
    let (status, v) = api
        .json("PUT", "/quota-algo", Some(json!({"quota_auto_enabled": 0})))
        .await;
    assert_eq!(
        (status, v["last"].clone()),
        (200, json!({"enabled": false}))
    );
    assert_eq!(
        Value::Object(e.state.jobs.last().await)["quota"]["summary"],
        "动态额度已关闭，未计算"
    );
}

#[tokio::test]
async fn a_failed_recompute_is_recorded_without_a_second_bug() {
    let _clock = clock(Some(NOW)).await;
    let e = setup(&[]).await;
    e.state
        .db
        .set_setting("quota_ceiling", "inf")
        .await
        .unwrap();
    let api = Admin::login(&e.state).await;
    let (status, _) = api
        .json("PUT", "/quota-algo", Some(json!({"quota_step": 30})))
        .await;
    assert_eq!(status, 500);
    let last = Value::Object(e.state.jobs.last().await);
    assert_eq!(last["quota"]["ok"], false);
    // 500 由接口那一层记 Bug；运行记录这边不再另记一条 job:quota
    let sources = bug_sources(&e.state).await;
    assert!(!sources.iter().any(|s| s == "job:quota"), "{sources:?}");
}
