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
# load / up / rollback / previous 之前会先看最近 10 分钟的出图请求数（只读 sqlite）：超过阈值
# （环境变量 CUTOVER_BUSY_THRESHOLD，默认 30）或现在正落在忙时（CUTOVER_BUSY_HOURS，如 "20-24"，默认不设）
# 就拒绝执行，除非加 --force（放在子命令后面或前面都行）。
#
# 2.x 的容器只停不删，回滚只需几秒。巡检跟着切换（systemd drop-in）：3.0 检查 owl-gate / owl-naimei / caddy，
# 回滚到 2.x 后改查 nai-gate / nai-gate-discord / caddy，不然巡检会把「owl-* 没在跑」当成危险，每 6 小时私信一次站长。
set -euo pipefail

OWL_DIR=/opt/owl
LEGACY_DIR=/opt/service-tools
ENV_FILE=$LEGACY_DIR/.env
DB=$LEGACY_DIR/data/nai_gate.db
BACKUPS=/opt/backups/pre-v3
LEGACY=(nai-gate-discord nai-gate-warden nai-gate)
COMPOSE=(docker compose -p owl --env-file "$ENV_FILE" -f "$OWL_DIR/compose.yml")
PATROL_DROPIN=/etc/systemd/system/nai-patrol.service.d/v3.conf
# 回滚到 2.x 时才有：让巡检去查 2.x 的容器名
PATROL_ROLLBACK_DROPIN=/etc/systemd/system/nai-patrol.service.d/rollback.conf
PATROL_CONTAINERS_V3=owl-gate,owl-naimei,caddy
PATROL_CONTAINERS_LEGACY=nai-gate,nai-gate-discord,caddy

BUSY_THRESHOLD=${CUTOVER_BUSY_THRESHOLD:-30}
BUSY_HOURS=${CUTOVER_BUSY_HOURS:-}
FORCE=0

log() { printf '[%(%F %T)T] %s\n' -1 "$*"; }
die() { log "✗ $*"; exit 1; }

# 最近 10 分钟的图片请求数（只读打开库；读不到返回空）
recent_images() {
  [[ -f $DB ]] || return 0
  sqlite3 -readonly "$DB" ".timeout 5000" \
    "SELECT COUNT(*) FROM usage_log WHERE kind LIKE 'image%' AND ts > CAST(strftime('%s','now') AS INTEGER) - 600;" \
    2>/dev/null | tail -1
}

# 现在是不是落在 CUTOVER_BUSY_HOURS（如 "20-24"，含起点、不含终点，可跨午夜 "22-6"）里
in_busy_hours() {
  [[ $BUSY_HOURS =~ ^([0-9]{1,2})-([0-9]{1,2})$ ]] || return 1
  local from=$((10#${BASH_REMATCH[1]})) to=$((10#${BASH_REMATCH[2]})) now
  now=$((10#$(date +%H)))
  if ((from <= to)); then ((now >= from && now < to)); else ((now >= from || now < to)); fi
}

# 部署 / 重启前的忙碌检查：忙就拒绝，--force 才放行
busy_guard() {
  local n reason=""
  n=$(recent_images || true)
  if [[ -z $n ]]; then
    log "! 读不到最近 10 分钟的出图数（库不可读？），跳过忙碌检查"
  else
    log "最近 10 分钟图片请求：$n（阈值 $BUSY_THRESHOLD）"
    if ((n > BUSY_THRESHOLD)); then reason="最近 10 分钟有 $n 张图片请求，超过阈值 $BUSY_THRESHOLD"; fi
  fi
  if [[ -z $reason ]] && in_busy_hours; then reason="现在（$(date +%H:%M)）在忙时 $BUSY_HOURS 内"; fi
  if [[ -n $reason ]]; then
    if ((FORCE)); then
      log "! $reason；已加 --force，继续"
    else
      die "$reason；生成中的图会被打断。等一等再来，或加 --force 强行执行"
    fi
  fi
  log "提醒：重启后每小时出图计数由用量日志补回，跨重启的那个小时窗口可能多算一次（只会更保守）"
}

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
  rm -f "$PATROL_ROLLBACK_DROPIN"   # 回到 3.0 / 上一版 3.0：容器名用默认值（owl-gate,owl-naimei,caddy）
  cat >"$PATROL_DROPIN" <<'UNIT'
[Service]
# 3.0：巡检用 nai-ops（宿主机静态二进制，工作目录仍是 /opt/service-tools，读同一份 .env 和 data/）。
# 0 正常 / 1 注意 / 2 危险；3 = 巡检自身出错，算失败。
ExecStart=
ExecStart=/opt/owl/nai-ops patrol --oneline --alert
# 只有「危险」才私信：同一批危险项 6 小时内只提醒一次，出现新的立刻提醒，解除后补一条「已恢复」
Environment=PATROL_COOLDOWN=21600
SuccessExitStatus=
SuccessExitStatus=0 1 2
UNIT
  systemctl daemon-reload
}

patrol_rollback() {
  rm -f "$PATROL_DROPIN"
  mkdir -p "$(dirname "$PATROL_ROLLBACK_DROPIN")"
  cat >"$PATROL_ROLLBACK_DROPIN" <<UNIT
[Service]
# 已回滚到 2.x：3.0 的 owl-* 容器是停着的，巡检改查 2.x 的容器
Environment=NAI_GATE_CONTAINERS=$PATROL_CONTAINERS_LEGACY
UNIT
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
  if ! "${COMPOSE[@]}" up -d --remove-orphans; then
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
  patrol_rollback
  log "✓ 已回到 2.x（巡检改查：$PATROL_CONTAINERS_LEGACY）"
}

cmd_previous() {
  docker image inspect owl-gate:previous >/dev/null 2>&1 || die "没有 owl-gate:previous"
  docker tag owl-gate:current owl-gate:failed
  docker tag owl-gate:previous owl-gate:current
  "${COMPOSE[@]}" up -d --remove-orphans
  wait_healthy 90 || die "上一版也起不来：先 cutover.sh rollback 回 2.x"
  patrol_v3
  log "✓ 已回到上一版 3.0（坏的那版标成 owl-gate:failed）"
}

cmd_status() {
  docker ps -a --filter name=owl- --filter name=nai-gate --format '{{.Names}}\t{{.Status}}\t{{.Image}}'
  curl -fsS -m 3 http://127.0.0.1:3003/healthz && echo
  if [[ -f $PATROL_ROLLBACK_DROPIN ]]; then
    echo "巡检：nai-ops，检查 2.x 容器（$PATROL_CONTAINERS_LEGACY）"
  else
    echo "巡检：nai-ops，检查 3.0 容器（$PATROL_CONTAINERS_V3）"
  fi
}

args=()
for a in "$@"; do
  if [[ $a == --force ]]; then FORCE=1; else args+=("$a"); fi
done
set -- "${args[@]+"${args[@]}"}"

case ${1:-status} in
  load) shift; busy_guard; cmd_load "$@" ;;
  install-ops) shift; cmd_install_ops "$@" ;;
  up) busy_guard; cmd_up ;;
  rollback) busy_guard; cmd_rollback ;;
  previous) busy_guard; cmd_previous ;;
  status) cmd_status ;;
  *) die "未知子命令：$1" ;;
esac
