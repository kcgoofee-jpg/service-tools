"""公开状态页 /status：版式照搬 status.claude.com（Atlassian Statuspage 标准），不自创流程。

· 事件（incident）按 Statuspage 标准生命周期发布：调查中 → 已定位 → 观察中 → 已解决（中途可发「更新」）。
  每次更新带时间，最新的在上面；未解决的事件显示在页首的彩色横幅里。
· 组件 90 天可用率条：每天的颜色 = 当天影响该组件的最严重事件；出图接口另外参考当天 5xx 占比。
  开始记录之前的日子是灰色「无数据」，不编造。
· 历史事件按天列出最近 15 天，没有事件的日子写「当天没有事件」。
站长 2026-10-10：后续所有 bug 按标准修复，修复进度发在这里。
"""
from __future__ import annotations

import html
import json
import time
from datetime import datetime, timedelta
from typing import Any, Optional

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse

SCHEMA = """
CREATE TABLE IF NOT EXISTS incidents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    impact TEXT NOT NULL DEFAULT 'minor',      -- none / minor / major / critical（Statuspage 的四级）
    components TEXT NOT NULL DEFAULT '[]',     -- JSON 数组，取值见 COMPONENTS
    started_at REAL NOT NULL,
    resolved_at REAL
);
CREATE TABLE IF NOT EXISTS incident_updates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    incident_id INTEGER NOT NULL,
    status TEXT NOT NULL,                      -- investigating / identified / update / monitoring / resolved
    body TEXT NOT NULL,
    at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_incident_updates ON incident_updates(incident_id, at);
"""

COMPONENTS = [
    ("api", "出图接口（gate.davidzhao.top）"),
    ("web", "官网与后台"),
    ("bot", "Discord 机器人（奶妹）"),
    ("upstream", "上游 NovelAI"),
]
STATUS_LABEL = {"investigating": "调查中", "identified": "已定位", "update": "更新",
                "monitoring": "观察中", "resolved": "已解决"}
IMPACT_RANK = {"none": 0, "minor": 1, "major": 2, "critical": 3}
IMPACT_COMPONENT = {0: "正常运行", 1: "性能下降", 2: "部分中断", 3: "严重中断"}
TRACKING_START = "2026-10-09"          # usage_log 第一条：10-09 18:18
DAYS = 90
HISTORY_DAYS = 15

router = APIRouter()


async def ensure_schema(db) -> None:
    await db._db.executescript(SCHEMA)
    await db._db.commit()


def _fmt(ts: float) -> str:
    t = datetime.fromtimestamp(ts)
    return f"{t.month} 月 {t.day} 日 {t:%H:%M}（北京时间）"


async def list_incidents(db, since: float = 0.0) -> list[dict[str, Any]]:
    rows = await db._db.execute_fetchall(
        "SELECT id, title, impact, components, started_at, resolved_at FROM incidents "
        "WHERE COALESCE(resolved_at, 9e18) >= ? ORDER BY started_at DESC", (since,))
    out = []
    for r in rows:
        ups = await db._db.execute_fetchall(
            "SELECT status, body, at FROM incident_updates WHERE incident_id=? ORDER BY at DESC, id DESC", (r[0],))
        out.append({"id": r[0], "title": r[1], "impact": r[2], "components": json.loads(r[3] or "[]"),
                    "started_at": r[4], "resolved_at": r[5],
                    "updates": [{"status": u[0], "body": u[1], "at": u[2]} for u in ups]})
    return out


async def create_incident(db, title: str, impact: str, components: list[str], status: str, body: str,
                          at: Optional[float] = None) -> int:
    at = time.time() if at is None else at
    cur = await db._db.execute(
        "INSERT INTO incidents(title, impact, components, started_at, resolved_at) VALUES (?,?,?,?,?)",
        (title, impact, json.dumps(components), at, at if status == "resolved" else None))
    await db._db.execute("INSERT INTO incident_updates(incident_id, status, body, at) VALUES (?,?,?,?)",
                         (cur.lastrowid, status, body, at))
    await db._db.commit()
    return int(cur.lastrowid)


async def add_update(db, incident_id: int, status: str, body: str, at: Optional[float] = None) -> None:
    at = time.time() if at is None else at
    await db._db.execute("INSERT INTO incident_updates(incident_id, status, body, at) VALUES (?,?,?,?)",
                         (incident_id, status, body, at))
    await db._db.execute("UPDATE incidents SET resolved_at=? WHERE id=?",
                         (at if status == "resolved" else None, incident_id))
    await db._db.commit()


async def _api_error_days(db, start: float) -> dict[str, float]:
    """出图接口每天的 5xx 占比（status_hourly）。"""
    out: dict[str, list[int]] = {}
    for hour, code, n in await db._db.execute_fetchall(
            "SELECT hour, code, n FROM status_hourly WHERE hour >= ?", (int(start // 3600),)):
        d = datetime.fromtimestamp(hour * 3600).strftime("%Y-%m-%d")
        tot = out.setdefault(d, [0, 0])
        tot[0] += n
        if code >= 500:
            tot[1] += n
    return {d: (bad / total if total else 0.0) for d, (total, bad) in out.items()}


async def snapshot(db, now: Optional[float] = None) -> dict[str, Any]:
    now = time.time() if now is None else now
    today = datetime.fromtimestamp(now).date()
    first = today - timedelta(days=DAYS - 1)
    start = datetime.combine(first, datetime.min.time()).timestamp()
    incidents = await list_incidents(db, since=start)
    errs = await _api_error_days(db, start)
    comps = []
    for key, name in COMPONENTS:
        days, down_minutes, tracked_minutes = [], 0.0, 0.0
        for i in range(DAYS):
            d = first + timedelta(days=i)
            ds = d.isoformat()
            d0 = datetime.combine(d, datetime.min.time()).timestamp()
            d1 = min(d0 + 86400, now)
            if ds < TRACKING_START:
                days.append({"day": ds, "level": None, "titles": []})
                continue
            level, titles = 0, []
            for inc in incidents:
                if key not in inc["components"]:
                    continue
                s, e = inc["started_at"], inc["resolved_at"] or now
                if s < d1 and e > d0:
                    rank = IMPACT_RANK.get(inc["impact"], 1)
                    level = max(level, rank)
                    titles.append(inc["title"])
                    if rank >= 2:
                        down_minutes += (min(e, d1) - max(s, d0)) / 60
            if key == "api" and errs.get(ds, 0) > 0.01:
                level = max(level, 1)
            tracked_minutes += max(0.0, d1 - d0) / 60
            days.append({"day": ds, "level": level, "titles": titles})
        live = [inc for inc in incidents if inc["resolved_at"] is None and key in inc["components"]]
        now_level = max([IMPACT_RANK.get(i["impact"], 1) for i in live], default=0)
        uptime = 100.0 * (1 - down_minutes / tracked_minutes) if tracked_minutes else None
        comps.append({"key": key, "name": name, "days": days, "uptime": uptime,
                      "status": IMPACT_COMPONENT[now_level], "level": now_level})
    return {"now": now, "components": comps, "incidents": incidents,
            "active": [i for i in incidents if i["resolved_at"] is None]}


# ------------------------------------------------------------------ 页面
COLORS = {None: "#c9ccd1", 0: "#76ad2a", 1: "#e3b341", 2: "#e8743b", 3: "#e5484d"}
TEXT_COLOR = {0: "#76ad2a", 1: "#d29a12", 2: "#e8743b", 3: "#e5484d"}
BANNER = {"none": "#76ad2a", "minor": "#e3a21a", "major": "#e8743b", "critical": "#e5484d"}

CSS = """
*{box-sizing:border-box}body{margin:0;background:#faf9f5;color:#1a1a1a;
font-family:-apple-system,BlinkMacSystemFont,"Helvetica Neue","PingFang SC","Microsoft YaHei",sans-serif;
-webkit-font-smoothing:antialiased}
.wrap{max-width:860px;margin:0 auto;padding:48px 16px 80px}
header{display:flex;align-items:center;justify-content:space-between;gap:16px;flex-wrap:wrap;margin-bottom:56px}
.logo{display:flex;align-items:center;gap:10px;font-size:30px;font-weight:600;letter-spacing:-.01em;color:#1a1a1a;text-decoration:none}
.logo span.mark{font-size:30px}
.sub{background:#1a1a1a;color:#fff;border-radius:6px;padding:12px 20px;font-size:13px;font-weight:600;letter-spacing:.12em;text-decoration:none}
.banner{border-radius:4px 4px 0 0;padding:20px 24px;color:#fff;font-size:20px;font-weight:600;display:flex;justify-content:space-between;align-items:center}
.ok{background:#76ad2a;border-radius:4px;padding:20px 24px;color:#fff;font-size:20px;font-weight:600;margin-bottom:48px}
.inc{border:1px solid;border-top:0;border-radius:0 0 4px 4px;padding:8px 24px 16px;margin-bottom:40px;background:#faf9f5}
.upd{margin:16px 0}.upd p{margin:0 0 4px;line-height:1.6;font-size:16px}.upd b{font-weight:700}
.when{color:#8a8a8a;font-size:14px}
.note{text-align:right;color:#8a8a8a;font-size:14px;margin:0 0 8px}
.box{border:1px solid #e3e1db;border-radius:4px;margin-bottom:56px}
.comp{padding:22px 24px;border-bottom:1px solid #e3e1db}.comp:last-child{border-bottom:0}
.row{display:flex;justify-content:space-between;gap:12px;align-items:baseline;flex-wrap:wrap}
.name{font-size:17px;font-weight:600}.state{font-size:15px}
.bars{display:flex;gap:2px;margin:14px 0 8px;height:34px}
.bars i{flex:1;border-radius:1px;min-width:1px}
.legend{display:flex;align-items:center;gap:12px;color:#8a8a8a;font-size:13px}
.legend hr{flex:1;border:0;border-top:1px solid #d4d2cc;margin:0}
h2{font-size:26px;font-weight:600;margin:0 0 24px}
.day{border-bottom:1px solid #e3e1db;padding-bottom:6px;margin:32px 0 16px;font-size:19px;font-weight:600}
.none{color:#8a8a8a;font-size:15px}
.title{font-size:19px;font-weight:600;margin:18px 0 8px;color:#e8743b}
.title.minor{color:#d29a12}.title.critical{color:#e5484d}.title.none{color:#1a1a1a}
footer{color:#8a8a8a;font-size:13px;margin-top:56px;border-top:1px solid #e3e1db;padding-top:16px}
@media(max-width:600px){.wrap{padding-top:28px}header{margin-bottom:32px}.logo{font-size:24px}.banner,.ok{font-size:17px}.bars{gap:1px;height:28px}}
"""


def _updates_html(updates: list[dict]) -> str:
    out = []
    for u in updates:
        out.append(f'<div class="upd"><p><b>{STATUS_LABEL.get(u["status"], u["status"])}</b> - '
                   f'{html.escape(u["body"])}</p><div class="when">{_fmt(u["at"])}</div></div>')
    return "".join(out)


def render(snap: dict[str, Any]) -> str:
    parts = [f"<!doctype html><html lang=zh-CN><head><meta charset=utf-8>"
             f"<meta name=viewport content='width=device-width,initial-scale=1'>"
             f"<title>猫头鹰公益站 · 运行状态</title><style>{CSS}</style></head><body><div class=wrap>",
             '<header><a class=logo href="/status"><span class=mark>🦉</span>猫头鹰公益站 状态</a>'
             '<a class=sub href="/">返回官网</a></header>']
    if snap["active"]:
        for inc in snap["active"]:
            c = BANNER.get(inc["impact"], "#e8743b")
            parts.append(f'<div class=banner style="background:{c}">{html.escape(inc["title"])}</div>'
                         f'<div class=inc style="border-color:{c}">{_updates_html(inc["updates"])}</div>')
    else:
        parts.append('<div class=ok>所有服务运行正常</div>')
    parts.append(f'<p class=note>过去 {DAYS} 天的可用率。</p><div class=box>')
    for comp in snap["components"]:
        bars = "".join(
            f'<i style="background:{COLORS[d["level"]]}" title="{d["day"]}：'
            f'{"无数据" if d["level"] is None else (html.escape("；".join(d["titles"])) or "没有事件")}"></i>'
            for d in comp["days"])
        up = f'{comp["uptime"]:.2f} % 可用' if comp["uptime"] is not None else "无数据"
        parts.append(f'<div class=comp><div class=row><span class=name>{html.escape(comp["name"])}</span>'
                     f'<span class=state style="color:{TEXT_COLOR[comp["level"]]}">{comp["status"]}</span></div>'
                     f'<div class=bars>{bars}</div><div class=legend><span>{DAYS} 天前</span><hr>'
                     f'<span>{up}</span><hr><span>今天</span></div></div>')
    parts.append('</div><h2>历史事件</h2>')
    today = datetime.fromtimestamp(snap["now"]).date()
    for i in range(HISTORY_DAYS):
        d = today - timedelta(days=i)
        d0 = datetime.combine(d, datetime.min.time()).timestamp()
        day_incs = [inc for inc in snap["incidents"] if d0 <= inc["started_at"] < d0 + 86400]
        parts.append(f'<div class=day>{d.year} 年 {d.month} 月 {d.day} 日</div>')
        if not day_incs:
            parts.append(f'<div class=none>{"今天没有事件。" if i == 0 else "当天没有事件。"}</div>')
        for inc in day_incs:
            parts.append(f'<div class="title {inc["impact"]}">{html.escape(inc["title"])}</div>'
                         f'{_updates_html(inc["updates"])}')
    parts.append('<footer>事件按标准流程更新：调查中 → 已定位 → 观察中 → 已解决。时间均为北京时间。'
                 '有问题请到 Discord 的 🛠️｜问题反馈 频道告诉我们。</footer></div></body></html>')
    return "".join(parts)


STATUS_HEADERS = {"Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'",
                  "Cache-Control": "no-cache", "X-Content-Type-Options": "nosniff"}


@router.get("/status", response_class=HTMLResponse)
async def status_page(request: Request):
    snap = await snapshot(request.app.state.gate.db)
    return HTMLResponse(render(snap), headers=STATUS_HEADERS)


@router.get("/status.json")
async def status_json(request: Request):
    snap = await snapshot(request.app.state.gate.db)
    return JSONResponse({"components": [{k: c[k] for k in ("key", "name", "status", "uptime")} for c in snap["components"]],
                         "active": snap["active"], "incidents": snap["incidents"][:50]},
                        headers={"Cache-Control": "no-cache"})


# ------------------------------------------------------------------ 后台接口（站长 / Claude 发布事件）
admin_router = APIRouter(prefix="/admin/api")


def _check(body: dict, need_title: bool) -> tuple[str, str]:
    status = str(body.get("status", ""))
    text = str(body.get("body", "")).strip()
    if status not in STATUS_LABEL or not text:
        raise HTTPException(422, "status 必须是 investigating / identified / update / monitoring / resolved，body 不能为空")
    if need_title and not str(body.get("title", "")).strip():
        raise HTTPException(422, "需要 title")
    return status, text[:2000]


@admin_router.post("/incidents")
async def admin_create_incident(request: Request):
    from .admin import require_admin
    from .body import read_json_body
    require_admin(request)
    body = await read_json_body(request)
    status, text = _check(body, True)
    impact = body.get("impact", "minor")
    if impact not in IMPACT_RANK:
        raise HTTPException(422, "impact 必须是 none / minor / major / critical")
    comps = [c for c in body.get("components", []) if c in dict(COMPONENTS)] or ["api"]
    at = float(body["at"]) if isinstance(body.get("at"), (int, float)) else None
    iid = await create_incident(request.app.state.gate.db, str(body["title"]).strip()[:200], impact, comps, status, text, at)
    return {"id": iid}


@admin_router.post("/incidents/{incident_id}/updates")
async def admin_add_update(request: Request, incident_id: int):
    from .admin import require_admin
    from .body import read_json_body
    require_admin(request)
    body = await read_json_body(request)
    status, text = _check(body, False)
    db = request.app.state.gate.db
    if not await db._db.execute_fetchall("SELECT 1 FROM incidents WHERE id=?", (incident_id,)):
        raise HTTPException(404, "事件不存在")
    at = float(body["at"]) if isinstance(body.get("at"), (int, float)) else None
    await add_update(db, incident_id, status, text, at)
    return {"ok": True}


@admin_router.get("/incidents")
async def admin_list_incidents(request: Request):
    from .admin import require_admin
    require_admin(request)
    return {"incidents": await list_incidents(request.app.state.gate.db)}
