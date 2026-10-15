//! 账号保护的运行层：`gate_core::guard::GuardCore`（纯逻辑，golden 对拍过）+ 读写库 + 告警。
//! 对应旧版 `app/guard.py` 里带 `await db` 的方法（load / save / seed_hour / token_block_reason /
//! on_upstream_429 / adapt_daily）。核心状态放在同步 Mutex 里，从不跨 await 持锁：先查库，再算。

use crate::params::ParamsStore;
use gate_core::guard::{AdaptPlan, GuardCore, GuardError, parse_setting_float};
use gate_core::reasons::Reason;
use gate_db::Db;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub struct Guard {
    core: Mutex<GuardCore>,
    db: Db,
    params: Arc<ParamsStore>,
    /// 有出图条目被移除（出完 / 被拒 / 取消）：叫醒同一把 Key 排在后面的那张（见 `admission::image_admission`）。
    pub released: tokio::sync::Notify,
}

#[derive(Debug, thiserror::Error)]
pub enum GuardIoError {
    #[error(transparent)]
    Db(#[from] gate_db::DbError),
    #[error("{0:?}")]
    Guard(GuardError),
}

impl From<GuardError> for GuardIoError {
    fn from(e: GuardError) -> Self {
        GuardIoError::Guard(e)
    }
}

impl Guard {
    pub fn new(db: Db, tz: &str, reset_hour: i64, params: Arc<ParamsStore>) -> Self {
        Self {
            core: Mutex::new(GuardCore::new(tz, reset_hour)),
            db,
            params,
            released: tokio::sync::Notify::new(),
        }
    }

    /// 移除一个出图条目，并叫醒等着轮到自己的同 Key 请求。
    pub fn release_image(&self, key_id: i64, entry_id: Option<i64>) {
        self.with(|g| g.release_image(key_id, entry_id));
        self.released.notify_waiters();
    }

    /// 只读 / 同步操作直接拿核心（不得跨 await）。
    pub fn with<R>(&self, f: impl FnOnce(&mut GuardCore) -> R) -> R {
        f(&mut self.core.lock().expect("guard"))
    }

    /// 启动时从 site_settings 读 `guard_<field>`。
    pub async fn load(&self) -> Result<(), GuardIoError> {
        let mut stored = BTreeMap::new();
        for f in gate_core::guard::FIELDS.iter() {
            let key = gate_core::guard::setting_key(f.name);
            if let Some(v) = self.db.get_setting(&key).await? {
                stored.insert(key, v);
            }
        }
        self.with(|c| c.load_settings(&stored))?;
        Ok(())
    }

    /// 后台保存：校验 → 写库 → 改内存。
    pub async fn save(
        &self,
        body: &Map<String, Value>,
    ) -> Result<Vec<(String, i64)>, GuardIoError> {
        let updates = GuardCore::validate_save(body)?;
        if !updates.is_empty() {
            let writes: Vec<(String, String)> = updates
                .iter()
                .map(|(k, v)| (gate_core::guard::setting_key(k), v.to_string()))
                .collect();
            self.db.set_settings_bulk(&writes).await?;
            self.with(|c| c.apply_updates(&updates));
        }
        Ok(updates)
    }

    /// 启动时从用量日志补回最近 3 小时的出图（避免重启把每小时计数清零、绕过上限）。
    pub async fn seed_hour(&self, token_ids: &[String], now: f64) -> Result<usize, GuardIoError> {
        let rows = self
            .db
            .recent_image_stamps(now - gate_core::guard::WINDOW_3H as f64)
            .await?;
        let stamps = gate_core::guard::stamps_from_rows(&rows);
        Ok(self.with(|c| c.seed_hour(token_ids, &stamps)))
    }

    /// 这把上游 Token 现在能不能出图（每日 / 每小时 / 安静时段 / 熔断 / 按时段放行）。
    pub async fn token_block_reason(
        &self,
        token_id: &str,
        day: &str,
        now: f64,
    ) -> Result<Option<Reason>, GuardIoError> {
        let c = self.db.get_upstream_counter(token_id, day).await?;
        Ok(self.with(|g| g.token_block_reason(token_id, c.images as f64, now)))
    }

    /// 上游 429：每小时上限减半（AIMD）。返回 (旧, 新)。
    pub async fn on_upstream_429(&self, now: f64) -> Result<Option<(i64, i64)>, GuardIoError> {
        let prev = parse_setting_float(self.db.get_setting("guard_429_cap_at").await?.as_deref())?;
        let params = self.params.get();
        let r = self.with(|g| g.on_upstream_429(now, prev, &params))?;
        self.db.set_settings_bulk(&r.writes).await?;
        Ok(r.change)
    }

    /// 每日加窗（每 24 小时最多一次）。
    pub async fn adapt_daily(&self, now: f64) -> Result<Option<(i64, i64)>, GuardIoError> {
        let last_429 = parse_setting_float(
            self.db
                .get_setting("guard_last_upstream_429")
                .await?
                .as_deref(),
        )?;
        let last_step = parse_setting_float(
            self.db
                .get_setting("guard_hourly_adapted_at")
                .await?
                .as_deref(),
        )?;
        let adapted = match GuardCore::adapt_daily_begin(now, last_429, last_step) {
            AdaptPlan::Done(a) => a,
            AdaptPlan::NeedUsage => {
                let hit = self
                    .db
                    .reason_hit_since(now - 86400.0, gate_core::reasons::HOURLY_CAP)
                    .await?;
                let first = self.db.first_log_ts().await?;
                let seen =
                    parse_setting_float(self.db.get_setting("guard_429_cap_at").await?.as_deref())?;
                let params = self.params.get();
                self.with(|g| g.adapt_daily_finish(now, hit, first, seen, &params))?
            }
        };
        // 旧版：只改时间戳时单独 `set_setting`；真的加窗（上限 + 时间戳）是 `_set_adaptive` 里一次 `set_settings_bulk`，同一个事务
        match adapted.writes.as_slice() {
            [] => {}
            [(k, v)] => self.db.set_setting(k, v).await?,
            many => self.db.set_settings_bulk(many).await?,
        }
        Ok(adapted.change)
    }

    pub fn record_start(&self, token_id: &str, images: i64, now: f64) {
        self.with(|g| g.record_start(token_id, now, images));
    }

    pub fn jitter(&self) -> f64 {
        self.with(|g| g.jitter(&mut || rand::random::<f64>()))
    }
}
