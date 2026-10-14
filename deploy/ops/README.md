# 运维脚本

3.0 的运维命令都在 `nai-ops`（Rust，见 `crates/gate-ops`），服务器上的定时脚本在 `deploy/v3/ops/`。
部署、切换、回滚、巡检、备份的完整流程见 [docs/OPERATIONS.md](../../docs/OPERATIONS.md)。这里只放三类东西：

| 文件 | 在哪跑 | 用途 |
|---|---|---|
| `migrate_restore.sh` | 新服务器 | 灾难恢复：从 S3 拉最新加密备份 → 解密 → 恢复库、`.env`、3.0 部署文件与 Caddy 配置 → 用 CI 镜像启动 |
| `pull-ro-snapshot.sh` | 本机 | 拉每天的脱敏只读快照（`/opt/backups/ro/`）到本地，排查问题用 |
| `pull-backup-mac.sh` | 本机 | 把加密备份多拉一份到本机（异地第三份） |
| `systemd/nai-patrol.{service,timer}` | 服务器 | 每 5 分钟跑一次 `/opt/owl/nai-ops patrol --oneline --alert` |
| `SYSTEM-FACTS.md` | — | 系统事实速查（数字以代码为准） |

## 巡检（systemd）

```bash
sudo cp deploy/ops/systemd/nai-patrol.* /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nai-patrol.timer
journalctl -u nai-patrol -n 20 --no-pager
```

退出码 0 正常 / 1 注意 / 2 危险都算执行成功；3 = 巡检自身出错。**只有「危险」才私信站长**
（同一批危险项 6 小时一次，解除后补一条「已恢复」），「注意」只记日志，后台总览能看到。

## nai-ops 常用子命令

```bash
/opt/owl/nai-ops patrol                 # 完整体检报告（在 /opt/service-tools 下跑，读同一份 .env 和 data/）
/opt/owl/nai-ops snapshot <目标>         # 一致快照（备份脚本用）
docker exec owl-gate nai-ops incident list
docker exec owl-gate nai-ops incident open --title "出图接口例行维护" --status investigating
docker exec owl-gate nai-ops smoke http://127.0.0.1:8000
```

`announce` / `dm-broadcast` 默认只演练，`--send` 才真发；**不要批量私信**（Discord 应用曾被风控 680009）。
