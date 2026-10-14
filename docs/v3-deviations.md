# 3.0 与 2.x 的有意差异（review 清单，上线后并入 CHANGELOG 3.0.0）

原则：额度 / 处罚 / 上游节奏 1:1；下面这些是**有意**改动，每条都写明原因。

| # | 位置 | 2.x 行为 | 3.0 行为 | 原因 |
|---|---|---|---|---|
| 1 | 上游 Retry-After | 超长数字（>4300 位）或越界日期使 `parse_retry_after` 抛异常，429 冷却没生效、请求 500 | 解析失败按默认 20 秒冷却 | 旧行为是 bug：被限流时反而不冷却 |
| 2 | `/public/my-export` | 必定 500：提交 98e6f2e 删防分享体系时把 `_EXPORT_AT` / `_EXPORT_LOCK` / `EXPORT_MAX_BYTES` 三个全局量一起删了，接口还在引用 | 恢复原意（取自提交 4033417）：单次最多打包最新的 **200 MB** 原图；**全站同一时刻只打包一份**（别人正在打包回 429「有人正在打包，请过半分钟再试」）；**每人每 60 秒一次**（429「打包有点重，请 1 分钟后再试」，失败的请求不占间隔）。文案与旧版一致 | 线上 bug |
| 3 | 上游 TLS / HTTP2 指纹 | Python OpenSSL 指纹 + Chrome 请求头 | wreq 模拟 Chrome 129/128 指纹，请求头集合 / 顺序 / 取值不变 | 站长 10-14 批准 |
| 4 | 前端 CSP | `'unsafe-inline'` | `script-src 'self'; style-src 'self'`，首屏数据用 JSON 注入 | 安全加固 |
| 5 | 巡检 `patrol` | 查已删除的表崩溃，systemd 当成功 | 去掉死表检查；内部错误退出码 3 | 线上 bug |
| 6 | 问卷 | 粘贴 Key 识别 | 用成员 Discord 会话识别（仍兼容 Bearer Key） | 只留 Discord 登录 |
| 7 | 「上游可能故障」私信（`upstream_5xx`） | 任何一个上游 500 都私信站长（30 分钟冷却） | 30 分钟内至少 2 把不同的 Key 遇到 5xx 才私信；单个成员的 500 仍记 Bug | 10-14 夜一位成员的 V5 请求（超长负面提示词、旧式参数）反复 500，站长每半小时收到一次误报，同时段其他成员一切正常 |
| 8 | 状态页「NovelAI 上游」当前状态 | 最近 1 小时失败率 > 1%（≥ 10 次请求）即「性能下降」 | 另需失败 ≥ 3 次且来自 ≥ 2 把不同的 Key；每日色条不变 | 同上：1/100 次失败就把全站状态标成性能下降 |
| 9 | 后台成员操作的私信 | 停用 / 删除 / 重置 / 重新生成默认私信成员 | 默认不发，勾选才发 | Discord 应用曾被风控（680009），避免批量私信 |
| 10 | 状态页 `/status` 配色 | 文字 / 底色对比度 1.88–3.5:1（浅灰说明文字、黄 / 绿状态字、白字配黄 / 绿横幅） | 所有文字与底色 ≥ 4.5:1：说明文字 `#6F6E67`、浅色主题下状态字加深（`#4B7A0E` / `#855600` / `#B23F1A` / `#C42B2B`）、深色主题红字提亮（`#EB5757`）、横幅底色加深，黄色横幅改深色字；`status::tests::palette_contrast` 逐对检查。90 天色条是图形不是文字，仍用 Statuspage 原色 | 可读性 |
| 11 | 状态页导航与锚点 | 过去事件的标题是没有 `href` 的 `<a>`；页面没有回首页的入口 | 标题链接到同页锚点 `#incident-<id>`（每个事件容器有对应 `id`）；顶部操作区新增「返回首页」（`/`）；新增 `:focus-visible` 轮廓、标题悬停下划线；页面带 `/favicon.svg` 图标 | 链接要能点；键盘可达 |
| 12 | 状态页安全头与资源加载 | CSP `default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'`；样式、主题切换脚本、颜色（`style="..."`）全部内联；无 `X-Frame-Options` | CSP `default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`，另加 `X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`（`Cache-Control: no-cache`、`nosniff` 照旧）。样式与脚本编进二进制，由 `/_owl/status-page.css`、`/_owl/status-page.js` 提供（地址带内容哈希，匹配时长期缓存）；页面里没有 `style` 属性和内联脚本，颜色改用类名（`level-N` / `tone-*`）。`/status.json`、Atom 的响应头不变 | 防点击劫持与脚本注入，与首页 / 后台 CSP 一致 |
| 13 | 状态页主题存储键 | 状态页自己的 `owl-status-theme` | 与首页 / 后台共用 `owl-theme`（`web/src/lib/theme.ts` 的约定）；升级后第一次访问回到「跟随系统」，且只认 `light` / `dark` | 三个页面主题一致 |
| 14 | 后台事件接口 `/admin/api/incidents*` | 旧版挂在普通路由上，不进后台操作日志 | 与其它后台写接口一致：发布 / 更新各记一条（「发布状态页事件」「更新状态页事件」，对象为「事件 #id」），校验失败记「失败（状态码）：原因」；`incident_id` 不是整数时在登录校验之前返回 FastAPI 风格的 422 | 可追溯谁改了状态页 |
| 15 | `HEAD /status.json`、`HEAD /status/feed.xml` | 405（旧版只声明了 GET） | 200、无正文（axum 的 GET 路由自带 HEAD；`/status` 本来就支持 HEAD） | 框架行为，无害 |
| 16 | `/public/live/stream`（新增） | 只有 1 秒轮询 | SSE：连上推一条，之后只在内容变化时推（不含 `t`），15 秒无事件发保活注释；`Cache-Control: no-store` + `X-Accel-Buffering: no`。快照放在 `AppState.live` 的 watch 通道里，`/public/live`（响应体不变）与 SSE 共用；HTTP 取数沿用 1 秒缓存语义，有订阅者时后台任务每秒重算。同时连接数上限 2000，超出回 503 + `Retry-After`（页面自动退回轮询）；`state.shutdown` 取消时流结束 | 服务端有变化才推，省掉每个标签页每秒一次的请求 |
| 17 | `/admin/api/events`（新增） | 无（后台页面自己每 15 秒定时刷新） | 后台会话才能连的 SSE：`{"live": <同 /public/live，不含 me>, "badges": {"bugs": 未处理 Bug, "donations": 未处理捐赠}}`。页面把任何一条消息都当作「该刷新了」，所以：真有变化才推（且至少隔 2 秒，倒计时类字段不算变化）；没变化也每 15 秒推一条，等价于旧的定时刷新。会话失效（改密码 / 过期）时流结束，重连得到 401。上限 32 条 | 新后台 `Admin.tsx` 的 `EventSource("/admin/api/events")` |
| 18 | `/terms` `/privacy` 文案 | 「多人共用或转售会被系统记录并回收 Key」；站点域名写死在代码里；没有登录方式说明；更新日期 2026-10-14 | 去掉「系统记录」；条款加一句「网页登录只支持 Discord 账号（OAuth），不提供其他登录方式」，隐私政策加一句「网页登录只有 Discord 一种方式，本站不收集你的 Discord 密码」；域名读 `SITE_URL`（没配就不写）；更新日期 2026-10-15。HTML 结构、响应头、CSP 与旧版逐字节一致 | 计划：登录只留 Discord，文案不再提「记录证据」；公开仓库不写死站点地址 |
| 19 | 首页首屏数据 | 服务端把登录态写进 `<body class="authed haskey">`，其余数据靠接口 | `/` 在 HTML 里注入 `<script type="application/json" id="owl-boot">`：`{"me", "live", "status"}`，三块分别等于 `/public/me`、`/public/live`（登录成员带「我的排队」）、`/public/status` 的响应；游客的 `me` 是 `{"logged_in": false}`；任何一块取不到就缺省，页面自己走接口 | CSP 去掉了内联脚本；首屏不再闪一下游客内容 |
| 20 | 网页报错上报的「注入脚本」过滤 | 堆栈里含 `/static/`、`/admin`、本站域名、本机地址才算本站文件 | 另外认 `/_owl/`（3.0 前端文件路径） | 否则本站脚本自己的报错会被当成浏览器插件注入的丢掉 |
| 21 | `/queue-status` 的 `image_min_interval` | 后台改过之后是整数（`15`），没改是浮点（`15.0`） | 恒为浮点（运行期限制统一存 f64） | 只是 JSON 数字写法，客户端不区分 |
| 22 | 公告文件不是 UTF-8 / `quota_algo_last` 不是 JSON 对象 / `Origin` 头无法解析（如 `http://[abc`） | `/announcement`、`/public/live`、`/public/client-error` 抛异常 → 500 | 公告按替换字符解码；`quota_algo_last` 当空对象；该 `Origin` 头按字面比较，对不上本站就 204 丢弃 | 旧行为是 bug，都只会被刻意构造的输入触发 |
| 23 | 后台 `GET /quota-shadow`、`GET /shadow`（及 `/anlas-pool`、`/share-guard`） | `/quota-shadow` 返回 `{"retired": true, ...}` 空壳；`/shadow` 回放近 24 / 168 小时请求做调度影子对比；后两个在 2.x 就没有路由，只剩操作日志里的名字 | 都不移植（404） | 影子模式已退役，新前端 `web/src` 没有任何引用；`gate-core::shadow` 仍在，要恢复只需加一个路由 |
| 24 | `PUT /admin/api/ops/registration` 的 `features` 含列表 / 对象元素（如 `[["a"]]`） | `x not in dict` 抛 `unhashable type`，500 并记 Bug | 422「features 必须是功能名列表」 | 与其它非法元素（数字、未知功能名）同一处理；请求体里这样写只可能是误操作 |
| 25 | 后台「首页加入按钮」「确认交叉校验」「发送公告」的操作日志 | 处理函数记一条、中间件再记一条「POST /…」 | 只留处理函数那条（更具体） | 重复行没有信息量，淹没真正的操作 |
| 26 | Discord REST 基地址 | 告警用不带版本的 `/api`，注册用 `/api/v10`，混用 | 全部 `/api/v10`，可用 `DISCORD_API_BASE` 指向本地假服务 | 不带版本的请求走 Discord 的旧默认版本 |
| 27 | 自助领 Key 的 Discord 请求头 | httpx 默认 User-Agent | 带 `DiscordBot (nai-gate, <版本>)`；不跟随重定向（httpx 默认也不跟，避免把机器人 Token 带到别处） | Discord 要求机器人带可识别的 User-Agent，reqwest 默认不带 |
| 28 | 机器人 `/ops limit` 名额超过 1000 | `set_registration` 抛未捕获的 `ValueError`，接口 500、机器人只显示「出了点问题」 | 422 `{"detail": "max_users 超出范围 0～1000"}`，机器人把原因原样转给管理员；操作日志照记「失败（422）」 | 旧行为是输入校验漏接 |
| 29 | 桥接 / 成员页 JSON 请求体 | 没有大小上限 | 上限 1 MiB（超出 413）；请求体校验错误仍是 FastAPI 的 422 列表形状 | 加固，正常请求只有几百字节 |
| 30 | 身份组核验时 Discord 回 200 但响应体不是 JSON | 未捕获的 `ValueError`，`/issue` 500 | 按「Discord 暂时无法核验身份组」拒绝（403） | 不让 Discord 的异常响应变成我们的 500 |
| 31 | 发 Key / 绑定邀请 Key 落库 | 建 Key、登记领取、改名分几次提交；中途失败会留下没人认领的 `Discord:<id>` Key | 登记领取 + 改名 + 身份组标志在一个事务里；登记失败就把刚建的 Key 删掉 | 不留孤儿 Key |
| 32 | 内核「风险检查」模块里的「出站请求头」「TLS 指纹」两条 | TLS 指纹条固定报「注意」：「TLS 握手是 Python 原生指纹，与 Chrome 请求头不一致」；请求头条在 HTTP/2 包没装好或客户端未启动时标「注意」，并有「HTTP/2：启用 / 未启用（缺 h2 包，已降级 HTTP/1.1）/ 关闭」一行；代理条写「httpx 默认信任它们」 | TLS 指纹条标「正常」：「TLS / HTTP2 握手由 wreq 模拟 Chrome（129 / 128，随 Token 档位），与请求头一致」；请求头条只要有 Token 就是「正常」，HTTP/2 一行只有「启用 / 关闭」；代理条只写「检测到代理环境变量：」。其余四条（代理为空时的文案、上游风控信号、用量集中度、今日 V5 消耗）逐字不变 | 出站指纹 3.0 换成 wreq（#3），旧文案会说谎；这两条只展示、不处罚、不记 Bug。对拍里两边都用固定占位，其余逐项比对 |
| 33 | 内核 / 额度 / 自动驾驶抛异常时 `kernel_last.<模块>.error` 与 Bug 标题里的消息 | `类型名: Python 异常消息`（如 `JSONDecodeError: Expecting property name enclosed in double quotes: line 1 column 2 (char 1)`），Bug 的详情带 Python 堆栈 | 类型名一致，消息是 Rust 侧的说明文字（额度 / 自动驾驶的消息固定为「内核输入数据异常…」），Bug 的特征码因调用位置不同而不同。已发生的写入、之后是否继续、错误发生在哪一步，与旧版一致（对拍里每种异常都有场景）。两处边角：私密参数 `allocation.v5_k_table` 写成不是 `[[下限, 系数], …]` 的形状时，旧版只在读到 V5 剩余读数且迭代到坏条目时才抛 TypeError，这里在取参数时就报；额度设置读不懂（已抛异常的那一轮）时，旧版在抛异常之前不会向上游发订阅查询，这里同样不发 | 异常消息本来就是实现细节，不进任何算法输入；站长看到的是出错这件事和类型 |
| 34 | 内核 tick / 额度重算的并发；关停时后台循环 | 全部在一个 asyncio 事件循环里，后台定时、后台改参数、改单个 Key 额度可以在 await 处交错；关停时 `task.cancel()` 在下一个 await 处切断循环，可能停在多步写入的中间 | 同一时刻只跑一轮 `tick_all`、只跑一次额度重算（排队，不丢）；循环只在睡觉时响应关停，一轮工作做完才退出；`kernel_last` 的运行记录在一轮结束时整体落库 | 交错执行的结果本来就是未定义的（两份同时写 `quota_algo_history` 会互相覆盖）；多步写入被腰斩会留下半套额度。结果等于旧版某一种合法的交错 |
| 35 | 后台 Key 管理里的 `anlas_auto`（Anlas 自动分配） | `POST /keys` 把 `quota_auto`、`anlas_auto` 都写成 -1；`PATCH /keys/{id}` 的 `anlas_mode`（off / auto / manual，非法值 422）和 Anlas 字段改动会写 `anlas_auto`；`GET /members` 返回 `anlas_auto`、`anlas_mode` | 不读不写 `anlas_auto` 列；`PATCH` 里的 `anlas_mode` 被忽略（不再校验、不再改 `allow_anlas` / `daily_anlas`）；`/members` 不再返回这两个字段 | Anlas 自动分配（`anlas_pool`）在 3.0 已删除；列留着（只增不删）。回滚到 2.x 时，3.0 期间新建 / 改过的 Key 的 `anlas_auto` 是默认 0（= 交给自动分配），2.x 的分配循环会按条件重新分配 |
| 36 | 后台 Key 管理里的 `v5_pinned`（V5 基础值）与手动改额度后的重算 | `PATCH` 的 `v5_pinned` 写进列并让动态额度算法立刻重算；手填 `daily_v5` 时顺手清空它并重算；`/members` 返回 `v5_pinned` | 不读不写（`PATCH` 里的 `v5_pinned` 被忽略，`/members` 不返回）；只有「交给算法」（`quota=auto` 或旧参数 `quota_mode=auto`）才触发 `quota_algo.run`，手动改额度只把 `quota_auto` 置 -1 | `v5_pinned` 在 3.0 已删除（启动时已把列清成 NULL）；手动额度的 Key 本来就不归算法管，那次重算只是顺带，下一轮定时重算同样会生效 |
