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
IMPACT_RANK = {"none": 0, "minor": 1, "major": 2, "critical": 3}
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
            "SELECT hour, code, n FROM status_hourly WHERE hour >= ?", (int(start),)):
        d = datetime.fromtimestamp(hour).strftime("%Y-%m-%d")        # hour 存的是整点的秒数（status_stats.record）
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
# 照 status.claude.com（Atlassian Statuspage 标准模板）：结构 / 类名 / 尺寸 / 颜色取自它的 HTML 与 status_manifest.css
# （2026-10-10 站长：字体、结构、文案都要照它）。文案用 Statuspage 自带的中文本地化措辞。
STATUS_LABEL = {"investigating": "调查中", "identified": "已确定", "update": "更新",
                "monitoring": "监控中", "resolved": "已解决"}
IMPACT_COMPONENT = {0: "运行正常", 1: "性能下降", 2: "部分中断", 3: "严重中断"}
BAR = {None: "#B3BAC5", 0: "#76AD2A", 1: "#FAA72A", 2: "#E86235", 3: "#E04343"}
IMPACT_COLOR = {"none": "#333333", "minor": "#FAA72A", "major": "#E86235", "critical": "#E04343"}
STATUS_TEXT = {0: "#76AD2A", 1: "#FAA72A", 2: "#E86235", 3: "#E04343"}

CSS = """
html{-webkit-text-size-adjust:100%}
body{margin:0;background-color:#FAF9F5;color:#141413;font-family:"Atlassian Sans","Helvetica Neue",Helvetica,Arial,"PingFang SC","Hiragino Sans GB","Microsoft YaHei",sans-serif;
font-weight:400;font-size:16px;line-height:24px;-webkit-font-smoothing:antialiased}
a{color:inherit;text-decoration:none}
small{font-size:.875rem;line-height:1.334375rem;color:#87867F}
.layout-content{width:90%;max-width:850px;margin:0 auto;padding-bottom:3rem}
.font-regular{font-size:1rem;line-height:1.5rem}
.font-large{font-weight:500;font-size:1.25rem;line-height:1.8125rem}
.font-largest{font-weight:500;font-size:1.75rem;line-height:2.3625rem}
.masthead{padding-top:70px;margin-bottom:70px;display:flex;align-items:center;justify-content:space-between;gap:16px}
.logo{display:flex;align-items:center;gap:.5rem;font-family:"Copernicus","Tiempos Headline",Georgia,"Songti SC","STSong",serif;
font-size:2.6rem;line-height:1;letter-spacing:-.02em;color:#141413;white-space:nowrap}
.logo .mark{font-size:2.2rem}
.show-updates-dropdown{background:#141413;color:#fff;border-radius:4px;padding:.8rem 1.6rem;font-size:.8125rem;font-weight:600;letter-spacing:.14em;white-space:nowrap}
.page-status{font-weight:500;border-radius:4px;border:1px solid rgba(0,0,0,.1);text-shadow:0 1px 0 rgba(0,0,0,.1);margin-bottom:70px;padding:.75rem 1.25rem;background:#76AD2A;color:#fff;font-size:1.25rem;line-height:1.8125rem}
.unresolved-incidents{margin-bottom:70px}
.unresolved-incident{margin-top:25px}.unresolved-incident:first-of-type{margin-top:0}
.unresolved-incident .incident-title{text-shadow:0 1px 0 rgba(0,0,0,.2);padding:.85rem 1.25rem .75rem;color:#fff;display:flex;justify-content:space-between;gap:1rem;border-radius:4px 4px 0 0}
.unresolved-incident .incident-title .subscribe{font-size:1rem;font-weight:500;white-space:nowrap}
.unresolved-incident .updates{padding:1.25rem;border-style:solid;border-width:1px;border-top:none;border-radius:0 0 4px 4px}
.update{margin-bottom:20px;overflow-wrap:break-word}.update:last-of-type{margin-bottom:0}
.whitespace-pre-wrap{white-space:pre-wrap}
.components-uptime-link{text-align:right;font-size:.85em;color:#87867F;margin-bottom:.4rem}
.components-section{margin-bottom:70px}
.component-container{padding:1.1rem 1.25rem 1rem;border:1px solid #DEDCD1;border-top-width:0}
.component-container:first-child{border-top-width:1px;border-radius:4px 4px 0 0}
.component-container:last-child{border-radius:0 0 4px 4px}
.component-inner-container{display:flex;justify-content:space-between;align-items:baseline;gap:1rem}
.component-container .name{font-weight:500;color:rgba(20,20,19,.8);overflow:hidden;white-space:nowrap;text-overflow:ellipsis;max-width:75%}
.component-container .component-status{font-size:.875rem;white-space:nowrap}
.uptime-90-days-wrapper{padding-top:5px;margin-bottom:-2px}
.uptime-90-days-wrapper svg{display:block;margin:0;padding:0;height:34px;width:100%;overflow:hidden}
.uptime-90-days-wrapper svg rect:hover{fill:#5e6c84}
.legend{display:flex;flex-direction:row;justify-content:space-between;position:relative;top:-2px}
.legend .legend-item{flex:0 0 auto;font-size:.875rem;color:#87867f}
.legend .spacer{flex:1;margin:.75rem 1rem 0 1rem;height:1px;background:#87867f;opacity:.3}
.incidents-list{margin-top:70px}
.incidents-list h2{margin:0}
.status-day{margin-top:35px}.status-day:nth-child(2){margin-top:20px}
.status-day .date{font-weight:500;border-bottom:1px solid #DEDCD1;padding-bottom:3px;margin:0 0 10px}
.status-day p{margin:0}
.color-secondary{color:#87867F}
.incident-container{margin-bottom:1.5rem}
.incident-container .incident-title{margin:.5rem 0}
.incident-title.impact-none a{color:#141413}.incident-title.impact-minor a{color:#FAA72A}
.incident-title.impact-major a{color:#E86235}.incident-title.impact-critical a{color:#E04343}
.updates-container .update{margin:0 0 1rem}
.page-footer{margin-top:3rem;border-top:1px solid #DEDCD1;padding-top:1rem;display:flex;justify-content:space-between;gap:1rem;flex-wrap:wrap}
@media(max-width:768px){.masthead{padding-top:60px;margin-bottom:60px}.page-status,.unresolved-incidents,.components-section{margin-bottom:60px}
.incidents-list{margin-top:60px}.font-largest{font-size:1.375rem;line-height:1.959375rem}.font-large{font-size:1.125rem;line-height:1.659375rem}
.component-container{padding:.85rem 1rem .75rem}.logo{font-size:2rem}.logo .mark{font-size:1.7rem}}
@media(max-width:450px){.masthead{padding-top:50px;margin-bottom:50px;flex-direction:column;align-items:center}.page-status,.unresolved-incidents,.components-section{margin-bottom:50px}
.font-regular{font-size:.875rem;line-height:1.334375rem}.font-large{font-size:1rem;line-height:1.5rem}small{font-size:.75rem}
.component-container{padding:.6rem .75rem .5rem}.unresolved-incident .updates{padding:.75rem}.components-uptime-link{text-align:center}
.unresolved-incident .incident-title{padding:.65rem .75rem .55rem}}
"""


def _stamp(ts: float, with_year: bool = True) -> str:
    t = datetime.fromtimestamp(ts)
    return (f"{t.year}年" if with_year else "") + f"{t.month}月{t.day}日 {t:%H:%M} UTC+8"


def _update_html(u: dict, with_year: bool = True) -> str:
    return (f'<div class="update font-regular {html.escape(u["status"])}"><strong>{STATUS_LABEL.get(u["status"], u["status"])}</strong> - '
            f'<span class="whitespace-pre-wrap">{html.escape(u["body"])}</span><br>'
            f'<small>{_stamp(u["at"], with_year)}</small></div>')


def _bars_svg(days: list[dict]) -> str:
    rects = []
    for i, d in enumerate(days):
        if d["level"] is None:
            tip = f'{d["day"]}：无数据'
        else:
            tip = f'{d["day"]}：' + ("；".join(d["titles"]) if d["titles"] else "没有记录的停机")
        rects.append(f'<rect height="34" width="3" x="{i * 5}" y="0" fill="{BAR[d["level"]]}"><title>{html.escape(tip)}</title></rect>')
    return (f'<svg class="availability-time-line-graphic" preserveAspectRatio="none" height="34" '
            f'viewBox="0 0 {len(days) * 5 - 2} 34">{"".join(rects)}</svg>')


def render(snap: dict[str, Any], subscribe_url: str = "/") -> str:
    status_cls = "status-none" if not snap["active"] else "status-" + max(
        (i["impact"] for i in snap["active"]), key=lambda x: IMPACT_RANK.get(x, 1))
    parts = [f'<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">'
             f'<meta name="viewport" content="width=device-width,initial-scale=1">'
             f'<title>猫头鹰公益站 状态</title><style>{CSS}</style></head>'
             f'<body class="status index {status_cls}"><div class="layout-content status status-index">',
             f'<div class="masthead"><a class="logo" href="/status"><span class="mark">🦉</span>猫头鹰公益站 状态</a>'
             f'<a class="show-updates-dropdown" href="{html.escape(subscribe_url)}" target="_blank" rel="noopener">订阅更新</a></div>']
    if snap["active"]:
        parts.append('<div class="unresolved-incidents">')
        for inc in snap["active"]:
            c = IMPACT_COLOR.get(inc["impact"], "#E86235")
            parts.append(f'<div class="unresolved-incident impact-{html.escape(inc["impact"])}">'
                         f'<div class="incident-title font-large" style="background-color:{c}">'
                         f'<span class="whitespace-pre-wrap actual-title">{html.escape(inc["title"])}</span>'
                         f'<a class="subscribe" href="{html.escape(subscribe_url)}" target="_blank" rel="noopener">订阅</a></div>'
                         f'<div class="updates font-regular" style="border-color:{c}">'
                         + "".join(_update_html(u) for u in inc["updates"]) + '</div></div>')
        parts.append('</div>')
    else:
        parts.append('<div class="page-status status-none"><span class="status font-large">所有系统运行正常</span></div>')
    parts.append(f'<div class="components-section font-regular"><div class="components-uptime-link">'
                 f'过去 {DAYS} 天的正常运行时间。</div><div class="components-container one-column">')
    for comp in snap["components"]:
        up = f'{comp["uptime"]:.2f} % 正常运行时间' if comp["uptime"] is not None else "无数据"
        parts.append(f'<div class="component-container border-color"><div class="component-inner-container">'
                     f'<span class="name" role="heading" aria-level="2">{html.escape(comp["name"])}</span>'
                     f'<span class="component-status" style="color:{STATUS_TEXT[comp["level"]]}">{comp["status"]}</span></div>'
                     f'<div class="shared-partial uptime-90-days-wrapper">{_bars_svg(comp["days"])}'
                     f'<div class="legend"><div class="legend-item light legend-item-date-range">{DAYS} 天前</div>'
                     f'<div class="spacer"></div><div class="legend-item legend-item-uptime-value">{up}</div>'
                     f'<div class="spacer"></div><div class="legend-item light legend-item-date-range">今天</div></div></div></div>')
    parts.append('</div></div><div class="incidents-list format-expanded">'
                 '<h2 class="font-largest no-link" id="past-incidents">过去的事件</h2>')
    today = datetime.fromtimestamp(snap["now"]).date()
    for i in range(HISTORY_DAYS):
        d = today - timedelta(days=i)
        d0 = datetime.combine(d, datetime.min.time()).timestamp()
        day_incs = [inc for inc in snap["incidents"] if d0 <= inc["started_at"] < d0 + 86400]
        parts.append(f'<div class="status-day font-regular{"" if day_incs else " no-incidents"}">'
                     f'<h3 class="date border-color font-large">{d.year}年{d.month}月{d.day}日</h3>')
        if not day_incs:
            parts.append(f'<p class="color-secondary">{"今天没有报告事件。" if i == 0 else "没有报告事件。"}</p>')
        for inc in day_incs:
            parts.append(f'<div class="incident-container"><div class="incident-title impact-{html.escape(inc["impact"])} font-large">'
                         f'<a class="whitespace-pre-wrap">{html.escape(inc["title"])}</a></div><div class="updates-container">'
                         + "".join(_update_html(u, with_year=False) for u in inc["updates"]) + '</div></div>')
        parts.append('</div>')
    parts.append('</div><div class="page-footer"><small>所有时间均为北京时间（UTC+8）。</small>'
                 '<small>事件按 调查中 → 已确定 → 监控中 → 已解决 更新</small></div></div></body></html>')
    return "".join(parts)


STATUS_HEADERS = {"Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'",
                  "Cache-Control": "no-cache", "X-Content-Type-Options": "nosniff"}


@router.get("/status", response_class=HTMLResponse)
async def status_page(request: Request):
    snap = await snapshot(request.app.state.gate.db)
    import os
    guild, chan = os.getenv("DISCORD_GUILD_ID", ""), os.getenv("ANNOUNCE_CHANNEL_ID", "")
    sub = f"https://discord.com/channels/{guild}/{chan}" if guild and chan else "/"
    return HTMLResponse(render(snap, sub), headers=STATUS_HEADERS)


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
