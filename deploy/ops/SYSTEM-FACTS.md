# 猫头鹰公益站 · 系统事实清单（唯一来源）

> **更新日期** 2026-10-15 · **版本** 3.0 · **规则**：数字以代码为准，改了代码先改这里，再同步首页、后台说明和其他文档。
>
> 来源标记：**码** = 代码里的常量或默认值（文件都在 `crates/` 下）；**线** = 线上 2026-10-15 的只读快照（站点设置 `site_settings` 与环境变量，不含密钥和成员数据）；**算** = 由前两者算出来的数。
> 表里「线上」一列会随后台调整变化，「代码默认」是重装后得到的值。线上一栏写「未核对」= 快照里没有这个变量或键，按代码默认理解，没有第二来源。
> 配置只有两层：**环境变量**（`.env`，只放密钥和基础设施）和**后台设置**（`site_settings`，后台可改；没设过时用代码里的常量）。运行时可调的参数没有环境变量；私密参数文件 `data/private_params.json` 已取消、程序不再读（回滚到 2.x 时旧版仍会读它，所以备份脚本还带着它）。

## 0. 一句话

站长的 NovelAI Opus 账号被所有成员共用：网关给每人发一把虚拟 Key，所有出图都经过上游账号。
额度是 **PVP**：全站共用的固定池子，先到先得；每人上限只是上限，不是承诺。服务日每天 07:00（Asia/Shanghai）切换。

- 线上现有活跃成员 Key 170 把、Discord 领取记录 164 条（线，两个数口径不同，未做第二来源核对）；名额不限（线；后台设置 `register_max_users=0`，10-15 核对过库里有这一行。环境变量 `REGISTER_MAX_USERS` 已删除）。
- 上游账号数：代码按令牌池大小计（`每日上限 × 账号数`）；线上快照没有直接给出账号数（V5 采样只见 1 个 Token），未核对。
- 定时任务（§4）：动态额度、上游 V5 快照、每小时上限调整、全站熔断、单 Key 限流每 10 分钟，维护每 5 分钟，都带 ±20% 抖动（下限 30 秒）；闲置回收每小时。巡检由宿主机定时器每 5 分钟跑一次（码 `jobs.rs`、`nai-patrol.timer`）。

## 1. 账号保护（`guard_*` 与运行期限制）

| 参数（设置键 / 环境变量） | 代码默认 | 线上 | 说明 |
|---|---|---|---|
| 出图并发（每个上游账号） | 1 | 未核对 | 每个 Token 一个出图槽；后台「上游令牌池」可调 1～4，这是唯一的开关（`SINGLE_IMAGE_SLOT_ENFORCED` 已删除） |
| 账号出图间隔 `runtime_image_min_interval`（`IMAGE_MIN_INTERVAL`） | 15 秒（代码常量；后台可改，范围 15～120） | 20 秒 | 同一个上游账号两次开始生成之间的最小间隔；出图结束后还要静默 1～3 秒（见下面「出图后静默」两行，取较晚者） |
| 间隔抖动 `guard_interval_jitter` | 5（0～30） | 5 | 每次间隔再加随机 0～N 秒。线上平均 22.5 秒一张，理论最快 3600÷22.5 = 160 张/小时（算），正好等于每小时上限 |
| 出图后静默下限 `guard_post_jitter_min` | 1（0～30） | 1 | 出图结束（成功或失败，只要发到了上游）后，下一张最早 = max(按间隔排好的时刻, 结束时刻 + 静默)；静默在下限～上限之间均匀随机，不超过账号出图间隔。原环境变量 `POST_REQUEST_JITTER_MIN`（已删除），库里没存过时用默认值 |
| 出图后静默上限 `guard_post_jitter_max` | 3（0～30） | 3 | 小于下限时按下限；两项都为 0 = 出图后不静默。原环境变量 `POST_REQUEST_JITTER_MAX` |
| 每小时上限 `guard_account_hourly_cap` | 150（0～240；自动调整范围 `guard_hourly_min` / `guard_hourly_max`，默认 100～160） | 160（自动调整范围 100～200，沿用 2.x 私密参数） | **滚动最近 60 分钟**，不是整点重置；自动调整规则见下 |
| 每日上限 `guard_account_daily_cap` | 1000（0～20000） | 1700 | 每个上游账号每个服务日的出图总量（V4.5 + V5 合计）；用完等次日 07:00 |
| 按时段放行 `guard_daily_burst` | 150（0～20000；0 = 不分时段） | 600 | 服务日起点先放 burst 张，之后每小时补「每日上限 ÷ 24」（线上约 70.8 张，算），封顶每日上限。本时段份额用完回 429（`paced`），网关替成员最多等 300 秒（每 20 秒看一次）再拒绝 |
| 安静时段 `guard_quiet_start` / `guard_quiet_end` | 0 / 0（两者相同 = 不启用） | 3 / 7 | 北京时间 03:00～07:00 |
| 安静时段上限 `guard_quiet_hourly_cap` | 20（0～240） | 0 | 0 = **完全暂停出图**（不是不限） |
| 每把 Key 排队 `guard_key_image_queue` | 1（0～3） | 1 | 正在生成的那张之外最多再排 1 张，即同一把 Key 最多 2 张在途；再发回 429（`key_busy`）。排着的那张等前一张出完（或被拒 / 取消）才往上游发，由排队登记本身保证；等待超过排队超时回 429（`queue_timeout`） |
| 全站排队 `guard_queue_per_account` | 5（0～100） | 5 | 每个上游账号最多同时 5 张（含正在生成的）；满了回 429（`queue_full`） |
| 每把 Key 每分钟请求数 | 成员 Key 5；后台手建 Key 10（代码常量，`defaults.rs`；手建时可在弹窗里改） | 未核对 | 生图 / 放大 / 导演工具 / Vibe 请求都计数，被拒的也计；标签补全不计。超出回 429（`key_rate`） |
| 排队超时 `runtime_queue_timeout` | 90 秒（代码常量；后台可改，范围 15～300） | 90 | 超时回 429（`queue_timeout`） |
| 上游 429 后的全站冷却 `runtime_image_429_cooldown_seconds` | 60 秒（代码常量；后台可改，范围 60～3600） | 180 | 实际冷却 = max(此值, 上游 Retry-After)，Retry-After 先夹到 5～3600 秒。冷却期内所有图片请求回 429（`cooldown`）；冷却状态跨重启保留（`image_cooldown_until`，超过 1 小时的值视为无效） |
| 上游连续 403（图片通道） | — | — | 连续 3 次 → 该 Token 冷却 5 → 15 → 45 → 60 分钟（`min(60, 5×3ⁿ)`），24 小时内没有 403 档位归零；私信站长 |
| 无效 Key 防爆破 | 同一 IP（IPv6 按 /64）600 秒内 ≥ 20 次无效 Key → 拦截 900 秒（代码常量） | — | 只拦无效 Key；有效 Key 不受影响。后台登录另有一套：每 IP 300 秒内最多 5 次（代码常量） |
| V5 官方额度查询 | 只在真实 V5 出图时刷新缓存 | 低提示线 20 | 缓存：剩 ≤ 2% 为 15 秒，低于「V5 余量低提示线」（`v5_alert_threshold`，默认 20）为 60 秒，其余 300 秒。定时任务「上游 V5 快照」每小时最多再主动读一次订阅信息 |

每把 Key 的限速只有上面两层：排队（同时生成 1 张 + 排队 1 张）和每分钟请求数。2.x 的「每把 Key 两张图至少隔 15 秒」（`KEY_IMAGE_MIN_INTERVAL` / `runtime_key_image_min_interval`）、每把 Key 一个信号量（`KEY_CONCURRENCY`）、「同一把 Key 同时在途 ≤ 4」和每个账号的「10 分钟突发窗口」（`burst_cap`）都已删除（与排队重复，或永远触发不了）。

**每小时上限的自动调整**（AIMD，`guard.rs` + 定时任务「每小时上限自动调整」）：

- 上游返回 429：立即减半（系数 0.5 是代码常量；不低于下限 `guard_hourly_min`，默认 100；手动压到下限以下时只维持或降低），并记下降档前的上限，之后加窗的天花板 = max(下限, 80% × 该值)。
- 每天上调一步（`guard_hourly_step`，默认 +10）：要求距上次调整和上次 429 都 ≥ 24 小时、过去 24 小时日志覆盖 ≥ 20 小时、且过去 24 小时有成员因 `hourly_cap` 被拒（上限真的顶到过）；不超过上限 `guard_hourly_max`（默认 160）；只升不降。第一次运行只开始计时。
- 下限 / 上限 / 步长三个数是后台「额度与保护 → 账号保护与排队 → 更多」里的设置（2.x 是私密参数 `capacity.hourly_*`，线上文件的值要在上线前核对后手动填进来）；下限不能高于上限。
- 线上 160 已到上限；最近一次 429 在 2026-10-13 01:30（`guard_last_upstream_429` 换算，算）。

## 2. 额度分配

每把成员 Key 的 `quota_auto=1`（新建默认）时由算法管理：定时任务「动态额度」每一轮把「V4.5 每人上限 A」和「V5 每人额度」下发给所有这样的 Key，同时写新领 Key 用的默认值（`register_daily_images`、`register_daily_v5`、`register_image_scope=all`）、`global_daily_v5` 和首页文案 `algo_notice`。动态额度开着（`quota_auto_enabled` ≠ 0）时，这四项后台改不了（回 400「… 由动态额度管理：关闭动态额度后才能手动修改」）；没设过时的起点是 30 / 0 / `legacy` / 150（码，对应的环境变量已删除）。开关就是 `quota_auto_enabled`（后台「额度与保护」页）。后台手动设置额度的 Key（`quota_auto=-1`）不归算法管。

### 2.1 V4.5 每人每日上限 A（`quota_ceiling`，线 95）

每个服务日「动态额度」的第一轮（07:00 后约 0～12 分钟）看前一个服务日重算一次：

1. **可分预算** = max(0, 账号数 × 每日上限 − 全站 V5 池)。线上若是 1 个账号即 1700 − 171 = 1529（算，账号数未核对）。
2. **用量** = 昨天每位算法管理的成员用掉的 V4.5 免费图张数（只算用过的人）。
3. 规则：
   - 昨天日志覆盖 < 20 小时（代码常量；2.x 可被私密参数 `allocation.min_coverage_hours` 改，私密参数文件已取消）→ A 不变；
   - 总用量 ≥ 预算的 95% → A = water-fill（用得少的人用多少算多少，余下的平分给用得多的人，取整数下限，至少 1），夹到 10～300；
   - 否则有人用满 A（最高用量 ≥ A）→ A + 25，不超过 300；
   - 否则不变。
4. 常量（码 `quota.rs`，线上快照没有覆盖这些键）：步长 25、下限 10、上限 300、起点 150（`quota_ceiling` 没有值时才用）。
5. 每人只有这一个上限 A，用满回 402「今日 V4.5 免费图额度已用完」。2.x 的「保底 B + 全站空闲时借用到 A」一层已删除（`guard_base_daily_images` 不再读写）。

### 2.2 每人当天就能用到 A

2.x 对非手动 Key 的 V4.5 免费图还有一层「每人平滑注水」（7 点先给 30 张、之后每小时补 4 张，一天最多约 126 张），3.0 已删除：每人当天就能用到 A。全站的节奏只由 §1 的「按时段放行」（`guard_daily_burst`，原因码 `paced`）管，对所有人一样。

### 2.3 V5 全站池与每人额度

| 量 | 公式 | 线上（10-15） |
|---|---|---|
| 恢复速度 rate（%/天） | 接口读数与实测值合并：两者都有 → 取较小；只有实测 → 实测；只有接口 → min(接口, 5)；都没有 → 5。读数超出 1～15 不用；接口与实测相差 > 50% 记「对不上」并取较小。实测来自日复盘（余量变化 + 用掉的量）或安静时段直接测量（≥ 4 个安静小时） | 11.0（接口 11.0 / 实测 11.17，状态「已核对」） |
| 全站 V5 池 G | ⌊rate × 14.2⌋；14.2 = 官方「1% ≈ 17.3 张（23 步）」按 28 步折算（码 `V5_IMAGES_PER_PERCENT`） | ⌊11.0 × 14.2⌋ = 156（算，按 10-15 快照的 rate；线上 10-15 当天仍是旧公式定下的 171） |
| 每人 D5 | max(1, ⌊G ÷ max(10, N)⌋)，N = 今天之前 3 个完整服务日里用过 V5 的人数（不再夹到 3～30） | 旧口径「近 3 日」75 人 → ⌊156 ÷ 75⌋ = 2（算；新口径的 N 未核对） |

- G 和每人额度在服务日第一次计算时（07:00 后）定下、写进 `quota_v5_day_plan`，当天不再改。2.x 的电量系数 k、3～30 夹取、剩余 ≥ 85% 的「洪峰」放宽和白天放宽 / 收紧都已删除（私密参数 `allocation.v5_k_*` / `min_people` / `v5_flood_*` / `v5_tighten_below` 随公式一起删除）。上线当天已按旧公式定下的方案沿用到次日 07:00。
- 每人 × N 可以大于 G（超卖）：每人额度只是个人上限，全站池先到先得，首页文案写「全站今日共 G 张，先到先得」。
- 出 V5 图前会向上游确认官方免费额度：确认已耗尽时，无 Anlas 权限的 Key 回 402「上游账号今日 V5 免费额度已耗尽（非个人原因），请改用 V4.5 生成」（`v5_exhausted`）；查询失败则回 503「V5 额度暂时无法确认，请稍后重试；未发送生图，未扣积分」。

### 2.4 Anlas

成员 Key 默认没有 Anlas 权限（`allow_anlas=false`），实际只有 Vibe（参考图编码）会花 Anlas，走全站 PVP 池。

| 项 | 代码默认 | 线上 | 说明 |
|---|---|---|---|
| 每人每日上限 `member_daily_anlas` | 50（0 = 不限） | 未设置，首页文案「Anlas 每人 50/天」与默认一致 | Key 自己设置了 `daily_anlas` > 0 则以 Key 为准（成员 Key 为 0）；Vibe 编码一张参考图约 2 点，50 点 ≈ 25 次 |
| 每人每月 | Key 自己的 `monthly_anlas`（成员 Key 为 0 = 不限） | — | |
| 全站每日 Anlas 池 `global_daily_anlas` | 200（0 = 不开放；后台「额度与保护 → Anlas 与 V5 预算」可改，范围 0～100000） | 1000 | Vibe PVP：全站合计先到先得，抢完回 402（`site_quota`）。开放截止时间戳 `global_daily_anlas_until` 已删除（后台从来没有写入口） |
| 全站每月预算 `global_monthly_anlas` | 10000（0 = 不限） | 9000 | 超出回 402 |

价格（码 `policy.rs`）：Vibe 编码 2 Anlas/张；精确参考 5 Anlas/张/图；V4 / V4.5 的 Vibe 超过 4 张，每多 1 张 +2/图。Anlas 自动分配（旧的分配池）已删除，快照里残留的 `anlas_pool_last` 等键不再被读取。

### 2.5 免费规格（对没有 Anlas 权限的 Key 生效；数值写死在 `policy.rs`，没有环境变量）

- 每次 1 张（`n_samples` 强制为 1）；步数 ≤ 28（`FREE_MAX_STEPS`）；SMEA 关闭。
- V4.5 及更早的模型：总像素 ≤ 1024×1024 = 1048576（`FREE_MAX_PIXELS`），宽高取 64 的倍数，超出等比缩小。
- V5：不超过三个预设之一——832×1216、1216×832、1024×1024，超出按长宽比吸附（> 1.1 → 1216×832，< 0.9 → 832×1216，其余 1024×1024）。
- 图生图 / 局部重绘只对后台勾了「允许图生图」（`allow_img2img`）且有 Anlas 权限的 Key 开放，其余回 400；ControlNet 回 400；角色参考（`characterReferences`）被移除。
- 新领 Key 开通的功能 `register_features`（线 `image,vibe,tags`；`*` = 不限）；功能可选 `image`、`upscale`、`augment`、`vibe`、`tags`（文本生成 / 语音 / OpenAI 兼容聊天 2026-10-15 已整套删除，库里 Key 的 `features` 或 `features_global` 里残留的 `text` / `voice` 读取时被忽略）。站长还有全局开关 `features_global`（JSON，缺省全开）。
- 上游对「模型不存在」回的是 403，网关会窥探一小段响应体把「模型级 403」和「账号级 403」分开（只有账号级才计入账号健康度和 403 冷却）。

### 2.6 重置券

站长在后台给某把 Key 发券；成员在首页使用后，重置**本人**今天已用的 V5 与 Anlas 计数（`daily_quota_offsets`），不含 V4.5，也不改变全站池。一张券只能用一次，有过期时间。

## 3. 自动驾驶（两个定时任务，各每 10 分钟）

两条规则一律执行。2.x 每条规则还有 `off` / `observe` / `enforce` 模式（设置 `autopilot_<规则>`），以及第三条规则「名额 +5」（`slots`），3.0 都已删除（名额规则从不生效，只用过 enforce），库里残留的 `autopilot_slots` / `autopilot_breaker` / `autopilot_key_guard` / `autopilot_slots_at` 不再读。`autopilot_last` / `autopilot_history`（每小时快照）3.0 起不再写；判断结果看后台「系统 → 定时任务」里两个任务的摘要。

| 规则 | 做什么 |
|---|---|
| `breaker` 熔断 | 最近 15 分钟图片请求里「上游失败」≥ 5 次且 ≥ 30%，且这些失败来自 ≥ 2 把不同的非测试 Key → 全站暂停出图 300 秒（`breaker`）。只算上游 5xx 和流中途断开；本地拦截、上游 429、4xx 不算；只看上次熔断结束之后的数据；失败只来自 1 把 Key（多半是这把 Key 的请求有问题）不熔断 |
| `key_guard` 单 Key 限流 | 1 小时内同一把 Key 被拒 ≥ 60 次 → 暂停该 Key 1 小时（回 403，`key_paused`）。统计不含站点层面的拒绝（排队满、各类账号上限、熔断、冷却、`v5_exhausted`、`queue_timeout`、`site_quota`、`key_paused` 本身）。提示「{原因}，{到期时间} 自动恢复。」（2.x 留下的旧行原因为空，写「这把 Key 已被临时暂停，{到期时间} 自动恢复。」）。2.x 还有「换网段」「多客户端全天在用」两条规则，输入来自早已删除的防分享证据、恒为空，3.0 已删除；数据库表 `share_state` 原样保留，现在只存每把 Key 的临时暂停 |

## 4. 定时任务（7 个，码 `jobs.rs`、`jobs/`）

每件事一个循环，启动后立即跑第一轮，之后按周期再跑；一轮做完才响应关停。每轮的结果（时间、正常 / 出错、一句摘要、耗时）记进设置 `job_last`，后台「系统」页的「定时任务」卡片显示：超过 3 个周期没跑或上次出错时，「系统」的侧栏角标和总览「N 个定时任务上次出错或超时没运行」会提醒。任务出错或 panic 只算这一轮失败（记 Bug，来源 `job:<名字>`），循环照常继续。卡片里没有开关。

| 任务 | 周期 | 做什么 | 开关 |
|---|---|---|---|
| `quota` 动态额度 | 10 分钟 ±20% | §2：服务日第一轮复盘昨天、定当天的 V5 方案；之后每轮把额度下发给算法管理的 Key。后台改动态额度参数、把 Key 交回算法时立即重算一次（同样记进 `job_last`） | `quota_auto_enabled`（「额度与保护」页）：关 = 不计算、不向上游查询，额度冻结在当前值，四项算法管着的设置恢复手改 |
| `upstream_snapshot` 上游 V5 快照 | 10 分钟 ±20%，距上次满 1 小时才记 | 主动读一次订阅，写一行 `upstream_snapshots`（`fresh=1` 才拿来做安静时段测速）；读不到也记一行无读数的 | — |
| `hourly_cap` 每小时上限自动调整 | 10 分钟 ±20% | §1 的 AIMD 加窗：平稳一天且顶到过上限 +10，24 小时最多一次，只升不降 | `guard_hourly_step=0` 不上调 |
| `breaker` 全站熔断 | 10 分钟 ±20% | §3 | — |
| `key_guard` 单 Key 限流 | 10 分钟 ±20% | §3 | — |
| `maintenance` 维护 | 5 分钟 ±20% | 清过期记录、状态码统计落库、原图保存期与容量上限、自动关闭不用处理的 Bug、注册服务的身份组同步、磁盘自检、官方状态；每一步各自出错、各自记 Bug（来源 `maintenance:<步骤>`） | — |
| `idle_recycle` 闲置回收 | 1 小时（不抖动） | §8.2 | `KEY_INACTIVITY_DELETE_DAYS=0` 关闭 |

2.x / 3.0 早期的「内核模块」（观测、容量、分配、自动驾驶、风险检查、领 Key）连同交叉自检、开关（`module_<名字>`）、运行条和后台「模块」页一起删除。原来线上核对的恒等式改由测试守住：生成记录条数 = 成功出图张数 = 今日计数、内存每小时计数 = 本小时日志张数、算法管理的 Key 都已同步。出站指纹的只读体检（风险检查）没有替代。

## 5. 告警：谁私信站长，谁只记日志

**原则**（站长 2026-10-15）：只有重大事故才私信——成员现在用不了，或账号 / 数据有风险，且需要站长动手。其余只写日志，后台总览「有 N 件事要处理」能看到同样的情况。只写日志的事件在发生的地方直接打一行日志，不经过告警通道、也不去重。私信只有一个通道：机器人（`DISCORD_BOT_TOKEN`）私信 `ALERT_USER_ID`（`ALERT_CHANNEL_ID`、`ALERT_WEBHOOK_URL` 不再读取）。

### 5.1 网关会私信的（共 5 个，见 `alerts.rs` 模块说明；同一个 kind 在冷却期内只发一次）

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
- `/register` 已开放但网关缺 `REGISTRATION_BRIDGE_SECRET`（关着时不报）。

「注意」只记巡检日志：读不到容器状态（docker 不可用）、`free` / `df` 读取失败、可用内存 < 350 MB、磁盘已用 > 80%。巡检只查网关自己报不了、或必须在宿主机上看的事；成员成功率 / 等待、拒绝结构、记账对数、未解决 Bug、上游 500、日容量、V5 余量、排队这类统计在后台总览里看。

### 5.3 只写日志的

上游 5xx（出图请求的 `[upstream-5xx]` 行带请求形态）、上游 429 与每小时上限自动下调、V5 余量低（`V5 low allowance`）、无效 Key 洪水（某 IP 被临时拦截）、Bug（每次记录都有一行 `bug` 日志）、单次 402 / 403。上游降级 / 恢复、上游表现变化（变慢 / 429 增多 / 失败率上升）不再单独记：状态页按最近 10 分钟算上游状态，后台「上游」页打开时现算表现。服务启动只有进程自己的启动日志。

## 6. Bug 追踪与自动处理

- 来源：接口未捕获的错误（500）、后台任务失败、记账写入失败、成员在图片返回前断开、网页脚本报错。同一个特征码只占一行（次数累加），复发会重新打开。不记 Key、提示词或 IP。
- **不入库**：浏览器网络失败（`Failed to fetch`、`NetworkError`、`Load failed`、`AbortError`、`cancelled`、`已取消` 等 10 种说法，不分大小写）；出错脚本不在 `/_owl/` 下的 `Uncaught …`；原因在上游的 5xx（它们在用量日志和「上游」页里照常有记录）。
- **自动关闭**（维护循环，最多每 10 分钟一次，启动后第一轮就跑）：上述噪音的存量立刻关；warn 级 24 小时、error 级 7 天没再出现的关。关闭原因存在 `error_events.auto_resolved`（`network` / `upstream` / `foreign_script` / `stale_warn` / `stale_error`），后台显示「自动处理」徽标。
- 已处理（含自动关闭）且 30 天没再出现的 Bug 会被清理（`purge_error_events`）。新 Bug / 复发只写日志，不私信。

## 7. 数据保留与备份

| 项 | 值 | 来源 |
|---|---|---|
| 提示词 | 开关 `audit_prompts`（线 1 = 记录，没设过 = 不记）；保留天数 `audit_retention_days`（线 0 = 长期保留；没设过 = 7；范围 0～3650）。环境变量 `AUDIT_*` 已删除（只看后台设置） | 线 / 码 |
| 原图 | 开关 `audit_thumbs`（线 1 = 保存成功结果的原图，设置名沿用旧名；没设过 = 不存）；`audit_image_keep_hours`（码 24，线 0 = 只按容量上限删）；`audit_image_cap_mb`（码 8192，线 8192）：超过后从最旧的开始删到约 90%；没有记录指向的原图文件（崩溃残留）超过 1 小时清掉 | 线 / 码 |
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
| Discord 账号年龄 | ≥ 7 天（代码常量 `defaults.rs`，没有环境变量） |
| 没被拉黑 | 闲置回收也会拉黑 |
| 一个 Discord 账号一把 Key | 重复领取会提示用 `/quota`、`/resetkey` |
| （填了邀请 Key 的在这里分流，直接绑定，不看下面三项） | |
| 每小时最多发放 `issue_hourly_cap` | 码 12，线 0（不限） |
| `register_open` 与名额 | `register_open` 线 1，关 = 「领 Key 当前已关闭」；名额 `register_max_users`（没设过 = 10；线 0 = 不限，须确认是库里的值），满了先到先得，没有候补 |
| 可选：身份组门槛 | `register_role_guild` / `register_role_id`，线均为空（不设门槛）；机器人不在该服务器里，改由成员打开 `/login?verify=1` 用 Discord 授权（scope `identify guilds.members.read`），网关用成员自己的授权查身份组，核验结果记在 `role_verifications` 表，`/register` 只查这张表；普通登录只要 `identify`、不受门槛影响 |

新 Key 的默认值：每日 V4.5 上限 = `register_daily_images`（线 95，算法同步）、每日 V5 = `register_daily_v5`（线 3，算法同步）、模型范围 `register_image_scope`（线 `all`，`legacy` 则不能用 V5）、有效期 `register_expires_days`（没设过 = 0 不过期；线 0）、每分钟请求数 5（代码常量）、功能 `register_features`（没设过 = `image`；线 `image,vibe,tags`）、无 Anlas 权限、无图生图权限。后台手动新建 Key 和一键邀请 Key 读同一份「新成员默认」（每日 V4.5 / V5 张数、模型范围、有效期、默认功能），弹窗里没填的字段取这些值；只有每日 / 每月 Anlas 上限（默认 0 = 跟随全站每人每日上限 / 2500）和每分钟请求数（10）是代码常量，由 `/admin/api/ops` 带给弹窗。环境变量 `DEFAULT_*` / `REGISTER_*` / `DISCORD_ROLE_ID` 已删除，限定身份组只在后台「领 Key」里设。领取成功的回复只有本人可见，不走 OAuth，不私信。

- **邀请 Key**：站长预先发的、带「邀请」标签；成员 `/register` 时填入即绑定到自己的账号，Key 不变，不受开关 / 名额 / 每小时上限限制。邀请 Key 豁免闲置回收和全站重置。
- **到期**：过期的 Key 回 403「该 Key 已过期」；成员再 `/register` 时旧 Key 和记录自动释放，可重新领取。被站长停用的 Key 不会因过期被释放。
- **换 Key**：`/resetkey`，旧 Key 立即失效。

### 8.2 闲置回收

`KEY_INACTIVITY_DELETE_DAYS`：码默认 3，线 3；0 = 关闭（什么都不删）。每小时扫一次。

- 判定：**连续 N 天没有成功生成过图片**（只看 `last_image_at`，从没生成过则从创建时间起算）；只在 Key 启用中且未过期的才算。
- 站点出不了图的日子不算：已完结的服务日全站成功出图不到 50 张（上游 Token 失效、站长关生图、熔断等）就跳过这一天，往回多数一天，凑够 N 天（最多回看 30 天）；当前服务日不在安静时段、且最近 60 分钟全站一张都没出成功时，本轮不回收。平时截止时间仍是「现在 − N 天」。
- 豁免：管理员、测试 Key、「邀请」标签的 Key；已被停用的 Key 也不回收；已过期的 Key 不算闲置（照旧在成员下次 `/register` 时释放，不拉黑）。
- 动作：删 Key、**拉黑**对应 Discord 账号（原因「连续 N 天未活动自动回收」）、摘掉身份组；不提前私信提醒（站长 2026-10-13）。要恢复只能站长在后台解除拉黑。
- 保险：网关停机超过 1 小时再启动时，闲置计时从启动那一刻重新起算（`key_inactivity_grace_started_at`），不会因停机一次性删光；该设置读不懂时不删任何 Key。「停机」看最后一次有动静的时间：请求日志和定时任务的运行记录（`job_last`，维护任务每 5 分钟一次）取较晚者。只看请求日志会把安静时段凌晨没人发请求时的重启误判成停机（10-14 清晨误判过一次）。
- 被闲置回收拉黑的人再来 `/register`：「你之前的 Key 连续 N 天没有出图，已被自动回收，这个 Discord 账号不能再领取 Key。」站长手动拉黑的照旧是「已被站长停用」。两种提示都不提解封。

### 8.3 退群回收

`member_sweep`（默认开）：每轮维护（约 5 分钟）用机器人 Token 核对 1 人，每人每天一次。只有 Discord 明确回复 404 + code 10007（不是成员）才删 Key、释放名额、写操作日志「退群回收 Key」；任何其他结果（限流、网络、机器人不在服务器）都不动，1 小时后重查。不拉黑，不私信。邀请 Key 只由站长手动删：持有人退群时不删 Key，只写操作日志「退群（邀请 Key 保留）」（对象「Key #编号 名字」）提醒站长，之后照常每天核对一次。

### 8.4 全站重置与停用原因

- 后台「全站重置 Key」（需要输入确认词）：失效所有非管理员、非「邀请」的 Key，写停用原因 `site_reset`，清空它们的 Discord 绑定和标签。成员拿旧 Key 请求回 403「这把 Key 在全站重置时已经失效：请在 Discord 服务器里用 /register 重新领取……」。
- 站长在后台手动停用：原因 `admin`（没写原因时同样处理），回 403「该 Key 已被站长停用，有疑问请联系站长。」；重新启用时清空原因。
- 被占名额：停用的成员仍占名额（避免停用后空出名额、启用后超限）。
- 封禁（拉黑）：写入封禁表，撤销现有 Key 与身份组，之后不能再领。

## 9. 共用站点设置（登记表 `site_flags.rs`）

读写只走 `site_flags::get / put`；缺失或格式不对一律回到默认值（默认值按 fail-closed 选）。下表是登记表的全部 10 项：前 9 项是 `ALL`，第 10 项 `member_daily_anlas` 已登记在表里但没列进 `ALL`。web_login_paused、home_join_open、global_daily_anlas_until、audit_image_retention_days 四项已删除，库里残留的值读到也忽略。各模块自己的参数组不在此表，见表后。

| 键 | 类型 | 默认 | 范围 | 说明 | 线上当前值（2026-10-15） |
|---|---|---|---|---|---|
| `member_sweep` | flag | True | | 退群回收：每人每天核对一次是否还在服务器，不在就删 Key | 未设置，按默认（开） |
| `issue_hourly_cap` | int | 12 | 0–500 | 每小时最多发放的新 Key（0 = 不限） | 0（不限） |
| `register_open` | flag | False | | 开放 /register 领取 | 1 |
| `global_daily_v5` | int | 150（码；环境变量 `GLOBAL_DAILY_V5` 已删除） | 0–100000 | 全站每日 V5 张数（0 = 不限）；算法每个 tick 写入，动态额度开着时后台改不了 | 171 |
| `global_daily_anlas` | int | 200 | 0–100000 | 全站每日 Anlas 额度（0 = 不开放 Vibe；Vibe PVP 抢额度） | 1000 |
| `global_monthly_anlas` | float | 10000.0 | 0–1e+06 | 全站每月 Anlas 预算（0 = 不限） | 9000 |
| `algo_notice` | str | '' | | 首页显示的今日额度说明（算法每个 tick 写入） | 每天变，不抄 |
| `audit_image_cap_mb` | int | 8192 | 0–500000 | 原图占用上限（MB）：超过后自动删掉最旧的；0 = 不限 | 8192 |
| `audit_image_keep_hours` | int | 24 | 0–8760 | 原图在服务器上保存几小时；0 = 只按容量上限删 | 0 |
| `member_daily_anlas`（第 14 项） | int | 50 | 0–100000 | 每人每日 Anlas 上限（仅对未手动设置 Anlas 日额度的 Key 生效；0 = 不限） | 未设置，按默认（首页文案核对为 50） |

其他参数组（都存在 `site_settings`，键名带前缀）：

| 组 | 键 | 默认 / 线上 |
|---|---|---|
| `guard_*` | 12 个账号保护字段（9 个上限 / 节奏字段，加每小时上限自动调整的下限 / 上限 / 步长），见 §1 | 见 §1 |
| `runtime_*` | `queue_timeout`、`image_min_interval`、`image_429_cooldown_seconds`，后台可调，重启后仍优先于代码常量 | 见 §1 |
| `quota_*` | `quota_auto_enabled` 1、`quota_target_avg` 150、`quota_ceiling_max` 300、`quota_base_min_fair` 10（A 的下限，名字沿用旧版）、`quota_step` 25（码）；运行态 `quota_ceiling`、`quota_algo_day`、`quota_algo_last`、`quota_algo_history`、`quota_v5_day_plan` | 线上没有覆盖前五个；运行态见 §2 |
| `autopilot_*` | `autopilot_breaker_until`（规则模式键、`autopilot_last`、`autopilot_history` 已不再读写，见 §3） | 运行态 |
| `register_*` | `register_open`、`register_max_users`、`register_daily_images`、`register_daily_v5`、`register_image_scope`、`register_expires_days`、`register_features`、`register_role_guild` / `register_role_id` / `register_role_note` | 线 1 / 未设 / 95 / 3 / all / 0 / `image,vibe,tags` / 空 |
| `audit_*` | `audit_prompts`、`audit_thumbs`、`audit_retention_days`（见 §7） | 线 1 / 1 / 0 |
| `bot_*` | `bot_status`（机器人心跳，奶妹每分钟经桥接接口写一次：账号 / 服务器 / 网关延迟 / 启动时间 / `seen_at`；3 分钟没更新算离线，后台 Discord 页的「机器人状态」读它） | 随机器人上报 |
| 其他 | `features_global`（JSON，缺省全开）、`v5_alert_threshold`（后台叫「V5 余量低提示线」，默认 20，范围 1～100；只决定官方余量缓存刷多勤和后台是否标「低」）、`job_last`（定时任务每轮的结果，见 §4） | 线 `v5_alert_threshold` = 20 |

线上库里还有一些代码已不再读取的残留键，看到不要当真：`anlas_pool_last`、`anlas_member_daily_cap`、`anlas_min_key_age_days`、`economy_mode`、`autopilot_economy`、`guard_daily_cap_fallback`、`guard_account_3h_cap`、`guard_account_10m_cap`、`ack_*`、`quota_shadow_*`、`guard_base_daily_images`、`quota_base` / `quota_base_min` / `quota_base_step` / `quota_base_now`、`quota_v5_min` / `quota_v5_max`、`runtime_key_image_min_interval`、`autopilot_slots` / `autopilot_breaker` / `autopilot_key_guard` / `autopilot_slots_at`、`autopilot_last` / `autopilot_history`、`kernel_last`、`module_*`（内核模块改成定时任务后不再读写）、`module_allocation`（3.0 额度精简删掉的），以及 `bot_gallery_*`（跑图分享设置）、`bot_events`（机器人最近动作）——后两者随功能一起删除。

## 10. 拒绝原因码（`usage_log.reason`，`reasons.rs`）

统计、自动驾驶和每小时上限的自动调整都按码数，不数中文说明。

| 码 | 状态 | 含义 |
|---|---|---|
| `queue_full` | 429 | 全站排队满 |
| `key_busy` | 429 | 这把 Key 上一张没出完就又发 |
| `hourly_cap` / `quiet_cap` / `daily_cap` / `paced` | 429 | 账号每小时 / 安静时段 / 每日上限 / 全站时段份额 |
| `cap_3h` / `burst_cap` | — | 3 小时上限、10 分钟突发窗口已删除，只剩历史记录 |
| `over_base` | — | 保底一层已删除，历史记录按说明第一句显示 |
| `breaker` / `cooldown` | 429 | 全站熔断 / 上游 429 后全站冷却 |
| `queue_timeout` | 429 | 排队或预算核对等待超时 |
| `key_rate` | 429 | 这把 Key 每分钟请求太多 |
| `key_paused` | 403 | Key 被自动驾驶暂停 |
| `feature_off` | 403 | 功能没给这把 Key 开通 / 全局关闭 / 模型范围不够 |
| `member_quota` | 402 | 这把 Key 自己的每日 / 每月额度用完 |
| `site_quota` / `v5_exhausted` | 402 | 全站共享额度用完 / 上游账号 V5 免费额度耗尽 |
| `param` | 400 | 参数不被支持（模型、采样器、角色数、权重写法等） |
| `body_abort` | — | 请求体传输中断（客户端或网络） |
