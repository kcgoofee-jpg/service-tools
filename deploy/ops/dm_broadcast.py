"""一次性站内私信广播（在 nai-gate 容器里跑，复用容器里的 bot token 与数据库）。

用途：站长级通知（如「今晚重置 Key」）。刻意做成脚本而不是常驻功能——
2026-10-10 的教训：批量私信正是 Discord 应用被标记的信号之一，所以限速默认 6 秒/人、
只发一次、不自动重试（撞 429 听 Discord 的多歇一会）。

防重复（2026-10-13 教训：同一晚发了 3 遍）：
  · 每个campaign按 --slug 记账到 /tmp/dm_state_<slug>.json，已发过的成员自动跳过（断点续发）；
  · 文案 hash 变了会提示这是新 campaign，不覆盖旧账本；
  · 全部发完后再次运行 = 0 发送，不会重发。

热更新已发出的私信：
  送达的每条消息都记录了 channel_id/message_id，改完文案后用 --edit 可原位 PATCH，不用重发。

用法：
  docker exec nai-gate python /tmp/dm_broadcast.py --slug reset --dry-run
  docker exec nai-gate python /tmp/dm_broadcast.py --slug reset --file /tmp/notice.txt
  docker exec nai-gate python /tmp/dm_broadcast.py --slug reset --edit /tmp/notice-v2.txt
"""
import argparse
import asyncio
import hashlib
import json
import os
import random
import sqlite3
import sys
import time

import httpx

API = "https://discord.com/api"
HEADERS = lambda token: {"Authorization": "Bot " + token}


def state_path(slug: str) -> str:
    return f"/tmp/dm_state_{slug}.json"


def load_state(path: str) -> dict:
    try:
        return json.load(open(path, encoding="utf-8"))
    except (OSError, ValueError):
        return {}


async def dm(http, token, discord_id, text):
    """发送私信，返回 (channel_id, message_id) 或 None。"""
    h = HEADERS(token)
    r = await http.post(f"{API}/users/@me/channels", json={"recipient_id": str(discord_id)})
    if r.status_code != 200:
        return None
    channel = r.json()["id"]
    m = await http.post(f"{API}/channels/{channel}/messages",
                        json={"content": text[:1900], "allowed_mentions": {"parse": []}})
    if m.status_code == 429:                      # 撞 Discord 限速：听它的
        retry = float((m.json() or {}).get("retry_after", 5) or 5)
        print(f"  Discord 限速，睡 {retry:.0f}s", flush=True)
        await asyncio.sleep(retry)
        m = await http.post(f"{API}/channels/{channel}/messages",
                            json={"content": text[:1900], "allowed_mentions": {"parse": []}})
    return f"{channel}:{m.json()['id']}" if m.status_code in (200, 201) else None


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--slug", default="notice", help="campaign 名：同名的账本互通（续发/编辑都靠它）")
    ap.add_argument("--text", default="")
    ap.add_argument("--file", default="")
    ap.add_argument("--edit", default="", help="不新发，把已送达的原位编辑成这个文案（文件）")
    ap.add_argument("--rate", type=float, default=6.0, help="每人间隔秒数（默认 6）")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    text = (args.text or (open(args.file, encoding="utf-8").read() if args.file else "")).strip()
    token = os.environ.get("DISCORD_BOT_TOKEN", "")
    if not token:
        print("容器里没有 DISCORD_BOT_TOKEN"); return 2

    spath = state_path(args.slug)
    state = load_state(spath)
    text_hash = hashlib.sha256(text.encode()).hexdigest()[:12] if text else ""

    db = sqlite3.connect("file:/app/data/nai_gate.db?mode=ro", uri=True)
    rows = [str(r[0]) for r in db.execute(
        """SELECT r.discord_id FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id
           WHERE k.enabled=1 ORDER BY r.discord_id""")]

    if args.edit:
        edit_text = (open(args.edit, encoding="utf-8").read() if args.edit else "").strip()
        sent = state.get("sent", {})
        if not sent:
            print("账本里没有已发送记录，没东西可编辑"); return 2
        print(f"原位编辑 {len(sent)} 条已送达私信（限速 {args.rate}s）"); 
    else:
        if not text:
            print("没有私信内容：用 --text / --file"); return 2
        if state.get("text_hash") and state["text_hash"] != text_hash:
            print(f"⚠ 文案与账本「{args.slug}」不同（旧 {state.get('text_hash')} ≠ 新 {text_hash}）。"
                  f"换个 --slug 开新 campaign，或确认是要替换的文案"); return 2
        sent = state.get("sent", {})
        pending = [d for d in rows if d not in sent]
        print(f"在册 {len(rows)} 人，已发 {len(sent)}，本次待发 {len(pending)}，限速 {args.rate}s/人")
        if args.dry_run:
            return 0

    ok = bad = 0
    started = time.time()
    state.setdefault("sent", {})
    async with httpx.AsyncClient(headers=HEADERS(token), timeout=15) as http:
        if args.edit:
            edit_text = edit_text[:1900]
            for i, (discord_id, ref) in enumerate(list(sent.items()), 1):
                channel, _, mid = ref.partition(":")
                try:
                    r = await http.patch(f"{API}/channels/{channel}/messages/{mid}",
                                         json={"content": edit_text, "allowed_mentions": {"parse": []}})
                    ok += r.status_code in (200, 201)
                    bad += r.status_code not in (200, 201)
                except httpx.HTTPError:
                    bad += 1
                if i % 20 == 0:
                    print(f"  进度 {i}/{len(sent)}（成功 {ok}，失败 {bad}）", flush=True)
                await asyncio.sleep(args.rate * (0.8 + 0.4 * random.random()))
        else:
            for i, discord_id in enumerate(pending, 1):
                try:
                    ref = await dm(http, token, discord_id, text)
                    if ref:
                        state["sent"][discord_id] = ref
                        ok += 1
                    else:
                        bad += 1
                except httpx.HTTPError:
                    bad += 1
                state["text_hash"] = text_hash
                state["updated_at"] = time.time()
                json.dump(state, open(spath, "w", encoding="utf-8"))     # 每发一条就落盘：中断可续
                if i % 20 == 0:
                    print(f"  进度 {i}/{len(pending)}（成功 {ok}，失败 {bad}，"
                          f"已用 {(time.time()-started)/60:.0f} 分钟）", flush=True)
                await asyncio.sleep(args.rate * (0.8 + 0.4 * random.random()))
    print(f"完成：成功 {ok}，失败 {bad}，共 {(time.time()-started)/60:.1f} 分钟；账本 {spath}")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
