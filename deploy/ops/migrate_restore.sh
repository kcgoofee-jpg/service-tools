#!/usr/bin/env bash
# 灾难恢复：在新机器上从 S3 拉最新备份并重建 NAI Gate。
#
# 前提（一次性）：
#   1. 新机器装好 Docker 与 docker compose，git clone 本仓库到 /opt/service-tools
#   2. 有能读备份桶的 AWS 凭证（写进 /root/.aws/credentials 的 [naigate-push]，
#      或用站长 IAM 凭证；没 AWS CLI 时也可从控制台手动下载 daily/latest/backup.tar.gz.enc）
#   3. 加密口令（服务器 /root/.backup_pass 的内容）——站长须在服务器之外另存
#
# 用法：bash deploy/ops/migrate_restore.sh
set -euo pipefail
cd "$(dirname "$0")/../.."                   # /opt/service-tools
export AWS_PROFILE=${AWS_PROFILE:-naigate-push}
ACC=$(aws sts get-caller-identity --query Account --output text 2>/dev/null) \
  || { echo "取不到 AWS 身份：先配好 /root/.aws/credentials"; exit 1; }
BUCKET="nai-gate-backups-${ACC}"
WORK=$(mktemp -d /tmp/nai-restore.XXXX)
trap 'rm -rf "$WORK"' EXIT

echo "== 1/5 下载最新备份 =="
aws s3 cp "s3://${BUCKET}/daily/latest/backup.tar.gz.enc" "$WORK/backup.tar.gz.enc" --no-progress

echo "== 2/5 解密（需要口令，输入 /root/.backup_pass 的内容） =="
openssl enc -d -aes-256-cbc -pbkdf2 -pass "pass:$BACKUP_PASSPHRASE" \
  -in "$WORK/backup.tar.gz.enc" -out "$WORK/all.tar.gz" 2>/dev/null \
  || openssl enc -d -aes-256-cbc -pbkdf2 -pass env:BACKUP_PASSPHRASE \
     -in "$WORK/backup.tar.gz.enc" -out "$WORK/all.tar.gz"

echo "== 3/5 拆包 =="
mkdir -p "$WORK/split"
tar -xzf "$WORK/all.tar.gz" -C "$WORK/split"
tar -xzf "$WORK/split/bundle.tar.gz" -C "$WORK/split"

echo "== 4/5 恢复数据与配置 =="
cp "$WORK/split/nai_gate.db.gz" /opt/service-tools/data/nai_gate.db.gz
gunzip -f /opt/service-tools/data/nai_gate.db.gz
cp "$WORK/split/.env" /opt/service-tools/.env
for f in secret_key upstream_tokens.json discord_layout.json; do
  [ -f "$WORK/split/data/$f" ] && cp "$WORK/split/data/$f" /opt/service-tools/data/ || true
done
mkdir -p /opt/service-tools/data/audit_images
chown -R 10001:10001 /opt/service-tools/data
chmod 600 /opt/service-tools/.env

echo "== 5/5 启动 =="
cd /opt/service-tools
docker compose --profile discord up -d --build
sleep 5
curl -fsS http://127.0.0.1:3003/healthz && echo && echo "恢复完成：把域名 A 记录切到本机即可"
