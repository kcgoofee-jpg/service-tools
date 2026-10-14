//! 应用状态：启动时构造一次，用 `Arc` 共享给所有请求与后台任务（没有全局变量）。

use crate::env::EnvConfig;
use crate::middleware::StatusStats;
use crate::web::Assets;
use gate_core::clock::Clock;
use gate_db::Db;
use std::ops::Deref;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub env: EnvConfig,
    pub db: Db,
    pub clock: Clock,
    pub assets: Assets,
    pub status_stats: StatusStats,
    pub version: &'static str,
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

impl AppState {
    pub fn new(env: EnvConfig, db: Db, assets: Assets) -> Self {
        let clock = Clock::new(&env.tz, env.day_reset_hour);
        Self(Arc::new(Inner {
            env,
            db,
            clock,
            assets,
            status_stats: StatusStats::default(),
            version: VERSION,
        }))
    }
}
