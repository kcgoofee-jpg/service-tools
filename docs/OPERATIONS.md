# 运维手册（3.0）

服务器上有两套目录：

| 目录 | 内容 |
|---|---|
| `/opt/service-tools` | 2.x 的检出（**不再更新**）：`.env`、`data/`（库、`secret_key`、上游 Token 存档）、旧版 cron 脚本。3.0 共用这里的 `.env` 和 `data/`。 |
| `/opt/owl` | 3.0：`compose.yml`（= 仓库 `deploy/v3/compose.yml`）、`cutover.sh`（= `deploy/v3/cutover.sh`）、宿主机用的运维命令 `nai-ops`、定时脚本 `ops/`（= 仓库 `deploy/v3/ops/`）。 |
| `/opt/caddy` | `Caddyfile`（= 仓库 `deploy/Caddyfile.production`）。 |

3.0 只有一个镜像 `owl-gate`，两个进程：`owl-gate`（网关，`nai-gate serve`）、`owl-naimei`（奶妹，`nai-bots naimei`）。
燕双鹰已删除（它什么都不做）；2.x 的容器（`nai-gate`、`nai-gate-discord`、`nai-gate-warden`）只停不删，用来秒级回滚。

## 发布

镜像由 GitHub Actions 构建（只在推 `main` 时出镜像和 `nai-ops`；PR 只跑检查），**服务器不编译**（2 核）。
镜像不推任何镜像仓库，作为 tar 附件由本机下载后 `scp` 过去、在服务器上 `docker load`（不 pull）；服务器上不放任何 GitHub 凭据：

```bash
# 本机：下载最新一次 CI 构建的镜像附件
gh run download --repo kcgoofee-jpg/service-tools -n owl-gate-image -n nai-ops-linux-musl -D /tmp/owl-image
scp /tmp/owl-image/owl-gate-image/owl-gate.tar.gz /tmp/owl-image/nai-ops-linux-musl/nai-ops root@$OWL_HOST:/opt/owl/
```

```bash
# 服务器
/opt/owl/cutover.sh load /opt/owl/owl-gate.tar.gz   # 新镜像成为 owl-gate:current，上一版留作 :previous
/opt/owl/cutover.sh install-ops /opt/owl/nai-ops
cd /opt/owl && docker compose -p owl --env-file /opt/service-tools/.env -f compose.yml up -d --remove-orphans
/opt/owl/cutover.sh status
```

`--remove-orphans` 会顺手删掉 compose 里已经没有的容器（比如已删除的 `owl-warden`）；手动 `docker rm -f owl-warden` 也行。
网关收到停止信号后：停止接新请求 → 进行中的出图最多等 110 秒收尾记账 → 落盘状态码统计 → WAL checkpoint。
compose 的 `stop_grace_period` 是 120 秒，所以发布时进行中的图不会丢账。

仓库里的配置文件**不会自动同步到服务器**：改了 `deploy/Caddyfile.production`（或 `compose.yml`、`ops/` 里的脚本）之后，要手动拷到服务器对应位置
（Caddyfile → `/opt/caddy/Caddyfile`，然后 `docker exec caddy caddy reload --config /etc/caddy/Caddyfile`）。

## 从 2.x 切到 3.0（只做一次）

```bash
/opt/owl/cutover.sh up
```

依次：在线备份库到 `/opt/backups/pre-v3/` 并做完整性校验 → 停 2.x（最多等 120 秒）→ 起 3.0 → 健康检查 → `nai-ops smoke` 冒烟 →
写巡检的 systemd drop-in（宿主机上的 `/opt/owl/nai-ops patrol`，检查 3.0 的容器）。**任何一步失败都会自动回到 2.x。**

## 回滚

| 情况 | 命令 | 说明 |
|---|---|---|
| 3.0 新版本有问题 | `cutover.sh previous` | 回到上一版 3.0 镜像；坏的那版标成 `owl-gate:failed` |
| 3.0 整体有问题 | `cutover.sh rollback` | 停 3.0、启 2.x 原容器。库只增不减，2.x 能直接读（切换前已演练：3.0 写过的库 2.x 正常启动，3.0 签发的后台 Cookie 2.x 也认） |

回滚到 2.x 时，巡检的 drop-in 会把检查的容器改成 2.x 的（`NAI_GATE_CONTAINERS=nai-gate,nai-gate-discord,caddy`）；
`up` / `previous` 时恢复默认（`owl-gate,owl-naimei,caddy`）。不改的话巡检会把「owl-* 没在跑」当成危险，每 6 小时私信站长一次。

## 巡检

`nai-patrol.timer` 每 5 分钟跑一次宿主机上的 `/opt/owl/nai-ops patrol --oneline --alert`
（CI 产出的静态二进制 `nai-ops-linux-musl`，用 `cutover.sh install-ops` 安装；巡检要看宿主机的 docker、磁盘和 `.env`）。
单元文件在仓库 `deploy/ops/systemd/`：

```bash
sudo cp deploy/ops/systemd/nai-patrol.* /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now nai-patrol.timer
```

- 退出码 0 正常 / 1 注意 / 2 危险：都算执行成功；退出码 3 = 巡检自身出错。
- **只有「危险」才私信站长**（同一批危险项 6 小时提醒一次，出现新的立刻发，解除后补一条「已恢复」）；
  「注意」只记在巡检日志里，后台总览的「有 N 件事要处理」能看到同样的情况。网关自己的告警同理：
  只有上游 Token 失效 / 连续 403 / Token 被改动 / 磁盘不足 10% 会私信，其余只写日志。
- 默认检查的容器 `owl-gate,owl-naimei,caddy`（第一个是网关），环境变量 `NAI_GATE_CONTAINERS` 可改。

## 备份与快照

快照由宿主机的 `nai-ops` 做（`/usr/local/bin/naigate-snapshot` 是共用入口），不经过网关容器，不锁库。恢复见 `deploy/ops/migrate_restore.sh`。

| 时间 | 脚本 | 内容 |
|---|---|---|
| 每晚 03:30 | `naigate-backup`（`/etc/cron.d/naigate-backup`，`deploy/harden.sh` 装的） | 本机 GFS 轮换：每日 14 份、每周日 8 份、每月 1 号 6 份，放在 `/opt/backups/{daily,weekly,monthly}/nai_gate-*.db.gz` |
| 每晚 04:30 | `nightly_backup.sh` | 一致快照（不含原图）+ `.env` / `secret_key` / 上游 Token 存档 / 3.0 部署文件 / Caddy 配置，客户端 AES-256 加密后推 S3（60 天过期）；口令在服务器 `/root/.backup_pass`，务必在服务器之外另存一份 |
| 每天 05:10 | `ro-snapshot.sh` | 脱敏只读快照 `/opt/backups/ro/owl-ro-YYYYMMDD.db.gz`（保留 7 份），本机用 `deploy/ops/pull-ro-snapshot.sh` 拉取 |
| 每 5 分钟 | `status_mirror.sh` | 状态页（HTML + 外置的样式 / 脚本）静态镜像推 S3，主站挂了镜像仍在；页面快照时间停了超过 10 分钟 = 主站可能故障 |

后三条是 root 的 crontab，原文在 `deploy/v3/ops/crontab`。本机还可以用 `deploy/ops/pull-backup-mac.sh` 把加密备份多拉一份（异地第三份）。

## 改密钥

3.0 和 2.x 共用 `/opt/service-tools/.env`（`NAI_TOKENS`、`DISCORD_BOT_TOKEN`、`REGISTRATION_BRIDGE_SECRET`、`ADMIN_PASSWORD` 等都在这里）。
改完要让 3.0 的三个容器重新读取：

```bash
nano /opt/service-tools/.env      # 或其它编辑器；保持 600 权限，值里不要有 $、引号、空格
cd /opt/owl && docker compose -p owl --env-file /opt/service-tools/.env -f compose.yml up -d
/opt/owl/cutover.sh status
```

`up -d` 只重建配置变了的容器；网关重启时进行中的出图照常收尾记账（见「发布」）。
**不要**在 `/opt/service-tools` 下执行 `docker compose up`：那里是 2.x 的检出，会把 2.x 的容器拉起来。
`ADMIN_PASSWORD` 只作初始 / 恢复用：后台改过密码的话以库里保存的为准。

## 配置：只有两层

- **环境变量**（`/opt/service-tools/.env`）：只放密钥和基础设施（上游 Token、Discord Token / ID、站点地址、数据目录、网络……），清单见仓库 `.env.example`；改完怎么生效见上面「改密钥」。
- **后台设置**（`site_settings`，在后台页面里改，不用重启）：额度、名额、新成员默认值、有效期、记录开关、账号保护、排队与冷却、Anlas 预算……没设过的项用代码里的默认值，逐项说明见 `deploy/ops/SYSTEM-FACTS.md`。「额度与保护」页管账号保护（含每小时上限自动调整的下限 / 上限 / 步长）、排队与冷却、Anlas 与 V5 预算（含 Vibe 每日 Anlas 池）、动态额度；「领 Key」页管开放、名额、新成员默认额度 / 有效期 / 功能、限定身份组、生成记录开关。后台手动新建 Key 和一键邀请 Key 读的是「领 Key」页同一份「新成员默认」。
- **没有第三层**：2.x 的私密参数文件 `data/private_params.json` 3.0 不再读（文件留在原处，备份脚本照旧带着它，只是为了能回滚到 2.x）。
- **代码常量**：时区 Asia/Shanghai、服务日 07:00 重置、标签补全并发 1、无效 Key 防爆破和后台登录限流的几个数、新 Key 的每分钟请求数等，没有后台入口也没有环境变量，要改只能改代码（`gate-core/src/clock.rs`、`gate-server/src/env.rs`、`gate-server/src/defaults.rs`）。

线上 `.env` 里残留的 `DEFAULT_*`、`REGISTER_*`、`AUDIT_*`、`DISCORD_ROLE_ID`、`SEED_DEMO_KEY`、`TZ`、`DAY_RESET_HOUR`、`GLOBAL_CONCURRENCY`、`GLOBAL_MONTHLY_ANLAS`、`GLOBAL_DAILY_ANLAS`、`QUEUE_TIMEOUT`、`IMAGE_MIN_INTERVAL`、`IMAGE_429_COOLDOWN_SECONDS`、`AUTH_FAIL_*`、`LOGIN_*`、`ALLOW_IMG2IMG`、`SAFE_CLAMP`、`MAX_PIXELS`、`MAX_STEPS`、`ALERT_CHANNEL_ID`、`ALERT_WEBHOOK_URL` 3.0 不再读，可以删掉。删之前先确认后台设置里已经有你要的值——比如名额 `register_max_users`、新领 Key 的有效天数 `register_expires_days`：库里没有这一行时，3.0 用代码默认（10 / 不过期），不再回落到 `.env`。回滚到 2.x 时 2.x 仍会读这些变量。

## 常用命令

```bash
docker logs --since 1h owl-gate                     # 网关日志（不含 Key、提示词、IP）
docker logs --since 1h owl-naimei                   # 奶妹日志（出错会带 [bug] 前缀）
journalctl -u nai-patrol -n 20 --no-pager           # 巡检日志
cd /opt/service-tools && /opt/owl/nai-ops patrol    # 手动跑一次，完整报告（读同一份 .env 和 data/）
/opt/owl/nai-ops snapshot <目标.db.gz>               # 一致快照（备份脚本用）
docker exec owl-gate nai-ops smoke http://127.0.0.1:8000   # 线上冒烟（不出图）；地址必填，也可设环境变量 SITE_URL
curl -s http://127.0.0.1:3003/healthz
```

状态页事件（发布 / 更新 / 解决）在后台「系统」页的「状态页事件」里操作；`nai-ops` 里没有 `incident`、`announce`、`dm-broadcast`、`prompt-stats`（已删除）。

## 安全红线

- 仓库是公开的：不提交 `.env`、数据库、快照、成员数据；夹具和测试只用合成数据（CI 有 gitleaks 和 `check-fixtures.sh`）；文档里也不写真实 Discord ID、App ID、IP、域名。
- **不私信成员**：机器人没有任何私信出口，不群发、不批量发消息；给站长的告警只在重大事故时发（上面「巡检」一节）。
- 上游请求的指纹 / 节奏改动要站长点头。

### 平台 / 第三方依赖中断预案（通用）

2026-10-10 Discord 把奶妹应用判为 Application Growth Disabled（错误码 680009），冻结了一切新的 OAuth 授权，
新用户领 Key 的两条路（网页「用 Discord 登录」和 `/register`）都走 OAuth，所以约 7.5 小时没有新增，老用户的 Key 和出图不受影响。
三个触发信号：网页登录刚上线就在一小时内有 ~43 人授权一个新应用并申请了敏感权限 `guilds.members.read`；机器人给每个新用户私信发 Key；
画廊频道里机器人自动点赞 + AI 评论。整改是：OAuth 只要 `identify`；`/register` 不走 OAuth，从斜杠命令的 interaction 直接发 Key，只在本人可见的临时消息里给；
不私信；关掉所有自动互动。处置记录之外，留下下面这些通用原则：

1. **核心功能（领 Key、出图）不要单点依赖任何可被第三方一键掐断的授权链路。** 斜杠命令的 interaction 本身就证明了身份，能不发起新授权就不发起。
2. **任何自动化群发 / 群互动默认关、能一键关。** 私信、自动点赞、自动评论、@全体……出事要能在 1 分钟内停。（现在的做法更彻底：这些出口都删掉了。）
3. **最小权限**：OAuth scope、Bot 权限、Intent 只要真正用到的（奶妹现在只有 GUILDS）；能用 Bot Token 核验的就别用用户授权。
4. **双通路**：面向用户的关键动作尽量有「不依赖该平台某项特性」的备用路。
5. **出事响应顺序**：先停掉可疑自动化（止血）→ 查根因（后台操作日志里的失败记录、容器日志）→ 上线绕开故障的修复，优先恢复核心 →
   向该平台正式申诉 → 发公告安抚用户。
6. **申诉要点**：小型非商业自用、成员自愿、已关闭所有自动化、已移除群发私信、权限收到最小、带 App ID 和错误码；一次性提交，别重复。
7. 申诉通过之前保持自动化全关；通过之后即使要恢复某项互动，也要限频 + 限总量 + 一键可关。
