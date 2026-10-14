#!/usr/bin/env bash
# 公开仓库红线：golden 夹具、测试与开发脚本里只允许合成数据。
# 拦截：17–20 位 Discord 雪花 ID（合成数据统一用 9000000000000000xx 段）、公网 IPv4、看起来像真实上游 Token 的串。
set -euo pipefail
cd "$(dirname "$0")/../.."
paths=(crates/*/tests tools/parity tools/dev web/e2e web/src)
fail=0
# 只看引号里的纯数字串（Discord ID 在 JSON / 代码里都是字符串；浮点夹具里的长数字不算）
if grep -rEn '"[1-8][0-9]{16,19}"' "${paths[@]}" 2>/dev/null | grep -vE '"9000000000000000[0-9]{2}"' | head -20 | grep .; then
  echo "↑ 疑似真实 Discord ID（合成数据请用 9000000000000000xx）" >&2; fail=1
fi
if grep -rEn '\b(([0-9]{1,3}\.){3}[0-9]{1,3})\b' "${paths[@]}" 2>/dev/null \
   | grep -vE '\b(127\.0\.0\.1|0\.0\.0\.0|10\.[0-9.]+|192\.168\.[0-9.]+|172\.(1[6-9]|2[0-9]|3[01])\.[0-9.]+|203\.0\.113\.[0-9]+|198\.51\.100\.[0-9]+|192\.0\.2\.[0-9]+)\b' \
   | grep -vE '([0-9]+\.){3}[0-9]+(-|\w)' | head -20 | grep .; then
  echo "↑ 疑似公网 IP（测试请用 RFC 5737 文档网段 203.0.113.0/24 等）" >&2; fail=1
fi
if grep -rEn 'pst-[A-Za-z0-9_-]{40,}' "${paths[@]}" 2>/dev/null | head -5 | grep .; then
  echo "↑ 疑似真实上游 Token" >&2; fail=1
fi
exit $fail
