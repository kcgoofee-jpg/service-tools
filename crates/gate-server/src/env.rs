//! 配置第一层：环境变量。启动时解析一次，之后只读。
//!
//! 解析规则逐个复刻旧版（app/config.py、registration.py、ops.py、status_page.py 里散落的 `os.getenv`）：
//! - 布尔：`1/true/yes/on/y`（不区分大小写、去空白）为真，其余为假；未设置用默认值；
//!   例外 `ADMIN_COOKIE_SECURE`：只有 `0/false/no/off` 为假（默认安全）。
//! - 整数 / 浮点：空串或解析失败都用默认值（`int(os.environ.get(name, "").strip() or default)`）。
//! - 每 Token 列表（`NAI_TOKEN_V5_DAILY_LIMITS` 等）补齐 / 截断到 Token 个数。
//!
//! 第二层（后台可改、存 site_settings）与第三层（private_params.json）见 `settings.rs`。

use gate_core::pyc;
use std::collections::HashMap;
use std::path::PathBuf;

/// 环境变量来源：生产用进程环境，测试用一个 HashMap。
pub trait EnvSource {
    fn get(&self, name: &str) -> Option<String>;
}

pub struct ProcessEnv;
impl EnvSource for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl EnvSource for HashMap<String, String> {
    fn get(&self, name: &str) -> Option<String> {
        HashMap::get(self, name).cloned()
    }
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y"
    )
}

struct Reader<'a>(&'a dyn EnvSource);

impl Reader<'_> {
    fn str(&self, name: &str, default: &str) -> String {
        self.0.get(name).unwrap_or_else(|| default.to_string())
    }
    fn trimmed(&self, name: &str) -> String {
        self.0
            .get(name)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    fn bool(&self, name: &str, default: bool) -> bool {
        self.0.get(name).map_or(default, |v| truthy(&v))
    }
    fn int(&self, name: &str, default: i64) -> i64 {
        let raw = self.0.get(name).unwrap_or_default();
        let raw = raw.trim();
        if raw.is_empty() {
            default
        } else {
            pyc::parse_int(raw).unwrap_or(default)
        }
    }
    fn float(&self, name: &str, default: f64) -> f64 {
        let raw = self.0.get(name).unwrap_or_default();
        let raw = raw.trim();
        if raw.is_empty() {
            default
        } else {
            pyc::parse_float(raw).unwrap_or(default)
        }
    }
    /// registration.py 的 `number()`：`max(0, int(os.getenv(name, default)))`，解析失败用默认值（不夹 0）。
    fn nonneg(&self, name: &str, default: i64) -> i64 {
        match self.0.get(name) {
            None => default.max(0),
            Some(v) => pyc::parse_int(&v).map(|n| n.max(0)).unwrap_or(default),
        }
    }
}

/// 自助领取 Key（Discord 机器人桥接）的配置；任一必填项缺失则为 None（旧版 `configured_service`）。
#[derive(Debug, Clone)]
pub struct RegistrationEnv {
    pub client_id: String,
    pub client_secret: String,
    pub bot_token: String,
    pub bridge_secret: String,
    pub guild_id: String,
    pub role_id: String,
    /// 以 `/` 结尾。
    pub site_url: String,
    /// `REGISTER_EXPIRES_DAYS`：新领的 Key 几天后过期，0 = 不过期（默认，站长 10-15）。
    /// 每日张数 / V5 / 模型范围归动态额度管，没有环境变量（见 `ops::algo_owned`）。
    pub key_expires_days: i64,
    pub key_rpm: i64,
    pub max_users: i64,
    pub reset_at: String,
    pub key_features: String,
    pub min_account_days: i64,
    pub member_role_id: String,
    pub admin_ids: Vec<String>,
    pub idle_days: i64,
}

#[derive(Debug, Clone)]
pub struct EnvConfig {
    // 基础
    pub data_dir: PathBuf,
    pub admin_password: String,
    pub admin_cookie_secure: bool,
    pub secret_key: String,
    pub tz: String,
    pub day_reset_hour: i64,
    pub cors_origins: Vec<String>,
    pub admin_allowed_origins: Vec<String>,
    /// 信任哪些直连对端带来的 X-Forwarded-For（uvicorn 的 FORWARDED_ALLOW_IPS，默认只信任本机）。
    pub forwarded_allow_ips: Vec<String>,
    // 上游
    pub nai_tokens: Vec<String>,
    pub nai_token_v5_daily_limits: Vec<i64>,
    pub nai_token_allow_anlas: Vec<bool>,
    pub image_host: String,
    pub text_host: String,
    pub text_host_legacy: String,
    pub upstream_proxy: String,
    pub upstream_user_agent: String,
    pub upstream_http2: bool,
    pub post_request_jitter_min: f64,
    pub post_request_jitter_max: f64,
    pub single_image_slot_enforced: bool,
    // 并发与限速
    pub global_concurrency: i64,
    pub queue_timeout: i64,
    pub image_min_interval: f64,
    pub image_429_cooldown_seconds: f64,
    pub auth_fail_max: i64,
    pub auth_fail_window: i64,
    pub auth_block_seconds: i64,
    pub login_max_attempts: i64,
    pub login_window_seconds: i64,
    pub key_inactivity_delete_days: i64,
    // Discord / 告警 / 公告
    pub discord_bot_token: String,
    pub site_url: String,
    pub alert_user_id: String,
    pub alert_channel_id: String,
    pub alert_webhook_url: String,
    pub announce_channel_id: String,
    pub discord_invite_url: String,
    pub discord_guild_id: String,
    /// Discord REST 基地址（测试指向本地假服务）。
    pub discord_api_base: String,
    // 记录
    pub audit_prompts: bool,
    pub audit_thumbs: bool,
    pub audit_retention_days: i64,
    pub usage_log_retention_days: i64,
    // 免费规格
    pub safe_clamp: bool,
    pub max_pixels: i64,
    pub max_steps: i64,
    pub allow_img2img: bool,
    // 全站预算
    pub global_monthly_anlas: f64,
    pub global_daily_anlas: i64,
    // 新 Key 默认值
    pub default_daily_images: i64,
    pub default_daily_anlas: f64,
    pub default_daily_v5: i64,
    pub default_monthly_anlas: f64,
    pub default_daily_text_tokens: i64,
    pub default_rpm: i64,
    pub default_expires_days: i64,
    // 文本
    pub max_text_output_tokens: i64,
    pub max_input_chars: i64,
    pub seed_demo_key: bool,
    pub registration: Option<RegistrationEnv>,
}

impl EnvConfig {
    pub fn from_process() -> Self {
        Self::load(&ProcessEnv)
    }

    pub fn load(src: &dyn EnvSource) -> Self {
        let r = Reader(src);
        let raw_tokens = src
            .get("NAI_TOKENS")
            .filter(|s| !s.is_empty())
            .or_else(|| src.get("NAI_TOKEN"))
            .unwrap_or_default();
        let nai_tokens: Vec<String> = raw_tokens
            .replace('\n', ",")
            .split(',')
            .map(|t| pyc::strip(t).to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let n = nai_tokens.len();
        let per_token_ints = |name: &str, default: i64| -> Vec<i64> {
            let mut v: Vec<i64> = r
                .str(name, "")
                .split(',')
                .map(|item| pyc::parse_int(item).map(|x| x.max(0)).unwrap_or(default))
                .collect();
            v.resize(v.len().max(n), default);
            v.truncate(n);
            v
        };
        let per_token_bools = |name: &str, default: bool| -> Vec<bool> {
            let mut v: Vec<bool> = r
                .str(name, "")
                .split(',')
                .map(|item| {
                    if pyc::strip(item).is_empty() {
                        default
                    } else {
                        truthy(item)
                    }
                })
                .collect();
            v.resize(v.len().max(n), default);
            v.truncate(n);
            v
        };
        let split_list = |name: &str, default: &str| -> Vec<String> {
            r.str(name, default)
                .split(',')
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect()
        };

        let registration = (|| {
            let names = [
                "DISCORD_CLIENT_ID",
                "DISCORD_CLIENT_SECRET",
                "DISCORD_BOT_TOKEN",
                "REGISTRATION_BRIDGE_SECRET",
                "DISCORD_GUILD_ID",
                "SITE_URL",
            ];
            if !names
                .iter()
                .all(|n| src.get(n).is_some_and(|v| !v.is_empty()))
            {
                return None;
            }
            let secret = src.get("REGISTRATION_BRIDGE_SECRET")?;
            if pyc::len_chars(&secret) < 32 {
                tracing::warn!("REGISTRATION_BRIDGE_SECRET 少于 32 个字符，自助注册保持关闭。");
                return None;
            }
            Some(RegistrationEnv {
                client_id: src.get("DISCORD_CLIENT_ID")?,
                client_secret: src.get("DISCORD_CLIENT_SECRET")?,
                bot_token: src.get("DISCORD_BOT_TOKEN")?,
                bridge_secret: secret,
                guild_id: src.get("DISCORD_GUILD_ID")?.trim().to_string(),
                role_id: r.trimmed("DISCORD_ROLE_ID"),
                site_url: format!("{}/", src.get("SITE_URL")?.trim_end_matches('/')),
                key_expires_days: r.nonneg("REGISTER_EXPIRES_DAYS", 0),
                key_rpm: r.nonneg("REGISTER_RPM", 5).max(1),
                max_users: r.nonneg("REGISTER_MAX_USERS", 10),
                reset_at: r.trimmed("REGISTER_RESET_AT"),
                key_features: {
                    let f = r.str("REGISTER_FEATURES", "image");
                    f.trim().to_string()
                },
                min_account_days: r.nonneg("REGISTER_MIN_ACCOUNT_DAYS", 7),
                member_role_id: r.trimmed("DISCORD_MEMBER_ROLE_ID"),
                admin_ids: r
                    .str("ADMIN_DISCORD_IDS", "")
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                    .collect(),
                idle_days: r.nonneg("KEY_INACTIVITY_DELETE_DAYS", 3),
            })
        })();

        Self {
            data_dir: PathBuf::from(r.str("DATA_DIR", "data")),
            admin_password: r.str("ADMIN_PASSWORD", ""),
            admin_cookie_secure: !matches!(
                r.str("ADMIN_COOKIE_SECURE", "1")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "0" | "false" | "no" | "off"
            ),
            secret_key: r.str("SECRET_KEY", ""),
            tz: r.str("TZ", "Asia/Shanghai"),
            day_reset_hour: r.int("DAY_RESET_HOUR", 7),
            cors_origins: split_list("CORS_ORIGINS", "*"),
            admin_allowed_origins: split_list("ADMIN_ALLOWED_ORIGINS", ""),
            nai_token_v5_daily_limits: per_token_ints("NAI_TOKEN_V5_DAILY_LIMITS", 0),
            nai_token_allow_anlas: per_token_bools("NAI_TOKEN_ALLOW_ANLAS", true),
            nai_tokens,
            image_host: r.str("NAI_IMAGE_HOST", "https://image.novelai.net"),
            text_host: r.str("NAI_TEXT_HOST", "https://text.novelai.net"),
            text_host_legacy: r.str("NAI_TEXT_HOST_LEGACY", "https://api.novelai.net"),
            upstream_proxy: r.trimmed("UPSTREAM_PROXY"),
            upstream_user_agent: r.trimmed("UPSTREAM_USER_AGENT"),
            upstream_http2: r.bool("UPSTREAM_HTTP2", true),
            post_request_jitter_min: r.float("POST_REQUEST_JITTER_MIN", 1.0),
            post_request_jitter_max: r.float("POST_REQUEST_JITTER_MAX", 3.0),
            single_image_slot_enforced: r.bool("SINGLE_IMAGE_SLOT_ENFORCED", false),
            global_concurrency: r.int("GLOBAL_CONCURRENCY", 1),
            queue_timeout: r.int("QUEUE_TIMEOUT", 90),
            image_min_interval: r.float("IMAGE_MIN_INTERVAL", 15.0),
            image_429_cooldown_seconds: r.float("IMAGE_429_COOLDOWN_SECONDS", 60.0),
            auth_fail_max: r.int("AUTH_FAIL_MAX", 20),
            auth_fail_window: r.int("AUTH_FAIL_WINDOW", 600),
            auth_block_seconds: r.int("AUTH_BLOCK_SECONDS", 900),
            login_max_attempts: r.int("LOGIN_MAX_ATTEMPTS", 5),
            login_window_seconds: r.int("LOGIN_WINDOW_SECONDS", 300),
            key_inactivity_delete_days: r.int("KEY_INACTIVITY_DELETE_DAYS", 3),
            discord_bot_token: r.str("DISCORD_BOT_TOKEN", ""),
            site_url: r.str("SITE_URL", ""),
            alert_user_id: r.str("ALERT_USER_ID", ""),
            alert_channel_id: r.str("ALERT_CHANNEL_ID", ""),
            alert_webhook_url: r.str("ALERT_WEBHOOK_URL", ""),
            announce_channel_id: r.str("ANNOUNCE_CHANNEL_ID", ""),
            discord_invite_url: r.str("DISCORD_INVITE_URL", ""),
            discord_guild_id: r.str("DISCORD_GUILD_ID", ""),
            discord_api_base: r.str("DISCORD_API_BASE", "https://discord.com/api/v10"),
            audit_prompts: r.bool("AUDIT_PROMPTS", false),
            audit_thumbs: r.bool("AUDIT_THUMBS", false),
            audit_retention_days: r.int("AUDIT_RETENTION_DAYS", 7),
            usage_log_retention_days: r.int("USAGE_LOG_RETENTION_DAYS", 0),
            safe_clamp: r.bool("SAFE_CLAMP", true),
            max_pixels: r.int("MAX_PIXELS", 1024 * 1024),
            max_steps: r.int("MAX_STEPS", 28),
            allow_img2img: r.bool("ALLOW_IMG2IMG", false),
            global_monthly_anlas: r.float("GLOBAL_MONTHLY_ANLAS", 10000.0),
            global_daily_anlas: r.int("GLOBAL_DAILY_ANLAS", 200),
            default_daily_images: r.int("DEFAULT_DAILY_IMAGES", 100),
            default_daily_anlas: r.float("DEFAULT_DAILY_ANLAS", 100.0),
            default_daily_v5: r.int("DEFAULT_DAILY_V5", 50),
            default_monthly_anlas: r.float("DEFAULT_MONTHLY_ANLAS", 2500.0),
            default_daily_text_tokens: r.int("DEFAULT_DAILY_TEXT_TOKENS", 150000),
            default_rpm: r.int("DEFAULT_RPM", 10),
            default_expires_days: r.int("DEFAULT_KEY_EXPIRES_DAYS", 30),
            max_text_output_tokens: r.int("MAX_TEXT_OUTPUT_TOKENS", 250),
            max_input_chars: r.int("MAX_INPUT_CHARS", 24000),
            seed_demo_key: r.bool("SEED_DEMO_KEY", false),
            forwarded_allow_ips: split_list("FORWARDED_ALLOW_IPS", "127.0.0.1"),
            registration,
        }
    }

    /// `site_flags.get(db, spec, settings)` 用的 `settings` 视图：只有带 env 回退的两个全站 Anlas 预算
    /// （全站 V5 归动态额度管，没有环境变量，见 `ops::algo_owned`）。
    pub fn settings_view(&self) -> serde_json::Value {
        serde_json::json!({
            "global_daily_anlas": self.global_daily_anlas,
            "global_monthly_anlas": self.global_monthly_anlas,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("nai_gate.db")
    }
    pub fn announcement_path(&self) -> PathBuf {
        self.data_dir.join("announcement.html")
    }
    pub fn upstream_tokens_path(&self) -> PathBuf {
        self.data_dir.join("upstream_tokens.json")
    }
    pub fn private_params_path(&self) -> PathBuf {
        self.data_dir.join("private_params.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_like_python() {
        let c = EnvConfig::load(&env(&[
            ("NAI_TOKENS", " pst-a ,\npst-b,,"),
            ("NAI_TOKEN_V5_DAILY_LIMITS", "150,x,7,9"),
            ("NAI_TOKEN_ALLOW_ANLAS", "0"),
            ("QUEUE_TIMEOUT", "  "),
            ("IMAGE_MIN_INTERVAL", "abc"),
            ("UPSTREAM_HTTP2", "Off"),
            ("ADMIN_COOKIE_SECURE", "nope"),
        ]));
        assert_eq!(c.nai_tokens, ["pst-a", "pst-b"]);
        assert_eq!(c.nai_token_v5_daily_limits, [150, 0]);
        assert_eq!(c.nai_token_allow_anlas, [false, true]);
        assert_eq!(c.queue_timeout, 90);
        assert_eq!(c.image_min_interval, 15.0);
        assert!(!c.upstream_http2);
        assert!(c.admin_cookie_secure, "只有 0/false/no/off 才关闭");
        assert!(c.registration.is_none());
        assert_eq!(c.max_text_output_tokens, 250);
    }

    #[test]
    fn registration_requires_long_secret() {
        let base = [
            ("DISCORD_CLIENT_ID", "1"),
            ("DISCORD_CLIENT_SECRET", "s"),
            ("DISCORD_BOT_TOKEN", "t"),
            ("DISCORD_GUILD_ID", " 42 "),
            ("SITE_URL", "https://x.test//"),
        ];
        let mut short = env(&base);
        short.insert("REGISTRATION_BRIDGE_SECRET".into(), "short".into());
        assert!(EnvConfig::load(&short).registration.is_none());
        let mut ok = env(&base);
        ok.insert("REGISTRATION_BRIDGE_SECRET".into(), "a".repeat(32));
        ok.insert("REGISTER_RPM".into(), "0".into());
        ok.insert("ADMIN_DISCORD_IDS".into(), "123, abc,456".into());
        let reg = EnvConfig::load(&ok).registration.unwrap();
        assert_eq!(reg.guild_id, "42");
        assert_eq!(reg.site_url, "https://x.test/");
        assert_eq!(reg.key_rpm, 1);
        assert_eq!(reg.admin_ids, ["123", "456"]);
        // 新领的 Key 默认不过期（站长 10-15，docs/v3-deviations.md #70）
        assert_eq!(reg.key_expires_days, 0);
        ok.insert("REGISTER_EXPIRES_DAYS".into(), "30".into());
        assert_eq!(
            EnvConfig::load(&ok).registration.unwrap().key_expires_days,
            30
        );
    }
}
