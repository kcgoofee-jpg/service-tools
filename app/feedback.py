"""成员反馈：/反馈 命令弹出 Discord 表单（最多 5 题），答案存库，后台查看与导出。

不私信、不群发：成员自己发起，回执只有本人可见（680009 申诉期规则）。
"""
from __future__ import annotations

import csv
import io
import json
import time
from datetime import datetime

# Discord 弹窗限制：最多 5 题，标题 ≤45 字，单题 ≤4000 字。问题改这里，机器人重启后生效。
QUESTIONS = [
    {"id": "use", "label": "你主要用公益站画什么？用的什么软件 / 客户端？", "style": "short", "required": False, "max": 200},
    {"id": "score", "label": "整体体验打几分？（1～10）", "style": "short", "required": True, "max": 10},
    {"id": "pain", "label": "最不满意的地方 / 遇到过的问题", "style": "long", "required": False, "max": 1000},
    {"id": "wish", "label": "最希望增加或改进什么？", "style": "long", "required": False, "max": 1000},
    {"id": "other", "label": "还有什么想对站长说的？", "style": "long", "required": False, "max": 1000},
]
COOLDOWN = 600          # 同一个人 10 分钟内只收一份，防刷


def clean(answers: dict) -> dict:
    out = {}
    for q in QUESTIONS:
        v = str((answers or {}).get(q["id"], "") or "").strip()[:q["max"]]
        if v:
            out[q["id"]] = v
    return out


async def submit(db, discord_id: str, username: str, key_id, answers: dict) -> str:
    """存一份反馈。返回给本人看的回执文字；不合格抛 ValueError。"""
    a = clean(answers)
    if not a:
        raise ValueError("表单是空的，没有收到内容。")
    last = await db._db.execute_fetchall("SELECT MAX(ts) FROM feedback WHERE discord_id=?", (discord_id,))
    if last and last[0][0] and time.time() - last[0][0] < COOLDOWN:
        raise ValueError("刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。")
    await db._db.execute("INSERT INTO feedback (ts, discord_id, username, key_id, answers) VALUES (?,?,?,?,?)",
                         (time.time(), discord_id, username[:80], key_id, json.dumps(a, ensure_ascii=False)))
    await db._db.commit()
    return "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"


async def listing(db, limit: int = 200) -> list[dict]:
    rows = await db._db.execute_fetchall(
        "SELECT f.id, f.ts, f.discord_id, f.username, f.key_id, f.answers, k.name FROM feedback f "
        "LEFT JOIN api_keys k ON k.id=f.key_id ORDER BY f.ts DESC LIMIT ?", (limit,))
    return [{"id": r[0], "ts": r[1], "discord_id": r[2], "username": r[3], "key_id": r[4],
             "answers": json.loads(r[5] or "{}"), "key_name": r[6] or ""} for r in rows]


def to_csv(items: list[dict]) -> bytes:
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(["time", "discord_id", "username", "key_id", "key_name"] + [q["label"] for q in QUESTIONS])
    for it in sorted(items, key=lambda x: x["ts"]):
        w.writerow([datetime.fromtimestamp(it["ts"]).strftime("%Y-%m-%d %H:%M:%S"), it["discord_id"], it["username"],
                    it["key_id"] or "", it["key_name"]] + [it["answers"].get(q["id"], "") for q in QUESTIONS])
    return ("﻿" + buf.getvalue()).encode("utf-8")
