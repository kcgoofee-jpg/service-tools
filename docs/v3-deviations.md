# 3.0 与 2.x 的有意差异（review 清单，上线后并入 CHANGELOG 3.0.0）

原则：额度 / 处罚 / 上游节奏 1:1；下面这些是**有意**改动，每条都写明原因。

| # | 位置 | 2.x 行为 | 3.0 行为 | 原因 |
|---|---|---|---|---|
| 1 | 上游 Retry-After | 超长数字（>4300 位）或越界日期使 `parse_retry_after` 抛异常，429 冷却没生效、请求 500 | 解析失败按默认 20 秒冷却 | 旧行为是 bug：被限流时反而不冷却 |
| 2 | `/public/my-export` | 必定 500（全局变量被误删） | 恢复 200 MB 上限 + 全站串行 + 每人 60 秒一次 | 线上 bug |
| 3 | 上游 TLS / HTTP2 指纹 | Python OpenSSL 指纹 + Chrome 请求头 | wreq 模拟 Chrome 129/128 指纹，请求头集合 / 顺序 / 取值不变 | 站长 10-14 批准 |
| 4 | 前端 CSP | `'unsafe-inline'` | `script-src 'self'; style-src 'self'`，首屏数据用 JSON 注入 | 安全加固 |
| 5 | 巡检 `patrol` | 查已删除的表崩溃，systemd 当成功 | 去掉死表检查；内部错误退出码 3 | 线上 bug |
| 6 | 问卷 | 粘贴 Key 识别 | 用成员 Discord 会话识别（仍兼容 Bearer Key） | 只留 Discord 登录 |
