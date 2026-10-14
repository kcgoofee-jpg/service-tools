//! 奶妹：网关事件循环、斜杠命令分发、反馈弹窗、@ 回复、与网关的配置 / 心跳同步。
//!
//! 与 `integration/discord_bot.py` 逐行对应；与 Discord 的交互次序、ephemeral 标志、deferred 时机都照抄：
//! - `/register /quota /resetkey /help /open /limit /ban /unban /slots /revoke`：先 `defer(ephemeral)`（3 秒内确认），
//!   再去问网关，最后用跟进消息（ephemeral）回话；
//! - `/反馈`：**先问网关要题目、再弹窗**（Python 就是这个顺序；网关慢于 3 秒时弹窗会失败，成员看到
//!   「应用程序没有响应」——保持一致，不在这里悄悄改行为）；
//! - 命令抛出任何错误 → 打 `[bug]` 日志，并回一句「出了点问题……」（已确认过就用跟进消息，否则直接回复）；
//! - 不发私信、不加表情、不碰论坛（画廊点赞 / AI 评论没有移植）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use gate_core::pyc;
use serde_json::{Map, Value, json};
use tracing::{debug, error, info, warn};
use twilight_gateway::{Config as GatewayConfig, Message, Shard, ShardId};
use twilight_model::gateway::CloseFrame;

use crate::api::{self, ApiError, DiscordApi};
use crate::bridge::{Bridge, BridgeError, who_payload};
use crate::commands::{self, Command, NAIMEI_INTENTS};
use crate::config::NaimeiConfig;
use crate::events::{
    self, Gateway, GuildInfo, INTERACTION_APPLICATION_COMMAND, INTERACTION_MODAL_SUBMIT,
    Interaction, MessageCreate, ReadyInfo, User,
};
use crate::handlers::{self, Backend, HandlerError};
use crate::pystr::py_str;

/// 反馈弹窗的有效期（`FeedbackModal(timeout=900)`）。
pub const MODAL_TTL: Duration = Duration::from_secs(900);
/// 配置同步 / 心跳间隔。
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// 命令执行中可能出现的错误；全部会被 `tree.error` 等价逻辑接住。
#[derive(Debug, thiserror::Error)]
pub enum CmdError {
    #[error("{0}")]
    Handler(#[from] HandlerError),
    #[error("{0}")]
    Api(#[from] ApiError),
    #[error("{0}")]
    Bridge(#[from] BridgeError),
    #[error("{0}")]
    Invalid(String),
}

impl CmdError {
    /// 日志里的「异常类型」，对应 Python 的 `type(error).__name__`。
    fn kind(&self) -> &'static str {
        match self {
            CmdError::Handler(HandlerError::Key(_)) | CmdError::Bridge(BridgeError::NoSecret) => {
                "KeyError"
            }
            CmdError::Handler(HandlerError::Type(_)) => "TypeError",
            CmdError::Bridge(BridgeError::Handler(_)) => "HandlerError",
            CmdError::Api(_) => "HTTPException",
            CmdError::Invalid(_) => "TransformerError",
        }
    }
}

/// 同一频道 60 秒 / 同一成员 5 分钟 / 全服 20 秒各回一次 @。纯逻辑，时间由调用方传入（单调秒）。
#[derive(Debug, Default)]
pub struct MentionLimiter {
    last: HashMap<LimitKey, f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LimitKey {
    Channel(u64),
    Author(u64),
    Global,
}

impl MentionLimiter {
    /// 返回是否放行；放行时同时记录三个时间戳（Python 在发送前记录，发送失败也不回退）。
    pub fn allow(&mut self, now: f64, channel: u64, author: u64) -> bool {
        let since = |last: &HashMap<LimitKey, f64>, k: LimitKey| {
            now - last.get(&k).copied().unwrap_or(-1e9)
        };
        if since(&self.last, LimitKey::Channel(channel)) < 60.0
            || since(&self.last, LimitKey::Author(author)) < 300.0
            || since(&self.last, LimitKey::Global) < 20.0
        {
            return false;
        }
        if self.last.len() > 500 {
            self.last.clear();
        }
        self.last.insert(LimitKey::Channel(channel), now);
        self.last.insert(LimitKey::Author(author), now);
        self.last.insert(LimitKey::Global, now);
        true
    }
}

/// 正文里明确的 `<@机器人ID>` / `<@!机器人ID>`；回复奶妹的消息（自动带 ping）不算。
pub fn mentions_bot(content: &str, bot_id: &str) -> bool {
    content.contains(&format!("<@{bot_id}>")) || content.contains(&format!("<@!{bot_id}>"))
}

struct PendingModal {
    created: Instant,
    /// (题目 id, 对应下拉框的 custom_id)，顺序即题目顺序。
    fields: Vec<(Value, String)>,
}

#[derive(Default)]
struct Inner {
    bot: Option<User>,
    guilds: HashMap<String, GuildInfo>,
    modals: HashMap<String, PendingModal>,
    mentions: MentionLimiter,
    sync_started: bool,
    /// 后台「Discord」页的配置（画廊相关，当前版本只读不执行）。
    bot_config: Map<String, Value>,
}

/// 墙钟（Unix 秒）。测试里换成固定值，对拍时「Key 剩余有效期」才不会随时间漂移。
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

pub struct Naimei {
    pub cfg: NaimeiConfig,
    pub api: DiscordApi,
    pub bridge: Bridge,
    clock: Clock,
    started: Instant,
    /// 最近一次心跳延迟（毫秒）；`u64::MAX` = 还没有数据。
    latency_ms: AtomicU64,
    ready: AtomicBool,
    inner: Mutex<Inner>,
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn random_hex() -> String {
    format!("{:032x}", rand::random::<u128>())
}

impl Naimei {
    /// `discord_proxy` 仅测试用，生产环境传 `None`。
    pub fn new(cfg: NaimeiConfig, discord_proxy: Option<String>) -> Arc<Self> {
        Self::with_clock(cfg, discord_proxy, Box::new(unix_now))
    }

    pub fn with_clock(cfg: NaimeiConfig, discord_proxy: Option<String>, clock: Clock) -> Arc<Self> {
        let api = DiscordApi::new(&cfg.token, discord_proxy);
        let bridge = Bridge::new(cfg.backend.clone(), cfg.secret.clone());
        let mut bot_config = Map::new();
        // 与 Python 的 CONFIG 默认值一致；拉不到后台配置时一律按「关」
        bot_config.insert("gallery_forum".into(), json!("跑图分享"));
        bot_config.insert("gallery_like".into(), json!(0));
        bot_config.insert("gallery_ai".into(), json!(0));
        bot_config.insert("gallery_ai_daily".into(), json!(30));
        bot_config.insert("gallery_ai_model".into(), json!("LongCat-2.5-Preview"));
        Arc::new(Naimei {
            cfg,
            api,
            bridge,
            clock,
            started: Instant::now(),
            latency_ms: AtomicU64::new(u64::MAX),
            ready: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                bot_config,
                ..Inner::default()
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mono_now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// 心跳延迟；上报时按 Python 的 `round(latency_seconds * 1000)` 四舍五入（银行家舍入）成毫秒。
    pub fn set_latency(&self, latency: Option<Duration>) {
        let ms = latency.map_or(u64::MAX, |d| {
            u64::try_from(pyc::round0(d.as_secs_f64() * 1000.0)).unwrap_or(u64::MAX)
        });
        self.latency_ms.store(ms, Ordering::Relaxed);
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    pub fn set_bot_user(&self, user: User) {
        self.lock().bot = Some(user);
    }

    pub fn upsert_guild(&self, g: GuildInfo) {
        if g.unavailable || g.name.is_none() {
            return;
        }
        self.lock().guilds.insert(g.id.clone(), g);
    }

    pub fn remove_guild(&self, id: &str) {
        self.lock().guilds.remove(id);
    }

    // ---------- 向网关要数据 ----------

    /// `backend(path, interaction, extra_id, extra)`。
    async fn backend(
        &self,
        path: &str,
        it: &Interaction,
        extra_id: Option<&str>,
        extra: &[(&str, Value)],
    ) -> Result<Backend, CmdError> {
        let payload = who_payload(it, extra_id, extra)?;
        Ok(self.bridge.command(path, &payload).await?)
    }

    // ---------- 交互入口 ----------

    /// 处理一条 INTERACTION_CREATE（斜杠命令 / 弹窗提交）；其他类型忽略。
    pub async fn handle_interaction(self: &Arc<Self>, it: Interaction) {
        match it.kind {
            INTERACTION_APPLICATION_COMMAND => self.handle_command(&it).await,
            INTERACTION_MODAL_SUBMIT => self.handle_modal_submit(&it).await,
            _ => {}
        }
    }

    async fn handle_command(self: &Arc<Self>, it: &Interaction) {
        let data = it.data.as_ref();
        let name = data.and_then(|d| d.name.as_deref());
        // 只认斜杠命令（type 1）；右键菜单命令我们没有注册，等同找不到命令
        let is_chat_input = data.and_then(|d| d.kind).is_none_or(|k| k == 1);
        let command = name.filter(|_| is_chat_input).and_then(Command::from_name);
        let responder = Responder::new(self, it);
        let result = match command {
            Some(c) => self.run_command(c, it, &responder).await,
            None => Err(CmdError::Invalid(format!(
                "Application command '{}' not found",
                name.unwrap_or("?")
            ))),
        };
        if let Err(err) = result {
            // `tree.error`：打日志（监控会抓 [bug]），并告诉成员不是他的问题
            let shown = command.map_or("?", Command::name);
            error!("[bug] bot command /{shown} failed: {}: {err}", err.kind());
            let text = handlers::GENERIC_ERROR;
            let sent = if responder.is_done() {
                responder.followup(text).await
            } else {
                responder.send_message(text).await
            };
            if let Err(e) = sent {
                debug!("错误提示也没发出去：{e}");
            }
        }
    }

    async fn run_command(
        self: &Arc<Self>,
        command: Command,
        it: &Interaction,
        r: &Responder<'_>,
    ) -> Result<(), CmdError> {
        match command {
            Command::Register => self.cmd_register(it, r).await,
            Command::Quota => self.cmd_quota(it, r).await,
            Command::Feedback => self.cmd_feedback(it, r).await,
            Command::ResetKey => self.cmd_resetkey(it, r).await,
            Command::Help => self.cmd_help(it, r).await,
            Command::Open => {
                let state = string_option(it, "state")?;
                if state != "on" && state != "off" {
                    return Err(CmdError::Invalid(format!("state 不是可选值：{state}")));
                }
                self.run_ops(it, r, "open", &[("value", json!(state))])
                    .await
            }
            Command::Limit => {
                // 0～1000 的范围由 Discord 客户端限制；discord.py 的 `Range` 在服务端不再校验，
                // 收到什么就原样 `str(count)` 转给网关（网关会拒绝非数字，见 golden `limit_out_of_range`）
                let count = it
                    .option("count")
                    .and_then(|o| o.value.as_ref())
                    .ok_or_else(|| CmdError::Invalid("missing option count".into()))?;
                self.run_ops(it, r, "limit", &[("value", json!(py_str(count)))])
                    .await
            }
            Command::Ban => {
                let target = user_option(it, "member")?;
                self.run_ops(it, r, "ban", &[("target", json!(target))])
                    .await
            }
            Command::Unban => {
                let target = user_option(it, "member")?;
                self.run_ops(it, r, "unban", &[("target", json!(target))])
                    .await
            }
            Command::Slots => self.cmd_slots(it, r).await,
            Command::Revoke => self.cmd_revoke(it, r).await,
        }
    }

    // ---------- 各命令 ----------

    /// `/register`（`discord_registration.handle_register`）。
    async fn cmd_register(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        let invite = pyc::strip(&string_option_opt(it, "key").unwrap_or_default()).to_owned();
        let same_guild = it
            .guild_id
            .as_deref()
            .and_then(|g| g.parse::<u64>().ok())
            .is_some_and(|g| g == self.cfg.guild_id);
        if !same_guild || it.actor().is_none() {
            return Ok(r.send_message(handlers::REGISTER_WRONG_GUILD).await?);
        }
        let secret = self.cfg.secret.as_deref().unwrap_or("");
        if secret.is_empty() {
            return Ok(r.send_message(handlers::REGISTER_NOT_CONFIGURED).await?);
        }
        r.defer(true).await?;
        let payload = handlers::register_payload(it, &invite)?;
        // 直发：斜杠命令已由 Discord 验明身份，不走 OAuth，Key 只在本人可见的临时消息里给
        let message = match self.bridge.issue(secret, &payload).await {
            Some((status, body)) => handlers::register_message(status, &body)?,
            None => handlers::REGISTER_UNAVAILABLE.to_owned(),
        };
        Ok(r.followup(&message).await?)
    }

    async fn cmd_quota(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        r.defer(true).await?;
        let quota = self.backend("/self-register/quota", it, None, &[]).await?;
        if !quota.is_ok() {
            return Ok(r.followup(&quota.text()).await?);
        }
        // Python 先拼完额度各行、再去问 /info；额度数据残缺时不会发第二个请求
        handlers::quota_text(&quota.data, None, self.now())?;
        let info = self.backend("/self-register/info", it, None, &[]).await?;
        let text =
            handlers::quota_text(&quota.data, info.is_ok().then_some(&info.data), self.now())?;
        Ok(r.followup(&text).await?)
    }

    async fn cmd_resetkey(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        r.defer(true).await?;
        let b = self
            .backend("/self-register/resetkey", it, None, &[])
            .await?;
        let text = if b.is_ok() {
            handlers::resetkey_text(&b.data)?
        } else {
            b.text()
        };
        Ok(r.followup(&text).await?)
    }

    async fn cmd_help(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        r.defer(true).await?;
        let b = self.backend("/self-register/info", it, None, &[]).await?;
        let text = handlers::help_text(&self.cfg.site, b.is_ok().then_some(&b.data))?;
        Ok(r.followup(&text).await?)
    }

    async fn run_ops(
        &self,
        it: &Interaction,
        r: &Responder<'_>,
        action: &str,
        extra: &[(&str, Value)],
    ) -> Result<(), CmdError> {
        r.defer(true).await?;
        let mut all: Vec<(&str, Value)> = vec![("action", json!(action))];
        all.extend(extra.iter().cloned());
        let b = self.backend("/self-register/ops", it, None, &all).await?;
        Ok(r.followup(&handlers::ops_text(&b)?).await?)
    }

    async fn cmd_slots(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        r.defer(true).await?;
        let b = self.backend("/self-register/slots", it, None, &[]).await?;
        let text = if b.is_ok() {
            handlers::slots_text(&b.data)?
        } else {
            b.text()
        };
        Ok(r.followup(&text).await?)
    }

    async fn cmd_revoke(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        let member = user_option(it, "member")?;
        r.defer(true).await?;
        let b = self
            .backend("/self-register/revoke", it, Some(&member), &[])
            .await?;
        Ok(r.followup(&handlers::revoke_text(&member, &b)).await?)
    }

    /// `/反馈`：现取题目 → 弹出 Discord 自带表单。成员自己发起，不私信、不群发。
    async fn cmd_feedback(&self, it: &Interaction, r: &Responder<'_>) -> Result<(), CmdError> {
        let b = self
            .backend("/self-register/feedback/questions", it, None, &[])
            .await?;
        if !b.is_ok() || handlers::questions_empty(&b.data)? {
            return Ok(r.send_message(handlers::FEEDBACK_UNAVAILABLE).await?);
        }
        let questions = handlers::parse_questions(&b.data)?;
        let modal_id = random_hex();
        let select_ids: Vec<String> = questions.iter().map(|_| random_hex()).collect();
        let modal = handlers::modal_json(&questions, &modal_id, &select_ids);
        r.send_modal(modal).await?;
        // discord.py 在 send_modal 成功后才登记弹窗并开始 900 秒计时
        let fields = questions
            .into_iter()
            .map(|q| q.id)
            .zip(select_ids)
            .collect();
        let mut inner = self.lock();
        inner.modals.retain(|_, m| m.created.elapsed() < MODAL_TTL);
        inner.modals.insert(
            modal_id,
            PendingModal {
                created: Instant::now(),
                fields,
            },
        );
        Ok(())
    }

    /// 弹窗提交（`on_submit`）。出错只记日志，不回成员消息（`Modal.on_error` 的默认行为）；
    /// 未知 / 过期的弹窗直接忽略。
    async fn handle_modal_submit(self: &Arc<Self>, it: &Interaction) {
        let Some(custom_id) = it.data.as_ref().and_then(|d| d.custom_id.clone()) else {
            return;
        };
        let fields = {
            let inner = self.lock();
            match inner.modals.get(&custom_id) {
                Some(m) if m.created.elapsed() < MODAL_TTL => m.fields.clone(),
                _ => return,
            }
        };
        match self.submit_feedback(it, &fields).await {
            // 无异常才 stop()；出错的弹窗继续留着（成员可以重新提交），直到超时
            Ok(()) => {
                self.lock().modals.remove(&custom_id);
            }
            Err(e) => error!("Ignoring exception in modal: {}: {e}", e.kind()),
        }
    }

    async fn submit_feedback(
        &self,
        it: &Interaction,
        fields: &[(Value, String)],
    ) -> Result<(), CmdError> {
        let r = Responder::new(self, it);
        r.defer(true).await?;
        let components = it
            .data
            .as_ref()
            .map(|d| d.components.as_slice())
            .unwrap_or(&[]);
        let submitted = events::modal_values(components);
        let answers = handlers::feedback_answers(fields, &submitted)?;
        let username = it.actor().map_or("", |u| u.username.as_str());
        let b = self
            .backend(
                "/self-register/feedback",
                it,
                None,
                &[
                    ("answers", Value::Object(answers)),
                    ("username", json!(pyc::take_chars(username, 80))),
                ],
            )
            .await?;
        Ok(r.followup(&handlers::feedback_receipt(&b)).await?)
    }

    // ---------- @奶妹 ----------

    /// 有人在频道里 @奶妹 → 回一句并 @ 服务器主人。限流见 [`MentionLimiter`]。
    pub async fn handle_message(self: &Arc<Self>, msg: MessageCreate) {
        let now = self.mono_now();
        self.handle_message_at(msg, now).await;
    }

    pub async fn handle_message_at(self: &Arc<Self>, msg: MessageCreate, now: f64) {
        let (guild, bot_id) = {
            let inner = self.lock();
            (
                msg.guild_id
                    .as_ref()
                    .and_then(|g| inner.guilds.get(g))
                    .cloned(),
                inner.bot.as_ref().map(|b| b.id.clone()),
            )
        };
        let (Some(guild), Some(bot_id)) = (guild, bot_id) else {
            return;
        };
        if msg.author.bot || !mentions_bot(&msg.content, &bot_id) {
            return; // 只认正文里明确的 @奶妹
        }
        let (Ok(channel), Ok(author)) =
            (msg.channel_id.parse::<u64>(), msg.author.id.parse::<u64>())
        else {
            return;
        };
        if !self.lock().mentions.allow(now, channel, author) {
            return;
        }
        let Some(owner) = guild
            .owner_id
            .as_deref()
            .and_then(|o| o.parse::<u64>().ok())
        else {
            return;
        };
        let content = handlers::mention_reply_text(owner);
        let nonce = rand::random::<u64>();
        let body = api::mention_reply_body(
            &content,
            &msg.id,
            &msg.channel_id,
            msg.guild_id.as_deref(),
            owner,
            nonce,
        );
        match self.api.create_message(&msg.channel_id, &body).await {
            Ok(()) => info!(
                "[mention] replied in #{} to {}",
                msg.channel_id, msg.author.id
            ),
            Err(e) => error!("[bug] mention reply failed: {e}"),
        }
    }

    // ---------- 启动 / 同步 ----------

    /// READY：把斜杠命令同步到指定服务器，然后（只启动一次）开始每分钟的配置同步 / 心跳。
    pub async fn on_ready(self: Arc<Self>, ready: ReadyInfo) {
        let defs = commands::definitions();
        if let Err(e) = self
            .api
            .sync_guild_commands(&ready.application_id, self.cfg.guild_id, &defs)
            .await
        {
            // discord.py 的 on_ready 在 tree.sync 抛错时就此中断（既不打印 ready 也不启动同步循环）
            error!("[bug] on_ready failed: {e}");
            return;
        }
        info!(
            "[bot] ready as {}, commands synced",
            ready.user.display_tag()
        );
        self.ready.store(true, Ordering::Relaxed);
        let start = {
            let mut inner = self.lock();
            !std::mem::replace(&mut inner.sync_started, true) // 断线重连也会再触发 READY，只启动一次
        };
        if start {
            tokio::spawn(self.clone().sync_loop());
        }
    }

    /// 每分钟：拉配置、报心跳（后台据此显示在线状态）。
    pub async fn sync_loop(self: Arc<Self>) {
        let ready_at = self.now();
        loop {
            if let Err(e) = self.sync_once(ready_at).await {
                warn!("[sync_loop] heart-beat warning: {e}");
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    }

    pub async fn sync_once(&self, ready_at: f64) -> Result<(), BridgeError> {
        if let Some(Value::Object(cfg)) = self
            .bridge
            .bot(reqwest::Method::GET, "/config", None)
            .await?
        {
            let mut inner = self.lock();
            for (k, v) in cfg {
                if inner.bot_config.contains_key(&k) {
                    inner.bot_config.insert(k, v);
                }
            }
        }
        let status = self.status_report(ready_at);
        self.bridge
            .bot(
                reqwest::Method::POST,
                "/report",
                Some(&json!({"status": status})),
            )
            .await?;
        Ok(())
    }

    /// 心跳里的 `status`：键顺序与 Python 一致。
    /// 画廊 AI 评论没有移植，`ai_ready` 恒为 false、`ai_today` 恒为 0。
    pub fn status_report(&self, ready_at: f64) -> Value {
        let inner = self.lock();
        let guild = inner
            .guilds
            .get(&self.cfg.guild_id.to_string())
            .and_then(|g| g.name.clone())
            .unwrap_or_default();
        let user = inner
            .bot
            .as_ref()
            .map(User::display_tag)
            .unwrap_or_else(|| "None".into());
        let latency = match self.latency_ms.load(Ordering::Relaxed) {
            u64::MAX => 0,
            ms => ms,
        };
        json!({
            "user": user,
            "guild": guild,
            "latency_ms": latency,
            "ready_at": ready_at,
            "ai_ready": false,
            "ai_today": 0,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// 后台配置的当前值（测试用）。
    pub fn bot_config(&self) -> Map<String, Value> {
        self.lock().bot_config.clone()
    }
}

/// 一次交互的回复器：记录是否已经响应过（对应 `interaction.response.is_done()`）。
pub struct Responder<'a> {
    app: &'a Naimei,
    it: &'a Interaction,
    done: AtomicBool,
}

impl<'a> Responder<'a> {
    pub fn new(app: &'a Naimei, it: &'a Interaction) -> Self {
        Responder {
            app,
            it,
            done: AtomicBool::new(false),
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    async fn callback(&self, body: Value) -> Result<(), ApiError> {
        self.app
            .api
            .callback(&self.it.id, &self.it.token, &body)
            .await?;
        self.done.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// `response.defer(ephemeral=...)`
    pub async fn defer(&self, ephemeral: bool) -> Result<(), ApiError> {
        let modal_submit = self.it.kind == INTERACTION_MODAL_SUBMIT;
        self.callback(api::defer_body(ephemeral, modal_submit))
            .await
    }

    /// `response.send_message(content, ephemeral=True)`
    pub async fn send_message(&self, content: &str) -> Result<(), ApiError> {
        self.callback(api::message_body(content, true)).await
    }

    /// `response.send_modal(modal)`
    pub async fn send_modal(&self, modal: Value) -> Result<(), ApiError> {
        self.callback(api::modal_body(modal)).await
    }

    /// `followup.send(content, ephemeral=True)`
    pub async fn followup(&self, content: &str) -> Result<(), ApiError> {
        self.app
            .api
            .followup(
                &self.it.application_id,
                &self.it.token,
                &api::followup_body(content, true),
            )
            .await
    }
}

// ---------- 选项解析（对应 discord.py 的参数转换，不合法 = TransformerError） ----------

fn string_option_opt(it: &Interaction, name: &str) -> Option<String> {
    it.option(name)
        .and_then(|o| o.value.as_ref())
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn string_option(it: &Interaction, name: &str) -> Result<String, CmdError> {
    string_option_opt(it, name).ok_or_else(|| CmdError::Invalid(format!("缺少选项 {name}")))
}

/// 用户选项（USER，type 6）的值是用户 ID 字符串；规范成 `str(member.id)`。
fn user_option(it: &Interaction, name: &str) -> Result<String, CmdError> {
    let raw = string_option(it, name)?;
    raw.parse::<u64>()
        .map(|n| n.to_string())
        .map_err(|_| CmdError::Invalid(format!("选项 {name} 不是用户 ID：{}", py_str(&json!(raw)))))
}

// ---------- 事件循环 ----------

/// 等 SIGINT / SIGTERM（docker stop）。
pub async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            std::future::pending::<()>().await;
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// 分发一条网关文本消息。
pub fn dispatch(app: &Arc<Naimei>, raw: &str) {
    match events::parse(raw) {
        Ok(Gateway::Ready(ready)) => {
            app.set_bot_user(ready.user.clone());
            tokio::spawn(app.clone().on_ready(ready));
        }
        Ok(Gateway::GuildCreate(g) | Gateway::GuildUpdate(g)) => app.upsert_guild(g),
        Ok(Gateway::GuildDelete(id)) => app.remove_guild(&id),
        Ok(Gateway::MessageCreate(m)) => {
            let app = app.clone();
            tokio::spawn(async move { app.handle_message(m).await });
        }
        Ok(Gateway::Interaction(it)) => {
            let app = app.clone();
            tokio::spawn(async move { app.handle_interaction(*it).await });
        }
        Ok(Gateway::Other) => {}
        Err(e) => warn!("无法解析的网关消息：{e}"),
    }
}

/// 运行奶妹，直到收到停止信号（正常返回）或网关致命关闭（Token 无效 / Intent 未开通等，返回 Err）。
pub async fn run(cfg: NaimeiConfig) -> anyhow::Result<()> {
    let app = Naimei::new(cfg.clone(), None);
    let config = GatewayConfig::new(cfg.token.clone(), NAIMEI_INTENTS);
    let mut shard = Shard::with_config(ShardId::ONE, config);
    let shutdown = wait_for_shutdown();
    tokio::pin!(shutdown);
    let mut closing = false;
    loop {
        let item = tokio::select! {
            item = shard.next() => item,
            () = &mut shutdown, if !closing => {
                info!("[bot] 收到停止信号，正在断开网关");
                shard.close(CloseFrame::NORMAL);
                closing = true;
                continue;
            }
        };
        match item {
            Some(Ok(Message::Text(raw))) => {
                app.set_latency(shard.latency().recent().first().copied());
                dispatch(&app, &raw);
            }
            Some(Ok(Message::Close(frame))) => {
                info!("[bot] 网关连接关闭：{frame:?}");
            }
            Some(Err(e)) => warn!("[bot] 网关消息出错：{e}"),
            None if closing => return Ok(()),
            None => anyhow::bail!(
                "网关连接被致命关闭（常见原因：DISCORD_BOT_TOKEN 无效或 Intent 未开通），详见上面的关闭码"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_detection_requires_explicit_tag() {
        assert!(mentions_bot(
            "hi <@900000000000000099> there",
            "900000000000000099"
        ));
        assert!(mentions_bot("<@!900000000000000099>", "900000000000000099"));
        assert!(!mentions_bot("<@900000000000000098>", "900000000000000099"));
        assert!(!mentions_bot("@everyone", "900000000000000099"));
        assert!(!mentions_bot("", "900000000000000099"));
    }

    #[test]
    fn limiter_channel_author_global_windows() {
        let mut l = MentionLimiter::default();
        assert!(l.allow(1000.0, 1, 10));
        // 全服 20 秒内谁都不行
        assert!(!l.allow(1010.0, 2, 20));
        // 20 秒后换频道换人可以
        assert!(l.allow(1021.0, 2, 20));
        // 频道 1 的 60 秒窗口：不同的人也不行
        assert!(!l.allow(1030.0, 1, 30));
        assert!(l.allow(1062.0, 1, 30));
        // 成员 10 的 5 分钟窗口：换个频道也不行
        assert!(!l.allow(1090.0, 3, 10));
        assert!(l.allow(1301.0, 3, 10));
    }

    #[test]
    fn limiter_clears_when_over_500_keys() {
        let mut l = MentionLimiter::default();
        for i in 0..300u64 {
            assert!(l.allow(i as f64 * 1000.0, i + 1, i + 10_000));
        }
        assert!(l.last.len() <= 501);
    }
}
