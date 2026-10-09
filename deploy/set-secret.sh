#!/usr/bin/env bash
# 安全地更新 .env 里的某个密钥：输入不回显、不进入 shell 历史，更新后自动重启服务。
# 用法（在服务器项目目录里）：bash deploy/set-secret.sh NAI_TOKENS
# 远程一条命令：ssh -t root@服务器 "cd /opt/service-tools && bash deploy/set-secret.sh DISCORD_BOT_TOKEN"
set -euo pipefail
cd "$(dirname "$0")/.."
name="${1:?用法: bash deploy/set-secret.sh <变量名>   例如 NAI_TOKENS}"
case "$name" in
  NAI_TOKENS|DISCORD_CLIENT_ID|DISCORD_CLIENT_SECRET|DISCORD_BOT_TOKEN|REGISTRATION_BRIDGE_SECRET|ALERT_WEBHOOK_URL|ADMIN_PASSWORD) ;;
  *) echo "不支持的变量：$name"; exit 1 ;;
esac
read -rsp "输入新的 $name（输入不会显示）: " value; echo
[ -n "$value" ] || { echo "不能为空"; exit 1; }
if [ "$name" = REGISTRATION_BRIDGE_SECRET ] && [ "${#value}" -lt 32 ]; then echo "桥接密钥至少 32 个字符"; exit 1; fi
tmp="$(mktemp)"; chmod 600 "$tmp"
NAME="$name" VALUE="$value" python3 - "$tmp" <<'PY'
import os, sys
name, value = os.environ["NAME"], os.environ["VALUE"]
lines = [l for l in open(".env").read().split("\n") if not l.startswith(name + "=")]
while lines and lines[-1] == "":
    lines.pop()
lines.append(f"{name}={value}")
open(sys.argv[1], "w").write("\n".join(lines) + "\n")
PY
cp "$tmp" .env; chmod 600 .env; rm -f "$tmp"
docker compose --profile discord up -d
echo "已更新 $name 并重启服务。"
[ "$name" = ADMIN_PASSWORD ] && echo "提示：如果你在后台改过密码，后台以数据库里保存的新密码为准，这里的 ADMIN_PASSWORD 只作初始/恢复用。"
exit 0
