"""公开状态页（app/status_page.py）的 golden 夹具，供 gate-server 的 tests/status_page.rs 对拍。

做法：每个场景从全新的库开始，用旧版的 `create_incident` / `add_update` 写入合成事件，再塞进合成的
`status_hourly` / `usage_log` / 官方状态设置，然后在固定的 `now` 上调用旧版的 `snapshot`，记录三个公开接口的输出：
  · `/status.json`  —— 整个响应体（字节串），Rust 必须逐字节相同；
  · `/status/feed.xml` —— Atom 整个响应体，同上（时间戳越界的场景不比 Atom，atom=None）；
  · `/status` HTML —— 不比整页（3.0 改了样式 / 脚本的加载方式，见 docs/v3-deviations.md），
    只记录旧页渲染出来的所有「看得见的字符串」（按出现顺序的文本节点 + title / aria-label / href 等属性），
    Rust 的页面必须按同样的顺序包含它们；
  · `/admin/api/incidents` 列表的响应体（同一批数据的写入路径对拍）。
夹具里也带着输入（事件 / 小时计数 / 上游请求 / 官方状态），Rust 测试照样写进自己的库。

唯一有意的差异：「现在」的上游状态（`_upstream_last_hour`）。3.0 站长批准的新规则——失败至少 3 次、
来自至少 2 把不同的 Key，才算上游异常——在这里用同样的规则替换旧函数，其余逻辑原样使用旧版代码。
（每天的色条仍用旧版的按日占比，没有变。）

只有合成数据：标题 / 正文都是编的，Key 编号是 1–5，Discord 编号用 9000000000000000xx 段。

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/status_page.py
"""
from __future__ import annotations

import asyncio
import html as htmllib
import json
import os
import shutil
import tempfile
from html.parser import HTMLParser
from pathlib import Path

import _golden
from _golden import ROOT, Writer

_golden.OUT_DIR = ROOT / "crates" / "gate-server" / "tests" / "fixtures"

# app.config 在 import 时读 DATA_DIR，必须先设
WORK = Path(tempfile.mkdtemp(prefix="status-page-golden-"))
os.environ["DATA_DIR"] = str(WORK)
os.environ.pop("NAI_TOKENS", None)
os.environ.pop("NAI_TOKEN", None)

from fastapi.responses import JSONResponse  # noqa: E402

from app import status_page  # noqa: E402  必须在设置 DATA_DIR 之后

# 2026-10-14 12:00:00 +08:00
NOW = 1791950400.0
HOUR = 3600.0
DAY = 86400.0
GUILD, CHANNEL = "9000000000000000001", "9000000000000000002"


async def _last_hour_v3(db, now):
    """3.0 的「现在」规则：不到 10 次不下结论；失败至少 3 次且来自至少 2 把不同的 Key 才算。"""
    bad = "up_status>=500 OR up_status IN (429,403)"
    r = await db._db.execute_fetchall(
        f"SELECT COUNT(*), SUM({bad}), COUNT(DISTINCT CASE WHEN {bad} THEN key_id END) "
        "FROM usage_log WHERE ts>=? AND up_status>0", (now - 3600,))
    total, failed, keys = int(r[0][0] or 0), int(r[0][1] or 0), int(r[0][2] or 0)
    return failed / total if total >= 10 and failed >= 3 and keys >= 2 else 0.0


status_page._upstream_last_hour = _last_hour_v3


def usage_rows(ts_start, ok, failures, keys=(1, 2, 3, 4, 5), spread=3000.0):
    """从 ts_start 起 spread 秒内均匀铺 ok 个成功请求（up_status 200）和 failures 个失败请求。
    failures 是 [(up_status, key_id), ...]。"""
    rows = []
    n = ok + len(failures)
    for i in range(ok):
        rows.append([ts_start + spread * i / max(n, 1), keys[i % len(keys)], 200])
    for j, (code, key) in enumerate(failures):
        rows.append([ts_start + spread * (ok + j) / max(n, 1) + 0.5, key, code])
    return rows


def hourly(day_offset, hour_of_day, counts):
    """某天（相对 NOW 的整天偏移）某个整点的状态码计数 → [[整点秒数, code, n], ...]。
    整点 = NOW 当天本地 00:00 + 偏移（Asia/Shanghai，NOW 当天 00:00 = NOW - 12h）。"""
    base = NOW - 12 * HOUR - day_offset * DAY + hour_of_day * HOUR
    return [[int(base), code, n] for code, n in counts.items()]


def inc(title, impact, components, *updates):
    return {"title": title, "impact": impact, "components": components,
            "updates": [list(u) for u in updates]}


SCENARIOS: list[dict] = []


def scenario(name, now=NOW, incidents=(), status_hourly=(), usage=(), official=None, discord=None, compare_atom=True):
    SCENARIOS.append({"scenario": name, "now": now, "incidents": list(incidents),
                      "status_hourly": [list(r) for r in status_hourly], "usage": [list(r) for r in usage],
                      "official": official, "discord": discord or {"guild": "", "channel": ""},
                      "compare_atom": compare_atom})


# 1 一切正常：没有事件，没有数据
scenario("healthy")

# 2 常见的一天：未解决的 major（多条更新，含特殊字符）+ 已解决的上游事件 + 只影响官网 / 机器人的事件（不上状态页）
scenario(
    "active_major",
    incidents=[
        inc("出图接口请求超时", "major", ["api"],
            ("investigating", "我们正在调查此问题。", NOW - 5400.0),
            ("identified", "第一行\n第二行 <b>粗体</b> & \"双引号\" 与 '单引号'", NOW - 3600.0),
            ("update", "我们正在继续处理此问题。", NOW - 1800.25)),
        inc("部分 V5 出图画质下降", "minor", ["upstream"],
            ("investigating", "我们正在调查此问题。", NOW - 2 * DAY - 3 * HOUR),
            ("monitoring", "修复已上线，我们正在观察结果。", NOW - 2 * DAY - 2.5 * HOUR),
            ("resolved", "此事件已解决。", NOW - 2 * DAY - 2 * HOUR)),
        inc("官网与后台访问缓慢", "major", ["web", "bot"],
            ("investigating", "只影响官网，不上状态页。", NOW - 4000.0)),
        inc("NovelAI 上游生图中断", "critical", ["api", "upstream"],
            ("investigating", "我们正在调查此问题。", NOW - 10 * DAY - 40 * 60),
            ("resolved", "此事件已解决。", NOW - 9 * DAY - 20 * HOUR)),
    ],
    status_hourly=hourly(1, 10, {200: 900, 500: 30}) + hourly(1, 11, {200: 950, 429: 5})
    + hourly(0, 9, {200: 400}) + hourly(3, 15, {200: 1000, 502: 3}),
    usage=(usage_rows(NOW - 3300.0, 120, [(429, 1), (500, 2), (429, 1)])
           + usage_rows(NOW - DAY - 20 * HOUR, 92, [(500, 3)] * 8)
           + usage_rows(NOW - 3 * DAY - 10 * HOUR, 85, [(429, 4)] * 15)),
    official={"overall": "Operational", "image": "Operational", "image_code": 100,
              "updated": "2026-10-14T03:00:00Z", "fetched_at": NOW - 300.0},
)

# 3 过去 15 天每天都有事件（严重度各异，含未知严重度），另有 20 天前的（只进色条）和 100 天前已解决的（被过滤）
history = []
impacts = ["none", "minor", "major", "critical", "weird"]
for d in range(15):
    t = NOW - d * DAY - 2 * HOUR - 17.5 * d
    history.append(inc(f"出图接口历史事件 {d}", impacts[d % 5], ["api"] if d % 2 else ["upstream"],
                       ("investigating", f"第 {d} 天的合成事件。", t),
                       ("resolved", "此事件已解决。", t + 1800 + 60 * d)))
history.append(inc("同一天的第二个事件", "minor", ["api", "upstream"],
                   ("identified", "问题已定位，正在实施修复。", NOW - 3 * DAY - 30 * 60),
                   ("resolved", "此事件已解决。", NOW - 3 * DAY - 10 * 60)))
history.append(inc("二十天前的事件", "major", ["api"],
                   ("investigating", "我们正在调查此问题。", NOW - 20 * DAY),
                   ("resolved", "此事件已解决。", NOW - 20 * DAY + 2 * HOUR)))
history.append(inc("一百天前已解决（早于 90 天窗口）", "critical", ["api"],
                   ("investigating", "我们正在调查此问题。", NOW - 100 * DAY),
                   ("resolved", "此事件已解决。", NOW - 99 * DAY)))
scenario("history_mix", incidents=history)

# 4 没有未解决事件，但上游最近一小时失败占比很高（来自 3 把 Key）→ 部分中断横幅；官方状态已过期不参考
scenario(
    "upstream_partial_outage",
    usage=usage_rows(NOW - 3000.0, 100, [(500, 1)] * 8 + [(429, 2)] * 7 + [(403, 3)] * 5),
    official={"overall": "Degraded", "image": "Degraded", "image_code": 500, "updated": "x", "fetched_at": NOW - 7200.0},
)

# 5 3.0 新规则：同一把 Key 反复失败（5 次 / 105）→ 上游仍然「运行正常」
scenario("single_key_failures", usage=usage_rows(NOW - 3000.0, 100, [(500, 4)] * 5))

# 6 3.0 新规则：两把 Key 各失败 → 性能下降（3 / 103）
scenario("two_key_failures", usage=usage_rows(NOW - 3000.0, 100, [(500, 1), (500, 1), (429, 2)]))

# 7 官方自己标了故障（新鲜）→ 上游「性能下降」，即使最近一小时请求都成功
scenario(
    "official_degraded",
    usage=usage_rows(NOW - 3000.0, 60, []),
    official={"overall": "Degraded", "image": "Major Outage", "image_code": 400, "updated": "x", "fetched_at": NOW - 600.0},
)

# 8 未解决事件的各种严重度（none / critical / 未知），并列时 <body> 取先出现的；订阅地址走 Discord 公告频道
scenario(
    "active_mixed_impacts",
    incidents=[
        inc("无影响的维护通知", "none", ["api"], ("monitoring", "修复已上线，我们正在观察结果。", NOW - 900.0)),
        inc("严重中断示例", "critical", ["upstream"], ("investigating", "我们正在调查此问题。", NOW - 1200.0)),
        inc("未知严重度示例", "weird", ["api"], ("identified", "问题已定位，正在实施修复。", NOW - 1500.0)),
        inc("第二个严重中断", "critical", ["api", "upstream"], ("update", "我们正在继续处理此问题。", NOW - 1800.0)),
    ],
    discord={"guild": GUILD, "channel": CHANNEL},
)

# 9 解决后又重新打开（resolved_at 被改回 NULL）
scenario(
    "reopened",
    incidents=[
        inc("出图接口间歇性失败", "minor", ["api"],
            ("investigating", "我们正在调查此问题。", NOW - 7200.0),
            ("resolved", "此事件已解决。", NOW - 5400.0),
            ("investigating", "问题复现，我们正在重新调查。", NOW - 1200.0)),
        inc("先开后关的事件", "major", ["upstream"],
            ("investigating", "我们正在调查此问题。", NOW - 4 * HOUR),
            ("resolved", "此事件已解决。", NOW - 3 * HOUR),
            ("update", "补充说明。", NOW - 2 * HOUR),
            ("resolved", "再次解决。", NOW - HOUR)),
    ],
)

# 10 很多事件：/status.json 只给最近 50 个，Atom 只给最近 20 条更新
many = [inc(f"批量合成事件 {i}", "minor", ["api"],
            ("investigating", f"事件 {i} 开始。", NOW - 80 * DAY + i * 3 * HOUR),
            ("resolved", f"事件 {i} 已解决。", NOW - 80 * DAY + i * 3 * HOUR + 600.5)) for i in range(60)]
scenario("many_incidents", incidents=many)

# 11 时间戳的小数 / 进位 / 状态词不在词表里（标签退回原文）/ 同一时刻的多条更新（排序稳定性）
scenario(
    "timestamp_shapes",
    incidents=[
        inc("时间戳带小数", "minor", ["api"],
            ("investigating", "整分前的一瞬间。", 1791932399.9999996),
            ("custom", "状态词不在词表里。", NOW - 5000.123456),
            ("update", "同一时刻的第一条。", NOW - 4000.0),
            ("monitoring", "同一时刻的第二条。", NOW - 4000.0)),
        inc("整数值的时间戳", "major", ["upstream"],
            ("investigating", "我们正在调查此问题。", 1791946800.0)),
    ],
)

# 12 越界时间戳：页面写「时间无效」。旧版 Atom 会把它格式化成 31690708 年，Rust 的日期库表示不了，不比 Atom
scenario(
    "invalid_time",
    incidents=[inc("时间戳越界的事件", "minor", ["api"], ("investigating", "我们正在调查此问题。", 1e15))],
    compare_atom=False,
)

# 13 很久以前开始、至今未解决的 major：每一天都受影响，可用率被拉低
scenario(
    "ancient_unresolved",
    incidents=[inc("长期未解决的事件", "major", ["upstream"],
                   ("investigating", "我们正在调查此问题。", NOW - 100 * DAY))],
)


class Visible(HTMLParser):
    """旧页面里所有看得见的字符串：按出现顺序的文本节点，以及 title / aria-label / href / role / lang 属性。"""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.texts: list[str] = []
        self.attrs: list[list[str]] = []
        self._skip = 0

    def handle_starttag(self, tag, attrs):
        if tag in ("script", "style"):
            self._skip += 1
        for k, v in attrs:
            if k in ("title", "aria-label", "href", "role", "lang") and v:
                self.attrs.append([k, v])

    def handle_endtag(self, tag):
        if tag in ("script", "style"):
            self._skip -= 1

    def handle_data(self, data):
        if not self._skip and data.strip():
            self.texts.append(data.strip())


def visible(doc: str):
    p = Visible()
    p.feed(doc)
    p.close()
    return p.texts, p.attrs


async def run_scenario(sc: dict) -> dict:
    from app.config import load_settings
    from app.database import Database

    for suffix in ("", "-wal", "-shm"):
        Path(str(WORK / "nai_gate.db") + suffix).unlink(missing_ok=True)
    settings = load_settings()
    db = Database(str(settings.db_path), settings.tz)
    await db.connect()
    await status_page.ensure_schema(db)
    try:
        for spec in sc["incidents"]:
            first, *rest = spec["updates"]
            iid = await status_page.create_incident(db, spec["title"], spec["impact"], spec["components"],
                                                    first[0], first[1], first[2])
            for status, body, at in rest:
                await status_page.add_update(db, iid, status, body, at)
        for hour, code, n in sc["status_hourly"]:
            await db._db.execute("INSERT INTO status_hourly(hour, code, n) VALUES (?,?,?)", (hour, code, n))
        for ts, key_id, up in sc["usage"]:
            await db._db.execute(
                "INSERT INTO usage_log(ts, key_id, kind, status, up_status) VALUES (?,?,?,?,?)",
                (ts, key_id, "image", "ok" if up == 200 else "error", up))
        await db._db.commit()
        if sc["official"] is not None:
            await db.set_setting(status_page.OFFICIAL_KEY, json.dumps(sc["official"], ensure_ascii=False))

        now = sc["now"]
        snap = await status_page.snapshot(db, now)
        body = JSONResponse({"components": [{k: c[k] for k in ("key", "name", "status", "uptime")}
                                            for c in snap["components"]],
                             "active": snap["active"], "incidents": snap["incidents"][:50]}).body.decode()
        atom = status_page._atom_feed(snap) if sc["compare_atom"] else None
        g = sc["discord"]
        sub = f"https://discord.com/channels/{g['guild']}/{g['channel']}" if g["guild"] and g["channel"] else "/status/feed.xml"
        doc = status_page.render(snap, sub)
        texts, attrs = visible(doc)
        admin_list = JSONResponse({"incidents": await status_page.list_incidents(db)}).body.decode()
        comp_rows = await db._db.execute_fetchall("SELECT id, components FROM incidents ORDER BY id")
        return {
            **sc,
            "expect": {
                "json": body, "atom": atom, "admin_list": admin_list,
                "html_texts": texts, "html_attrs": attrs, "subscribe_url": sub,
                "statuses": {c["key"]: c["status"] for c in snap["components"]},
                "active": [i["id"] for i in snap["active"]],
                "components_column": [r[1] for r in comp_rows],
            },
        }
    finally:
        await db.close()


def main() -> None:
    assert htmllib  # 只是确认标准库可用
    with Writer("status_page") as w:
        for sc in SCENARIOS:
            w.record(**asyncio.run(run_scenario(sc)))
    shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    main()
