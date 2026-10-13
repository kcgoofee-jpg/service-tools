#!/usr/bin/env bash
# 状态页独立镜像：把 /status 渲染成静态 HTML 推到 S3（每 5 分钟一次，cron）。
# 主站挂了，镜像仍在：http://nai-gate-status-<账号>.s3-website.us-east-2.amazonaws.com/
# 页面底部的快照时间停止更新超过 10 分钟，本身就是「主站故障」的信号。
set -euo pipefail
cd "$(dirname "$0")/.."                     # /opt/service-tools
export AWS_PROFILE=naigate-push
ACC=$(aws sts get-caller-identity --query Account --output text 2>/dev/null || true)
[ -n "$ACC" ] || { echo "$(date '+%F %T') 无法取 AWS 身份，跳过"; exit 0; }
BUCKET="nai-gate-status-${ACC}"

TMP=$(mktemp /tmp/status-mirror.XXXX.html)
trap 'rm -f "$TMP"' EXIT
curl -fsS -m 20 http://127.0.0.1:3003/status -o "$TMP"

# 注入镜像横幅：说明这是快照、正常 5 分钟一更
python3 - "$TMP" <<'PY'
import sys, datetime
p = sys.argv[1]
html = open(p, encoding="utf-8").read()
ts = datetime.datetime.now().strftime("%m-%d %H:%M")
banner = ('<div style="position:fixed;bottom:0;left:0;right:0;padding:5px 12px;background:#151a23;'
          'color:#8b93a3;font:12px/1.5 -apple-system,sans-serif;text-align:center;z-index:9999">'
          f'🔒 独立镜像 · 快照生成于 {ts}（北京时间），正常每 5 分钟自动更新；'
          '本页超过 10 分钟未更新 = 主站可能故障</div>')
html = html.replace("</body>", banner + "</body>")
open(p, "w", encoding="utf-8").write(html)
PY

aws s3 cp "$TMP" "s3://${BUCKET}/index.html" --cache-control "max-age=60" --content-type "text/html; charset=utf-8" --no-progress
echo "$(date '+%F %T') mirror pushed"
