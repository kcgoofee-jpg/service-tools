//! 环境变量（与 Python 版、docker-compose.yml 的 `discord-bot` / `warden` 服务完全一致）。
//!
//! | 变量 | 谁用 | 缺省 |
//! |---|---|---|
//! | `DISCORD_BOT_TOKEN` | 奶妹 | 必填 |
//! | `DISCORD_GUILD_ID` | 奶妹 | 必填 |
//! | `REGISTRATION_BRIDGE_SECRET` | 奶妹 | 没配时 `/register` 回「领 Key 功能尚未配置完成。」，其他命令回「出了点问题……」 |
//! | `REGISTRATION_BACKEND_URL` | 奶妹 | `http://127.0.0.1:3003`（去掉末尾 `/`） |
//! | `SITE_URL` | 奶妹 | 空串（去掉末尾 `/`） |
//! | `WARDEN_BOT_TOKEN` | 燕双鹰 | 空 = 不启动，睡到天荒地老 |
//!
//! docker-compose 里还给奶妹传了 `PRAISE_*` / `GALLERY_AI_DAILY` / `AUDIT_*`，那是画廊 AI 评论用的，
//! 该功能没有移植（默认关闭、站长尚未决定），这些变量会被忽略。

use anyhow::{Context, bail};

use gate_core::pyc;

#[derive(Debug, Clone)]
pub struct NaimeiConfig {
    pub token: String,
    pub guild_id: u64,
    /// 网关地址，已去掉末尾 `/`。
    pub backend: String,
    /// 站点地址，已去掉末尾 `/`。
    pub site: String,
    /// 桥接密钥；`None` = 环境变量没设置。
    pub secret: Option<String>,
}

pub const DEFAULT_BACKEND: &str = "http://127.0.0.1:3003";

impl NaimeiConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// 便于测试：从任意查找函数构造。
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let token = get("DISCORD_BOT_TOKEN").context("缺少环境变量 DISCORD_BOT_TOKEN")?;
        if token.is_empty() {
            bail!("DISCORD_BOT_TOKEN 为空");
        }
        let guild = get("DISCORD_GUILD_ID").context("缺少环境变量 DISCORD_GUILD_ID")?;
        let guild_id = guild
            .trim()
            .parse::<u64>()
            .with_context(|| "DISCORD_GUILD_ID 不是整数".to_owned())?;
        Ok(NaimeiConfig {
            token,
            guild_id,
            backend: get("REGISTRATION_BACKEND_URL")
                .unwrap_or_else(|| DEFAULT_BACKEND.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            site: get("SITE_URL")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
            secret: get("REGISTRATION_BRIDGE_SECRET"),
        })
    }
}

/// 燕双鹰的 Token：`os.environ.get("WARDEN_BOT_TOKEN", "").strip()`。
pub fn warden_token_from(get: impl Fn(&str) -> Option<String>) -> String {
    pyc::strip(&get("WARDEN_BOT_TOKEN").unwrap_or_default()).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_and_trailing_slashes() {
        let cfg = NaimeiConfig::from_lookup(env(&[
            ("DISCORD_BOT_TOKEN", "t"),
            ("DISCORD_GUILD_ID", "900000000000000001"),
        ]))
        .unwrap();
        assert_eq!(cfg.backend, "http://127.0.0.1:3003");
        assert_eq!(cfg.site, "");
        assert_eq!(cfg.secret, None);
        assert_eq!(cfg.guild_id, 900000000000000001);

        let cfg = NaimeiConfig::from_lookup(env(&[
            ("DISCORD_BOT_TOKEN", "t"),
            ("DISCORD_GUILD_ID", "900000000000000001"),
            ("REGISTRATION_BACKEND_URL", "http://nai-gate:8000//"),
            ("SITE_URL", "https://site.test/"),
            ("REGISTRATION_BRIDGE_SECRET", ""),
        ]))
        .unwrap();
        assert_eq!(cfg.backend, "http://nai-gate:8000");
        assert_eq!(cfg.site, "https://site.test");
        assert_eq!(cfg.secret.as_deref(), Some(""));
    }

    #[test]
    fn required_vars() {
        assert!(NaimeiConfig::from_lookup(env(&[("DISCORD_GUILD_ID", "1")])).is_err());
        assert!(NaimeiConfig::from_lookup(env(&[("DISCORD_BOT_TOKEN", "t")])).is_err());
        assert!(
            NaimeiConfig::from_lookup(env(&[
                ("DISCORD_BOT_TOKEN", "t"),
                ("DISCORD_GUILD_ID", "abc")
            ]))
            .is_err()
        );
    }

    #[test]
    fn warden_token_is_stripped() {
        assert_eq!(
            warden_token_from(env(&[("WARDEN_BOT_TOKEN", "  abc \n")])),
            "abc"
        );
        assert_eq!(warden_token_from(env(&[])), "");
        assert_eq!(warden_token_from(env(&[("WARDEN_BOT_TOKEN", "   ")])), "");
    }
}
