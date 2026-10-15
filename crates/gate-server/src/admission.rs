//! 出图准入与额度预留（旧版 `image_routes.py` 的 guard_precheck / image_admission / _acquire_slots /
//! quota_image_check / ImageReservation / reserve_image_budget）。
//!
//! 每一步都是一个 RAII 守卫：被拒或客户端断开时自动归还。真正发往上游之后，守卫随上游操作一起
//! 移进后台任务（见 `routes::image::shielded`），保证「先记账、再释放额度预留和并发槽」。
//!
//! 每把 Key 的限速只有两层：这里的排队（同时生成 1 张 + 排队 1 张）和每分钟次数（RPM，`auth::check_rpm`）。
//! 2.x 另有每把 Key 15 秒出图间隔（`wait_for_user_image_slot` / `refund_local_reject`）和每把 Key 一个的
//! 信号量（`KEY_CONCURRENCY`），都与排队重复，已删除（docs/v3-deviations.md #64）。

use crate::auth::site_flag_f64;
use crate::error::{ApiError, GateError, InternalError};
use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::{AppState, Reservation};
use gate_core::reasons::{self, Reason};
use gate_core::site_flags;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// 替成员等时段份额的上限：更长的等待仍然直接 429（挂着连接等半小时不现实）。
pub const PACED_WAIT_CAP: f64 = 300.0;
/// 轮询间隔：份额注水约每小时 daily/24 张，20 秒粒度足够。
pub const PACED_POLL_SECONDS: f64 = 20.0;

/// 一次出图的估算消耗（旧版 `est` 字典）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Est {
    pub anlas: f64,
    pub v5: i64,
    /// 带参考图 / Vibe：没开 Anlas 的 Key 也能抢全站 Vibe 池。
    pub is_vibe: bool,
}

impl Est {
    pub fn from_cost(c: gate_core::policy::ImageCost) -> Self {
        Self {
            anlas: c.anlas as f64,
            v5: c.v5,
            is_vibe: false,
        }
    }
}

fn gate(status: u16, r: Reason) -> ApiError {
    GateError::with_code(status, r.text().to_string(), r.code().to_string()).into()
}

fn guard_io(e: crate::guard::GuardIoError) -> ApiError {
    InternalError(e.to_string()).into()
}

/// 当前可用的上游账号（`[t for t in pool if t.usable]`）。
pub async fn usable_token_ids(state: &AppState) -> Vec<String> {
    let now = state.now();
    state
        .nai
        .tokens()
        .await
        .into_iter()
        .filter(|t| t.usable(now))
        .map(|t| t.token_id.clone())
        .collect()
}

async fn block_reasons(state: &AppState, pool: &[String]) -> Result<Vec<Option<Reason>>, ApiError> {
    let day = state.day();
    let mut out = Vec::with_capacity(pool.len());
    for t in pool {
        out.push(
            state
                .guard
                .token_block_reason(t, &day, state.now())
                .await
                .map_err(guard_io)?,
        );
    }
    Ok(out)
}

/// 出图前先看账号保护：所有可用账号都到了上限时直接 429 并说明原因。
/// 例外：原因全是「按时段放行」时份额是线性注水的，替成员在网关里等位（最多 5 分钟），
/// 免得客户端 429 自动重试又撞 RPM 形成重试风暴。等待期间客户端断开会被正常取消，不记账。
pub async fn guard_precheck(state: &AppState) -> Result<(), ApiError> {
    let pool = usable_token_ids(state).await;
    if pool.is_empty() {
        return Ok(());
    }
    let rs = block_reasons(state, &pool).await?;
    if rs.iter().any(Option::is_none) {
        return Ok(());
    }
    let mut first = rs.into_iter().next().flatten().expect("non-empty");
    if first.code() != reasons::PACED {
        return Err(gate(429, first)); // 每日 / 每小时 / 熔断：等也不会好，立即说明
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(PACED_WAIT_CAP);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs_f64(PACED_POLL_SECONDS)).await;
        let rs = block_reasons(state, &pool).await?;
        if rs.iter().any(Option::is_none) {
            return Ok(()); // 有账号放出份额了，继续正常准入
        }
        first = rs.into_iter().next().flatten().expect("non-empty");
        if first.code() != reasons::PACED {
            break; // 等成了更严重的上限：按新原因拒绝
        }
    }
    Err(gate(429, first))
}

/// 排队登记：每把 Key 同时生成 1 张、最多再排 N 张；全站排队总数有上限。
pub struct Admission {
    guard: Arc<crate::guard::Guard>,
    key_id: i64,
    entry_id: i64,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.guard.release_image(self.key_id, Some(self.entry_id));
    }
}

fn queue_timeout() -> GateError {
    GateError::with_code(
        429,
        "排队等待超时，当前站点繁忙，请稍后再试",
        reasons::QUEUE_TIMEOUT,
    )
}

/// 排进队以后等轮到这把 Key 的这一张：前一张出完（或被拒 / 取消）才往下走。超过排队超时回 429。
async fn wait_key_turn(state: &AppState, key_id: i64, entry_id: i64) -> Result<(), GateError> {
    if state.guard.with(|g| g.key_turn(key_id, entry_id)) {
        return Ok(());
    }
    struct Waiting<'a>(&'a AppState);
    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            self.0.global_waiting.fetch_sub(1, Ordering::Relaxed);
        }
    }
    state.global_waiting.fetch_add(1, Ordering::Relaxed);
    let _w = Waiting(state);
    let turn = async {
        loop {
            let released = state.guard.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if state.guard.with(|g| g.key_turn(key_id, entry_id)) {
                return;
            }
            released.await;
        }
    };
    let t = state.limits().queue_timeout;
    tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), turn)
        .await
        .map_err(|_| queue_timeout())
}

pub async fn image_admission(
    state: &AppState,
    ctx: &ReqCtx,
    key: &Key,
) -> Result<Option<Admission>, ApiError> {
    if key.is_admin() {
        return Ok(None);
    }
    guard_precheck(state).await?;
    let accounts = usable_token_ids(state).await.len() as i64;
    let key_id = key.id();
    let now = state.now();
    // 登记与取编号在同一次加锁里完成，不会被别人插队
    let admitted = state
        .guard
        .with(|g| match g.admit_image(key_id, accounts, now) {
            Some(r) => Err(r),
            None => Ok(g.entries.last().map_or(0, |e| e.id)),
        });
    let entry_id = admitted.map_err(|r| gate(429, r))?;
    // 先拿到登记：等待时超时 / 客户端断开，登记照样归还
    let admission = Admission {
        guard: state.guard.clone(),
        key_id,
        entry_id,
    };
    wait_key_turn(state, key_id, entry_id).await?;
    let guard = state.guard.clone();
    ctx.on_sent(move || guard.with(|g| g.mark_running(key_id, Some(entry_id))));
    Ok(Some(admission))
}

/// 文本请求的全站并发槽与「正在处理」计数（出图请求只记数，不占槽）。
pub struct Slots {
    _global: Option<tokio::sync::OwnedSemaphorePermit>,
    state: AppState,
}

impl Drop for Slots {
    fn drop(&mut self) {
        self.state.global_active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// `_key` 只为调用方签名不变而保留：每把 Key 的信号量已删除（docs/v3-deviations.md #64）——出图由
/// [`image_admission`] 的排队保证同一把 Key 同时只生成 1 张，标签补全由补全队列保证同一把 Key 一次一条，
/// 文本请求走全站并发槽。
pub async fn acquire_slots(state: &AppState, _key: &Key, image: bool) -> Result<Slots, GateError> {
    struct Waiting<'a>(&'a AppState);
    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            self.0.global_waiting.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let g = if image {
        None
    } else {
        let t = state.limits().queue_timeout;
        state.global_waiting.fetch_add(1, Ordering::Relaxed);
        let w = Waiting(state);
        let got = tokio::time::timeout(
            Duration::from_secs_f64(t.max(0.0)),
            state.global_sem.clone().acquire_owned(),
        )
        .await;
        drop(w);
        Some(got.map_err(|_| queue_timeout())?.expect("semaphore closed"))
    };
    state.global_active.fetch_add(1, Ordering::Relaxed);
    Ok(Slots {
        _global: g,
        state: state.clone(),
    })
}

/// 额度检查。调用方必须持有 `state.budget` 锁（与旧版 `image_budget_lock` 一样，检查和登记预留不可交错）。
/// `exclude`：更新自己的预留时不把自己算进去。
pub async fn quota_image_check(
    state: &AppState,
    budget: &crate::state::Budget,
    key: &Key,
    est: &Est,
    legacy_free_images: i64,
    exclude: Option<u64>,
) -> Result<(), ApiError> {
    if key.is_admin() {
        return Ok(());
    }
    let day = state.day();
    let c = state.db.get_counter(key.id(), &day).await?;
    // 跨过 7 点才结算的请求也保守地计入当前周期
    let pending: Vec<&Reservation> = budget
        .reservations
        .iter()
        .filter(|(id, _)| Some(**id) != exclude)
        .map(|(_, r)| r)
        .collect();
    let own: Vec<&&Reservation> = pending.iter().filter(|r| r.key_id == key.id()).collect();
    let daily_images = key.daily_images();
    if legacy_free_images != 0 && daily_images > 0 {
        let used_legacy = c.legacy_free_images + own.iter().map(|r| r.legacy).sum::<i64>();
        if used_legacy + legacy_free_images > daily_images {
            return Err(GateError::with_code(
                402,
                format!("今日 V4.5 免费图额度已用完（{daily_images} 张/天），明日恢复"),
                reasons::MEMBER_QUOTA,
            )
            .into());
        }
        // 每人只有每日上限：2.x 的「每人注水」（底仓 30 张、每小时补 4 张，docs/v3-deviations.md #62）和
        // 「保底 + 全站空闲时借用」（#61）都已删除；全站按时段放行在 guard_precheck 里对所有人一视同仁。
    }
    if est.v5 > 0 {
        // V5 周额度是账户级共享资源，用全站日计数镜像
        let daily_v5 = key.daily_v5();
        if daily_v5 > 0 && c.v5 + own.iter().map(|r| r.v5).sum::<i64>() + est.v5 > daily_v5 {
            return Err(GateError::with_code(
                402,
                format!("今日 V5 额度已用完（{daily_v5} 张/天），明天恢复后再用"),
                reasons::MEMBER_QUOTA,
            )
            .into());
        }
        let g = crate::ops::algo_owned(&state.db).await?.global_daily_v5 as f64;
        if g > 0.0 && !key.exclude_global_v5() {
            let total = state.db.day_v5_total(&day).await?;
            let in_flight: i64 = pending
                .iter()
                .filter(|r| !r.exclude_global_v5)
                .map(|r| r.v5)
                .sum();
            if (total + in_flight + est.v5) as f64 > g {
                return Err(GateError::with_code(
                    402,
                    format!(
                        "全站今日 V5 额度已用完（{} 张/天），明天再来",
                        g.trunc() as i64
                    ),
                    reasons::SITE_QUOTA,
                )
                .into());
            }
        }
    }
    if est.anlas > 0.0 {
        let until = site_flag_f64(state, &site_flags::GLOBAL_DAILY_ANLAS_UNTIL).await?;
        let vibe_cap = if until > 0.0 && state.now() >= until {
            0.0
        } else {
            site_flag_f64(state, &site_flags::GLOBAL_DAILY_ANLAS).await?
        };
        let global_in_flight = || gate_core::pyc::sum(pending.iter().map(|r| r.anlas));
        let pool_check = |used: f64, label: &str| -> Result<(), ApiError> {
            if used + global_in_flight() + est.anlas > vibe_cap {
                return Err(GateError::with_code(
                    402,
                    format!(
                        "全站今日 {label} 额度已抢完（{} Anlas/天），明天服务日重置后恢复",
                        vibe_cap.trunc() as i64
                    ),
                    reasons::SITE_QUOTA,
                )
                .into());
            }
            Ok(())
        };
        if est.is_vibe && !key.allow_anlas() {
            if vibe_cap <= 0.0 {
                return Err(GateError::with_code(
                    402,
                    "该请求会消耗 Anlas，全站 Vibe 每日额度暂未开放",
                    reasons::SITE_QUOTA,
                )
                .into());
            }
            pool_check(state.db.day_anlas_total(&day).await?, "Vibe")?;
        } else if !key.allow_anlas() {
            return Err(GateError::with_code(
                402,
                "该请求会消耗 Anlas，此 Key 未开通付费额度权限",
                reasons::MEMBER_QUOTA,
            )
            .into());
        } else if vibe_cap > 0.0 {
            pool_check(state.db.day_anlas_total(&day).await?, "Anlas")?;
        }
        // 每人每日 Anlas 上限：没手动设置的 Key 落到全站默认 member_daily_anlas
        let own_anlas = || gate_core::pyc::sum(own.iter().map(|r| r.anlas));
        let mut member_cap = key.daily_anlas();
        if member_cap <= 0.0 {
            member_cap = site_flag_f64(state, &site_flags::MEMBER_DAILY_ANLAS).await?;
        }
        if member_cap > 0.0 && c.anlas + own_anlas() + est.anlas > member_cap {
            return Err(GateError::with_code(
                402,
                format!(
                    "今日 Anlas 额度不足（已用 {:.0} / 上限 {member_cap:.0}），明日恢复",
                    c.anlas
                ),
                reasons::MEMBER_QUOTA,
            )
            .into());
        }
        let month = state.month();
        let used = state.db.month_anlas(key.id(), &month).await?;
        let monthly = key.monthly_anlas();
        if monthly > 0.0 && used + own_anlas() + est.anlas > monthly {
            return Err(GateError::with_code(
                402,
                format!("本月 Anlas 额度不足（已用 {used:.0}/{monthly:.0}）"),
                reasons::MEMBER_QUOTA,
            )
            .into());
        }
        let budget_cap = site_flag_f64(state, &site_flags::GLOBAL_MONTHLY_ANLAS).await?;
        if budget_cap > 0.0 {
            let all_used = state.db.month_anlas_all(&month).await?;
            if all_used + global_in_flight() + est.anlas > budget_cap {
                return Err(GateError::with_code(
                    402,
                    format!("全站本月 Anlas 预算已耗尽（{budget_cap:.0}）"),
                    reasons::SITE_QUOTA,
                )
                .into());
            }
        }
    }
    Ok(())
}

/// 已登记的额度预留；释放时在预算锁下移除（与旧版一样不和进行中的检查交错）。
pub struct Reserved {
    state: AppState,
    id: u64,
    key: Key,
}

impl Reserved {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// V5 额度确认后改按 Anlas 估算：在预算锁下复核（不算自己），通过才改预留。
    pub async fn update(&self, est: &Est, legacy: i64) -> Result<(), ApiError> {
        let mut b = self.state.budget.lock().await;
        quota_image_check(&self.state, &b, &self.key, est, legacy, Some(self.id)).await?;
        if let Some(r) = b.reservations.get_mut(&self.id) {
            r.anlas = est.anlas;
            r.v5 = est.v5;
            r.legacy = legacy;
        }
        Ok(())
    }
}

fn remove_reservation(b: &mut crate::state::Budget, id: u64, state: &AppState) {
    b.reservations.remove(&id);
    if b.reservations.is_empty() {
        state.budget_idle.notify_waiters();
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let (state, id) = (self.state.clone(), self.id);
        match state.budget.try_lock() {
            Ok(mut b) => remove_reservation(&mut b, id, &state),
            Err(_) => {
                // 有人正在核对额度：等它核对完再移除（多占一会儿只会更保守）
                let s = state.clone();
                state.tasks.spawn(async move {
                    let mut b = s.budget.lock().await;
                    remove_reservation(&mut b, id, &s);
                });
            }
        }
    }
}

/// 核对额度并登记预留（排队等待期间就占住，防止并发请求一起超额）。
pub async fn reserve_image_budget(
    state: &AppState,
    key: &Key,
    est: &Est,
    legacy_free_images: i64,
) -> Result<Reserved, ApiError> {
    let t = state.limits().queue_timeout;
    let res = tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), async {
        let mut b = state.budget.lock().await;
        quota_image_check(state, &b, key, est, legacy_free_images, None).await?;
        b.next_id += 1;
        let id = b.next_id;
        b.reservations.insert(
            id,
            Reservation {
                key_id: key.id(),
                exclude_global_v5: key.exclude_global_v5(),
                anlas: est.anlas,
                v5: est.v5,
                legacy: legacy_free_images,
            },
        );
        Ok::<u64, ApiError>(id)
    })
    .await;
    match res {
        Ok(Ok(id)) => Ok(Reserved {
            state: state.clone(),
            id,
            key: key.clone(),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(GateError::with_code(
            429,
            "图片预算核对排队超时，请稍后再试",
            reasons::QUEUE_TIMEOUT,
        )
        .into()),
    }
}
