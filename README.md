# NAI Gate

一个自托管的 **NovelAI 公益分发网关**：站长在服务端配置上游 Token，给受邀成员发放各自独立的虚拟 Key（`nai-…`），成员用常用的 NovelAI 客户端接入，**从不接触上游 Token**。站长通过后台控制谁能用什么、用多少，并能看到用量。适合十几个人以内的小圈子共享一个账号。

> 本仓库是 [fangchen2003/service-tools](https://github.com/fangchen2003/service-tools)（NAI Gate）的修改版，在原项目基础上增加了 Discord 自助领取、功能开关、告警、成员落地页、重新设计的后台和一批安全加固。原项目未声明开源许可证，使用与再分发前请自行确认。

> ⚠ 共享账号仍有被上游限流或封禁的风险；本项目的限额与冷却只能降低风险，不能保证账号安全。请自行确认使用方式符合 NovelAI 的条款，使用专用小号，并妥善保管 `.env` 和数据库。

## 功能一览

- **虚拟 Key**：每个成员一把，可单独设置每日图片 / V5 / Anlas 额度、请求频率、有效期、可用模型（仅 V4.5 及以下，或含 V5）和**功能权限**。
- **功能开关**：文生图、放大、导演工具、Vibe 编码、标签补全、文本 / OpenAI 兼容聊天、语音合成，既能全局开关，也能按 Key 授权；新自助注册的成员默认只开通文生图。
- **安全的免费档**：未开通 Anlas 的 Key 会被自动限制在免费规格内（单张、默认 ≤28 步、≤1024×1024、关闭 SMEA），不会消耗付费点数。
- **Discord 自助领取**：成员在服务器里输入 `/register`，授权后 Key 私信发给本人；名额先到先得，Key 到期自动释放名额，可再次领取。
- **后台**（`/admin`）：总览、成员用量、生成记录、密钥管理、用量日志、功能与开放、设置，亮暗双主题，手机可用。
- **成员落地页**（`/`）：实时上游状态、注册名额、三步接入教程、输入 Key 查额度（Key 只在浏览器里使用，不保存）。
- **告警**：上游 Token 失效 / 限流 / 故障、V5 额度低、磁盘快满、有人刷无效 Key，通过 Discord 私信、频道或 Webhook 通知站长，同类事件有冷却不刷屏。
- **防护**：单 IP 无效 Key 临时拦截、后台登录限流、请求体与响应大小限制、并发与排队保护上游账号。
- **生成记录（可选，默认关）**：保存图片提示词和小缩略图用于防滥用，自动过期；开启后会在首页、`/help`、领 Key 私信里向成员明示。

## 架构

```
客户端(柏宝绘等) ──HTTPS──▶ Caddy ──▶ NAI Gate (FastAPI, 127.0.0.1:3003) ──▶ NovelAI
                                        ▲        │
Discord 用户 ──/register──▶ Discord Bot ─┘(桥接密钥)└─▶ SQLite (data/nai_gate.db)
```

网关只监听本机，对外只有反向代理的 80/443。Discord 机器人是独立容器，只持有它需要的几个环境变量，拿不到上游 Token。

## 快速部署（Docker Compose）

需要一台能访问 `image.novelai.net`、`text.novelai.net` 的海外服务器（大陆机器通常不通）、一个域名、Docker 与 Docker Compose。

```bash
git clone https://github.com/<你的账号>/service-tools.git && cd service-tools
cp .env.example .env
# 编辑 .env：至少设置强 ADMIN_PASSWORD 和真实 NAI_TOKENS
docker compose up -d --build
curl http://127.0.0.1:3003/healthz        # {"ok":true,"upstream":true}
```

`docker-compose.yml` 已把端口绑定为 `127.0.0.1:3003`，不会直接暴露公网。接着用 Caddy 配置 HTTPS（见 [deploy/Caddyfile.example](deploy/Caddyfile.example)），把域名的 A 记录指向服务器，打开 `https://你的域名/admin` 登录。后台默认只接受 HTTPS Cookie。

- 使用 Nginx 时必须保留原始 Host：`proxy_set_header Host $host;`（后台的跨站防护依赖它）；Caddy 默认已保留。
- Cloudflare：把记录设为**仅 DNS（灰云）**。开启代理会对非浏览器请求触发人机验证，导致客户端连不上；如需代理，务必对 `/ai/*`、`/v1/*`、`/user/*` 放行。
- 服务器基础加固（防火墙、SSH 仅密钥、fail2ban、自动更新、交换分区、每日备份）可参考 [deploy/harden.sh](deploy/harden.sh)，运行前确认你已能用密钥登录。

## 站长怎么用

1. 在后台“功能与开放”确认注册开关、名额上限、新成员默认额度与功能；在“密钥管理”可手动创建或调整单个 Key。
2. 把站点地址发给成员（或开启 Discord 自助领取，见下）。
3. “成员”页看每个人今日 / 近 7 天 / 累计用量；“用量日志”和“生成记录”用来排查问题。
4. “总览”看上游 Token 池、V5 额度和 Anlas 核对；“设置”调整排队超时、图片任务间隔与上游 429 冷却。

多把上游 Token 用英文逗号写入 `NAI_TOKENS`。`NAI_TOKEN_ALLOW_ANLAS=1,0` 按顺序指定哪些 Token 允许付费；`NAI_TOKEN_V5_DAILY_LIMITS=0,150` 设初始免费 V5 日限额（`0` 不限）。后台保存的设置优先于环境变量。

## Discord 自助领取（可选）

1. 在 [Discord 开发者后台](https://discord.com/developers/applications) 新建应用：
   - **OAuth2 → Redirects** 添加 `https://你的域名/self-register/callback`；
   - **Bot** 页：复制 Token，**关闭 Public Bot**，三个 Privileged Intent 全部**关闭**（本机器人只用斜杠命令，不需要）。
2. 邀请机器人进服务器（scope：`bot`、`applications.commands`）。机器人**日常只需要**：查看频道、发送消息、嵌入链接、阅读消息历史；若启用“领到 Key 自动挂身份组”，再加**管理身份组**，并把机器人的角色排在 King 之下、目标身份组之上。
3. 在 `.env` 里配置（完整说明见 [.env.example](.env.example)）：

   ```
   DISCORD_CLIENT_ID=…  DISCORD_CLIENT_SECRET=…  DISCORD_BOT_TOKEN=…
   REGISTRATION_BRIDGE_SECRET=<随机长字符串>  DISCORD_GUILD_ID=<服务器ID>  SITE_URL=https://你的域名
   # 可选：DISCORD_MEMBER_ROLE_ID（领到 Key 自动挂身份组）、DISCORD_INVITE_URL、ANNOUNCE_CHANNEL_ID、ALERT_USER_ID
   REGISTER_MAX_USERS=10  REGISTER_EXPIRES_DAYS=7  REGISTER_MIN_ACCOUNT_DAYS=7
   ```
4. 启动机器人：`docker compose --profile discord up -d --build`。

**机制**：先到先得；名额只统计“启用且未过期”的 Key，到期自动释放，原成员可再次 `/register`；Discord 账号需注册满 `REGISTER_MIN_ACCOUNT_DAYS` 天；每个 Discord 账号同一时间一把 Key。设置了 `DISCORD_ROLE_ID` 才会额外要求某个身份组。

| 命令 | 谁能用 | 作用 |
| --- | --- | --- |
| `/register` | 所有人 | 领取 Key（授权后私信发送） |
| `/quota` | 已领取者 | 今日额度、模型范围、已开通功能、剩余有效期 |
| `/resetkey` | 已领取者 | 重置 Key，旧 Key 立即失效 |
| `/status` | 所有人 | 上游是否正常、是否在限流冷却 |
| `/help` | 所有人 | 使用说明（含记录声明） |
| `/slots` `/open` `/limit` `/grant` `/audit` `/revoke` | 管理员（“管理服务器”权限） | 查看名额、开关注册、改名额上限、给成员开关功能、开关生成记录、撤销某人的 Key |

后台和机器人读取同一份运行时设置，**改完立刻生效**，不需要重启。开启 / 关闭生成记录、开放 / 暂停注册、调整名额时，会在 `ANNOUNCE_CHANNEL_ID` 频道自动发公告。

## 用户怎么接入

所有生成请求使用 `Authorization: Bearer nai-…`。支持自定义 NovelAI API 地址的客户端填写站点根地址，例如 `https://你的域名`；OpenAI 兼容的文本客户端填写 `https://你的域名/v1`。

| 用途 | 接口 |
| --- | --- |
| 图片生成 / 流式预览 | `POST /ai/generate-image` / `POST /ai/generate-image-stream` |
| 放大、导演工具、Vibe 编码、标签补全 | `POST /ai/upscale`、`/ai/augment-image`、`/ai/encode-vibe`、`/ai/generate-image/suggest-tags` |
| NovelAI 文本 / 语音 | `POST /ai/generate-stream` / `POST /ai/generate-voice` |
| OpenAI 兼容文本 | `GET /v1/models`、`POST /v1/chat/completions` |
| 当前 Key 的额度与功能 | `GET /v1/me` |
| 上游状态等公开信息 | `GET /public/status` |

图片请求需要带完整的 `parameters` 对象（V4/V4.5 还需 `v4_prompt` 结构，参考官方前端的请求格式）。图片流通过 `parameters.stream` 选择 `sse` 或 `msgpack`。是否可用某项功能取决于站长授权，被拒时返回 403 并说明原因。

## 默认限制（新 Key，站长可调）

| 项目 | 环境变量默认值 |
| --- | --- |
| V4.5 及以下免费生图 | 每 Key 每天 100 张（自助领取默认 300，实际受间隔限制） |
| 免费 V5 | 每 Key 每天 50 张（自助领取默认 0，可调）；全站每天 150 张 |
| Anlas（须单独授权） | 每 Key 每天 100、每月 2500 |
| 请求频率 | 每 Key 每分钟 10 次 |
| 图片任务间隔 | 每个用户 Key 与每把上游 Token 各至少 15 秒 |

上游图片请求触发 429 后，全站图片生成默认冷却 60 秒。详细规则见 [用户限制说明.md](用户限制说明.md)。

## 安全与隐私要点

- 管理后台：会话 Cookie 绑定管理员密码摘要（改密码即令旧会话失效），所有写操作校验 `Origin`，登录有限流，示例密码 `changeme-please` 会被拒绝登录。
- 首页公告以沙箱化 iframe 展示，其中的脚本不会执行；落地页使用严格 CSP 且不含后台入口。
- 对反向代理：容器内设置了 `FORWARDED_ALLOW_IPS=*`，**仅因为端口只绑定在 127.0.0.1**，只有本机反向代理能连上；若你改成对外暴露端口，请同时收紧该变量。
- 无效 Key 请求按真实访客 IP 计数，超过阈值（`AUTH_FAIL_MAX` 等）会被临时拦截并告警。
- 开启生成记录会保存成员的提示词与缩略图：请确保向成员明示，并设置合理的保留天数；图片本体不保存。
- 备份运行中的 SQLite 数据库请用 SQLite 的在线备份（`sqlite3 … ".backup"`），不要直接复制文件。

## 配置与测试

完整环境变量见 [.env.example](.env.example)。`ADMIN_PASSWORD`、`NAI_TOKENS` 必须自行设置，不要提交 `.env`。

```bash
python -m venv .venv && .venv/bin/pip install -r requirements.txt pytest pytest-asyncio pytest-timeout
.venv/bin/python -m pytest -q
```

测试使用假上游，不消耗真实 NovelAI 额度。
