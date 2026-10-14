//! 往公告频道发一条消息（移植自 deploy/ops/announce.py）：正文从 stdin（或 `--file`）读，
//! 发到 `ANNOUNCE_CHANNEL_ID`，用 `DISCORD_BOT_TOKEN`，`allowed_mentions` 为空（文案里的 @ 不会生效）。
//!
//! 与旧版的差异：发送失败（HTTP 非 2xx）退出码为 1（旧版永远 0，脚本没法发现失败）；
//! 多了 `--dry-run`（只打印将要发的内容）。

use crate::discord::{self, Discord};
use anyhow::{Result, bail};
use gate_core::pyc;
use std::time::Duration;

pub struct Args<'a> {
    pub api_base: &'a str,
    pub token: &'a str,
    pub channel: &'a str,
    pub text: &'a str,
    pub dry_run: bool,
}

/// 返回 (退出码, 要打印的一行)。
pub async fn run(a: &Args<'_>) -> Result<(i32, String)> {
    let text = pyc::strip(a.text);
    if text.is_empty() {
        bail!("没有公告内容（从 stdin 或 --file 读到的是空文本）");
    }
    if a.channel.is_empty() {
        bail!("缺少 ANNOUNCE_CHANNEL_ID");
    }
    if a.dry_run {
        let shown = pyc::take_chars(text, 1900);
        return Ok((
            0,
            format!(
                "dry-run：将发到频道 {} 共 {} 字（上限 1900）：\n{shown}",
                a.channel,
                pyc::len_chars(shown)
            ),
        ));
    }
    if a.token.is_empty() {
        bail!("缺少 DISCORD_BOT_TOKEN");
    }
    let d = Discord::new(a.api_base, a.token, Duration::from_secs(10))?;
    let resp = d
        .post_message(a.channel, &discord::message_body(text))
        .await?;
    let detail = if resp.status == 200 {
        resp.json()
            .and_then(|j| j.get("id").map(gate_core::pyext::py_str))
            .unwrap_or_else(|| "None".to_string())
    } else {
        pyc::take_chars(&resp.body, 200).to_string()
    };
    Ok((
        if resp.ok() { 0 } else { 1 },
        format!("posted {} {detail}", resp.status),
    ))
}
