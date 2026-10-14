#!/usr/bin/env bash
# 3.0 切换 / 回滚（在服务器上以 root 运行）。
#
#   deploy/v3/cutover.sh load <owl-gate.tar.gz>   载入 CI 构建的镜像：旧的 current 改名 previous，新的成为 current
#   deploy/v3/cutover.sh install-ops <nai-ops>     安装宿主机用的运维命令（静态二进制，巡检要用宿主机的 docker / 磁盘 / .env）
#   deploy/v3/cutover.sh up                        2.x → 3.0：备份库 → 停 2.x → 起 3.0 → 健康检查 + 冒烟，失败自动回滚
#   deploy/v3/cutover.sh rollback                  3.0 → 2.x：停 3.0 → 起 2.x 原容器（库是兼容的，只增不减）
#   deploy/v3/cutover.sh previous                  3.0 回到上一版 3.0 镜像（owl-gate:previous）
#   deploy/v3/cutover.sh status
#
# 2.x 的容器只停不删，回滚只需几秒。巡检定时器跟着切换（systemd drop-in）。
set -euo pipefail

OWL_DIR=/opt/owl
LEGACY_DIR=/opt/service-tools
ENV_FILE=$LEGACY_DIR/.env
DB=$LEGACY_DIR/data/nai_gate.db
BACKUPS=/opt/backups/pre-v3
LEGACY=(nai-gate-discord nai-gate-warden nai-gate)
COMPOSE=(docker compose -p owl --env-file "$ENV_FILE" -f "$OWL_DIR/compose.yml")
PATROL_DROPIN=/etc/systemd/system/nai-patrol.service.d/v3.conf

log() { printf '[%(%F %T)T] %s\n' -1 "$*"; }
die() { log "✗ $*"; exit 1; }

wait_healthy() {
  local deadline=$((SECONDS + ${1:-90}))
  while ((SECONDS < deadline)); do
    if curl -fsS -m 3 http://127.0.0.1:3003/healthz >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  return 1
}

patrol_v3() {
  mkdir -p "$(dirname "$PATROL_DROPIN")"
  cat >"$PATROL_DROPIN" <<'UNIT'
[Service]
# 3.0：巡检用 nai-ops（宿主机静态二进制，工作目录仍是 /opt/service-tools，读同一份 .env 和 data/）。
# 0 正常 / 1 注意 / 2 危险；3 = 巡检自身出错，算失败。
ExecStart=
ExecStart=/opt/owl/nai-ops patrol --oneline --alert
# 同样的问题（摘要不变）6 小时内只提醒一次；出现新问题摘要就变，立刻提醒
Environment=PATROL_COOLDOWN=21600
SuccessExitStatus=
SuccessExitStatus=0 1 2
UNIT
  systemctl daemon-reload
}

patrol_legacy() {
  rm -f "$PATROL_DROPIN"
  systemctl daemon-reload
}

smoke() {
  docker exec owl-gate /usr/local/bin/nai-ops smoke http://127.0.0.1:8000
}

cmd_load() {
  local tar=${1:?用法：cutover.sh load <owl-gate.tar.gz>}
  [[ -f $tar ]] || die "找不到 $tar"
  local loaded
  loaded=$(gunzip -c "$tar" | docker load | sed -n 's/^Loaded image: //p' | tail -1)
  [[ -n $loaded ]] || die "docker load 没有输出镜像名"
  if docker image inspect owl-gate:current >/dev/null 2>&1; then
    docker tag owl-gate:current owl-gate:previous
  fi
  docker tag "$loaded" owl-gate:current
  log "✓ owl-gate:current = $loaded（$(docker image inspect -f '{{.Id}}' owl-gate:current | cut -c8-19)）"
}

cmd_install_ops() {
  local bin=${1:?用法：cutover.sh install-ops <nai-ops>}
  "$bin" --version >/dev/null || die "$bin 在这台机器上跑不起来"
  if [[ $(realpath "$bin") != $(realpath -m "$OWL_DIR/nai-ops") ]]; then
    install -m 755 "$bin" "$OWL_DIR/nai-ops"
  else
    chmod 755 "$OWL_DIR/nai-ops"
  fi
  log "✓ $OWL_DIR/nai-ops = $("$OWL_DIR/nai-ops" --version)"
}

cmd_up() {
  docker image inspect owl-gate:current >/dev/null 2>&1 || die "先 load 镜像"
  [[ -f $OWL_DIR/compose.yml ]] || die "缺 $OWL_DIR/compose.yml"
  [[ -x $OWL_DIR/nai-ops ]] || die "先 install-ops（巡检要用）"
  mkdir -p "$BACKUPS"
  local snap
  snap="$BACKUPS/nai_gate-$(date +%Y%m%d-%H%M%S).db"
  log "备份数据库 → $snap"
  sqlite3 "$DB" ".timeout 15000" ".backup '$snap'"
  sqlite3 "$snap" "PRAGMA integrity_check;" | grep -qx ok || die "备份校验失败，中止（2.x 仍在运行）"
  chmod 600 "$snap"

  log "停 2.x（最多等 120 秒让进行中的出图收尾记账）"
  docker stop -t 120 "${LEGACY[@]}" >/dev/null || true

  log "起 3.0"
  if ! "${COMPOSE[@]}" up -d; then
    log "✗ 3.0 启动失败，回滚"
    cmd_rollback
    exit 1
  fi
  if ! wait_healthy 90; then
    log "✗ 3.0 健康检查超时，回滚"
    "${COMPOSE[@]}" logs --tail 80 gate || true
    cmd_rollback
    exit 1
  fi
  if ! smoke; then
    log "✗ 冒烟失败，回滚"
    cmd_rollback
    exit 1
  fi
  patrol_v3
  log "✓ 3.0 已上线（2.x 容器保留，回滚：cutover.sh rollback）"
}

cmd_rollback() {
  log "停 3.0"
  "${COMPOSE[@]}" stop -t 120 || true
  "${COMPOSE[@]}" rm -f || true
  log "起 2.x"
  docker start nai-gate >/dev/null
  wait_healthy 120 || die "2.x 也没起来：docker logs nai-gate"
  docker start nai-gate-discord nai-gate-warden >/dev/null || true
  patrol_legacy
  log "✓ 已回到 2.x"
}

cmd_previous() {
  docker image inspect owl-gate:previous >/dev/null 2>&1 || die "没有 owl-gate:previous"
  docker tag owl-gate:current owl-gate:failed
  docker tag owl-gate:previous owl-gate:current
  "${COMPOSE[@]}" up -d
  wait_healthy 90 || die "上一版也起不来：先 cutover.sh rollback 回 2.x"
  log "✓ 已回到上一版 3.0（坏的那版标成 owl-gate:failed）"
}

cmd_status() {
  docker ps -a --filter name=owl- --filter name=nai-gate --format '{{.Names}}\t{{.Status}}\t{{.Image}}'
  curl -fsS -m 3 http://127.0.0.1:3003/healthz && echo
  [[ -f $PATROL_DROPIN ]] && echo "巡检：3.0（nai-ops）" || echo "巡检：2.x（patrol.py）"
}

case ${1:-status} in
  load) shift; cmd_load "$@" ;;
  install-ops) shift; cmd_install_ops "$@" ;;
  up) cmd_up ;;
  rollback) cmd_rollback ;;
  previous) cmd_previous ;;
  status) cmd_status ;;
  *) die "未知子命令：$1" ;;
esac
