# 运维手册（3.0）

服务器上有两套目录：

| 目录 | 内容 |
|---|---|
| `/opt/service-tools` | 2.x 的检出（**不再更新**）：`.env`、`data/`（库、`secret_key`、上游 Token 存档）、Caddy 配置副本、旧版 cron 脚本。3.0 共用这里的 `.env` 和 `data/`。 |
| `/opt/owl` | 3.0：`compose.yml`（= 仓库 `deploy/v3/compose.yml`）和 `cutover.sh`（= `deploy/v3/cutover.sh`）。 |

3.0 只有一个镜像 `owl-gate`，三个进程：`owl-gate`（网关，`nai-gate serve`）、`owl-naimei`（奶妹，`nai-bots naimei`）、`owl-warden`（燕双鹰，`nai-bots warden`）。
2.x 的容器（`nai-gate`、`nai-gate-discord`、`nai-gate-warden`）只停不删，用来秒级回滚。

## 发布

镜像由 GitHub Actions 构建（推 `rewrite/v3` / `main` 时），**服务器不编译**（2 核）。服务器上不放任何 GitHub 凭据：

```bash
# 本机：下载最新一次 CI 构建的镜像附件
gh run download --repo kcgoofee-jpg/service-tools -n owl-gate-image -n nai-ops-linux-musl -D /tmp/owl-image
scp /tmp/owl-image/owl-gate-image/owl-gate.tar.gz /tmp/owl-image/nai-ops-linux-musl/nai-ops root@$OWL_HOST:/opt/owl/
```

```bash
# 服务器
/opt/owl/cutover.sh load /opt/owl/owl-gate.tar.gz   # 新镜像成为 owl-gate:current，上一版留作 :previous
/opt/owl/cutover.sh install-ops /opt/owl/nai-ops
cd /opt/owl && docker compose -p owl --env-file /opt/service-tools/.env -f compose.yml up -d
/opt/owl/cutover.sh status
```

网关收到停止信号后：停止接新请求 → 进行中的出图最多等 110 秒收尾记账 → 落盘状态码统计 → WAL checkpoint。
compose 的 `stop_grace_period` 是 120 秒，所以发布时进行中的图不会丢账。

## 从 2.x 切到 3.0（只做一次）

```bash
/opt/owl/cutover.sh up
```

依次：在线备份库到 `/opt/backups/pre-v3/` 并做完整性校验 → 停 2.x（最多等 120 秒）→ 起 3.0 → 健康检查 → `nai-ops smoke` 冒烟 →
巡检定时器改为 `docker exec owl-gate nai-ops patrol`。**任何一步失败都会自动回到 2.x。**

## 回滚

| 情况 | 命令 | 说明 |
|---|---|---|
| 3.0 新版本有问题 | `cutover.sh previous` | 回到上一版 3.0 镜像；坏的那版标成 `owl-gate:failed` |
| 3.0 整体有问题 | `cutover.sh rollback` | 停 3.0、启 2.x 原容器。库只增不减，2.x 能直接读（切换前已演练：3.0 写过的库 2.x 正常启动，3.0 签发的后台 Cookie 2.x 也认） |

## 巡检

`nai-patrol.timer` 每 5 分钟跑一次。3.0 下由 systemd drop-in（`/etc/systemd/system/nai-patrol.service.d/v3.conf`）改为宿主机上的
`/opt/owl/nai-ops patrol`（CI 产出的静态二进制 `nai-ops-linux-musl`，用 `cutover.sh install-ops` 安装；巡检要看宿主机的 docker、磁盘和 .env）：

- 退出码 0 正常 / 1 注意 / 2 危险：都算执行成功，告警由巡检自己发；
- 退出码 3 = 巡检自身出错（以前崩溃也被当成成功，看不出来）。

```bash
journalctl -u nai-patrol -n 20 --no-pager
cd /opt/service-tools && /opt/owl/nai-ops patrol   # 手动跑一次，完整报告
```

## 备份与快照

cron（root）跑的都是 `/opt/owl/ops/` 里的脚本（= 仓库 `deploy/v3/ops/`），快照由宿主机的 `nai-ops` 做，不经过网关容器：

| 时间 | 脚本 | 内容 |
|---|---|---|
| 每晚 04:30 | `nightly_backup.sh` | 一致快照（不含原图）+ `.env` / `secret_key` / 上游 Token 存档 / 3.0 部署文件 / Caddy 配置，客户端 AES-256 加密后推 S3（60 天过期） |
| 每天 05:10 | `ro-snapshot.sh` | 脱敏只读快照 `/opt/backups/ro/owl-ro-YYYYMMDD.db.gz`（保留 7 份），本机用 `deploy/ops/pull-ro-snapshot.sh` 拉取 |
| 每 5 分钟 | `status_mirror.sh` | 状态页静态镜像推 S3（主站挂了镜像仍在） |

`/usr/local/bin/naigate-snapshot` 是这几处共用的快照入口（`nai-ops snapshot`）。恢复见 `deploy/ops/migrate_restore.sh`。

## 常用命令

```bash
docker logs --since 1h owl-gate                     # 网关日志（不含 Key、提示词、IP）
docker exec owl-gate nai-ops incident list          # 状态页事件
docker exec owl-gate nai-ops incident open --title "…" --status investigating
docker exec owl-gate nai-ops smoke http://127.0.0.1:8000
curl -s http://127.0.0.1:3003/healthz
```

## 安全红线

- 仓库是公开的：不提交 `.env`、数据库、快照、成员数据；夹具只用合成数据（CI 有 gitleaks 和 `check-fixtures.sh`）。
- 不批量私信（Discord 应用曾被风控 680009）；`dm-broadcast` 默认只演练，`--send` 才真发。
- 上游请求的指纹 / 节奏改动要站长点头。
