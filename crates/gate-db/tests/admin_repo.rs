//! 后台「成员 / 记录」接口用到的裸 SQL（`repo::admin_members` / `repo::admin_records`）。
//! 路由层的逐字节快照在 gate-server 的 `admin_snapshot.rs`，这里只验证 SQL 本身的语义。

use gate_db::Db;
use gate_db::repo::admin_members::BindOutcome;
use gate_db::repo::admin_records::ActionFilter;
use gate_db::repo::keys::NewKey;
use serde_json::json;

async fn open() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db"), 1_000.0, &[]).unwrap();
    (db, dir)
}

async fn key(db: &Db, name: &str) -> i64 {
    let k: NewKey = serde_json::from_value(json!({
        "name": name, "token": format!("nai-{name}"), "daily_images": 10, "monthly_anlas": 0.0,
        "daily_text_tokens": 0, "rpm": 5,
    }))
    .unwrap();
    db.create_key(&k, 1_000.0).await.unwrap()["id"]
        .as_i64()
        .unwrap()
}

async fn sql(db: &Db, s: &str) {
    let s = s.to_string();
    db.write(move |tx| tx.execute_batch(&s)).await.unwrap();
}

#[tokio::test]
async fn tags_invites_and_quota_owner() {
    let (db, _d) = open().await;
    let (a, b) = (key(&db, "邀请-3").await, key(&db, "普通").await);
    db.upsert_key_tag(a, "邀请", "n1", "站长", 5.0)
        .await
        .unwrap();
    db.upsert_key_tag(a, "邀请", "n2", "运维", 6.0)
        .await
        .unwrap();
    db.upsert_key_tag(b, "观察", "", "站长", 4.0).await.unwrap();
    assert_eq!(db.invite_key_ids().await.unwrap(), [a]);
    let tags = db.key_tag_rows().await.unwrap();
    assert_eq!(
        tags.iter()
            .map(|t| (t.key_id, t.tag.as_str(), t.note.as_str(), t.by.as_str()))
            .collect::<Vec<_>>(),
        [(b, "观察", "", "站长"), (a, "邀请", "n2", "运维")],
        "按时间升序；同名标签覆盖备注 / 操作者 / 时间"
    );
    assert!(!db.delete_key_tag(a, "不存在").await.unwrap());
    assert!(db.delete_key_tag(b, "观察").await.unwrap());
    assert_eq!(db.key_names().await.unwrap(), ["邀请-3", "普通"]);
    db.set_quota_auto(b, -1).await.unwrap();
    assert_eq!(db.get_key(b).await.unwrap().unwrap()["quota_auto"], -1);
    assert_eq!(
        db.get_key(a).await.unwrap().unwrap()["quota_auto"],
        1,
        "新建默认交给算法"
    );
    assert_eq!(
        db.key_names_for(&[b, a, 999])
            .await
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        [(a, "邀请-3".to_string()), (b, "普通".to_string())].into()
    );
    assert!(db.key_names_for(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn bind_discord_outcomes() {
    let (db, _d) = open().await;
    let (a, b) = (key(&db, "甲").await, key(&db, "乙").await);
    // 新绑定：头像留空、领取时间 = now；同时写 api_keys.discord_id
    match db
        .bind_discord(a, "900000000000000001", "u1", "一号", 50.0)
        .await
        .unwrap()
    {
        BindOutcome::Bound(Some(r)) => {
            assert_eq!(
                (
                    r.key_id,
                    r.discord_id.as_str(),
                    r.username.as_deref(),
                    r.display_name.as_deref(),
                    r.avatar.as_deref(),
                    r.created_at
                ),
                (
                    a,
                    "900000000000000001",
                    Some("u1"),
                    Some("一号"),
                    Some(""),
                    Some(50.0)
                )
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        db.get_key(a).await.unwrap().unwrap()["discord_id"],
        "900000000000000001"
    );
    // 别的 Key 想要同一个账号：被拒，什么都不改
    assert_eq!(
        db.bind_discord(b, "900000000000000001", "x", "x", 60.0)
            .await
            .unwrap(),
        BindOutcome::Taken(a)
    );
    assert!(db.get_key(b).await.unwrap().unwrap()["discord_id"].is_null());
    // 换绑：同一把 Key，领取时间不变
    match db
        .bind_discord(a, "900000000000000002", "u2", "", 70.0)
        .await
        .unwrap()
    {
        BindOutcome::Bound(Some(r)) => assert_eq!(
            (r.discord_id.as_str(), r.created_at),
            ("900000000000000002", Some(50.0))
        ),
        other => panic!("{other:?}"),
    }
    // 同一账号重复绑同一把 Key：只更新资料
    match db
        .bind_discord(a, "900000000000000002", "again", "again", 80.0)
        .await
        .unwrap()
    {
        BindOutcome::Bound(Some(r)) => assert_eq!(r.username.as_deref(), Some("again")),
        other => panic!("{other:?}"),
    }
    assert_eq!(db.registration_rows().await.unwrap().len(), 1);
}

#[tokio::test]
async fn member_page_aggregates() {
    let (db, _d) = open().await;
    let a = key(&db, "甲").await;
    sql(
        &db,
        &format!(
            "INSERT INTO share_state(key_id, paused_until) VALUES ({a}, 5.0);\
             INSERT INTO counters(key_id, day, images, anlas, text_tokens, requests, v5) VALUES ({a}, '2026-09-22', 3, 1.5, 7, 4, 2);\
             INSERT INTO admin_actions(ts, actor, action, target, detail, ok) VALUES \
               (100.0, 'x', '重置今日额度', 'Key #{a} 甲', '', 1), (50.0, 'x', '重置今日额度', 'Key #{a} 甲', '', 1), \
               (120.0, 'x', '重置今日额度', 'Key #{a} 甲', '', 0), (130.0, 'x', '别的', 'Key #{a}', '', 1);\
             INSERT INTO usage_log(ts, key_id, key_name, kind, model, status, detail) VALUES \
               (10000.0, {a}, '甲', 'image', 'm', 'ok', ''), (10000.0, {a}, '甲', 'upscale', 'm', 'ok', ''), \
               (10000.0, {a}, '甲', 'chat', 'm', 'ok', ''), (13700.0, {a}, '甲', 'vibe_encode', 'm', 'ok', ''), \
               (10001.0, {a}, '甲', 'image', 'm', 'rejected', '画师串 NaN 且 img2img'), \
               (10002.0, {a}, '甲', 'image', 'm', 'error', '角色太多'), (10003.0, {a}, '甲', 'image', 'm', 'rejected', '正常的拒绝'), \
               (5.0, {a}, '甲', 'image', 'm', 'rejected', '请求过于频繁');"
        ),
    )
    .await;
    assert_eq!(db.pause_rows().await.unwrap(), [(a, 5.0)]);
    assert_eq!(
        db.counter_rows("2026-09-22").await.unwrap(),
        [(a, 3, 2, 1.5, 7, 4)]
    );
    assert!(db.counter_rows("2026-09-23").await.unwrap().is_empty());
    assert_eq!(
        db.quota_reset_marks(60.0).await.unwrap(),
        [(100.0, format!("Key #{a} 甲"))],
        "只要成功的、不早于起点的"
    );
    // 生图速率：hour0 = 9999 起每小时一格；chat 不算，vibe_encode 算
    let mut bars = db.hourly_image_counts(9999).await.unwrap();
    bars.sort();
    assert_eq!(bars, [(a, 0, 2), (a, 1, 1)]);
    // 自动标签：先命中先得（img2img 在 NaN 前面）；只看被拒 / 失败；不早于 since
    let pats = [
        ("传了底图", "%img2img%"),
        ("画师串 NaN", "%NaN%"),
        ("角色超 6 个", "%角色太多%"),
        ("请求过快", "%请求过于频繁%"),
    ];
    let mut tags = db.auto_tag_counts(10.0, &pats).await.unwrap();
    tags.sort();
    assert_eq!(
        tags,
        [
            (a, "传了底图".to_string(), 1),
            (a, "角色超 6 个".to_string(), 1)
        ]
    );
    assert_eq!(db.auto_tag_counts(0.0, &pats).await.unwrap().len(), 3);
}

#[tokio::test]
async fn reset_all_keys_spares_admins_and_invites() {
    let (db, _d) = open().await;
    let (user, invite, admin) = (
        key(&db, "普通").await,
        key(&db, "邀请").await,
        key(&db, "站长").await,
    );
    sql(
        &db,
        &format!(
            "UPDATE api_keys SET is_admin=1 WHERE id={admin};\
             INSERT INTO key_tags(key_id, tag, ts) VALUES ({invite}, '邀请', 1.0), ({invite}, '观察', 1.0), ({user}, '观察', 1.0), ({admin}, '常驻', 1.0), (999, '孤儿', 1.0);\
             INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES ('1', {user}, 1.0), ('2', {invite}, 1.0);\
             INSERT INTO share_state(key_id, paused_until) VALUES ({user}, 9.0);"
        ),
    )
    .await;
    assert_eq!(db.reset_all_keys().await.unwrap(), (1, 1));
    assert_eq!(db.get_key(user).await.unwrap().unwrap()["enabled"], 0);
    assert_eq!(db.get_key(invite).await.unwrap().unwrap()["enabled"], 1);
    assert_eq!(db.get_key(admin).await.unwrap().unwrap()["enabled"], 1);
    let regs = db.registration_rows().await.unwrap();
    assert_eq!(
        regs.iter()
            .map(|r| r.discord_id.as_str())
            .collect::<Vec<_>>(),
        ["2"]
    );
    let tags: Vec<(i64, String)> = db
        .key_tag_rows()
        .await
        .unwrap()
        .into_iter()
        .map(|t| (t.key_id, t.tag))
        .collect();
    assert!(
        tags.contains(&(invite, "观察".into()))
            && tags.contains(&(admin, "常驻".into()))
            && !tags.contains(&(user, "观察".into()))
    );
    assert!(
        !tags.contains(&(999, "孤儿".into())),
        "没有对应 Key 的孤儿标签也清掉（旧版如此）"
    );
    assert!(db.pause_rows().await.unwrap().is_empty());
    // 再来一次：没有可失效的了（已经失效的算在内——UPDATE 命中的行数）
    assert_eq!(db.reset_all_keys().await.unwrap(), (1, 1));
}

#[tokio::test]
async fn action_log_filters() {
    let (db, _d) = open().await;
    for (ts, actor, action) in [
        (1.0, "站长", "添加黑名单"),
        (2.0, "系统", "私信成员"),
        (3.0, "Discord:900000000000000001", "成员自助重置"),
        (4.0, "后台 *.*.0.1", "登录后台"),
        (5.0, "后台 *.*.0.1", "添加黑名单"),
    ] {
        db.add_admin_action(actor, action, "", "", true, ts)
            .await
            .unwrap();
    }
    let f = |kind: &str, actions: &[&str]| ActionFilter {
        actor_kind: kind.into(),
        actions: actions.iter().map(|s| s.to_string()).collect(),
    };
    assert_eq!(db.count_actions_filtered(&f("", &[])).await.unwrap(), 5);
    assert_eq!(
        db.count_actions_filtered(&f("admin", &[])).await.unwrap(),
        3
    );
    assert_eq!(
        db.count_actions_filtered(&f("system", &[])).await.unwrap(),
        1
    );
    assert_eq!(
        db.count_actions_filtered(&f("member", &[])).await.unwrap(),
        1
    );
    assert_eq!(
        db.count_actions_filtered(&f("whatever", &[]))
            .await
            .unwrap(),
        5
    );
    assert_eq!(
        db.count_actions_filtered(&f("admin", &["添加黑名单"]))
            .await
            .unwrap(),
        2
    );
    let rows = db
        .list_actions_filtered(&f("", &["添加黑名单", "私信成员"]), 2, 1)
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["ts"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        [2.0, 1.0],
        "id 倒序 + 偏移"
    );
    assert_eq!(db.action_kinds().await.unwrap().len(), 4);
}

#[tokio::test]
async fn audit_queries_bind_the_key_id_as_text_like_the_old_gateway() {
    let (db, _d) = open().await;
    let a = key(&db, "甲 乙").await;
    sql(
        &db,
        &format!(
            "INSERT INTO generation_audit(ts, key_id, key_name, prompt, negative, extra, image, image_type, image_size) VALUES \
             (10.0, {a}, '甲 乙', 'p1', 'n1', '', X'89504e47', 'image/png', 4), (20.0, {a}, '甲 乙', 'p2', '', '', NULL, '', NULL);"
        ),
    )
    .await;
    // 文本 "1" / "1.0" 都按数值转换后匹配；垃圾、空串、逗号串匹配不到
    for (q, n) in [
        ("1", 2),
        ("1.0", 2),
        (" 1", 2),
        ("abc", 0),
        ("", 0),
        ("1,2", 0),
    ] {
        let (rows, total) = db.list_audit_text(24, 0, Some(q)).await.unwrap();
        assert_eq!((total, rows.len() as i64), (n, n), "{q:?}");
    }
    let (rows, total) = db.list_audit_text(24, 0, None).await.unwrap();
    assert_eq!(
        (
            total,
            rows[0]["id"].clone(),
            rows[0]["has_image"].clone(),
            rows[1]["has_image"].clone()
        ),
        (2, json!(2), json!(0), json!(1))
    );
    assert_eq!(
        db.key_name_text("1.0").await.unwrap().as_deref(),
        Some("甲 乙")
    );
    assert_eq!(db.key_name_text("1,2").await.unwrap(), None);
    let imgs = db.audit_images_for_text("1", 1 << 20, 0.0).await.unwrap();
    assert_eq!(imgs.len(), 1);
    assert_eq!(
        (imgs[0].prompt.as_str(), imgs[0].image.as_slice()),
        ("p1", &[0x89, b'P', b'N', b'G'][..])
    );
    assert!(
        db.audit_images_for_text("1,2", 1 << 20, 0.0)
            .await
            .unwrap()
            .is_empty()
    );
}
