# 运维脚本（服务器上放在 /opt/backups/）

| 文件 | 用途 |
|---|---|
| `deploy.sh <版本号>` | 等队列空闲 → 备份数据库与 .env 到 `/opt/backups/pre-deploy-<时间>` → 重建容器 → 验证 |
| `nai-watch.sh` | 实时事件流（60 秒轮询）：成员、拒绝、报错、Bug、动态额度、自动驾驶、注册就绪度、跑图分享 |
| `patrol.py` | 只读体检（适合每 10 分钟跑一次）：系统 / 上游容量 / 注册就绪度 / 成员出图请求 / 数据一致性；退出码 0 正常 · 1 注意 · 2 危险；`--alert` 有问题时给站长发 Discord 告警（同一问题 30 分钟只发一次），`--oneline`/`--json` 给机器用 |
| `lab-daily.sh` | 实验室日报：`sqlite3 .backup` 出副本到 /tmp → `docker compose --profile lab run --rm lab`（回放校准 + 蒙特卡洛 + 日报图，结果在 `data/lab/<日期>/`）→ `post_report.py` 默认 dry-run；`LAB_POST=1` 才真的发到公告频道 |
| `post_report.py` | 把 `report.png` + `summary.md` 以 multipart 发到 `ANNOUNCE_CHANNEL_ID`（`allowed_mentions` 为空）；默认 `--dry-run` 只打印，`--send` 才发 |
| `announce.py` | 往公告频道发一条消息（正文从 stdin 读）：`docker cp announce.py nai-gate:/tmp/ && docker exec -i -u 0 nai-gate python /tmp/announce.py` |

## 7×24 监控（systemd）

`systemd/` 里是现成的单元，装一次就会随机器重启自动拉起：

```bash
sudo cp deploy/ops/systemd/*.service deploy/ops/systemd/*.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nai-watch.service      # 常驻：实时事件流 → journalctl -u nai-watch -f
sudo systemctl enable --now nai-patrol.timer       # 每 10 分钟体检 → journalctl -u nai-patrol
```

- 巡检状态存在 `/var/lib/nai-patrol/`（告警去重用），数据库只读打开，不写业务表。
- 告警通道用 `.env` 里的 `DISCORD_BOT_TOKEN` + `ALERT_USER_ID`（或 `ALERT_CHANNEL_ID`）；没配就只写 journal。
- 阈值都在 `patrol.py` 顶部的常量里；`PATROL_COOLDOWN` 控制同一问题的告警间隔（默认 1800 秒）。

只重启机器人：`docker compose --profile discord up -d --no-deps discord-bot`（不加 `--no-deps` 会连带重建网关）。
