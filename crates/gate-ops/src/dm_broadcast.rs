//! 一次性站内私信广播（移植自 deploy/ops/dm_broadcast.py）。
//!
//! 用途：站长级通知（如「今晚重置 Key」）。刻意做成命令行而不是常驻功能——
//! 2026-10-10 的教训：批量私信正是 Discord 应用被标记的信号之一，所以限速默认 6 秒/人、
//! 只发一次、不自动重试（撞 429 听 Discord 的多歇一会）。
//!
//! 防重复（2026-10-13 教训：同一晚发了 3 遍）：
//! - 每个 campaign 按 `--slug` 记账，已发过的成员自动跳过（断点续发）；
//! - 文案 hash 变了会拒绝（换 `--slug` 开新 campaign），不覆盖旧账本；
//! - 全部发完后再次运行 = 0 发送。
//!
//! 与旧版的差异（都是为了不再重复发送）：
//! - 账本放在 `$DATA_DIR/ops/dm_broadcast_sent.json`（宿主机持久目录）。旧版在容器的 `/tmp/dm_state_<slug>.json`，
//!   每次部署都被清空，于是「已发过」的记录丢了、又发一遍；
//! - **默认只演练**（dry-run），必须加 `--send` 才真的发；`--edit` 也一样；
//! - 账本文件损坏时拒绝运行（旧版当成空账本，等于全员重发）；
//! - 发 token 只在真发时才要求。
//!
//! 热更新已发出的私信：送达的每条消息都记了 `channel_id:message_id`，改完文案后用 `--edit` 原位 PATCH，不用重发。

use crate::discord::{self, Discord};
use anyhow::{Context, Result};
use gate_core::pyc;
use rand::Rng;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const RECORD_FILE: &str = "dm_broadcast_sent.json";

/// 账本文件位置：`$DATA_DIR/ops/dm_broadcast_sent.json`。
pub fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ops").join(RECORD_FILE)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Campaign {
    #[serde(default)]
    pub text_hash: String,
    #[serde(default)]
    pub updated_at: f64,
    /// discord_id → "channel_id:message_id"
    #[serde(default)]
    pub sent: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    campaigns: BTreeMap<String, Campaign>,
}

/// 账本：读进内存，每发一条就原子落盘（中断可续）。
pub struct Store {
    path: PathBuf,
    rec: Record,
}

impl Store {
    /// 文件不存在 = 空账本；**存在但读不懂 = 报错**（绝不能当成没发过）。
    pub fn load(path: &Path) -> Result<Self> {
        let rec = match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str::<Record>(&t).with_context(|| {
                format!(
                    "账本 {} 损坏，拒绝运行（修好或确认后手工删除再来）",
                    path.display()
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record {
                version: 1,
                ..Record::default()
            },
            Err(e) => return Err(e).with_context(|| format!("读账本 {} 失败", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            rec,
        })
    }

    pub fn campaign(&self, slug: &str) -> Campaign {
        self.rec.campaigns.get(slug).cloned().unwrap_or_default()
    }

    pub fn save(&mut self, slug: &str, c: &Campaign) -> Result<()> {
        self.rec.version = 1;
        self.rec.campaigns.insert(slug.to_string(), c.clone());
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&self.rec)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("写 {} 失败", tmp.display()))?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("改名到 {} 失败", self.path.display()))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 在册且 Key 启用的成员（按 discord_id 排序）。
pub fn load_recipients(db_path: &Path) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(
        crate::util::sqlite_uri(db_path, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打不开数据库 {}", db_path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT r.discord_id FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id \
         WHERE k.enabled=1 ORDER BY r.discord_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let v: rusqlite::types::Value = r.get(0)?;
            Ok(crate::util::sv_str(&v))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct Opts {
    pub slug: String,
    /// 新发的文案（已 strip）
    pub text: String,
    /// `--edit` 的新文案；Some 表示编辑模式
    pub edit_text: Option<String>,
    pub rate: f64,
    /// false = 只演练（默认）
    pub send: bool,
}

fn text_hash(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        hex::encode(Sha256::digest(text.as_bytes()))[..12].to_string()
    }
}

/// 每人间隔：`rate * (0.8 + 0.4 * random())`，不整齐的节奏更不像机器。
async fn pace(rate: f64) {
    if rate > 0.0 {
        let jitter = 0.8 + 0.4 * rand::rng().random::<f64>();
        tokio::time::sleep(Duration::from_secs_f64(rate * jitter)).await;
    }
}

/// 发送私信，返回 `channel_id:message_id`；失败返回 None（不重试，唯一例外是撞 429 时听 Discord 的等一次）。
async fn dm(
    d: &Discord,
    discord_id: &str,
    text: &str,
    out: &mut dyn Write,
) -> Result<Option<String>> {
    let r = d.create_dm(discord_id).await?;
    if r.status != 200 {
        return Ok(None);
    }
    let Some(channel) = r
        .json()
        .and_then(|j| j.get("id").and_then(|v| v.as_str().map(String::from)))
    else {
        return Ok(None);
    };
    let body = discord::message_body(text);
    let mut m = d.post_message(&channel, &body).await?;
    if m.status == 429 {
        // 撞 Discord 限速：听它的（上限 10 分钟，防止异常值把脚本挂死）
        let retry = m
            .json()
            .and_then(|j| j.get("retry_after").cloned())
            .and_then(|v| match v {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => pyc::parse_float(&s),
                _ => None,
            })
            .filter(|r| *r > 0.0)
            .unwrap_or(5.0)
            .min(600.0);
        writeln!(out, "  Discord 限速，睡 {retry:.0}s")?;
        tokio::time::sleep(Duration::from_secs_f64(retry)).await;
        m = d.post_message(&channel, &body).await?;
    }
    if m.ok() {
        let id = m
            .json()
            .and_then(|j| j.get("id").and_then(|v| v.as_str().map(String::from)));
        return Ok(id.map(|id| format!("{channel}:{id}")));
    }
    Ok(None)
}

/// 主流程。`discord` 为 None 只允许演练。返回退出码（0 成功 / 2 参数或账本问题）。
pub async fn run(
    discord: Option<&Discord>,
    store: &mut Store,
    recipients: &[String],
    o: &Opts,
    out: &mut dyn Write,
) -> Result<i32> {
    let mut state = store.campaign(&o.slug);
    let hash = text_hash(&o.text);
    let rate = pyc::float_repr(o.rate);

    let pending: Vec<&String>;
    if let Some(edit_text) = &o.edit_text {
        if state.sent.is_empty() {
            writeln!(out, "账本里没有已发送记录，没东西可编辑")?;
            return Ok(2);
        }
        writeln!(
            out,
            "原位编辑 {} 条已送达私信（限速 {rate}s）",
            state.sent.len()
        )?;
        if edit_text.is_empty() {
            writeln!(out, "编辑文案为空")?;
            return Ok(2);
        }
        pending = Vec::new();
    } else {
        if o.text.is_empty() {
            writeln!(out, "没有私信内容：用 --text / --file")?;
            return Ok(2);
        }
        if !state.text_hash.is_empty() && state.text_hash != hash {
            writeln!(
                out,
                "⚠ 文案与账本「{}」不同（旧 {} ≠ 新 {hash}）。换个 --slug 开新 campaign，或确认是要替换的文案",
                o.slug, state.text_hash
            )?;
            return Ok(2);
        }
        pending = recipients
            .iter()
            .filter(|d| !state.sent.contains_key(*d))
            .collect();
        writeln!(
            out,
            "在册 {} 人，已发 {}，本次待发 {}，限速 {rate}s/人",
            recipients.len(),
            state.sent.len(),
            pending.len()
        )?;
    }
    if !o.send {
        writeln!(
            out,
            "（演练：没有发送任何私信，账本也没有改动；确认无误后加 --send 才会真的发）"
        )?;
        return Ok(0);
    }
    let discord = discord.context("--send 需要 DISCORD_BOT_TOKEN")?;

    let (mut ok, mut bad) = (0u32, 0u32);
    let started = Instant::now();
    if let Some(edit_text) = &o.edit_text {
        let body = discord::message_body(edit_text);
        let total = state.sent.len();
        for (i, (_, reference)) in state.sent.iter().enumerate() {
            // Python `ref.partition(":")`
            let (channel, mid) = reference
                .split_once(':')
                .unwrap_or((reference.as_str(), ""));
            match discord.edit_message(channel, mid, &body).await {
                Ok(r) if r.ok() => ok += 1,
                _ => bad += 1,
            }
            if (i + 1) % 20 == 0 {
                writeln!(out, "  进度 {}/{total}（成功 {ok}，失败 {bad}）", i + 1)?;
            }
            pace(o.rate).await;
        }
    } else {
        let total = pending.len();
        for (i, discord_id) in pending.into_iter().enumerate() {
            match dm(discord, discord_id, &o.text, out).await {
                Ok(Some(reference)) => {
                    state.sent.insert(discord_id.clone(), reference);
                    ok += 1;
                }
                _ => bad += 1, // 传输错误同样算失败，不重试
            }
            state.text_hash = hash.clone();
            state.updated_at = crate::util::now_secs();
            store.save(&o.slug, &state)?; // 每发一条就落盘：中断可续
            if (i + 1) % 20 == 0 {
                writeln!(
                    out,
                    "  进度 {}/{total}（成功 {ok}，失败 {bad}，已用 {:.0} 分钟）",
                    i + 1,
                    started.elapsed().as_secs_f64() / 60.0
                )?;
            }
            pace(o.rate).await;
        }
    }
    writeln!(
        out,
        "完成：成功 {ok}，失败 {bad}，共 {:.1} 分钟；账本 {}",
        started.elapsed().as_secs_f64() / 60.0,
        store.path().display()
    )?;
    Ok(0)
}

/// 读文案文件（UTF-8，去首尾空白）。
pub fn read_text_file(path: &Path) -> Result<String> {
    let t = std::fs::read_to_string(path).with_context(|| format!("读 {} 失败", path.display()))?;
    Ok(pyc::strip(&t).to_string())
}

/// 命令行互斥检查用：`--text` 优先于 `--file`（与旧版一致）。
pub fn pick_text(text: &str, file: Option<&Path>) -> Result<String> {
    if !text.is_empty() {
        return Ok(pyc::strip(text).to_string());
    }
    match file {
        Some(p) => read_text_file(p),
        None => Ok(String::new()),
    }
}
