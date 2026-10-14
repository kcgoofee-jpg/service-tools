"""从旧 Python 代码导出建表 SQL，并用 Python 的 connect() 建一个全新库、导出 sqlite_master 作为 gate-db 的对照。

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/dump_schema.py
"""
from __future__ import annotations

import asyncio
import json
import sqlite3
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from app import database, status_page  # noqa: E402

OUT = ROOT / "crates" / "gate-db"
(OUT / "src" / "sql" / "schema.sql").write_text(database.SCHEMA.strip() + "\n", encoding="utf-8")
(OUT / "src" / "sql" / "status_schema.sql").write_text(status_page.SCHEMA.strip() + "\n", encoding="utf-8")


def master(path: str) -> list[dict]:
    con = sqlite3.connect(path)
    rows = con.execute("SELECT type, name, tbl_name, sql FROM sqlite_master "
                       "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name").fetchall()
    cols = {}
    for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"):
        cols[t] = [list(r) for r in con.execute(f"PRAGMA table_info({t})")]
    con.close()
    return {"objects": [{"type": a, "name": b, "tbl_name": c, "sql": d} for a, b, c, d in rows], "columns": cols}


async def main() -> None:
    with tempfile.TemporaryDirectory() as d:
        path = str(Path(d) / "fresh.db")
        db = database.Database(path)
        await db.connect()
        await status_page.ensure_schema(db)
        await db.close()
        ref = master(path)
    (OUT / "tests" / "fixtures" / "python_master.json").write_text(
        json.dumps(ref, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print("tables:", len(ref["columns"]), "objects:", len(ref["objects"]))


asyncio.run(main())
