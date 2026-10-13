#!/usr/bin/env python3
"""提示词统计 + 敏感内容自查（本地出 HTML，不上服务器、不对外）。

用法：
    python3 deploy/ops/prompt_stats.py [--db /app/data/nai_gate.db] [--out prompt-stats.html]
                                        [--days 30] [--top 400] [--prompts 3000]

输出一个自包含的 HTML（点表头排序、带搜索框），三张表：
  1. 画师串（artist:xxx）出现次数 / 占比 / 成员数 / 最近出现
  2. 标签频率（提示词里所有标签）
  3. 提示词去重计数（同一条提示词被谁、什么时候用过）
最后一节是「敏感内容自查」：只列命中条数与定位信息（时间 / 成员 / 摘要），
**内容只留在本地这个文件里**，脚本不会把它发到任何地方。

数据来源是 generation_audit（本站自己留的出图记录，audit_prompts 开着才会存），
数据库只读打开。这是给站长做内容治理用的，别把这个文件转发出去。
"""
from __future__ import annotations

import argparse
import base64
import collections
import html
import json
import re
import sqlite3
import time
from pathlib import Path

# 敏感内容自查用的关键词表（分类给理由用）。命中后只在本地 HTML 里列定位信息，
# 由站长判断怎么处理；脚本本身不做任何处置。
SENSITIVE_TERMS: dict[str, tuple[str, ...]] = {
    "政治人物与机构": (
        "习近平", "江泽民", "胡锦涛", "温家宝", "毛泽东", "周恩来", "邓小平", "刘少奇", "朱德",
        "林彪", "周恩来", "华国锋", "习近平", "彭丽媛", "李克强", "栗战书", "汪洋", "王沪宁",
        "赵乐际", "韩正", "蔡奇", "丁薛祥", "李希", "陈敏尔", "杨晓渡", "郭声琨", "周强", "张军",
        "令计划", "薄熙来", "周永康", "徐才厚", "郭伯雄", "孙政才", "李源潮", "曾庆红", "贺国强",
        "罗干", "吴官正", "李长春", "贾庆林", "曾培炎", "回良玉", "张德江", "俞正声", "刘云山",
        "王岐山", "张高丽", "马凯", "刘延东", "孙春兰", "李建国", "陈昌智", "严隽琪", "王晨",
        "沈跃跃", "吉炳轩", "张平", "向巴平措", "艾力更·依明巴海", "万鄂湘", "陈竺", "丁仲礼",
        "郝明金", "蔡达峰", "武维华", "帕巴拉·格列朗杰", "万钢", "林文漪", "罗富和", "齐续春",
        "马培华", "刘晓峰", "王钦敏", "梁振英", "董建华", "何厚铧", "崔世安", "林郑月娥",
        "特朗普", "拜登", "奥巴马", "普京", "泽连斯基", "莫迪", "安倍晋三", "金正恩",
        "中共中央", "中纪委", "国家主席", "国务院", "全国人大", "政协", "中央军委", "政治局",
        "总书记", "总理", "部长", "省长", "市委书记", "党支部", "党委书记", "人大代表",
        "共产党", "国民党", "民进党", "民革", "民盟", "民建", "民进", "农工党", "致公党",
        "九三学社", "台盟", "共青团", "少先队", "信访局", "公安局", "国安局", "统战部",
    ),
    "政治事件与口号": (
        "六四", "天安门", "64事件", "学潮", "反革命", "文革", "文化大革命", "大跃进", "反右",
        "文革", "批斗", "红卫兵", "四人帮", "改革开放", "一国两制", "港独", "台独", "疆独",
        "藏独", "法轮功", "民运", "示威", "游行", "集会", "罢工", "静坐", "绝食", "戒严",
        "平反", "翻案", "政变", "夺权", "打倒", "万岁", "万岁万岁", "万岁万万岁", "革命",
        "解放", "斗争", "路线", "阶级斗争", "无产阶级", "资产阶级", "修正主义", "帝国主义",
        "霸权主义", "颜色革命", "阿拉伯之春", "太阳花", "雨伞革命", "反送中", "国安法",
        "清零", "封城", "大白", "核酸", "健康码", "行程卡", "网格员", "动态清零",
    ),
    "旗帜符号与军事": (
        "五星红旗", "中共党旗", "党旗", "国旗", "青天白日", "十八星旗", "铁血旗", "万字旗",
        "解放军", "红军", "八路军", "新四军", "志愿军", "民兵", "预备役", "核武器", "氢弹",
        "原子弹", "导弹", "洲际导弹", "东风导弹", "航母", "潜艇", "战机", "歼20", "歼-20",
        "运20", "轰6", "辽宁舰", "山东舰", "福建舰", "无人机", "察打一体", "军演", "阅兵",
        "长征", "天问", "嫦娥", "北斗", "东风", "巨浪", "红旗", "红旗9", "红箭", "霹雳火",
    ),
}

ARTIST_RE = re.compile(r"artist:\s*([^,{}\n\[\]]+)", re.IGNORECASE)
WS = re.compile(r"\s+")


def norm(text: str) -> str:
    return WS.sub(" ", (text or "").strip())


def mask(name: str) -> str:
    """成员名只留首尾，避免本地文件里出现完整 ID（定位够用就行）。"""
    name = norm(name)
    if len(name) <= 2:
        return name[:1] + "*"
    return name[:2] + "***" + name[-2:]


def load_rows(db_path: str, days: int) -> list[dict]:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=10)
    conn.row_factory = sqlite3.Row
    since = time.time() - days * 86400
    rows = conn.execute(
        "SELECT ts, key_name, kind, model, status, prompt, negative FROM generation_audit "
        "WHERE ts>=? ORDER BY ts DESC", (since,)).fetchall()
    conn.close()
    return [dict(r) for r in rows]


def summarize(rows: list[dict]) -> dict:
    artists: collections.Counter = collections.Counter()
    artist_members: dict[str, set] = collections.defaultdict(set)
    artist_last: dict[str, float] = {}
    tags: collections.Counter = collections.Counter()
    prompts: dict[str, dict] = {}
    sensitive: list[dict] = []

    for row in rows:
        prompt = norm(row.get("prompt"))
        negative = norm(row.get("negative"))
        stamp = float(row["ts"])
        who = mask(row.get("key_name") or "")
        blob = f"{prompt}\n{negative}"

        for raw in ARTIST_RE.findall(blob):
            name = norm(raw)
            if not name:
                continue
            key = name.lower()
            artists[key] += 1
            artist_members[key].add(who)
            artist_last[key] = max(artist_last.get(key, 0), stamp)

        for raw in re.split(r"[,，\n\[\]{}]", blob):
            tag = norm(raw)
            if tag and len(tag) <= 60 and not tag.lower().startswith("artist:"):
                tags[tag.lower()] += 1

        if prompt:
            entry = prompts.setdefault(prompt.lower(), {
                "text": prompt, "count": 0, "members": set(), "first": stamp, "last": stamp})
            entry["count"] += 1
            entry["members"].add(who)
            entry["first"] = min(entry["first"], stamp)
            entry["last"] = max(entry["last"], stamp)

        for category, terms in SENSITIVE_TERMS.items():
            hits = sorted({t for t in terms if t in blob})
            if hits:
                sensitive.append({"ts": stamp, "who": who, "kind": row.get("kind"),
                                  "model": row.get("model"), "status": row.get("status"),
                                  "category": category, "hits": hits,
                                  "snippet": blob[:120]})

    return {"artists": artists, "artist_members": artist_members, "artist_last": artist_last,
            "tags": tags, "prompts": prompts, "sensitive": sensitive}


def sortable_table(head: str, columns: list[tuple[str, str]], rows: list[list], note: str) -> str:
    """columns: [(字段, 是否数值排序)]; rows: 与字段同顺序的单元格列表。"""
    idx = html.escape(head.lower().replace(" ", "_"))
    ths = "".join(f'<th data-col="{i}">{html.escape(label)}</th>' for i, (label, _) in enumerate(columns))
    numeric = json.dumps([bool(n) for _, n in columns])
    body = "".join(
        "<tr>" + "".join(
            f'<td>{html.escape(str(cell)) if not isinstance(cell, str) or not cell.startswith("<") else cell}</td>'
            for cell in row) + "</tr>" for row in rows)
    return (f'<h3>{html.escape(head)}<span class="muted">{html.escape(note)}</span></h3>'
            f'<div class="search"><input type="search" placeholder="搜索这一张表…" oninput="filterTable(this,\'{idx}\')"></div>'
            f'<div class="scroll"><table id="{idx}" data-num="{numeric}"><thead><tr>{ths}</tr></thead>'
            f"<tbody>{body}</tbody></table></div>")


def render(data: dict, out_path: Path, days: int, total: int) -> None:
    def share(n: int) -> str:
        return f"{n / total * 100:.1f}%" if total else "—"

    artist_rows = [[name, count, share(count), len(data["artist_members"][name]),
                    time.strftime("%m-%d %H:%M", time.localtime(data["artist_last"][name]))]
                   for name, count in data["artists"].most_common()]
    tag_rows = [[tag, count, share(count)] for tag, count in data["tags"].most_common()]
    prompt_rows = [[entry["text"], entry["count"], len(entry["members"]),
                    time.strftime("%m-%d %H:%M", time.localtime(entry["last"]))]
                   for entry in sorted(data["prompts"].values(),
                                       key=lambda e: -e["count"])[:args.prompts]]
    sensitive_rows = [[time.strftime("%m-%d %H:%M", time.localtime(s["ts"])), s["who"],
                       s["kind"] or "", s["model"] or "", s["status"] or "", s["category"],
                       "、".join(s["hits"]), html.escape(s["snippet"])] for s in data["sensitive"]]

    def table(head, cols, rows, note):
        return sortable_table(head, cols, rows, note)

    doc = f"""<!doctype html><html lang="zh"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>提示词统计（本地）· {time.strftime('%Y-%m-%d')}</title>
<style>
:root{{--bg:#0f1117;--card:#171a23;--line:#262b36;--tx:#e6e8ee;--mu:#9aa3b2;--ac:#7c8cff}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--tx);
font:14px/1.6 -apple-system,"PingFang SC","Microsoft YaHei",sans-serif}}
header{{padding:20px 24px;border-bottom:1px solid var(--line)}}header h1{{margin:0;font-size:18px}}
header p{{margin:4px 0 0;color:var(--mu);font-size:12.5px}}
main{{padding:16px 24px 60px}}h3{{font-size:15px;margin:26px 0 8px;display:flex;gap:8px;align-items:baseline}}
h3 .muted{{font-size:12px;color:var(--mu);font-weight:400}}
.search input{{width:100%;max-width:420px;padding:7px 10px;border-radius:8px;
border:1px solid var(--line);background:var(--card);color:var(--tx)}}
.scroll{{max-height:520px;overflow:auto;border:1px solid var(--line);border-radius:10px}}
table{{width:100%;border-collapse:collapse;font-size:13px}}th{{position:sticky;top:0;background:#1c202b;
text-align:left;padding:8px 10px;border-bottom:1px solid var(--line);cursor:pointer;white-space:nowrap}}
th:hover{{color:var(--ac)}}td{{padding:7px 10px;border-bottom:1px solid #1e222c;vertical-align:top}}
tr:hover td{{background:#1a1e27}}td:nth-child(n+2){{white-space:nowrap}}
.muted{{color:var(--mu)}}.tag{{display:inline-block;padding:1px 6px;border-radius:6px;background:#232838;margin:1px 3px 1px 0}}
</style></head><body>
<header><h1>提示词统计（本地文件，不要外传）</h1>
<p>数据来源：本站出图审计记录 generation_audit（最近 {days} 天，共 {total} 条）。
点表头排序；每张表带搜索框。最后一节是敏感内容自查，仅列定位信息。</p></header>
<main>
{table("画师串（artist:）", [("画师串", False), ("次数", True), ("占比", True), ("成员数", True), ("最近出现", True)],
       artist_rows, f"共 {len(artist_rows)} 个")}
{table("标签频率", [("标签", False), ("次数", True), ("占比", True)], tag_rows, f"共 {len(tag_rows)} 个（取前 {args.top}）")}
{table("提示词去重", [("提示词", False), ("次数", True), ("成员数", True), ("最近出现", True)],
       prompt_rows, f"共 {len(data['prompts'])} 条不同的提示词，按次数取前 {len(prompt_rows)}")}
<h3>敏感内容自查<span class="muted">命中 {len(sensitive_rows)} 条（时间 / 成员 / 类别 / 关键词）</span></h3>
<div class="scroll"><table><thead><tr><th>时间</th><th>成员</th><th>类型</th><th>模型</th><th>状态</th>
<th>类别</th><th>命中词</th><th>摘要</th></tr></thead><tbody>
{"".join("<tr>" + "".join(f"<td>{html.escape(str(c))}</td>" for c in r) + "</tr>" for r in sensitive_rows)
 or '<tr><td colspan="8" class="muted">没有命中</td></tr>'}
</tbody></table></div>
</main>
<script>
const tables = document.querySelectorAll('table[data-num]');
tables.forEach(tb => {{
  tb.querySelectorAll('th').forEach((th, i) => th.addEventListener('click', () => {{
    const numeric = JSON.parse(tb.dataset.num)[i];
    const body = tb.tBodies[0], rows = [...body.rows];
    const dir = th.dataset.dir === '1' ? -1 : 1;
    tb.querySelectorAll('th').forEach(x => delete x.dataset.dir);
    th.dataset.dir = String(dir);
    rows.sort((a, b) => {{
      const x = a.cells[i].textContent, y = b.cells[i].textContent;
      return (numeric ? (parseFloat(x) || 0) - (parseFloat(y) || 0) : x.localeCompare(y, 'zh')) * dir;
    }});
    rows.forEach(r => body.appendChild(r));
  }}));
}});
function filterTable(input, id){{
  const q = input.value.trim().toLowerCase();
  document.querySelectorAll('#' + id + ' tbody tr').forEach(tr => {{
    tr.hidden = q && !tr.textContent.toLowerCase().includes(q);
  }});
}}
</script></body></html>"""
    out_path.write_text(doc, encoding="utf-8")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="/app/data/nai_gate.db")
    ap.add_argument("--out", default="prompt-stats.html")
    ap.add_argument("--days", type=int, default=30)
    ap.add_argument("--top", type=int, default=400)
    ap.add_argument("--prompts", type=int, default=3000)
    args = ap.parse_args()
    rows = load_rows(args.db, args.days)
    if not rows:
        raise SystemExit(f"最近 {args.days} 天没有审计记录（确认 audit_prompts 开着）")
    data = summarize(rows)
    render(data, Path(args.out), args.days, len(rows))
    print(f"记录 {len(rows)} 条 · 画师串 {len(data['artists'])} 个 · 标签 {len(data['tags'])} 个 · "
          f"提示词 {len(data['prompts'])} 条 · 敏感命中 {len(data['sensitive'])} 条")
    top = data["artists"].most_common(5)
    print("画师串前 5：", "、".join(f"{n} ×{c}" for n, c in top))
    cats = collections.Counter(s["category"] for s in data["sensitive"])
    if cats:
        print("敏感命中分布：", dict(cats))
