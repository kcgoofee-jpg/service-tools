//! SQLite 存取层。整个项目只有这个 crate 写 SQL。
//!
//! 并发模型：
//! - **一个写线程**独占写连接，所有写操作排队执行，每个任务是一个 `BEGIN IMMEDIATE` 事务；
//!   遇到 `SQLITE_BUSY`（备份脚本、旧版 Python 同时开着库等）按旧版的节奏重试 4 次。
//! - **只读连接池**（默认 2 个）：WAL 下读不阻塞写，查询放在 `spawn_blocking` 里跑。
//!
//! `repo` 模块是旧版 `class Database`（app/database.py）的逐方法移植，`tests/repo_golden.rs` 用
//! `tools/parity/golden/db_methods.py` 在旧 Python 上生成的夹具逐方法对拍（返回值 + 整库内容 + 原图目录）。
//!
//! 旧版的问题（共享主连接上一条语句失败后事务不结束 → 之后所有写都 locked，见 database.py
//! `_rollback_on_error`）在这里不存在：事务由 `rusqlite::Transaction` 的 Drop 自动回滚。

pub mod bootstrap;
pub mod repo;
pub mod value;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("写线程已退出")]
    WriterGone,
    #[error("读任务异常退出")]
    ReaderPanicked,
    /// 调用参数不合法（对应旧版里抛 ValueError / KeyError / 绑定参数失败的场景）。
    #[error("参数错误：{0}")]
    Invalid(String),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DbError>;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// 数据库句柄；克隆成本很低（内部是 Arc）。
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    /// 审计原图目录：`<库所在目录>/audit_images`（旧版 `Database.image_dir`）。
    image_dir: PathBuf,
    /// 写进 usage_log.ver 的网关版本（旧版模块级 `RULE_VERSION`，启动时由 main 设置）。
    rule_version: RwLock<String>,
    writer: std::sync::mpsc::SyncSender<Job>,
    readers: Mutex<Vec<Connection>>,
    reader_count: usize,
}

/// 写事务里遇到锁冲突时的重试次数（与旧版 `record` 的 4 次一致）。
const WRITE_ATTEMPTS: u32 = 4;

impl Db {
    /// 打开（不存在则创建）数据库并执行建表迁移。`now` 与 `backfill` 见 [`bootstrap::run`]。
    pub fn open(path: impl AsRef<Path>, now: f64, backfill: &[(&str, &str)]) -> Result<Self> {
        Self::open_with(path, now, backfill, 2)
    }

    pub fn open_with(
        path: impl AsRef<Path>,
        now: f64,
        backfill: &[(&str, &str)],
        readers: usize,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut writer_conn = Connection::open(&path)?;
        bootstrap::run(&mut writer_conn, now, backfill)?;

        let mut pool = Vec::with_capacity(readers.max(1));
        for _ in 0..readers.max(1) {
            let c = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_URI,
            )?;
            c.busy_timeout(Duration::from_secs(15))?;
            pool.push(c);
        }

        // 有界队列：写线程卡住时调用方会等待而不是无限堆积内存
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(1024);
        std::thread::Builder::new()
            .name("gate-db-writer".into())
            .spawn(move || {
                let mut conn = writer_conn;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
                // 通道关闭（进程退出）：做一次被动 checkpoint，让 WAL 尽量并回主库
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
            })
            .expect("能创建写线程");

        // 旧版：Path(path).resolve().parent / "audit_images"
        let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let image_dir = abs
            .parent()
            .map(|p| p.join("audit_images"))
            .unwrap_or_else(|| PathBuf::from("audit_images"));

        Ok(Self {
            inner: Arc::new(Inner {
                path,
                image_dir,
                rule_version: RwLock::new(String::new()),
                writer: tx,
                reader_count: pool.len(),
                readers: Mutex::new(pool),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// 审计原图目录（与库放在一起）。
    pub fn image_dir(&self) -> &Path {
        &self.inner.image_dir
    }

    /// 设置写入 usage_log.ver 的网关版本（旧版启动时 `database.RULE_VERSION = ...`）。
    pub fn set_rule_version(&self, version: impl Into<String>) {
        *self.inner.rule_version.write().expect("版本锁") = version.into();
    }

    pub fn rule_version(&self) -> String {
        self.inner.rule_version.read().expect("版本锁").clone()
    }

    /// 在写线程上执行一个事务；闭包返回 Err 或 panic 时整个事务回滚。
    pub async fn write<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: Fn(&Transaction<'_>) -> rusqlite::Result<R> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let mut attempt = 0;
            let result = loop {
                attempt += 1;
                let r = (|| {
                    let t = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let out = f(&t)?;
                    t.commit()?;
                    Ok(out)
                })();
                match r {
                    Err(e) if is_busy(&e) && attempt < WRITE_ATTEMPTS => {
                        std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
                    }
                    other => break other,
                }
            };
            let _ = tx.send(result);
        });
        self.inner
            .writer
            .send(job)
            .map_err(|_| DbError::WriterGone)?;
        rx.await
            .map_err(|_| DbError::WriterGone)?
            .map_err(DbError::from)
    }

    /// 在只读连接上执行查询（WAL 快照读，不阻塞写）。
    pub async fn read<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = loop {
                if let Some(c) = inner.readers.lock().expect("读连接池锁").pop() {
                    break c;
                }
                // 读连接都在用：短暂让出（读查询都很快，池子小是有意的——2 核机器）
                std::thread::sleep(Duration::from_millis(2));
            };
            let out = f(&conn);
            inner.readers.lock().expect("读连接池锁").push(conn);
            out.map_err(DbError::from)
        })
        .await
        .map_err(|_| DbError::ReaderPanicked)?
    }

    /// 读连接数量（测试与诊断用）。
    pub fn reader_count(&self) -> usize {
        self.inner.reader_count
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}
