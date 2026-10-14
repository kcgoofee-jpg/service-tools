#!/usr/bin/env bash
# 本地起旧版 Python 网关（端口 18000）+ 假上游（端口 18001），带合成演示数据。前端开发：另开 `cd web && pnpm dev`。
# 用法：[GATE_PORT=18000 MOCK_PORT=18001] tools/dev/run-legacy.sh [数据目录，默认 /tmp/owl-dev]
# 前端：VITE_GATE=http://127.0.0.1:$GATE_PORT pnpm --dir web exec vite --port 5173
set -euo pipefail
cd "$(dirname "$0")/../.."
DATA=${1:-/tmp/owl-dev}
GATE_PORT=${GATE_PORT:-18000}
MOCK_PORT=${MOCK_PORT:-18001}
rm -rf "$DATA" && mkdir -p "$DATA"
PY="uv run --python 3.12 --with-requirements requirements.txt"
$PY python tools/dev/seed_demo.py "$DATA"
$PY uvicorn tests.mock_nai:app --host 127.0.0.1 --port "$MOCK_PORT" --log-level warning &
MOCK=$!
trap 'kill $MOCK 2>/dev/null' EXIT
DATA_DIR="$DATA" ADMIN_PASSWORD=dev-admin-password-123 NAI_TOKENS=pst-devdevdevdevdevdevdev \
  NAI_IMAGE_HOST=http://127.0.0.1:$MOCK_PORT NAI_TEXT_HOST=http://127.0.0.1:$MOCK_PORT NAI_TEXT_HOST_LEGACY=http://127.0.0.1:$MOCK_PORT \
  DISCORD_CLIENT_ID=1 DISCORD_CLIENT_SECRET=dev DISCORD_BOT_TOKEN=dev DISCORD_GUILD_ID=1 SITE_URL=http://127.0.0.1:5173 \
  REGISTRATION_BRIDGE_SECRET=dev-bridge-secret-0123456789abcdefghij \
  ADMIN_ALLOWED_ORIGINS=${ADMIN_ALLOWED_ORIGINS:-http://localhost:5173,http://127.0.0.1:5173} ADMIN_COOKIE_SECURE=0 IMAGE_MIN_INTERVAL=0.2 KEY_IMAGE_MIN_INTERVAL=0.2 TZ=Asia/Shanghai \
  $PY uvicorn app.main:app --host 127.0.0.1 --port "$GATE_PORT" --log-level warning
