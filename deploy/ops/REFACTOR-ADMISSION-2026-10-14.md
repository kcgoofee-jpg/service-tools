# 准入控制器重构（2026-10-09 评审 §9.1/9.2/9.3 落地 · 安静时段版）

## 背景
2026-10-09 全面评审（docs/REVIEW_2026-10-09.md §9）提出六条架构建议，其中"每 Token 一个准入控制器"列为体量最大的一条。此后部分已被实现（request_timing 派发判定、complete_image_operation 两阶段取消、PACED 等位、PR #85 删闲置层）。本计划落地剩余部分，并对齐问卷 Top 诉求（32 份）：报错提示更清楚（16 提及）＞ 排队和等待体验（12）＞ 稳定性（11）。

## 现状路径（2026-10-14 探索代理实测，file:line 见文末附录）
auth → 全站冷却 → RPM → feature → **读 body** → 归一化/估价 → quota_image_check → key 间隔 → guard_precheck（PACED ≤5min 轮询）→ guard.admit_image（每 Key 1 跑 1 队）→ key_sem → 预算预留 → nai.request：pick_token（轮询）→ **AdjustableLimiter.acquire()（竞速）** → token 间隔 → 派发。

## 改动

### A. 预准入 `_hard_block_snapshot()`（image_routes.py，新 ~35 行）
位置：`_generate_image` / `generate_image_stream` 里，`read_image_payload` **之前**（auth 之后）。
内容：调 `guard.token_block_reason` 快照（新只读方法，不记账不吃令牌）：
- 安静时段硬暂停（quiet_hourly_cap≤0）→ 立即 429，文案与现有一致（含 7 点恢复时间）
- 全部 Token 因日上限/上游停用被硬挡且非 PACED → 立即 429（与现在 guard_precheck 会给出的理由相同，只是提前）
**边界**：body 未读、模型未知 → 成员额度 / V5 判定**不放**在这里（V5 请求成员额度另算，提前拒会误伤）——只挡"模型无关的确定性拒绝"。
**fail-open**：预准入任何异常 → 记 warning 放行到旧路径；预准入只能提前返回既有拒绝，绝不新增准入。

### B. Token 级 FIFO 公平队列（concurrency.py 新类 + nai.py 接线，~100 行）
现况：每 Token `AdjustableLimiter(1..4).acquire()`，超时后所有等待者竞速，后到者可能先得；单 Key 多并发可垄断。
改：`TokenSlotQueue`：
- FIFO deque 唤醒队头；管理员并发数变化（`set_image_concurrency`）实时生效（保留 AdjustableLimiter 可调语义）
- 跨 Key 轮询：派发过一次 Key K 后，若队列里仍有**其他 Key** 的等待者，K 的下一个请求让位一次（防垄断，等价评审"按 Key 的 FIFO 队列轮询调度"）
- 取消安全：waiter 取消时从 deque 摘除（finally）；外层 `asyncio.timeout(queue_timeout)` 与 429 QUEUE_TIMEOUT 文案不变
- 接线点：nai.py `request`（668-669）与 `image_stream`（829-832）替换 acquire/release；`image_tool.perform`、`perform_encoding` 走同一 `request`，自动覆盖

### C. 统一上游响应策略 `app/upstream_policy.py`（新模块 ~90 行）
- `parse_retry_after(raw, now, cap)` → 秒或 None（边界：缺失/非数字/过去时间/超大值钳制）
- `classify(status, peek)` → `{kind: rate_limited|forbidden_model|forbidden_account|unauthorized|other, retry_after, message}`
- nai.py `request`（723-740）与 `image_stream`（859-876）的 429/403/401 分支改调 classify；**逐分支行为保持**：429→mark_rate_limited+on_rate_limited、403 peek 区分"模型不存在"（177-198 判据搬家不改）与 mark_forbidden 指数退避、401 停用 Token

### D. 卫生（顺带）
- 迁移：`DROP TABLE IF EXISTS key_idle_reminders`（半死：仅 delete_key 里 DELETE，无读写）、`DROP TABLE IF EXISTS share_confirm`（生产有表、代码零引用）；同步删 delete_key 里对 key_idle_reminders 的 DELETE
- 删除 `app/core/state_holder.py`（零导入者）

## 不做（今晚）
- 文本通道并入同一控制器（文本已有 global_sem+分道冷却；风险高收益低）
- Key 哈希存储、全量 pydantic schema 化、DB 写入全面收口（另行迭代）

## 不变量（对抗审查锚点）
1. 状态码与成员可见文案不变；预准入新增的拒绝必须是"旧路径也会给出的同一理由"，只是更早
2. fail-open：A 的任何异常放行到旧路径
3. 取消语义：排队可取消、派发后不可取消（request_timing.was_sent 不动）；排队取消不记账，派发后断开走既有 complete_image_operation
4. 测试契约不动：FakeNai.request 关键字参数、FakeState 属性、main.py:582-605 兼容别名
5. 记账 shield 与 disconnect bug 记录不动
6. 管理员运维面不回退：set_image_concurrency、set_admin_enabled、清除冷却仍可用

## 测试
- 新增：test_admission_preflight（安静暂停 / 全挡非 PACED / PACED 放行 / fail-open）
- 新增：test_token_slot_fifo（FIFO 顺序、跨 Key 轮询让位、取消摘除、并发数调大唤醒）
- 新增：test_upstream_policy（Retry-After 边界、403 两分支、401）
- 回归：全量 pytest（重点 test_generation_integration、test_image_admission、test_paced_wait、test_nai_integration、test_image_stream_routes、test_v5_*）

## 部署
安静时段 03:00–07:00（quiet_hourly_cap=0 硬暂停、零生图流量）→ deploy.sh → healthz + 冒烟（/status 200、一次假请求 401 路径、看板数据）。夜哨（5 分钟一轮）在窗口内值班到 07:15。

## 附录：请求路径关键锚点（探索代理 2026-10-14）
- 路由：image_routes.py:987-1001（register_routes）；`_generate_image` 604-913；读 body 在 611
- 预检/准入：guard_precheck 139-166（PACED_CAP=300/POLL=20 @135-136）；image_admission 169-190（guard.admit_image @179）；_acquire_slots 204-225
- guard：token_block_reason guard.py:327-365；admit_image 368-381；release 394-406
- nai：request 640-764（pick_token 657→420-469；槽 668-669；间隔 672；_dispatch_guard 683；POST 693-712；429/403/401 723-740；_settle 618-631）；image_stream 789-892（槽 829-832；错误 859-876）
- 取消/记账：complete_image_operation 383-400；refund_local_reject 442-456；main.settle_record 643-651；request_timing.py:82-102
- 并发：concurrency.py:9-75 AdjustableLimiter；TokenState.image_slots nai.py:103；set_image_concurrency nai.py:410-418
- 死代码：key_idle_reminders（database.py:217 CREATE、704 DELETE）；share_confirm（生产遗留）；app/core/state_holder.py（零导入）
