# 猫头鹰公益站 · 系统事实清单（唯一来源）

> **更新日期** 2026-10-15 · **版本** 3.0 · **规则**：数字以代码为准，改了代码先改这里，再同步首页、后台说明和其他文档。
>
> 来源标记：**码** = 代码里的常量或默认值（文件都在 `crates/` 下）；**线** = 线上 2026-10-15 的只读快照（站点设置 `site_settings` 与环境变量，不含密钥和成员数据）；**算** = 由前两者算出来的数。
> 表里「线上」一列会随后台调整变化，「代码默认」是重装后得到的值。线上一栏写「未核对」= 快照里没有这个变量或键，按代码默认理解，没有第二来源。
> 取值优先级：后台设置（`site_settings`）> 环境变量（`.env`）> 代码默认。额度算法的内部参数还可以被 `data/private_params.json` 覆盖（线上内容未核对，下文标「私密参数可覆盖」）。

## 0. 一句话

站长的 NovelAI Opus 账号被所有成员共用：网关给每人发一把虚拟 Key，所有出图都经过上游账号。
额度是 **PVP**：全站共用的固定池子，先到先得；每人上限只是上限，不是承诺。服务日每天 07:00（Asia/Shanghai）切换。

- 线上现有活跃成员 Key 170 把、Discord 领取记录 164 条（线，两个数口径不同，未做第二来源核对）；名额不限（`REGISTER_MAX_USERS=0`，线）。
- 上游账号数：代码按令牌池大小计（`每日上限 × 账号数`）；线上快照没有直接给出账号数（V5 采样只见 1 个 Token），未核对。
- 内核循环每 10 分钟、维护循环每 5 分钟，都带 ±20% 抖动（下限 30 秒）；巡检由宿主机定时器每 5 分钟跑一次（码 `jobs.rs`、`nai-patrol.timer`）。

## 1. 账号保护（`guard_*` 与运行期限制）

| 参数（设置键 / 环境变量） | 代码默认 | 线上 | 说明 |
|---|---|---|---|
| 出图并发（每个上游账号） | 1 | 未核对 | 每个 Token 一个出图槽；后台「上游令牌池」可调 1～4；`SINGLE_IMAGE_SLOT_ENFORCED`（默认 0）设为 1 时恒为 1 |
| 账号出图间隔 `runtime_image_min_interval`（`IMAGE_MIN_INTERVAL`） | 15 秒（范围 15～120） | 20 秒 | 同一个上游账号两张图之间的最小间隔；请求结束后还会静默 1.0～3.0 秒（`POST_REQUEST_JITTER_*`，取与间隔的较大者） |
| 间隔抖动 `guard_interval_jitter` | 5（0～30） | 5 | 每次间隔再加随机 0～N 秒。线上平均 22.5 秒一张，理论最快 3600÷22.5 = 160 张/小时（算），正好等于每小时上限 |
| 每小时上限 `guard_account_hourly_cap` | 150（0～240；自动调整 100～160） | 160 | **滚动最近 60 分钟**，不是整点重置；自动调整规则见下 |
| 10 分钟突发窗口 | 每小时上限 ÷ 4，向上取整 | 40（算） | 10 分钟内已出 ≥ ⌈1.5×40⌉ = 60 张，或已出 ≥ 40 张且本小时剩余 ≤ 40 张，就拒绝（`burst_cap`）。目的：一小时的额度不能挤在十分钟里发完 |
| 每日上限 `guard_account_daily_cap` | 1000（0～20000） | 1700 | 每个上游账号每个服务日的出图总量（V4.5 + V5 合计）；用完等次日 07:00 |
| 按时段放行 `guard_daily_burst` | 150（0～20000；0 = 不分时段） | 600 | 服务日起点先放 burst 张，之后每小时补「每日上限 ÷ 24」（线上约 70.8 张，算），封顶每日上限。本时段份额用完回 429（`paced`），网关替成员最多等 300 秒（每 20 秒看一次）再拒绝 |
| 安静时段 `guard_quiet_start` / `guard_quiet_end` | 0 / 0（两者相同 = 不启用） | 3 / 7 | 北京时间 03:00～07:00 |
| 安静时段上限 `guard_quiet_hourly_cap` | 20（0～240） | 0 | 0 = **完全暂停出图**（不是不限） |
| 每把 Key 排队 `guard_key_image_queue` | 1（0～3） | 1 | 正在生成的那张之外最多再排 1 张，即同一把 Key 最多 2 张在途；再发回 429（`key_busy`） |
| 全站排队 `guard_queue_per_account` | 5（0～100） | 5 | 每个上游账号最多同时 5 张（含正在生成的）；满了回 429（`queue_full`） |
| 每把 Key 每分钟请求数 | 成员 Key 5（`REGISTER_RPM`）；后台手建 Key 10（`DEFAULT_RPM`） | 未核对 | 生图 / 放大 / 导演工具 / Vibe 请求都计数，被拒的也计；标签补全不计。超出回 429（`key_rate`） |
| 每把 Key 同时在途请求 | 4（常量） | — | 含排队中的；超出回 429（`key_rate`） |
| 每把 Key 同时处理的请求 `KEY_CONCURRENCY` | 1 | 未核对 | 其余的排队，受排队超时约束 |
| 每把 Key 出图间隔 `runtime_key_image_min_interval`（`KEY_IMAGE_MIN_INTERVAL`） | 15 秒（15～120） | 15 | 同一把 Key 两张图之间；请求在发往上游前被本地拒绝时退还这段间隔 |
| 排队超时 `runtime_queue_timeout`（`QUEUE_TIMEOUT`） | 90 秒（15～300） | 90 | 超时回 429（`queue_timeout`） |
| 上游 429 后的全站冷却 `runtime_image_429_cooldown_seconds`（`IMAGE_429_COOLDOWN_SECONDS`） | 60 秒（60～3600） | 180 | 实际冷却 = max(此值, 上游 Retry-After)，Retry-After 先夹到 5～3600 秒。冷却期内所有图片请求回 429（`cooldown`）；冷却状态跨重启保留（`image_cooldown_until`，超过 1 小时的值视为无效） |
| 上游连续 403（图片通道） | — | — | 连续 3 次 → 该 Token 冷却 5 → 15 → 45 → 60 分钟（`min(60, 5×3ⁿ)`），24 小时内没有 403 档位归零；私信站长 |
| 无效 Key 防爆破（`AUTH_FAIL_*`） | 同一 IP（IPv6 按 /64）600 秒内 ≥ 20 次无效 Key → 拦截 900 秒 | 未核对 | 只拦无效 Key；有效 Key 不受影响。后台登录另有一套：每 IP 300 秒内最多 5 次（`LOGIN_*`） |
| V5 官方额度查询 | 只在真实 V5 出图时刷新缓存 | 阈值 20 | 缓存：剩 ≤ 2% 为 15 秒，低于告警阈值（`v5_alert_threshold`，默认 20）为 60 秒，其余 300 秒。内核每小时最多再主动读一次订阅信息做快照 |

**每小时上限的自动调整**（AIMD，`guard.rs` + 容量模块）：

- 上游返回 429：立即减半（不低于 100；手动压到 100 以下时只维持或降低），并记下降档前的上限，之后加窗的天花板 = max(100, 80% × 该值)。私密参数 `capacity.hourly_*` 可覆盖，线上未核对。
- 每天 +10：要求距上次调整和上次 429 都 ≥ 24 小时、过去 24 小时日志覆盖 ≥ 20 小时、且过去 24 小时有成员因 `hourly_cap` 被拒（上限真的顶到过）；不超过 160；只升不降。第一次运行只开始计时。
- 线上 160 已到上限；最近一次 429 在 2026-10-13 01:30（`guard_last_upstream_429` 换算，算）。

## 2. 额度分配

每把成员 Key 的 `quota_auto=1`（新建默认）时由算法管理：内核每个 tick 把「V4.5 每人上限 A」和「V5 每人额度」下发给所有这样的 Key，同时写新领 Key 用的默认值（`register_daily_images`、`register_daily_v5`、`register_image_scope=all`）、`global_daily_v5` 和首页文案 `algo_notice`。后台手动设置额度的 Key（`quota_auto=-1`）不归算法管，也不走下面的平滑注水。

### 2.1 V4.5 每人每日上限 A（`quota_ceiling`，线 95）

每个服务日的第一个内核 tick（07:00 后约 0～12 分钟）看前一个服务日重算一次：

1. **可分预算** = max(0, 账号数 × 每日上限 − 全站 V5 池)。线上若是 1 个账号即 1700 − 171 = 1529（算，账号数未核对）。
2. **用量** = 昨天每位算法管理的成员用掉的 V4.5 免费图张数（只算用过的人）。
3. 规则：
   - 昨天日志覆盖 < 20 小时（私密参数 `allocation.min_coverage_hours`）→ A 不变；
   - 总用量 ≥ 预算的 95% → A = water-fill（用得少的人用多少算多少，余下的平分给用得多的人，取整数下限，至少 1），夹到 10～300；
   - 否则有人用满 A（最高用量 ≥ A）→ A + 25，不超过 300；
   - 否则不变。
4. 常量（码 `quota.rs`，线上快照没有覆盖这些键）：步长 25、下限 10、上限 300、起点 150（`quota_ceiling` 没有值时才用）。
5. **保底 B = A**：算法每次把 `guard_base_daily_images` 同步成 A（线 95 = 95 = 95，已核对），所以「超过保底后只在全站空闲时放行」这一层（`base < 每日上限` 才生效）实际不触发，只剩每日上限。

### 2.2 每人的平滑注水

非手动 Key 的 V4.5 免费图按「底仓 + 匀速补」放行（码 `FREE_BURST = 30`、`FREE_REFILL_PER_HOUR = 4`）：

- 到目前为止最多可用 = min(A, min(30, A) + 4 × 服务日起点以来的小时数)，**连续计算**（每 15 分钟补 1 张），不是整点跳变；超过回 429（`paced`，提示约几分钟后可用）。
- 全天最多实际能用 = min(A, 30 + 4×24 = **126**)（算）。A > 126 的人到不了 A；首页文案用同一组常量，A > 126 时会写「每小时自动恢复 4 张，当天最多约 126 张」。

### 2.3 V5 全站池与每人额度

| 量 | 公式 | 线上（10-15） |
|---|---|---|
| 恢复速度 rate（%/天） | 接口读数与实测值合并：两者都有 → 取较小；只有实测 → 实测；只有接口 → min(接口, 5)；都没有 → 5。读数超出 1～15 不用；接口与实测相差 > 50% 记「对不上」并取较小。实测来自日复盘（余量变化 + 用掉的量）或安静时段直接测量（≥ 4 个安静小时） | 11.0（接口 11.0 / 实测 11.17，状态「已核对」） |
| 系数 k（按账号 V5 剩余 %） | ≥ 90 → 1.3；≥ 70 → 1.1；≥ 40 → 0.9；≥ 20 → 0.6；其余 0.3；读不到 → 0.9（私密参数 `allocation.v5_k_*` 可覆盖，线上未核对） | 剩余 81% → 1.1 |
| 全站 V5 池 G | ⌊rate × 14.2 × k⌋；14.2 = 官方「1% ≈ 17.3 张（23 步）」按 28 步折算（码 `V5_IMAGES_PER_PERCENT`） | ⌊11.0 × 14.2 × 1.1⌋ = 171（`global_daily_v5` 也是 171，自洽） |
| 每人 D5 | ⌊G ÷ max(10, 近 3 个自然日用过 V5 的人数)⌋，夹到 3～30 | 近 3 日 75 人 → ⌊171 ÷ 75⌋ = 2 → 夹到 3（`register_daily_v5` = 3） |
| 剩余 ≥ 85% | 每人至少 10、全站至少 260（`allocation.v5_flood_*`，私密参数可覆盖） | 未触发（81%） |

- 当天方案在 07:00 后第一次计算时定下；之后只在「新方案每人更大」时放宽，或「账号剩余 < 40% 且新方案更小」时收紧。
- 每人 × 人数可以远大于 G（超卖）：此时每人额度只是个人上限，全站池先到先得，首页文案写「全站今日共 G 张，先到先得」。
- 出 V5 图前会向上游确认官方免费额度：确认已耗尽时，无 Anlas 权限的 Key 回 402「上游账号今日 V5 免费额度已耗尽（非个人原因），请改用 V4.5 生成」（`v5_exhausted`）；查询失败则回 503「V5 额度暂时无法确认，请稍后重试；未发送生图，未扣积分」。

### 2.4 Anlas

成员 Key 默认没有 Anlas 权限（`allow_anlas=false`），实际只有 Vibe（参考图编码）会花 Anlas，走全站 PVP 池。

| 项 | 代码默认 | 线上 | 说明 |
|---|---|---|---|
| 每人每日上限 `member_daily_anlas` | 50（0 = 不限） | 未设置，首页文案「Anlas 每人 50/天」与默认一致 | Key 自己设置了 `daily_anlas` > 0 则以 Key 为准（成员 Key 为 0）；Vibe 编码一张参考图约 2 点，50 点 ≈ 25 次 |
| 每人每月 | Key 自己的 `monthly_anlas`（成员 Key 为 0 = 不限） | — | |
| 全站每日 Anlas 池 `global_daily_anlas` | 200（`GLOBAL_DAILY_ANLAS`；0 = 不开放） | 1000 | Vibe PVP：全站合计先到先得，抢完回 402（`site_quota`）；`global_daily_anlas_until` 为开放截止时间戳（0 = 不限时，线 0） |
| 全站每月预算 `global_monthly_anlas` | 10000（`GLOBAL_MONTHLY_ANLAS`） | 9000 | 超出回 402 |

价格（码 `policy.rs`）：Vibe 编码 2 Anlas/张；精确参考 5 Anlas/张/图；V4 / V4.5 的 Vibe 超过 4 张，每多 1 张 +2/图。Anlas 自动分配（旧的分配池）已删除，快照里残留的 `anlas_pool_last` 等键不再被读取。

### 2.5 免费规格（对没有 Anlas 权限的 Key 生效，`SAFE_CLAMP` 默认开）

- 每次 1 张（`n_samples` 强制为 1）；步数 ≤ 28（`MAX_STEPS`）；SMEA 关闭。
- V4.5 及更早的模型：总像素 ≤ 1024×1024 = 1048576（`MAX_PIXELS`），宽高取 64 的倍数，超出等比缩小。
- V5：不超过三个预设之一——832×1216、1216×832、1024×1024，超出按长宽比吸附（> 1.1 → 1216×832，< 0.9 → 832×1216，其余 1024×1024）。
- 图生图 / 局部重绘不开放（`ALLOW_IMG2IMG` 默认 0，回 400）；ControlNet 回 400；角色参考（`characterReferences`）被移除。
- 新领 Key 开通的功能 `register_features`（线 `image,vibe,tags`；`*` = 不限）；功能可选 `image`、`upscale`、`augment`、`vibe`、`tags`（文本生成 / 语音 / OpenAI 兼容聊天 2026-10-15 已整套删除，库里 Key 的 `features` 或 `features_global` 里残留的 `text` / `voice` 读取时被忽略）。站长还有全局开关 `features_global`（JSON，缺省全开）。
- 上游对「模型不存在」回的是 403，网关会窥探一小段响应体把「模型级 403」和「账号级 403」分开（只有账号级才计入账号健康度和 403 冷却）。

### 2.6 重置券

站长在后台给某把 Key 发券；成员在首页使用后，重置**本人**今天已用的 V5 与 Anlas 计数（`daily_quota_offsets`），不含 V4.5，也不改变全站池。一张券只能用一次，有过期时间。

## 3. 自动驾驶（每个内核 tick）

每条规则的模式是设置 `autopilot_<规则>`：`off` / `observe` / `enforce`，不认识的值按 `observe`。`observe` 只记录，有「会执行的动作」时写操作日志「自动驾驶（观察）」。每小时留一条快照，保留 168 条（7 天）。

| 规则 | 线上模式 | 做什么 |
|---|---|---|
| `slots` 名额 | enforce | 只加不减：空位 ≤ 2、名额 < 100、昨天用量 < 60%、账号 V5 ≥ 50%、昨天被每小时上限拦的小时数 < 3、排队拥挤（某小时 `queue_full` ≥ 5 次）的小时数 < 3、昨天日志覆盖 ≥ 20 小时 → 名额 +5；最多每 24 小时一次。名额为 0（不限）时不动——**线上就是不限，已核对**（`autopilot_last` 写「名额不限（站长设定），不调整」） |
| `breaker` 熔断 | enforce | 最近 15 分钟图片请求里「上游失败」≥ 5 次且 ≥ 30% → 全站暂停出图 300 秒（`breaker`）。只算上游 5xx 和流中途断开；本地拦截、上游 429、4xx 不算；只看上次熔断结束之后的数据 |
| `key_guard` 单 Key 限流 | enforce | 1 小时内同一把 Key 被拒 ≥ 60 次 → 暂停该 Key 1 小时（回 403，`key_paused`）。统计不含站点层面的拒绝（排队满、各类账号上限、熔断、冷却、`v5_exhausted`、`queue_timeout`、`site_quota`、`over_base`、`key_paused` 本身）。提示「{原因}，{到期时间} 自动恢复。」（2.x 留下的旧行原因为空，写「这把 Key 已被临时暂停，{到期时间} 自动恢复。」）。2.x 还有「换网段」「多客户端全天在用」两条规则，输入来自早已删除的防分享证据、恒为空，3.0 已删除（`docs/v3-deviations.md` #51）；数据库表 `share_state` 原样保留，现在只存每把 Key 的临时暂停 |

## 4. 内核模块（6 个，每 10 分钟 tick + 交叉校验）

每个模块的结论用另一个独立数据来源核对。校验没通过只在后台「模块」页和总览「N 个模块自检没通过」里显示，**不记 Bug**（模块自己抛异常、校验函数自己出错才记，见 `docs/v3-deviations.md` #44）。

| 模块 | 周期任务 | 校验 | 关掉的后果 |
|---|---|---|---|
| 观测 | — | 生成记录条数 ↔ 用量日志张数（差 ≤ max(2, 0.5%)，从本地 0 点算）；V5 恢复速度「接口 ↔ 实测」不是「对不上」 | 不再核对，日志照常记 |
| 容量 | 每小时最多记一次上游订阅快照；每日 +10 加窗 | 上游 V5 消耗 ↔ 日志 V5 张数（每 1% 应在 7～28 张，需要 ≥ 6 小时数据且剩余 < 95%）；内存每小时计数 ≥ 日志 − 2；最近 60 分钟出图 ≤ 上限 × 1.05 + 2；没有可用上游账号则只有一条失败 | 不再自动调每小时上限，各项上限照常 |
| 分配 | 额度重算（§2） | 算法管理的 Key 都已同步（额度与算法结果一致）；保底 ≤ 每日上限 | 额度冻结在当前值 |
| 自动驾驶 | 见 §3 | 无 | 三条规则保持当前设置 |
| 风险检查 | — | 只读体检，不处罚：出站请求头、TLS 指纹（wreq 模拟 Chrome 129 / 128）、代理 / 出口链路、上游风控信号（429 / 401 / 成功率与 7 天基线对比）、用量集中度（单把 Key 占全站 ≥ 50% 提示）、今日 V5 消耗（≥ 80% 注意，≥ 100% 风险） | 不再体检 |
| 领 Key | — | 已领人数 ≤ 名额（名额 0 = 不限） | 开关就是 `register_open`：关 = 暂停领取，已领的和邀请 Key 绑定不受影响 |

## 5. 告警：谁私信站长，谁只记日志

**原则**（站长 2026-10-15）：只有重大事故才私信——成员现在用不了，或账号 / 数据有风险，且需要站长动手。其余只写日志，后台总览「有 N 件事要处理」能看到同样的情况。同一个 kind 在冷却期内只处理一次（只写日志的也一样）。通道优先级：`ALERT_WEBHOOK_URL` > `ALERT_CHANNEL_ID` > 私信 `ALERT_USER_ID`。

### 5.1 网关会私信的（`alerts.rs` 的 `PAGE_KINDS`，共 5 个；`upstream_403_text` 随文本通道删除，不会再触发）

| kind | 触发 | 冷却 |
|---|---|---|
| `upstream_401` | 上游返回 401：Token 失效，生图全部不可用 | 30 分钟 |
| `upstream_403_cooldown` | 图片通道连续 3 次 403，Token 进入冷却 | 30 分钟 |
| `upstream_token_changed` | 后台改动了上游 Token（安全事件） | 无 |
| `disk_low` | 数据盘剩余不足 10% | 6 小时 |
| `test` | 后台「发送测试告警」 | 无 |

### 5.2 巡检会私信的（`nai-ops patrol`，只有「危险」）

同一批危险项（按检查项标识去重，不看数字）每 6 小时（`PATROL_COOLDOWN`）提醒一次，出现新的危险项立刻发，全部解除后补一条「已恢复」。退出码 0 正常 / 1 注意 / 2 危险 / 3 巡检自己出错。危险项：

- 容器没运行（默认检查 `owl-gate`、`owl-naimei`、`caddy`）；`/healthz` 不是 200；可用内存 < 200 MB；磁盘已用 > 90%；
- 近 1 小时出现过上游 429（账号级红线）；
- `/register` 已开放但网关缺 `REGISTRATION_BRIDGE_SECRET`。

「注意」只记巡检日志，例如：有未解决的 error 级 Bug、近 30 分钟出图上游 500（一位成员反复提交上游处理不了的请求就能刷出一串，不私信）、全站排队 waiting > 20 或上游冷却中、今日出图 ≥ 日容量 90%、V5 剩余 ≤ 2%、近 1 小时最长等待 > 120 秒、24 小时出错率 > 5%（且 ≥ 5 次出错，被拒不算故障）、拒绝里 70% 以上是账号上限类、同 Key 12 秒内连发 > 3 次、24 小时新注册 > 288 人、记账三方计数不一致、可用内存 < 350 MB、磁盘 > 80%。

### 5.3 只写日志的

上游 5xx（30 分钟内至少 2 把不同的 Key 遇到才算「上游可能故障」）、上游降级 / 恢复（最近 10 分钟 ≥ 4 次且失败过半）、上游表现变化（变慢 / 429 增多 / 失败率上升，同类 3 小时一次）、上游 429、每小时上限自动调整、V5 余量低（6 小时一次）、无效 Key 洪水、新 Bug / 复发（1 小时一次）、服务启动、单次 402 / 403。

## 6. Bug 追踪与自动处理

- 来源：接口未捕获的错误（500）、后台任务失败、记账写入失败、成员在图片返回前断开、网页脚本报错。同一个特征码只占一行（次数累加），复发会重新打开。不记 Key、提示词或 IP。
- **不入库**：浏览器网络失败（`Failed to fetch`、`NetworkError`、`Load failed`、`AbortError`、`cancelled`、`已取消` 等 10 种说法，不分大小写）；出错脚本不在 `/_owl/` 下的 `Uncaught …`；原因在上游的 5xx（它们在用量日志和「上游」页里照常有记录）。
- **自动关闭**（维护循环，最多每 10 分钟一次，启动后第一轮就跑）：上述噪音的存量立刻关；warn 级 24 小时、error 级 7 天没再出现的关。关闭原因存在 `error_events.auto_resolved`（`network` / `upstream` / `foreign_script` / `stale_warn` / `stale_error`），后台显示「自动处理」徽标。
- 已处理（含自动关闭）且 30 天没再出现的 Bug 会被清理（`purge_error_events`）。新 Bug / 复发只写日志，不私信。

## 7. 数据保留与备份

| 项 | 值 | 来源 |
|---|---|---|
| 提示词 | 开关 `audit_prompts`（线 1 = 记录）；保留天数 `audit_retention_days`（线 0 = 长期保留；环境变量 `AUDIT_RETENTION_DAYS` 默认 7，被站点设置压过；范围 0～3650） | 线 / 码 |
| 原图 | 开关 `audit_thumbs`（线 1 = 保存成功结果的原图，设置名沿用旧名）；`audit_image_keep_hours`（码 24，线 0 = 只按容量上限删）；`audit_image_cap_mb`（码 8192，线 8192）：超过后从最旧的开始删到约 90%；没有记录指向的原图文件（崩溃残留）超过 1 小时清掉 | 线 / 码 |
| 用量日志 | `USAGE_LOG_RETENTION_DAYS` 默认 0 = 长期保留；大于 0 时按 max(7, N) 天清理。线上未核对。每日计数器和账本不受影响 | 码 |
| 后台操作日志 | 180 天 | 码 |
| 状态码统计 / 已处理的 Bug | 状态码统计保留 30 天；已处理的 Bug 超过 30 天没再出现才清理 | 码 |
| 披露 | 首页、`/register` 回复、`/help` 都用同一个函数（`audit_disclosure`）生成「本站记录了什么」，以它为准；线上当前设置下的结果是「提示词长期保存、原图在服务器空间满时自动删最旧的」 | 码 / 线 |

| 备份 | 时间 | 内容 |
|---|---|---|
| 本机 GFS（`naigate-backup`，`/etc/cron.d`） | 每晚 03:30 | 一致快照（不含原图）：每日留 14 份、每周日留 8 份、每月 1 号留 6 份，在 `/opt/backups/{daily,weekly,monthly}` |
| 异地加密（`nightly_backup.sh`） | 每晚 04:30 | 快照 + `.env` + `secret_key` + 上游 Token 存档等配置 + 3.0 部署文件 + Caddy 配置，AES-256 客户端加密后推 S3（脚本注释写 60 天过期，桶的生命周期配置未核对）。加密口令在服务器 `/root/.backup_pass`，**必须在服务器之外另存一份** |
| 脱敏只读快照（`ro-snapshot.sh`） | 每天 05:10 | 去掉 Key 原文、盐、提示词、捐赠的上游 Token；留 7 份 |
| 状态页镜像（`status_mirror.sh`） | 每 5 分钟 | 静态 HTML 推 S3，主站挂了镜像仍在 |
| 站长 Mac（`pull-backup-mac.sh`） | 每天 04:30 | 拉最新的本机每晚备份，校验后保留 30 份 |

快照由宿主机 `nai-ops snapshot` 逐表复制（只读事务，不经过网关容器，不锁库）。部署、切换、回滚、灾难恢复见 `docs/OPERATIONS.md` 与 `deploy/ops/migrate_restore.sh`。停机时网关最多等 110 秒让生成中的图完成并记账，compose 的 `stop_grace_period` 为 120 秒。

## 8. Key 的一生

### 8.1 领取（Discord `/register` 是唯一发 Key 的路径）

| 检查（按代码里的先后顺序） | 值 |
|---|---|
| 必须在指定服务器里发命令 | `DISCORD_GUILD_ID` |
| Discord 账号年龄 | ≥ 7 天（线 `REGISTER_MIN_ACCOUNT_DAYS=7`；码默认 7；0 = 不限） |
| 没被拉黑 | 闲置回收也会拉黑 |
| 一个 Discord 账号一把 Key | 重复领取会提示用 `/quota`、`/resetkey` |
| （填了邀请 Key 的在这里分流，直接绑定，不看下面三项） | |
| 每小时最多发放 `issue_hourly_cap` | 码 12，线 0（不限） |
| `register_open` 与名额 | `register_open` 线 1，关 = 「领 Key 当前已关闭」；名额 `register_max_users`（缺省取 `REGISTER_MAX_USERS`）线 0（不限），满了先到先得，没有候补 |
| 可选：身份组门槛 | `register_role_guild` / `register_role_id`，线均为空（不设门槛）；用机器人 Token 核验，不用用户 OAuth |

新 Key 的默认值：每日 V4.5 上限 = `register_daily_images`（线 95，算法同步）、每日 V5 = `register_daily_v5`（线 3，算法同步）、模型范围 `register_image_scope`（线 `all`，`legacy` 则不能用 V5）、有效期 `register_expires_days`（线 30；0 = 不过期）、每分钟请求数 `REGISTER_RPM`（码 5）、功能 `register_features`（线 `image,vibe,tags`）、无 Anlas 权限、无图生图权限。领取成功的回复只有本人可见，不走 OAuth，不私信。

- **邀请 Key**：站长预先发的、带「邀请」标签；成员 `/register` 时填入即绑定到自己的账号，Key 不变，不受开关 / 名额 / 每小时上限限制。邀请 Key 豁免闲置回收、全站重置和每日清空。
- **到期**：过期的 Key 回 403「该 Key 已过期」；成员再 `/register` 时旧 Key 和记录自动释放，可重新领取。被站长停用的 Key 不会因过期被释放。
- **换 Key**：`/resetkey`，旧 Key 立即失效。
- **每日清空**：配置了 `REGISTER_RESET_AT`（HH:MM）才有，清空全部自助注册用户（删 Key 和记录，邀请 Key 除外）；线上快照未列该变量，未核对。

### 8.2 闲置回收

`KEY_INACTIVITY_DELETE_DAYS`：码默认 3，线 3；0 = 关闭（什么都不删）。每小时扫一次。

- 判定：**连续 N 天没有成功生成过图片**（只看 `last_image_at`，从没生成过则从创建时间起算）；只在 Key 启用中的才算。
- 豁免：管理员、测试 Key、「邀请」标签的 Key；已被停用的 Key 也不回收。
- 动作：删 Key、**拉黑**对应 Discord 账号（原因「连续 N 天未活动自动回收」）、摘掉身份组；不提前私信提醒（站长 2026-10-13）。要恢复只能站长在后台解除拉黑。
- 保险：网关停机超过 1 小时再启动时，闲置计时从启动那一刻重新起算（`key_inactivity_grace_started_at`），不会因停机一次性删光；该设置读不懂时不删任何 Key。

### 8.3 退群回收

`member_sweep`（默认开）：每轮维护（约 5 分钟）用机器人 Token 核对 1 人，每人每天一次。只有 Discord 明确回复 404 + code 10007（不是成员）才删 Key、释放名额、写操作日志「退群回收 Key」；任何其他结果（限流、网络、机器人不在服务器）都不动，1 小时后重查。不拉黑，不私信。

### 8.4 全站重置与停用原因

- 后台「全站重置 Key」（需要输入确认词）：失效所有非管理员、非「邀请」的 Key，写停用原因 `site_reset`，清空它们的 Discord 绑定和标签。成员拿旧 Key 请求回 403「这把 Key 在全站重置时已经失效：请在 Discord 服务器里用 /register 重新领取……」。
- 站长在后台手动停用：原因 `admin`（没写原因时同样处理），回 403「该 Key 已被站长停用，有疑问请联系站长。」；重新启用时清空原因。
- 被占名额：停用的成员仍占名额（避免停用后空出名额、启用后超限）。
- 封禁（拉黑）：写入封禁表，撤销现有 Key 与身份组，之后不能再领。

## 9. 共用站点设置（登记表 `site_flags.rs`）

读写只走 `site_flags::get / put`；缺失或格式不对一律回到默认值（默认值按 fail-closed 选）。下表是登记表的全部 16 项：前 15 项是 `ALL`，第 16 项 `member_daily_anlas` 已登记在表里但没列进 `ALL`（所以 `facts_table()` 不会生成它）。各模块自己的参数组不在此表，见表后。

| 键 | 类型 | 默认 | 范围 | 说明 | 线上当前值（2026-10-15） |
|---|---|---|---|---|---|
| `member_sweep` | flag | True | | 退群回收：每人每天核对一次是否还在服务器，不在就删 Key | 未设置，按默认（开） |
| `web_login_paused` | flag | True | | 网页 Discord 登录暂停开关（暂停时首页不显示登录按钮，`/login` 与回调也不发起 OAuth） | 0（登录开放） |
| `dm_enabled` | flag | False | | 私信总闸门（关 = 任何私信都不发；开启后也限速：10 分钟 ≤ 3 条、24 小时 ≤ 20 条） | 未设置，按默认（关） |
| `issue_hourly_cap` | int | 12 | 0–500 | 每小时最多发放的新 Key（0 = 不限） | 0（不限） |
| `register_open` | flag | False | | 开放 /register 领取 | 1 |
| `home_join_open` | flag | True | | 首页「加入 Discord」按钮（只管按钮显示，关 = 置灰「关闭注册」） | 1 |
| `global_daily_v5` | int | `GLOBAL_DAILY_V5`（码 150） | 0–100000 | 全站每日 V5 张数（0 = 不限）；算法每个 tick 写入 | 171 |
| `global_daily_anlas` | int | `GLOBAL_DAILY_ANLAS`（码 200） | 0–100000 | 全站每日 Anlas 额度（0 = 不开放，Vibe PVP 抢额度） | 1000 |
| `global_daily_anlas_until` | float | 0.0 | 0–2e+09 | 全站每日 Anlas 开放截止时间戳（0 = 不限时） | 0 |
| `global_monthly_anlas` | float | `GLOBAL_MONTHLY_ANLAS`（码 10000） | 0–1e+06 | 全站每月 Anlas 预算（0 = 不限） | 9000 |
| `guard_base_daily_images` | int | 100 | 0–100000 | V4.5 每人每天保底张数；算法每个 tick 同步成 A | 95 |
| `algo_notice` | str | '' | | 首页显示的今日额度说明（算法每个 tick 写入） | 每天变，不抄 |
| `audit_image_retention_days` | int | 3 | 0–365 | 已停用（原图按容量上限和保存小时数清理） | 未设置 |
| `audit_image_cap_mb` | int | 8192 | 0–500000 | 原图占用上限（MB）：超过后自动删掉最旧的；0 = 不限 | 8192 |
| `audit_image_keep_hours` | int | 24 | 0–8760 | 原图在服务器上保存几小时；0 = 只按容量上限删 | 0 |
| `member_daily_anlas`（第 16 项） | int | 50 | 0–100000 | 每人每日 Anlas 上限（仅对未手动设置 Anlas 日额度的 Key 生效；0 = 不限） | 未设置，按默认（首页文案核对为 50） |

其他参数组（都存在 `site_settings`，键名带前缀）：

| 组 | 键 | 默认 / 线上 |
|---|---|---|
| `guard_*` | 10 个账号保护字段，见 §1 | 见 §1 |
| `runtime_*` | `queue_timeout`、`key_image_min_interval`、`image_min_interval`、`image_429_cooldown_seconds`，后台可调，重启后仍优先于 `.env` | 见 §1 |
| `quota_*` | `quota_auto_enabled` 1、`quota_target_avg` 150、`quota_base` 100、`quota_ceiling_max` 300、`quota_base_min` 50、`quota_base_min_fair` 10、`quota_step` 25、`quota_base_step` 10、`quota_v5_min` 3、`quota_v5_max` 30（码；`quota_base_min`、`quota_base_step` 在极简模式里不参与计算）；运行态 `quota_ceiling`、`quota_base_now`、`quota_algo_day`、`quota_algo_last`、`quota_algo_history`、`quota_v5_day_plan` | 线上没有覆盖前十个；运行态见 §2 |
| `autopilot_*` | `autopilot_slots` / `autopilot_breaker` / `autopilot_key_guard`（`off` / `observe` / `enforce`，缺省 `observe`） | 线 enforce / enforce / enforce |
| `register_*` | `register_open`、`register_max_users`、`register_daily_images`、`register_daily_v5`、`register_image_scope`、`register_expires_days`、`register_features`、`register_role_guild` / `register_role_id` / `register_role_note` | 线 1 / 未设（环境变量 0）/ 95 / 3 / all / 30 / `image,vibe,tags` / 空 |
| `audit_*` | `audit_prompts`、`audit_thumbs`、`audit_retention_days`（见 §7） | 线 1 / 1 / 0 |
| `bot_*` | `bot_status`（机器人心跳，奶妹每分钟经桥接接口写一次：账号 / 服务器 / 网关延迟 / 启动时间 / `seen_at`；3 分钟没更新算离线，后台 Discord 页的「机器人状态」读它） | 随机器人上报 |
| 其他 | `features_global`（JSON，缺省全开）、`v5_alert_threshold`（默认 20，范围 1～100）、模块开关（`module_<名字>`，默认开；「领 Key」模块的开关就是 `register_open`） | 线 `v5_alert_threshold` = 20 |

线上库里还有一些代码已不再读取的残留键，看到不要当真：`anlas_pool_last`、`anlas_member_daily_cap`、`anlas_min_key_age_days`、`economy_mode`、`autopilot_economy`、`guard_daily_cap_fallback`、`guard_account_3h_cap`、`guard_account_10m_cap`、`ack_*`、`quota_shadow_*`，以及 `bot_gallery_*`（跑图分享设置）、`bot_events`（机器人最近动作）——后两者随功能一起删除，见 `docs/v3-deviations.md` #54。

## 10. 拒绝原因码（`usage_log.reason`，`reasons.rs`）

统计、自动驾驶和每小时上限的自动调整都按码数，不数中文说明。

| 码 | 状态 | 含义 |
|---|---|---|
| `queue_full` | 429 | 全站排队满 |
| `key_busy` | 429 | 这把 Key 上一张没出完就又发 |
| `hourly_cap` / `quiet_cap` / `burst_cap` / `daily_cap` / `paced` | 429 | 账号每小时 / 安静时段 / 10 分钟突发 / 每日上限 / 时段份额 |
| `cap_3h` | — | 3 小时上限已删除，只剩历史记录 |
| `over_base` | 429 | 超过保底后全站不空闲（保底 = 每人上限时不会出现） |
| `breaker` / `cooldown` | 429 | 全站熔断 / 上游 429 后全站冷却 |
| `queue_timeout` | 429 | 排队或预算核对等待超时 |
| `key_rate` | 429 | 这把 Key 请求太快或在途请求太多 |
| `key_paused` | 403 | Key 被自动驾驶暂停 |
| `feature_off` | 403 | 功能没给这把 Key 开通 / 全局关闭 / 模型范围不够 |
| `member_quota` | 402 | 这把 Key 自己的每日 / 每月额度用完 |
| `site_quota` / `v5_exhausted` | 402 | 全站共享额度用完 / 上游账号 V5 免费额度耗尽 |
| `param` | 400 | 参数不被支持（模型、采样器、角色数、权重写法等） |
| `body_abort` | — | 请求体传输中断（客户端或网络） |
