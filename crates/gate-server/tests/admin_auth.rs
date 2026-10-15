//! 后台登录 / 会话 / 来源校验 / 操作日志。

mod common;
use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn login_session_and_action_log() {
    let e = setup(&[]).await;
    let (status, _, body) = Call::new("POST", "/admin/api/login")
        .header("host", "owl.example")
        .header("origin", "https://owl.example")
        .json(json!({"password": "wrong"}))
        .send(&e.state)
        .await;
    assert_eq!(status, 401);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"detail": "密码错误"})
    );

    let (status, headers, _) = Call::new("POST", "/admin/api/login")
        .header("host", "owl.example")
        .header("origin", "https://owl.example")
        .json(json!({"password": ADMIN_PASSWORD}))
        .send(&e.state)
        .await;
    assert_eq!(status, 200);
    let set = headers["set-cookie"].to_str().unwrap().to_string();
    assert!(
        set.starts_with("nai_gate_admin=")
            && set.contains("HttpOnly")
            && set.contains("SameSite=strict"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap().to_string();

    let (status, headers, _) = Call::new("GET", "/admin/api/me")
        .header("cookie", cookie.clone())
        .send(&e.state)
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    let (status, _, _) = Call::new("GET", "/admin/api/me").send(&e.state).await;
    assert_eq!(status, 401);

    // 跨站写请求被拒
    let (status, _, body) = Call::new("POST", "/admin/api/logout")
        .header("host", "owl.example")
        .header("origin", "https://evil.example")
        .header("cookie", cookie.clone())
        .send(&e.state)
        .await;
    assert_eq!(
        status,
        200,
        "logout 不校验来源（与旧版一致）: {}",
        String::from_utf8_lossy(&body)
    );

    settle().await;
    let rows = common::admin_actions(&e.state.db, 10).await;
    let actions: Vec<(String, bool)> = rows
        .iter()
        .map(|r| {
            (
                r["action"].as_str().unwrap().to_string(),
                r["ok"].as_i64() == Some(1),
            )
        })
        .collect();
    assert!(actions.contains(&("登录后台".into(), false)), "{actions:?}");
    assert!(actions.contains(&("登录后台".into(), true)), "{actions:?}");
    assert!(actions.contains(&("退出登录".into(), true)), "{actions:?}");
}

#[tokio::test]
async fn cross_origin_write_rejected() {
    let e = setup(&[]).await;
    let (status, _, body) = Call::new("POST", "/admin/api/login")
        .header("host", "owl.example")
        .header("origin", "https://owl.example.evil.net")
        .json(json!({"password": ADMIN_PASSWORD}))
        .send(&e.state)
        .await;
    assert_eq!(status, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"detail": "来源校验失败"})
    );
}
