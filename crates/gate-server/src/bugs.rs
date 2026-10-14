//! Bug 追踪：把所有「不该发生」的错误按特征归并记下来（旧版 `app/errors.py` 的 Tracker）。
//!
//! 来源：接口未捕获的错误（500）、上游 5xx、后台任务失败、记账写入失败、成员在图片返回前断开、网页脚本报错。
//! 同一个 bug 只占一行（count 累加）；新出现或复发时私信站长一次。不记录 Key、提示词或 IP。
//! 永远不会让调用方出错或阻塞：入库在后台任务里做。

use crate::alerts::Alerter;
use gate_core::errors::{Capture, ExcInfo, StoreState, build_event, notice_for, should_print_stack};
use gate_db::Db;
use gate_db::repo::errors::{ErrorRow, Stored};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Bugs {
    db: Db,
    alerter: Arc<Alerter>,
    printed: Mutex<HashMap<String, f64>>,
    recent: Mutex<Vec<gate_core::errors::ErrorEvent>>,
    /// 同一个特征码的读-改-写串行（旧版用一把全局锁）。
    store_lock: Arc<tokio::sync::Mutex<()>>,
    tasks: tokio_util::task::TaskTracker,
}

/// 一次记录的参数（旧版 capture 的关键字参数）。
#[derive(Default, Clone)]
pub struct Report<'a> {
    pub source: &'a str,
    /// (错误类型名, 消息)；Rust 里没有 Python 的堆栈帧，位置用错误类型所在模块代替。
    pub error: Option<(&'a str, &'a str)>,
    pub title: &'a str,
    pub detail: &'a str,
    pub path: &'a str,
    pub key_id: Option<i64>,
    pub rid: &'a str,
    /// "error"（默认）或 "warn"
    pub level: &'a str,
}

impl Bugs {
    pub fn new(db: Db, alerter: Arc<Alerter>, tasks: tokio_util::task::TaskTracker) -> Self {
        Self {
            db,
            alerter,
            printed: Mutex::new(HashMap::new()),
            recent: Mutex::new(Vec::new()),
            store_lock: Arc::new(tokio::sync::Mutex::new(())),
            tasks,
        }
    }

    /// 记录一次错误，返回特征码。
    pub fn capture(&self, r: Report<'_>) -> String {
        let now = crate::time::now();
        let level = if r.level.is_empty() { "error" } else { r.level };
        let exc = r.error.map(|(ty, msg)| ExcInfo { type_name: ty, message: msg, app_frame: r.source, stack: msg });
        let c = build_event(
            &Capture { source: r.source, exc, title: r.title, detail: r.detail, path: r.path, key_id: r.key_id, rid: r.rid, level },
            now,
        );
        tracing::warn!(target: "bug", "{}", c.log_line);
        if r.error.is_some() && !c.stack_text.is_empty() {
            let mut p = self.printed.lock().expect("bugs");
            let last = p.get(&c.event.sig).copied();
            if should_print_stack(now, last) {
                p.insert(c.event.sig.clone(), now);
                tracing::warn!(target: "bug", "{}", c.stack_text);
            }
        }
        {
            let mut recent = self.recent.lock().expect("bugs");
            recent.push(c.event.clone());
            let n = recent.len();
            if n > 50 {
                recent.drain(..n - 50);
            }
        }
        let e = c.event.clone();
        let db = self.db.clone();
        let alerter = self.alerter.clone();
        let lock = self.store_lock.clone();
        self.tasks.spawn(async move {
            let _g = lock.lock().await;
            let row = ErrorRow {
                sig: e.sig.clone(),
                source: e.source.clone(),
                level: e.level.clone(),
                title: e.title.clone(),
                detail: e.detail.clone(),
                path: e.path.clone(),
                key_id: e.key_id,
                rid: e.rid.clone(),
                ts: e.ts,
            };
            match db.store_error_event(&row).await {
                Ok(stored) => {
                    let state = match stored {
                        Stored::New => StoreState::New,
                        Stored::Regressed => StoreState::Regressed,
                        Stored::Repeat => StoreState::Known,
                    };
                    if let Some(n) = notice_for(state, &e) {
                        alerter.notify(&n.key, &n.text, f64::from(n.cooldown_secs));
                    }
                }
                Err(_) => tracing::warn!(target: "bug", "store failed"),
            }
        });
        c.event.sig
    }

    /// 写库失败时也能在内存里看到最近 50 条。
    pub fn recent(&self) -> Vec<gate_core::errors::ErrorEvent> {
        self.recent.lock().expect("bugs").clone()
    }
}
