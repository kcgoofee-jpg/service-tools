//! 环境变量：只放密钥和基础设施（地址、ID、网络、数据目录）。启动时解析一次，之后只读。
//!
//! 配置只有两层：环境变量（这里）和后台可改的数据库设置（`site_settings`，没设过时用代码里的常量）。
//! 运行时可调的参数不再有环境变量（docs/v3-deviations.md #91～#95），也不再有私密参数文件 `private_params.json`；
//! 没有后台入口、只是代码常量的几个数（排队超时、登录限流……）在下面的 `pub const` 里，
//! [`EnvConfig`] 里对应的字段只是方便测试和 `cargo xtask dev` 覆盖，生产不读环境变量。
//!
//! 解析规则逐个复刻旧版（app/config.py、registration.py、ops.py、status_page.py 里散落的 `os.getenv`）：
//! - 布尔：`1/true/yes/on/y`（不区分大小写、去空白）为真，其余为假；未设置用默认值；
//!   例外 `ADMIN_COOKIE_SECURE`：只有 `0/false/no/off` 为假（默认安全）。
//! - 整数 / 浮点：空串或解析失败都用默认值（`int(os.environ.get(name, "").strip() or default)`）。
//! - 每 Token 列表（`NAI_TOKEN_ALLOW_ANLAS`）补齐 / 截断到 Token 个数。
//! - 3.0 删掉的上游相关变量（[`REMOVED_UPSTREAM_VARS`]）不再读取；还留在 `.env` 里时启动日志提醒一句。

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

// ---- 代码常量（原来是环境变量，现在不读环境了）----

/// 一张图在某个排队环节最多等多久（秒）。后台「额度与保护 → 图片排队与冷却」改过的值（`runtime_queue_timeout`）优先。
pub const QUEUE_TIMEOUT: i64 = 90;
/// 每个上游账号两次发图的最小间隔（秒）。后台改过的值（`runtime_image_min_interval`）优先。
pub const IMAGE_MIN_INTERVAL: f64 = 15.0;
/// 上游 429 后全站暂停出图的最短冷却（秒）。后台改过的值（`runtime_image_429_cooldown_seconds`）优先。
pub const IMAGE_429_COOLDOWN_SECONDS: f64 = 60.0;
/// 单 IP 在窗口内使用无效 Key 的次数上限，超过则临时拦截。
pub const AUTH_FAIL_MAX: i64 = 20;
pub const AUTH_FAIL_WINDOW: i64 = 600;
pub const AUTH_BLOCK_SECONDS: i64 = 900;
/// 后台登录：每 IP 窗口内最多尝试次数。
pub const LOGIN_MAX_ATTEMPTS: i64 = 5;
pub const LOGIN_WINDOW_SECONDS: i64 = 300;

/// 3.0 删掉的上游相关环境变量与替代做法（docs/v3-deviations.md #86 #87 #88）。
pub const REMOVED_UPSTREAM_VARS: [(&str, &str); 7] = [
    ("NAI_TOKEN", "改用 NAI_TOKENS（只有一把也用这个名字）"),
    (
        "NAI_TOKEN_V5_DAILY_LIMITS",
        "在后台「上游令牌池」按 Token 设 V5 日限额",
    ),
    (
        "SINGLE_IMAGE_SLOT_ENFORCED",
        "在后台「上游令牌池」按 Token 设出图并发（1～4）",
    ),
    ("UPSTREAM_USER_AGENT", "浏览器指纹不可配置"),
    ("UPSTREAM_HTTP2", "HTTP/2 由 TLS ALPN 协商"),
    (
        "POST_REQUEST_JITTER_MIN",
        "在后台「额度与保护」设出图后静默",
    ),
    (
        "POST_REQUEST_JITTER_MAX",
        "在后台「额度与保护」设出图后静默",
    ),
];

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
/// 领 Key 的名额、有效期、功能、身份组限定都是数据库设置（见 `ops::registration_settings`），不在这里。
#[derive(Debug, Clone)]
pub struct RegistrationEnv {
    pub client_id: String,
    pub client_secret: String,
    pub bot_token: String,
    pub bridge_secret: String,
    pub guild_id: String,
    /// 以 `/` 结尾。
    pub site_url: String,
    pub member_role_id: String,
}

#[derive(Debug, Clone)]
pub struct EnvConfig {
    // 基础
    pub data_dir: PathBuf,
    pub admin_password: String,
    pub admin_cookie_secure: bool,
    pub secret_key: String,
    pub cors_origins: Vec<String>,
    pub admin_allowed_origins: Vec<String>,
    /// 信任哪些直连对端带来的 X-Forwarded-For（uvicorn 的 FORWARDED_ALLOW_IPS，默认只信任本机）。
    pub forwarded_allow_ips: Vec<String>,
    // 上游
    pub nai_tokens: Vec<String>,
    /// 只在没有 `upstream_tokens.json`（后台没接管过令牌池）时用：每把 Token 能不能花 Anlas，默认允许。
    pub nai_token_allow_anlas: Vec<bool>,
    pub image_host: String,
    pub upstream_proxy: String,
    // 以下 8 个是代码常量（见文件开头的 `pub const`），不读环境变量；字段只为让测试和 `cargo xtask dev` 能改
    pub queue_timeout: i64,
    pub image_min_interval: f64,
    pub image_429_cooldown_seconds: f64,
    pub auth_fail_max: i64,
    pub auth_fail_window: i64,
    pub auth_block_seconds: i64,
    pub login_max_attempts: i64,
    pub login_window_seconds: i64,
    /// 连续这么多天没成功出过图的 Key 自动回收（0 = 关）。只在这里解析一次，机器人、后台、定时任务共用。
    pub key_inactivity_delete_days: i64,
    // Discord / 告警 / 公告
    pub discord_bot_token: String,
    pub site_url: String,
    pub alert_user_id: String,
    pub announce_channel_id: String,
    pub discord_invite_url: String,
    pub discord_guild_id: String,
    /// Discord REST 基地址（测试指向本地假服务）。
    pub discord_api_base: String,
    // 记录
    pub usage_log_retention_days: i64,
    // 全站预算
    pub global_monthly_anlas: f64,
    pub global_daily_anlas: i64,
    // 新 Key 默认值
    pub default_daily_images: i64,
    pub default_daily_anlas: f64,
    pub default_daily_v5: i64,
    pub default_monthly_anlas: f64,
    pub default_rpm: i64,
    pub default_expires_days: i64,
    pub seed_demo_key: bool,
    pub registration: Option<RegistrationEnv>,
}

impl EnvConfig {
    pub fn from_process() -> Self {
        Self::load(&ProcessEnv)
    }

    pub fn load(src: &dyn EnvSource) -> Self {
        let r = Reader(src);
        for (name, instead) in REMOVED_UPSTREAM_VARS {
            if src.get(name).is_some() {
                tracing::warn!("环境变量 {name} 已删除、不再读取：{instead}");
            }
        }
        let raw_tokens = src.get("NAI_TOKENS").unwrap_or_default();
        let nai_tokens: Vec<String> = raw_tokens
            .replace('\n', ",")
            .split(',')
            .map(|t| pyc::strip(t).to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let n = nai_tokens.len();
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
                site_url: format!("{}/", src.get("SITE_URL")?.trim_end_matches('/')),
                member_role_id: r.trimmed("DISCORD_MEMBER_ROLE_ID"),
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
            cors_origins: split_list("CORS_ORIGINS", "*"),
            admin_allowed_origins: split_list("ADMIN_ALLOWED_ORIGINS", ""),
            nai_token_allow_anlas: per_token_bools("NAI_TOKEN_ALLOW_ANLAS", true),
            nai_tokens,
            image_host: r.str("NAI_IMAGE_HOST", "https://image.novelai.net"),
            upstream_proxy: r.trimmed("UPSTREAM_PROXY"),
            queue_timeout: QUEUE_TIMEOUT,
            image_min_interval: IMAGE_MIN_INTERVAL,
            image_429_cooldown_seconds: IMAGE_429_COOLDOWN_SECONDS,
            auth_fail_max: AUTH_FAIL_MAX,
            auth_fail_window: AUTH_FAIL_WINDOW,
            auth_block_seconds: AUTH_BLOCK_SECONDS,
            login_max_attempts: LOGIN_MAX_ATTEMPTS,
            login_window_seconds: LOGIN_WINDOW_SECONDS,
            key_inactivity_delete_days: r.nonneg("KEY_INACTIVITY_DELETE_DAYS", 3),
            discord_bot_token: r.str("DISCORD_BOT_TOKEN", ""),
            site_url: r.str("SITE_URL", ""),
            alert_user_id: r.str("ALERT_USER_ID", ""),
            announce_channel_id: r.str("ANNOUNCE_CHANNEL_ID", ""),
            discord_invite_url: r.str("DISCORD_INVITE_URL", ""),
            discord_guild_id: r.str("DISCORD_GUILD_ID", ""),
            discord_api_base: r.str("DISCORD_API_BASE", "https://discord.com/api/v10"),
            usage_log_retention_days: r.int("USAGE_LOG_RETENTION_DAYS", 0),
            global_monthly_anlas: r.float("GLOBAL_MONTHLY_ANLAS", 10000.0),
            global_daily_anlas: r.int("GLOBAL_DAILY_ANLAS", 200),
            default_daily_images: r.int("DEFAULT_DAILY_IMAGES", 100),
            default_daily_anlas: r.float("DEFAULT_DAILY_ANLAS", 100.0),
            default_daily_v5: r.int("DEFAULT_DAILY_V5", 50),
            default_monthly_anlas: r.float("DEFAULT_MONTHLY_ANLAS", 2500.0),
            default_rpm: r.int("DEFAULT_RPM", 10),
            default_expires_days: r.int("DEFAULT_KEY_EXPIRES_DAYS", 30),
            seed_demo_key: r.bool("SEED_DEMO_KEY", false),
            forwarded_allow_ips: split_list("FORWARDED_ALLOW_IPS", "127.0.0.1"),
            registration,
        }
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
            ("NAI_TOKEN_ALLOW_ANLAS", "0"),
            ("ADMIN_COOKIE_SECURE", "nope"),
        ]));
        assert_eq!(c.nai_tokens, ["pst-a", "pst-b"]);
        assert_eq!(c.nai_token_allow_anlas, [false, true]);
        assert!(c.admin_cookie_secure, "只有 0/false/no/off 才关闭");
        assert!(c.registration.is_none());
    }

    /// 删掉的变量不再读：`NAI_TOKEN` 不再是 `NAI_TOKENS` 的别名（docs/v3-deviations.md #88）。
    #[test]
    fn removed_upstream_vars_are_ignored() {
        let c = EnvConfig::load(&env(&[
            ("NAI_TOKEN", "pst-only"),
            ("NAI_TOKEN_V5_DAILY_LIMITS", "150"),
            ("SINGLE_IMAGE_SLOT_ENFORCED", "1"),
        ]));
        assert!(c.nai_tokens.is_empty());
        assert!(c.nai_token_allow_anlas.is_empty());
    }

    #[test]
    fn code_constants_ignore_the_environment() {
        // 这几个原来是环境变量，现在是代码常量：环境里设了也不读（docs/v3-deviations.md #93 #95）
        let c = EnvConfig::load(&env(&[
            ("QUEUE_TIMEOUT", "5"),
            ("IMAGE_MIN_INTERVAL", "0"),
            ("IMAGE_429_COOLDOWN_SECONDS", "1"),
            ("AUTH_FAIL_MAX", "7"),
            ("LOGIN_MAX_ATTEMPTS", "9"),
            ("TZ", "UTC"),
            ("DAY_RESET_HOUR", "3"),
        ]));
        assert_eq!(
            (
                c.queue_timeout,
                c.image_min_interval,
                c.image_429_cooldown_seconds
            ),
            (
                QUEUE_TIMEOUT,
                IMAGE_MIN_INTERVAL,
                IMAGE_429_COOLDOWN_SECONDS
            )
        );
        assert_eq!(
            (c.auth_fail_max, c.login_max_attempts),
            (AUTH_FAIL_MAX, LOGIN_MAX_ATTEMPTS)
        );
    }

    #[test]
    fn inactivity_days_parse_once() {
        let days = |v: Option<&str>| {
            let mut m = env(&[]);
            if let Some(v) = v {
                m.insert("KEY_INACTIVITY_DELETE_DAYS".into(), v.into());
            }
            EnvConfig::load(&m).key_inactivity_delete_days
        };
        assert_eq!(days(None), 3);
        assert_eq!(days(Some(" 10 ")), 10);
        assert_eq!(days(Some("0")), 0, "0 = 关闭回收");
        assert_eq!(days(Some("-5")), 0, "负数按 0 处理");
        assert_eq!(days(Some("abc")), 3, "读不懂用默认值");
        assert_eq!(days(Some("")), 3);
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
        ok.insert(
            "DISCORD_MEMBER_ROLE_ID".into(),
            " 900000000000000055 ".into(),
        );
        let reg = EnvConfig::load(&ok).registration.unwrap();
        assert_eq!(reg.guild_id, "42");
        assert_eq!(reg.site_url, "https://x.test/");
        assert_eq!(reg.member_role_id, "900000000000000055");
    }
}
