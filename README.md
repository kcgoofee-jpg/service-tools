# 猫头鹰公益站（NAI Gate 3.0）

自托管的 **NovelAI 公益分发网关**：站长在服务端配置上游 Token，成员在 Discord 里自助领取各自的虚拟 Key（`nai-…`），
用常用的 NovelAI 客户端接入，**从不接触上游 Token**。网关负责鉴权、排队、额度、参数钳制、记账和账号保护。

> 本仓库源自 [fangchen2003/service-tools](https://github.com/fangchen2003/service-tools)，3.0 起整体用 Rust + TypeScript 重写。
> 原项目未声明开源许可证，使用与再分发前请自行确认。
>
> ⚠ 共享账号仍有被上游限流或封禁的风险；本项目的限额与冷却只能降低风险，不能保证账号安全。请自行确认使用方式符合 NovelAI 的条款。

## 组成

```
客户端（柏宝绘 / Launcher 等 NovelAI 客户端）──HTTPS──▶ Caddy ──▶ nai-gate ──(Chrome TLS 指纹)──▶ NovelAI
浏览器（首页 / 后台 / 状态页）               ──HTTPS──▶ Caddy ──▶ nai-gate ──▶ SQLite（data/nai_gate.db）
Discord（/register 等）──▶ nai-bots naimei ──(桥接密钥)──▶ nai-gate
```

一个镜像、三个二进制：

| 二进制 | 作用 |
|---|---|
| `nai-gate` | 网关：成员接口（出图 / 流式 / 工具 / 标签补全 / 查询自己的额度）、首页与实时看板、后台、状态页、Discord 登录、定时任务（动态额度、上游快照、每小时上限调整、熔断、单 Key 限流、维护、闲置回收）。 |
| `nai-bots` | Discord 机器人 `naimei`（奶妹）：`/register` 领 Key、`/quota` 看额度、`/resetkey` 换 Key、`/help`、`/反馈`。不私信、不读频道消息、没有管理员命令；只拿自己需要的变量，拿不到上游 Token。 |
| `nai-ops` | 运维：巡检（含队列 / Anlas / 原图 / 证书等扩展检查）、`notify`（状态变化才私信站长）、`set` / `get`（走后台接口改设置）、`report`（只读周报）、`traffic`（Caddy 访问日志汇总）、快照、脱敏快照、冒烟。 |

代码结构：

| 目录 | 内容 |
|---|---|
| `crates/gate-core` | 纯逻辑（无 I/O）：参数策略、额度算法、自动驾驶、账号保护、流协议、Python 语义（舍入、JSON 序列化）。 |
| `crates/gate-db` | 唯一写 SQL 的地方：建表迁移（只增不减，可回滚到 2.x）、各类查询。 |
| `crates/gate-upstream` | NovelAI 客户端：wreq 模拟 Chrome TLS / HTTP2 指纹、令牌池、出图间隔、取消安全的结算。 |
| `crates/gate-server` | 网关本体（axum）：路由、鉴权、准入与额度预留、记账、后台任务、关停。 |
| `crates/gate-discord` / `gate-ops` | 机器人 / 运维命令。 |
| `crates/gate-testkit` | 测试用的假 NovelAI 上游。 |
| `web/` | 前端：Vite + TypeScript + Preact（首页、问卷、后台 8 个分区），严格 CSP、亮暗主题、无障碍对比度 ≥ 4.5:1。 |

## 部署

镜像由 GitHub Actions 构建（`.github/workflows/ci.yml`），服务器只加载、不编译。完整流程（发布、切换、回滚、巡检、备份）见
[docs/OPERATIONS.md](docs/OPERATIONS.md)。最少需要：

1. 一台能访问 `image.novelai.net` 的服务器、一个域名、Docker 与 Caddy（示例配置 `deploy/Caddyfile.example`，与 `compose.yml` 的 `service-tools_edge` 网络配套）。
2. `.env`：参照 `.env.example`，至少设置强 `ADMIN_PASSWORD` 和 `NAI_TOKENS`；Discord 自助领取另需 `DISCORD_*` 与 `REGISTRATION_BRIDGE_SECRET`。
3. `deploy/v3/compose.yml` + 镜像 `owl-gate:current`，`docker compose up -d`；`curl http://127.0.0.1:3003/healthz` → `{"ok":true,...}`。

## 成员怎么接入

在 Discord 里 `/register` 领 Key，然后在客户端里填：接口地址填站点域名（NovelAI 客户端）；
Key 填领到的 `nai-…`。首页用 Discord 登录后能看到自己的额度和排队位置。

## 开发

```bash
cargo xtask dev                             # 本地全套：网关 :18000 + 假 NovelAI 上游 + 合成演示数据（target/owl-dev）
cd web && pnpm vite                         # 前端热更新 :5173，接口代理到上面的网关
```

提交前：

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                      # 单元、快照、手写规则测试、集成（假上游）
cd web && pnpm biome check . && pnpm tsc --noEmit && pnpm vitest run
cd web && E2E_BASE=http://localhost:5174 pnpm exec playwright test   # 先跑 cargo xtask dev 和 vite --port 5174
tools/ci/check-fixtures.sh                  # 公开仓库红线：夹具只许合成数据
```

`web/e2e/layout.spec.ts` 是排版硬规则：各宽度下不许出现「最后一行只剩一两个字」、同一行控件必须等高对齐。
快照测试：输入与当前输出存在各 crate 的 `tests/snapshots/**/*.jsonl`，一行一例。行为有意改动时跑
`UPDATE_SNAPSHOT=1 cargo test -p <crate> --test <名字>` 重写快照，在 PR 里写明改了哪些快照、为什么；CI 不许更新快照。
行为改动同时写进 [CHANGELOG.md](CHANGELOG.md)。2.x 的 Python 代码和上线期间的逐位对拍夹具在 git 标签 `py-final`、
`v3-deviations-final` 里。

## 文档

- [docs/OPERATIONS.md](docs/OPERATIONS.md)：运维手册
- [docs/用户限制说明.md](docs/用户限制说明.md)：给成员看的限制说明
- [deploy/ops/SYSTEM-FACTS.md](deploy/ops/SYSTEM-FACTS.md)：系统事实清单（额度、账号保护、告警、Key 的一生，数字以代码为准）
- [docs/research/novelai-v5-官方资料整理.md](docs/research/novelai-v5-官方资料整理.md)：NovelAI V5 官方资料整理（额度算法的依据）
- [CHANGELOG.md](CHANGELOG.md)：更新日志（3.0 相对 2.x 的改动按主题汇总）

## 安全与隐私

- 上游 Token 只在服务端；成员 Key 只在请求头里，日志不记 Key、不记 IP 明文。
- 后台写接口校验来源（Origin）并记操作日志；后台与成员会话 Cookie 均为 HttpOnly + HMAC 签名。
- 生成记录（提示词 / 原图）默认关闭，开启后会在首页和领 Key 回复里向成员明示。
- 仓库是公开的：不提交 `.env`、数据库、快照与任何成员数据；CI 有 gitleaks 与夹具检查。
