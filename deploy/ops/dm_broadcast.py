"""一次性站内私信广播（在 nai-gate 容器里跑，复用容器里的 bot token 与数据库）。

用途：站长级通知（如「今晚重置 Key」）。刻意做成脚本而不是常驻功能——
2026-10-10 的教训：批量私信正是 Discord 应用被标记的信号之一，所以
限速默认 6 秒/人（约 150 人 = 15 分钟）、只发一次、发完即止，不做任何重试。

用法：
  docker exec nai-gate python /app/deploy/ops/dm_broadcast.py --dry-run   # 先看要发多少人
  docker exec nai-gate python /app/deploy/ops/dm_broadcast.py --text "……"
  （或 --file /tmp/notice.txt）
"""
import argparse
import asyncio
import random
import sqlite3
import sys
import time

import httpx

API = "https://discord.com/api"


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--text", default="")
    ap.add_argument("--file", default="")
    ap.add_argument("--rate", type=float, default=6.0, help="每人间隔秒数（默认 6）")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    text = args.text or (open(args.file, encoding="utf-8").read() if args.file else "")
    text = text.strip()
    if not text:
        print("没有私信内容：用 --text 或 --file 传入"); return 2
    token = __import__("os").environ.get("DISCORD_BOT_TOKEN", "")
    if not token:
        print("容器里没有 DISCORD_BOT_TOKEN"); return 2

    db = sqlite3.connect("file:/app/data/nai_gate.db?mode=ro", uri=True)
    rows = [r[0] for r in db.execute(
        """SELECT r.discord_id FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id
           WHERE k.enabled=1 ORDER BY r.discord_id""")]
    print(f"计划私信 {len(rows)} 位在册成员，限速 {args.rate}s/人，预计 {len(rows) * args.rate / 60:.0f} 分钟")
    if args.dry_run:
        return 0

    headers = {"Authorization": "Bot " + token}
    sent = failed = 0
    started = time.time()
    async with httpx.AsyncClient(headers=headers, timeout=15) as http:
        for i, discord_id in enumerate(rows, 1):
            try:
                r = await http.post(f"{API}/users/@me/channels", json={"recipient_id": str(discord_id)})
                if r.status_code != 200:
                    failed += 1
                else:
                    channel = r.json()["id"]
                    m = await http.post(f"{API}/channels/{channel}/messages",
                                        json={"content": text[:1900], "allowed_mentions": {"parse": []}})
                    sent += m.status_code in (200, 201)
                    if m.status_code == 429:                      # 撞 Discord 限速：听它的，多歇一会
                        retry = float(m.json().get("retry_after", 5) or 5)
                        print(f"  [{i}] Discord 限速，睡 {retry:.0f}s")
                        await asyncio.sleep(retry)
            except httpx.HTTPError as exc:
                failed += 1
                print(f"  [{i}] {discord_id} 网络错误：{type(exc).__name__}")
            if i % 20 == 0:
                print(f"  进度 {i}/{len(rows)}（成功 {sent}，失败 {failed}，已用 {(time.time()-started)/60:.0f} 分钟）",
                      flush=True)
            await asyncio.sleep(args.rate * (0.8 + 0.4 * random.random()))
    print(f"完成：成功 {sent}，失败 {failed}，共 {(time.time()-started)/60:.1f} 分钟")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
