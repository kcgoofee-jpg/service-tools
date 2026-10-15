//! `nai-ops`：猫头鹰公益站运维命令行。子命令见 `nai-ops --help`；各命令的行为说明在对应模块的文档里。
//!
//! 退出码：`patrol` 为 0 正常 / 1 注意 / 2 危险 / 3 巡检自己出错；其它命令 0 成功、非 0 失败
//! （参数错误 2，运行错误 1）。

use anyhow::Result;
use clap::{Parser, Subcommand};
use gate_ops::{patrol, sanitize, smoke, snapshot, util};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nai-ops", version, about = "猫头鹰公益站运维命令")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 只读体检：系统 / 上游容量 / 成员请求 / 数据一致性；退出码 0 正常 · 1 注意 · 2 危险 · 3 巡检自身出错
    Patrol {
        /// 有问题时给站长发 Discord 告警（同一问题 30 分钟只发一次）
        #[arg(long)]
        alert: bool,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
        /// 只输出一行摘要
        #[arg(long)]
        oneline: bool,
        /// 数据库路径（默认 $NAI_GATE_DB，其次 $DATA_DIR/nai_gate.db）
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// 一致快照（不含审计原图）→ 完整性校验 → gzip（权限 600）
    Snapshot {
        /// 输出文件，如 /opt/backups/daily/nai_gate-2026-10-14.db.gz
        out: PathBuf,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// 把快照原地脱敏（Key 换短哈希、删密钥类设置 / 提示词 / 缩略图 / 头像）后 VACUUM，给协作 AI 只读分析用
    RoSanitize {
        /// 快照数据库文件（.db，不是 .db.gz）
        snapshot: PathBuf,
    },
    /// 线上冒烟：只做不出图的检查；SMOKE_KEY 环境变量有值时多测一条 /v1/me
    Smoke {
        /// 站点根地址（也可用环境变量 SITE_URL）
        #[arg(env = "SITE_URL")]
        base: String,
        /// 单个请求的超时（秒）
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
}

fn main() {
    let cli = Cli::parse();
    let is_patrol = matches!(cli.cmd, Cmd::Patrol { .. });
    if is_patrol {
        // 巡检自己 panic 也要退出码 3（systemd 才分得出「巡检坏了」与「有警告」）
        std::panic::set_hook(Box::new(|info| {
            eprintln!("[patrol] 内部错误（panic）：{info}");
            std::process::exit(3);
        }));
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("错误：创建运行时失败：{e}");
            std::process::exit(if is_patrol { 3 } else { 1 });
        }
    };
    let code = match rt.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(e) => {
            if is_patrol {
                eprintln!("[patrol] 内部错误：{e:#}");
                3
            } else {
                eprintln!("错误：{e:#}");
                1
            }
        }
    };
    std::process::exit(code);
}

async fn dispatch(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Patrol {
            alert,
            json,
            oneline,
            db,
        } => {
            let cfg = patrol::Config::from_env(db.as_deref())?;
            patrol::run(
                &cfg,
                &patrol::Args {
                    alert,
                    json,
                    oneline,
                },
            )
            .await
        }
        Cmd::Snapshot { out, db } => {
            let line = snapshot::run(&util::resolve_db(db.as_deref()), &out)?;
            println!("{line}");
            Ok(0)
        }
        Cmd::RoSanitize { snapshot } => {
            let n = sanitize::sanitize(&snapshot)?;
            println!("sanitized {n} keys");
            Ok(0)
        }
        Cmd::Smoke { base, timeout } => {
            let key = std::env::var("SMOKE_KEY")
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty());
            let report = smoke::run(&base, key.as_deref(), Duration::from_secs(timeout)).await?;
            print!("{}", report.summary());
            Ok(report.exit_code())
        }
    }
}
