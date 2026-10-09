#!/usr/bin/env bash
# 每日实验室：备份副本 → 跑 lab（回放校准 + 蒙特卡洛 + 日报图）→ 发帖（默认 dry-run，只打印）。
# 建议 cron（北京时间 00:20 总结前一天）：20 0 * * * bash /opt/service-tools/deploy/ops/lab-daily.sh >> /opt/backups/lab.log 2>&1
# 环境变量：
#   DAY=YYYY-MM-DD        报告日，默认昨天
#   LAB_ARGS="..."        额外参数，例如 "--sweep account_hourly_cap=100,120,150,180,200"
#   LAB_POST=1            真的发到 Discord（需要 DISCORD_BOT_TOKEN、ANNOUNCE_CHANNEL_ID，可从 .env 读）；默认只 dry-run
set -euo pipefail
ROOT=${ROOT:-/opt/service-tools}
cd "$ROOT"
DAY=${DAY:-$(TZ=Asia/Shanghai date -d yesterday +%F 2>/dev/null || TZ=Asia/Shanghai date -v-1d +%F)}
SNAP=$(mktemp /tmp/nai-lab-XXXXXX.db)
trap 'rm -f "$SNAP"' EXIT
chmod 644 "$SNAP"                      # 容器内用户 10001 需要能读

# 1) 在线一致性备份（不锁住网关写入）
if command -v sqlite3 >/dev/null; then
  sqlite3 data/nai_gate.db ".backup '$SNAP'"
else
  python3 -c "import sqlite3,sys; s=sqlite3.connect('data/nai_gate.db'); d=sqlite3.connect(sys.argv[1]); s.backup(d); d.close(); s.close()" "$SNAP"
fi
echo "snapshot $SNAP ($(du -h "$SNAP" | cut -f1))"

# 2) 跑 lab（结果写 data/lab/<DAY>/，data/ 不进 git）
mkdir -p data/lab
chown 10001:10001 data/lab 2>/dev/null || true
docker compose --profile lab run --rm -v "$SNAP:/snapshot/gate.db:ro" lab \
  --db /snapshot/gate.db --out /app/data/lab --day "$DAY" --replicates "${REPLICATES:-200}" --max-scale "${MAX_SCALE:-10}" ${LAB_ARGS:-}

# 3) 发帖：默认 dry-run；LAB_POST=1 才真的发
OUT="data/lab/$DAY"
if [ "${LAB_POST:-0}" = "1" ]; then
  export $(grep -E '^(DISCORD_BOT_TOKEN|ANNOUNCE_CHANNEL_ID)=' .env | xargs)
  python3 deploy/ops/post_report.py --dir "$OUT" --send
else
  python3 deploy/ops/post_report.py --dir "$OUT"
fi
