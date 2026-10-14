"""状态页事件命令行（deploy/ops/incident.py）的 golden 夹具，供 gate-ops 的 `incident` 子命令对拍。

做法：把 incident.py 里真正在 nai-gate 容器里跑的那段 `PROGRAM` 原样 exec，数据库指到临时目录，
`status_page.time` 打桩成每一步固定的时间戳；每个场景从全新的库开始，逐步执行 open / update / resolve / list，
记录每一步的 stdout，最后把 `incidents` 与 `incident_updates` 两张表的整表行存下来。

夹具里只有合成数据（标题 / 正文都是编的）。输出：crates/gate-ops/tests/golden/ops_incident.jsonl

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/ops_incident.py
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import types
from pathlib import Path

import _golden
from _golden import ROOT, Writer

# 这份夹具属于 gate-ops，不放进 gate-core 的 tests/golden
_golden.OUT_DIR = ROOT / "crates" / "gate-ops" / "tests" / "golden"

# app.config 在 import 时读 DATA_DIR，必须先设
WORK = Path(tempfile.mkdtemp(prefix="ops-incident-golden-"))
os.environ["DATA_DIR"] = str(WORK)
os.environ.pop("NAI_TOKENS", None)
os.environ.pop("NAI_TOKEN", None)

spec = importlib.util.spec_from_file_location("incident_cli", ROOT / "deploy" / "ops" / "incident.py")
incident_cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(incident_cli)
PROGRAM = incident_cli.PROGRAM

from app import status_page  # noqa: E402  必须在设置 DATA_DIR 之后

DB = WORK / "nai_gate.db"


def reset_db() -> None:
    """全新的库：先按线上启动的顺序建好表（Database.connect + status_page.ensure_schema，
    incident.py 的 PROGRAM 默认这两张表已经存在——线上由网关启动时建好）。"""
    import asyncio

    from app.config import load_settings
    from app.database import Database

    for suffix in ("", "-wal", "-shm"):
        Path(str(DB) + suffix).unlink(missing_ok=True)

    async def prepare() -> None:
        settings = load_settings()
        db = Database(str(settings.db_path), settings.tz)
        await db.connect()
        await status_page.ensure_schema(db)
        await db.close()

    asyncio.run(prepare())


def run_step(action: str, payload: dict, now: float) -> str:
    """原样执行容器里的那段程序，返回它的 stdout。"""
    status_page.time = types.SimpleNamespace(time=lambda: now)
    saved_argv = sys.argv
    sys.argv = ["-", action, json.dumps(payload, ensure_ascii=False)]
    buf = io.StringIO()
    try:
        with contextlib.redirect_stdout(buf):
            exec(compile(PROGRAM, "<incident PROGRAM>", "exec"), {"__name__": "__main__"})
    finally:
        sys.argv = saved_argv
    return buf.getvalue()


def dump_table(sql: str) -> list[dict]:
    conn = sqlite3.connect(DB)
    try:
        cur = conn.execute(sql)
        names = [d[0] for d in cur.description]
        return [dict(zip(names, row)) for row in cur.fetchall()]
    finally:
        conn.close()


def open_p(title, body, impact="minor", components="api", status="identified"):
    return {"title": title, "body": body, "impact": impact, "components": components, "status": status}


T0 = 1790000000.0

SCENARIOS: dict[str, list[tuple[str, dict, float]]] = {
    # 完整生命周期：发布 → 观察 → 解决 → 列表
    "lifecycle": [
        ("open", open_p("出图接口请求超时", "问题已定位，正在实施修复。"), T0 + 0.25),
        ("update", {"id": 1, "status": "monitoring", "body": "修复已上线，我们正在观察结果。"}, T0 + 600.5),
        ("resolve", {"id": 1, "body": "此事件已解决。"}, T0 + 1200.125),
        ("list", {"limit": 5}, T0 + 1300.0),
    ],
    # 发布时状态就是 resolved：resolved_at 等于发布时间
    "open_already_resolved": [
        ("open", open_p("部分 V5 出图画质下降", "此事件已解决。", impact="none", status="resolved"), T0 + 5000.75),
        ("list", {"limit": 5}, T0 + 5001.0),
    ],
    # 多组件 + 严重度；组件 JSON 用 Python 默认分隔符 ", "
    "multi_components": [
        ("open", open_p("NovelAI 上游生图中断", "我们正在调查此问题。", impact="critical",
                        components="upstream,api", status="investigating"), T0 + 100.5),
        ("update", {"id": 1, "status": "identified", "body": "问题已定位，正在实施修复。"}, T0 + 160.5),
        ("update", {"id": 1, "status": "update", "body": "我们正在继续处理此问题。"}, T0 + 220.5),
        ("list", {"limit": 5}, T0 + 230.0),
    ],
    # 解决后又追加非 resolved 的更新：resolved_at 被改回 NULL（重新打开）
    "reopen_after_resolve": [
        ("open", open_p("官网与后台访问缓慢", "我们正在调查此问题。", impact="major",
                        components="web", status="investigating"), T0 + 10.0),
        ("resolve", {"id": 1, "body": "此事件已解决。"}, T0 + 70.0),
        ("update", {"id": 1, "status": "identified", "body": "问题复现，已重新定位。"}, T0 + 130.0),
        ("list", {"limit": 5}, T0 + 131.0),
        ("resolve", {"id": 1, "body": "再次解决。"}, T0 + 190.0),
        ("list", {"limit": 5}, T0 + 191.0),
    ],
    # 多个事件：id 自增、list 按 id 倒序并受 limit 限制
    "several_incidents": [
        ("open", open_p("出图接口例行维护", "我们正在进行例行维护，期间出图接口可能有短暂中断。", impact="minor"), T0 + 1.0),
        ("open", open_p("Discord 机器人响应延迟", "我们正在调查此问题。", impact="minor",
                        components="bot", status="investigating"), T0 + 2.0),
        ("open", open_p("出图接口请求超时", "问题已定位，正在实施修复。", impact="major",
                        components="api,upstream"), T0 + 3.0),
        ("resolve", {"id": 2, "body": "此事件已解决。"}, T0 + 4.0),
        ("list", {"limit": 2}, T0 + 5.0),
        ("list", {"limit": 5}, T0 + 6.0),
        ("list", {"limit": 1}, T0 + 7.0),
    ],
    # 非 ASCII / 引号 / 换行：正文原样入库；标题里的 emoji 与中文标点
    "unicode_and_newlines": [
        ("open", open_p("出图接口「部分」请求失败 🦉", "第一行\n第二行 \"带引号\" 与 'single' 与 <b>标签</b>"), T0 + 42.5),
        ("update", {"id": 1, "status": "monitoring", "body": "多行\n正文\t带制表符"}, T0 + 43.5),
        ("list", {"limit": 5}, T0 + 44.0),
    ],
    # 空库 list：什么都不输出
    "empty_list": [
        ("list", {"limit": 5}, T0),
    ],
    # 时间戳是整数值的 float 与带长小数的 float
    "timestamp_shapes": [
        ("open", open_p("时间戳整数值", "我们正在调查此问题。", status="investigating"), 1790003600.0),
        ("update", {"id": 1, "status": "monitoring", "body": "修复已上线，我们正在观察结果。"}, 1790003600.123456),
        ("resolve", {"id": 1, "body": "此事件已解决。"}, 1790007200.999999),
    ],
}


def main() -> None:
    with Writer("ops_incident") as w:
        for name, steps in SCENARIOS.items():
            reset_db()
            recorded = []
            for action, payload, now in steps:
                out = run_step(action, payload, now)
                recorded.append({"action": action, "payload": payload, "now": now, "stdout": out})
            w.record(
                scenario=name,
                steps=recorded,
                incidents=dump_table("SELECT * FROM incidents ORDER BY id"),
                updates=dump_table("SELECT * FROM incident_updates ORDER BY id"),
            )
    # 常量也对拍：标准句式 / 组件 / 严重度 / 状态词表，改了旧版这里就会变
    with Writer("ops_incident_constants") as w:
        w.record(
            standard_body=status_page.STANDARD_BODY,
            components=[c[0] for c in status_page.COMPONENTS],
            shown=list(status_page.SHOWN),
            impacts=list(status_page.IMPACT_RANK),
            statuses=list(status_page.STATUS_LABEL),
        )
    shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    main()
