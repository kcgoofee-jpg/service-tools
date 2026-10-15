//! 动态额度每日复盘的取数（`repo/kernel.rs` 的 `quota_daily_inputs`）：3.0 的口径。

use gate_db::Db;
use gate_db::repo::keys::NewKey;
use gate_db::repo::usage::NewLog;
use serde_json::json;

fn rejected(key_id: i64, reason: &str, detail: &str) -> NewLog {
    NewLog {
        key_id: Some(key_id),
        kind: "image".into(),
        model: "nai-diffusion-4-5-full".into(),
        status: "rejected".into(),
        reason: reason.into(),
        detail: detail.into(),
        ..Default::default()
    }
}

/// 被每小时上限拦的小时按原因码 `hourly_cap` 数，不看文案（旧版按 `LIKE '%出图量已达上限%'` 数，
/// 文案一改就静默失效；保底文案的计数随保底删除，docs/v3-deviations.md #61）。
#[tokio::test]
async fn hourly_blocks_are_counted_by_reason_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("gate.db"), 0.0, &[]).unwrap();
    let key: NewKey = serde_json::from_value(json!({
        "name": "m", "token": "nai-synthetic-000000000000000000000001", "daily_images": 5,
        "monthly_anlas": 0.0, "daily_text_tokens": 0, "rpm": 5,
    }))
    .unwrap();
    let kid = db.create_key(&key, 1.0).await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let start = 36_000.0 * 100.0; // 整点
    for (ts, reason, detail) in [
        // 同一小时两条：只算一个小时；第二条没有文案也照样算
        (start + 10.0, "hourly_cap", "最近 60 分钟的出图量已达上限"),
        (start + 20.0, "hourly_cap", ""),
        // 另一个小时、文案改过
        (start + 7200.0, "hourly_cap", "这一小时满了"),
        // 文案像、原因码不是：不算
        (start + 3600.0, "daily_cap", "本站今天的出图量已达上限"),
        // 旧版的保底文案：不再单独计数
        (
            start + 3700.0,
            "",
            "保底 95 张已用完。当前高峰期有其他人在排队",
        ),
        // 按时段放行
        (start + 5000.0, "paced", "这个时段的份额已用完"),
        // 窗口之外
        (start + 86_400.0, "hourly_cap", ""),
    ] {
        db.add_log(&rejected(kid, reason, detail), ts)
            .await
            .unwrap();
    }
    let d = db
        .quota_daily_inputs("1970-01-01", start, start + 86_400.0)
        .await
        .unwrap();
    let mut hours = d.yesterday.hourly_block_hours.clone();
    hours.sort_unstable();
    assert_eq!(hours, vec![1000, 1002]);
    assert_eq!(d.yesterday.paced_blocks, 1);
}

/// 节约模式的折算已删除（docs/v3-deviations.md #67）：上游计数只按张数累加；
/// `units` 列照旧跟着 `images` 一起涨（回滚到 2.x 时它拿 `units` 和每日上限比）。
#[tokio::test]
async fn upstream_counter_is_plain_image_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("gate.db"), 0.0, &[]).unwrap();
    db.bump_upstream_image_counter("token-synthetic", "2030-03-15", 3)
        .await
        .unwrap();
    db.bump_upstream_image_counter("token-synthetic", "2030-03-15", 2)
        .await
        .unwrap();
    db.bump_upstream_image_counter("token-synthetic", "2030-03-15", 0)
        .await
        .unwrap();
    let c = db
        .get_upstream_counter("token-synthetic", "2030-03-15")
        .await
        .unwrap();
    assert_eq!((c.images, c.v5), (5, 0));
    let rows = db
        .execute_fetchall_compat(
            "SELECT images, units FROM upstream_token_counters WHERE token_id=?",
            &[json!("token-synthetic")],
        )
        .await
        .unwrap();
    assert_eq!(rows, vec![json!({"images": 5, "units": 5.0})]);
}
