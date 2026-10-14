//! 第三层配置：`data/private_params.json`（站长私有调参，不进仓库）。
//! 旧版每次读取都 stat 一次文件、mtime 变了就重读；这里最多每秒 stat 一次，结果放在 ArcSwap 里无锁读取。

use arc_swap::ArcSwap;
use gate_core::params::Params;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

pub struct ParamsStore {
    path: PathBuf,
    current: ArcSwap<Params>,
    checked: Mutex<(Instant, Option<SystemTime>)>,
}

impl ParamsStore {
    pub fn new(path: PathBuf) -> Self {
        let s = Self {
            path,
            current: ArcSwap::from_pointee(Params::empty()),
            checked: Mutex::new((Instant::now() - std::time::Duration::from_secs(10), None)),
        };
        s.reload_if_changed(true);
        s
    }

    fn reload_if_changed(&self, force: bool) {
        let mut c = self.checked.lock().expect("params");
        if !force && c.0.elapsed().as_secs_f64() < 1.0 {
            return;
        }
        c.0 = Instant::now();
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if force || mtime != c.1 {
            c.1 = mtime;
            let p = match std::fs::read_to_string(&self.path) {
                Ok(text) => Params::from_json_text(&text),
                Err(_) => Params::empty(),
            };
            self.current.store(Arc::new(p));
        }
    }

    /// 当前参数（文件改了最多 1 秒后生效）。
    pub fn get(&self) -> Arc<Params> {
        self.reload_if_changed(false);
        self.current.load_full()
    }

    /// 后台改参数：合并后原子写回（0600），立即生效。
    pub fn update(
        &self,
        values: &serde_json::Map<String, serde_json::Value>,
    ) -> std::io::Result<Arc<Params>> {
        let mut p = (*self.get()).clone();
        p.apply_update(values);
        // 旧版：json.dump(indent=2, sort_keys=True, ensure_ascii=False)
        let sorted: std::collections::BTreeMap<_, _> = p.as_map().iter().collect();
        let body = serde_json::to_vec_pretty(&sorted).map_err(std::io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::fsutil::write_private(&self.path, &body)?;
        let p = Arc::new(p);
        self.current.store(p.clone());
        self.reload_if_changed(true);
        Ok(p)
    }
}
