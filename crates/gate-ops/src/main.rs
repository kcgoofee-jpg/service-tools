//! `nai-ops`：猫头鹰公益站运维命令行。子命令见 `nai-ops --help`；各命令的行为说明在对应模块的文档里。
//!
//! 退出码：`patrol` 为 0 正常 / 1 注意 / 2 危险 / 3 巡检自己出错；其它命令 0 成功、非 0 失败
//! （参数错误 2，运行错误 1）。

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use gate_ops::{
    announce, discord, dm_broadcast, incident, patrol, prompt_stats, sanitize, smoke, snapshot,
    util,
};
use std::io::Read;
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
    /// 只读体检：系统 / 上游容量 / 注册就绪度 / 成员请求 / 数据一致性；退出码 0 正常 · 1 注意 · 2 危险 · 3 巡检自身出错
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
    /// 提示词统计 + 敏感内容自查，输出本地 HTML（含成员提示词原文，文件权限 600，别外传）
    PromptStats {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value = "prompt-stats.html")]
        out: PathBuf,
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// 仅写进「标签频率」表头的说明文字，不截断行数（与旧版一致）
        #[arg(long, default_value_t = 400)]
        top: i64,
        #[arg(long, default_value_t = 3000)]
        prompts: usize,
    },
    /// 状态页事件（Statuspage 流程）：open / update / resolve / list
    Incident {
        #[arg(long)]
        db: Option<PathBuf>,
        #[command(subcommand)]
        action: IncidentCmd,
    },
    /// 往公告频道发一条消息（正文从 stdin 或 --file 读；需要 ANNOUNCE_CHANNEL_ID 与 DISCORD_BOT_TOKEN）
    Announce {
        /// 从文件读正文（缺省读 stdin）
        #[arg(long)]
        file: Option<PathBuf>,
        /// 只打印将要发的内容，不发送
        #[arg(long)]
        dry_run: bool,
    },
    /// 一次性站内私信广播。默认只演练；加 --send 才真的发。账本在 $DATA_DIR/ops/dm_broadcast_sent.json
    DmBroadcast {
        /// campaign 名：同名的账本互通（续发 / 编辑都靠它）
        #[arg(long, default_value = "notice")]
        slug: String,
        #[arg(long, default_value = "")]
        text: String,
        #[arg(long)]
        file: Option<PathBuf>,
        /// 不新发，把已送达的原位编辑成这个文案（文件）
        #[arg(long)]
        edit: Option<PathBuf>,
        /// 每人间隔秒数（默认 6）
        #[arg(long, default_value_t = 6.0)]
        rate: f64,
        /// 真的发送（缺省只演练）
        #[arg(long, conflicts_with = "dry_run")]
        send: bool,
        /// 只演练（默认行为；保留这个开关是为了兼容旧命令行）
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// 线上冒烟：只做不出图的检查；SMOKE_KEY 环境变量有值时多测一条 /v1/me
    Smoke {
        /// 站点根地址
        #[arg(default_value = "https://gate.davidzhao.top", env = "SMOKE_BASE")]
        base: String,
        /// 单个请求的超时（秒）
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum IncidentCmd {
    /// 发布新事件
    Open {
        #[arg(long)]
        title: String,
        /// 正文；留空用该状态的 Statuspage 标准句式
        #[arg(long)]
        body: Option<String>,
        #[arg(long, default_value = "minor", value_parser = ["none", "minor", "major", "critical"])]
        impact: String,
        /// upstream / api / web / bot，逗号分隔（状态页只展示 upstream 与 api）
        #[arg(long, default_value = "api")]
        components: String,
        #[arg(long, default_value = "identified", value_parser = ["investigating", "identified", "monitoring", "resolved"])]
        status: String,
    },
    /// 追加更新
    Update {
        id: i64,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, default_value = "monitoring", value_parser = ["investigating", "identified", "monitoring", "update", "resolved"])]
        status: String,
    },
    /// 标记已解决
    Resolve {
        id: i64,
        #[arg(long)]
        body: Option<String>,
        /// 兼容旧命令行：被忽略，resolve 永远写 resolved
        #[arg(long, hide = true)]
        status: Option<String>,
    },
    /// 最近事件
    List {
        #[arg(long, default_value_t = 5)]
        limit: i64,
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

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("读 stdin 失败")?;
    Ok(s)
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
        Cmd::PromptStats {
            db,
            out,
            days,
            top,
            prompts,
        } => {
            let args = prompt_stats::Args {
                db: util::resolve_db(db.as_deref()),
                out,
                days,
                top,
                prompts,
            };
            print!(
                "{}",
                prompt_stats::run(&args, util::now_secs(), util::local_tz())?
            );
            Ok(0)
        }
        Cmd::Incident { db, action } => {
            let action = match action {
                IncidentCmd::Open {
                    title,
                    body,
                    impact,
                    components,
                    status,
                } => incident::Action::Open {
                    title,
                    body,
                    impact,
                    components,
                    status,
                },
                IncidentCmd::Update { id, body, status } => {
                    incident::Action::Update { id, status, body }
                }
                IncidentCmd::Resolve { id, body, .. } => incident::Action::Resolve { id, body },
                IncidentCmd::List { limit } => incident::Action::List { limit },
            };
            print!(
                "{}",
                incident::execute(&util::resolve_db(db.as_deref()), &action, util::now_secs())
                    .await?
            );
            Ok(0)
        }
        Cmd::Announce { file, dry_run } => {
            let text = match file {
                Some(p) => std::fs::read_to_string(&p)
                    .with_context(|| format!("读 {} 失败", p.display()))?,
                None => read_stdin()?,
            };
            let token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
            let channel = std::env::var("ANNOUNCE_CHANNEL_ID").unwrap_or_default();
            let (code, line) = announce::run(&announce::Args {
                api_base: &discord::api_base_from_env(),
                token: &token,
                channel: &channel,
                text: &text,
                dry_run,
            })
            .await?;
            println!("{line}");
            Ok(code)
        }
        Cmd::DmBroadcast {
            slug,
            text,
            file,
            edit,
            rate,
            send,
            dry_run,
            db,
        } => {
            if send && dry_run {
                bail!("--send 与 --dry-run 不能同时用");
            }
            let text = dm_broadcast::pick_text(&text, file.as_deref())?;
            let edit_text = edit
                .as_deref()
                .map(dm_broadcast::read_text_file)
                .transpose()?;
            let mut store =
                dm_broadcast::Store::load(&dm_broadcast::record_path(&util::data_dir()))?;
            let recipients = dm_broadcast::load_recipients(&util::resolve_db(db.as_deref()))?;
            let token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
            if send && token.is_empty() {
                println!("没有 DISCORD_BOT_TOKEN");
                return Ok(2);
            }
            let client = if send {
                Some(discord::Discord::new(
                    &discord::api_base_from_env(),
                    &token,
                    Duration::from_secs(15),
                )?)
            } else {
                None
            };
            let opts = dm_broadcast::Opts {
                slug,
                text,
                edit_text,
                rate,
                send,
            };
            let mut out = std::io::stdout();
            dm_broadcast::run(client.as_ref(), &mut store, &recipients, &opts, &mut out).await
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
