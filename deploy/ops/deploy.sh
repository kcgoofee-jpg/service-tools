set -e
# 用法：bash deploy.sh <版本号>   等队列空闲 → 备份数据库与 .env → 重建容器 → 验证健康与首页
VER=${1:?usage: deploy.sh <version>}
cd /opt/service-tools
for i in $(seq 1 12); do q=$(curl -s -m 5 127.0.0.1:3003/queue-status); echo "$q" | grep -q '"active":0,"waiting":0' && break; echo "busy: $q"; sleep 5; done
B=/opt/backups/pre-deploy-$(date +%Y%m%d-%H%M%S); mkdir -p $B
/usr/local/bin/naigate-snapshot $B/nai_gate.db.gz    # 失败（含完整性校验失败）会中止部署：没有可用备份就不部署
cp -a data/secret_key data/discord_layout.json $B/ 2>/dev/null || true; cp -a .env $B/env; chmod -R go-rwx $B; echo "backup $B"
ls -1dt /opt/backups/pre-deploy-* | tail -n +11 | xargs -r rm -rf          # 部署前快照只留最近 10 份
docker compose --profile discord up -d --build 2>&1 | tail -3
docker builder prune -f --filter until=72h >/dev/null 2>&1 || true          # 构建缓存只留 3 天内的，保证下次构建仍然快
for i in $(seq 1 20); do curl -s -m 3 127.0.0.1:3003/healthz | grep -q "\"$VER\"" && break; sleep 2; done
curl -s 127.0.0.1:3003/healthz; echo
curl -s -D - -o /dev/null 127.0.0.1:3003/public/live | grep -i x-request-id
sleep 8
curl -s 127.0.0.1:3003/public/live | python3 -c 'import json,sys; d=json.load(sys.stdin); print("rules.quota", d["rules"].get("quota"))'
curl -s -o /dev/null -w "landing %{http_code}\n" 127.0.0.1:3003/
docker ps --format "{{.Names}} {{.Status}}"
docker logs --since 1m nai-gate 2>&1 | grep -E "Traceback|\[bug\]|\[error\]" | head -5
docker logs --since 1m nai-gate-discord 2>&1 | tail -2
