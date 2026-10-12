#!/usr/bin/env python3
"""按 Statuspage 流程发布 / 更新状态页事件（在服务器上跑）。

以前每次都得往容器里 docker cp 一个小脚本；现在一条命令：

    python3 deploy/ops/incident.py open --title "出图接口例行维护" --body "…"
    python3 deploy/ops/incident.py update 14 --status monitoring --body "升级已上线，正在观察"
    python3 deploy/ops/incident.py resolve 14 --body "维护完成，一切正常"
    python3 deploy/ops/incident.py list

状态取值：investigating（调查中）/ identified（已定位）/ monitoring（观察中）/ resolved（已解决）。
写法照 status_page.py 的约定：标题 = 功能 + 现象，正文只写成员能感知的影响，不写内部细节。

实现：把一段小程序用 stdin 交给 nai-gate 容器里的 python 执行（容器里有 app 模块和数据库路径），
宿主机不需要装任何依赖。
"""
from __future__ import annotations

import argparse
import subprocess
import sys

CONTAINER = "nai-gate"
DEFAULT_BODY = "我们正在进行例行维护，期间出图接口可能有短暂中断。"

PROGRAM = r"""
import asyncio, json, sys
from app import status_page
from app.config import load_settings
from app.database import Database

ACTION, PAYLOAD = sys.argv[1], json.loads(sys.argv[2])

async def main():
    settings = load_settings()
    db = Database(str(settings.db_path), settings.tz)
    await db.connect()
    try:
        if ACTION == "open":
            incident_id = await status_page.create_incident(
                db, PAYLOAD["title"], PAYLOAD["impact"],
                [c for c in PAYLOAD["components"].split(",") if c], PAYLOAD["status"], PAYLOAD["body"])
            print(f"created {incident_id}")
        elif ACTION == "update":
            await status_page.add_update(db, int(PAYLOAD["id"]), PAYLOAD["status"], PAYLOAD["body"])
            print(f"updated {PAYLOAD['id']}")
        elif ACTION == "resolve":
            # add_update 里 status=resolved 会顺手写 resolved_at，不用另外调函数
            await status_page.add_update(db, int(PAYLOAD["id"]), "resolved", PAYLOAD["body"])
            print(f"resolved {PAYLOAD['id']}")
        else:
            rows = await db._db.execute_fetchall(
                "SELECT id, title, impact, resolved_at FROM incidents ORDER BY id DESC LIMIT ?",
                (int(PAYLOAD.get("limit", 5)),))
            for row in rows:
                print(f"#{row[0]} [{'已解决' if row[3] else '进行中'}] {row[2]} {row[1]}")
    finally:
        await db.close()

asyncio.run(main())
"""


def run(action: str, payload: dict) -> int:
    import json
    proc = subprocess.run(
        ["docker", "exec", "-i", "-e", "PYTHONPATH=/app", "-w", "/app", CONTAINER,
         "python", "-", action, json.dumps(payload, ensure_ascii=False)],
        input=PROGRAM, text=True, capture_output=True)
    sys.stdout.write(proc.stdout)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-1500:])
    return proc.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description="发布 / 更新状态页事件")
    sub = parser.add_subparsers(dest="action", required=True)
    open_p = sub.add_parser("open", help="发布新事件")
    open_p.add_argument("--title", required=True)
    open_p.add_argument("--body", default=DEFAULT_BODY)
    open_p.add_argument("--impact", default="minor", choices=("none", "minor", "major", "critical"))
    open_p.add_argument("--components", default="api", help="upstream / api（状态页只展示这两个）")
    open_p.add_argument("--status", default="identified",
                        choices=("investigating", "identified", "monitoring", "resolved"))
    for name in ("update", "resolve"):
        p = sub.add_parser(name, help="追加更新 / 标记已解决")
        p.add_argument("id", type=int)
        p.add_argument("--body", default=DEFAULT_BODY)
        p.add_argument("--status", default="monitoring" if name == "update" else "resolved",
                       choices=("investigating", "identified", "monitoring", "update", "resolved"))
    list_p = sub.add_parser("list", help="最近事件")
    list_p.add_argument("--limit", type=int, default=5)
    args = parser.parse_args()

    if args.action == "open":
        return run("open", {"title": args.title, "body": args.body, "impact": args.impact,
                            "components": args.components, "status": args.status})
    if args.action == "update":
        return run("update", {"id": args.id, "status": args.status, "body": args.body})
    if args.action == "resolve":
        return run("resolve", {"id": args.id, "body": args.body})
    return run("list", {"limit": args.limit})


if __name__ == "__main__":
    sys.exit(main())
