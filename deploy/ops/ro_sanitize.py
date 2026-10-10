"""把 naigate-snapshot 出来的快照再脱敏一遍，给协作 AI（Antigravity）做只读分析用。

去掉：成员 API Key 原文（换成不可逆短哈希，同一把 Key 仍能对上）、key_source_salt、
提示词 / 负面词 / 附加参数原文（成员创作内容）、头像。保留：用量、请求特征哈希、网段、Discord ID 等分析需要的字段。
用法：python3 ro_sanitize.py <snapshot.db>（原地改写，然后 VACUUM）
"""
import hashlib
import sqlite3
import sys

db = sqlite3.connect(sys.argv[1])
rows = db.execute("SELECT id, token FROM api_keys").fetchall()
for kid, tok in rows:
    db.execute("UPDATE api_keys SET token=? WHERE id=?", ("sha256:" + hashlib.sha256(str(tok).encode()).hexdigest()[:16], kid))
db.execute("DELETE FROM site_settings WHERE key IN ('key_source_salt')")
cols = {r[1] for r in db.execute("PRAGMA table_info(generation_audit)")}
for c in ("prompt", "negative", "extra"):
    if c in cols:
        db.execute(f"UPDATE generation_audit SET {c}=''")
if "thumb" in cols:
    db.execute("UPDATE generation_audit SET thumb=NULL")
db.execute("UPDATE discord_registrations SET avatar=''")
db.commit()
left = db.execute("SELECT COUNT(*) FROM api_keys WHERE token NOT LIKE 'sha256:%'").fetchone()[0]
assert left == 0, "api_keys.token 没脱敏干净"
db.execute("VACUUM")
db.close()
print("sanitized", len(rows), "keys")
