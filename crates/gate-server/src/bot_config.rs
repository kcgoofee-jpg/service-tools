//! Discord 机器人（奶妹）的配置与运行状态的读写（旧版 `app/bot_config.py` 里碰数据库的部分）。
//! 纯逻辑（校验 / 解析 / 心跳白名单 / 事件裁剪 / 在线判断）在 `gate_core::bot_config`，这里只负责
//! 读写 `site_settings` 里的 `bot_*` 键。

use gate_core::bot_config as core;
use gate_db::{Db, DbError};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum BotConfigError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{0}")]
    Data(#[from] core::BotConfigError),
}

/// `load(db)`：读出全部配置字段，缺的 / 读不懂的用默认值。
pub async fn load(db: &Db) -> Result<Map<String, Value>, DbError> {
    let mut stored = BTreeMap::new();
    for f in core::FIELDS {
        let key = core::setting_key(f.name);
        if let Some(v) = db.get_setting(&key).await? {
            stored.insert(key, v);
        }
    }
    Ok(core::load_from(&stored))
}

/// `save(db, values)`：`values` 是 [`gate_core::bot_config::validate`] 校验过的字段。
pub async fn save(db: &Db, values: &Map<String, Value>) -> Result<(), DbError> {
    if values.is_empty() {
        return Ok(());
    }
    db.set_settings_bulk(&core::to_settings(values)).await
}

/// `report(db, status, event, now)`：机器人上报。心跳整体覆盖；事件追加到最近 50 条。
pub async fn report(
    db: &Db,
    status: Option<&Map<String, Value>>,
    event: Option<&Map<String, Value>>,
    now: f64,
) -> Result<(), BotConfigError> {
    if let Some(s) = status {
        db.set_setting(core::STATUS_KEY, &core::status_to_store(s, now))
            .await?;
    }
    if let Some(e) = event {
        let stored = db.get_setting(core::EVENTS_KEY).await?;
        if let Some(text) = core::append_event(stored.as_deref(), e, now)? {
            db.set_setting(core::EVENTS_KEY, &text).await?;
        }
    }
    Ok(())
}

/// `snapshot(db, now)`：后台「Discord」页用。
pub async fn snapshot(db: &Db, now: f64) -> Result<Value, BotConfigError> {
    let config = load(db).await?;
    let status = db.get_setting(core::STATUS_KEY).await?;
    let events = db.get_setting(core::EVENTS_KEY).await?;
    Ok(core::snapshot(
        config,
        status.as_deref(),
        events.as_deref(),
        now,
    )?)
}
