"""本地开发用：在临时数据目录里建一份合成演示数据（成员 / 用量 / 设置），并签发后台与成员会话 Cookie。

    uv run --python 3.12 --with-requirements requirements.txt python tools/dev/seed_demo.py <数据目录>

输出 <数据目录>/cookies.json：{"admin": "...", "member": "...", "member_discord_id": "..."}。
全部是假数据（公开仓库）。旧 Python 网关与 3.0 Rust 网关共用同一份库与 secret_key，两边都能用这些 Cookie。
"""
from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import random
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from app import database, status_page  # noqa: E402

ADMIN_PASSWORD = "dev-admin-password-123"
SECRET = "0" * 16 + "dev-secret-key-not-for-production-use-0000"


def sign(secret: str, payload: str) -> str:
    return hmac.new(secret.encode(), payload.encode(), hashlib.sha256).hexdigest()


async def main(data_dir: Path) -> None:
    data_dir.mkdir(parents=True, exist_ok=True)
    (data_dir / "secret_key").write_text(SECRET)
    db = database.Database(str(data_dir / "nai_gate.db"))
    await db.connect()
    await status_page.ensure_schema(db)
    rng = random.Random(7)
    now = time.time()
    names = ["月见草", "白鸟", "nightjar", "雾岛", "tester-04", "柚子", "Kestrel", "长尾山雀"]
    key_ids = []
    for i, name in enumerate(names):
        token = f"nai-demo{i:02d}" + "x" * 30
        cur = await db._db.execute(
            "INSERT INTO api_keys (name, token, enabled, daily_images, daily_v5, daily_anlas, rpm, created_at, "
            "last_used_at, last_image_at, expires_at, features, discord_id) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (name, token, 1, 104, 10, 50, 5, now - 86400 * (i + 2), now - 600 * i, now - 900 * i,
             now + 86400 * 20, "image,vibe,tags", str(900000000000000000 + i)))
        key_ids.append(cur.lastrowid)
        await db._db.execute(
            "INSERT INTO discord_registrations (discord_id, key_id, created_at, username, display_name) VALUES (?,?,?,?,?)",
            (str(900000000000000000 + i), cur.lastrowid, now - 86400 * (i + 2), f"user{i}", name))
    await db._db.commit()
    models = ["nai-diffusion-4-5-full", "nai-diffusion-4-5-curated", "nai-diffusion-5-full"]
    for _ in range(400):
        kid = rng.choice(key_ids)
        ts = now - rng.uniform(0, 3 * 86400)
        status = rng.choices(["ok", "rejected", "error"], [85, 12, 3])[0]
        await db._db.execute(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, model, status, images, anlas, tokens, detail, wait_ms, dur_ms, client) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (ts, kid, names[key_ids.index(kid)], "image", rng.choice(models), status, 1 if status == "ok" else 0,
             0, 0, "" if status == "ok" else "429 请求过于频繁", rng.randint(0, 20000), rng.randint(3000, 9000), "柏宝绘"))
    await db._db.commit()
    await db.close()
    exp = int(now) + 7 * 86400
    admin_secret = SECRET + ":" + hashlib.sha256(ADMIN_PASSWORD.encode()).hexdigest()
    admin_cookie = f"{exp}.{sign(admin_secret, str(exp))}"
    member_id = str(900000000000000000)
    tag = hashlib.sha256(("nai-demo00" + "x" * 30).encode()).hexdigest()[:16]
    payload = f"{member_id}:{int(now) + 30 * 86400}:{tag}"
    member_cookie = payload + "." + sign(SECRET + ":member-login", payload)
    (data_dir / "cookies.json").write_text(json.dumps({
        "admin": admin_cookie, "member": member_cookie, "member_discord_id": member_id,
        "admin_password": ADMIN_PASSWORD, "member_key": "nai-demo00" + "x" * 30}, indent=1))
    print(f"演示数据已写入 {data_dir}")


asyncio.run(main(Path(sys.argv[1])))
