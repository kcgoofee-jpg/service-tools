"""客户端日志导出：站长定期下载，独立复核有没有共用 / 转卖 Key 的「蛀虫」。

一个 zip，里面是几张 CSV（UTF-8 带 BOM，Excel 直接打开不乱码）和一份字段说明：
- requests.csv   每次请求一行，按时间排好
- keys.csv       每把 Key 一行：Discord 身份、用量、用过的客户端种类、标签
不含提示词、图片和任何 Key / Token 原文（防分享指纹与网段聚合已随旧机制移除，10/13）。
"""
from __future__ import annotations

import csv
import io
import time
import zipfile
from datetime import datetime

README = """猫头鹰公益站 · 客户端日志导出
导出时间：{now}（UTC+8）  范围：{scope}

requests.csv（每次请求一行，按时间升序）
  time 时间 · key_id / key_name / discord_id / discord_user 是谁
  kind 功能（image / image_stream / chat …）· model 模型 · status 结果（ok / rejected / error / cancelled）
  images 张数 · anlas 消耗 · wait_ms 排队 · dur_ms 生成耗时 · up_status 上游状态码
  user_agent 客户端自报的 UA · detail / reason 网关备注、拒绝原因 · rid 错误编号

keys.csv（每把 Key 一行）
  ok_images 成功出图张数 · client_kinds 用过的客户端种类（UA 去重）
  tags 后台标签 · quota_auto 1=算法管、-1=手动 · is_test 测试 Key

怎么看「蛀虫」（思路，不是结论）：
  - 一把 Key 的 client_kinds 明显偏多（同一人在多个软件间切换是少数，转卖/共用往往多种客户端混用）
  - 24 小时不停、节奏像机器（间隔几乎恒定）
  - 深夜高频、拒绝（rejected）远多于成功
"""


def _ts(v) -> str:
    try:
        return datetime.fromtimestamp(float(v)).strftime("%Y-%m-%d %H:%M:%S") if v else ""
    except (TypeError, ValueError, OverflowError, OSError):
        return ""


def safe_cell(v):
    """防 CSV 公式注入：以 = + - @ 制表符 回车开头的文本前加 '（UA、Key 名都可能是成员自己填的）。"""
    if isinstance(v, str) and v[:1] in ("=", "+", "-", "@", "\t", "\r"):
        return "'" + v
    return v


def _csv(rows: list[list], header: list[str]) -> bytes:
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(header)
    w.writerows([[safe_cell(c) for c in r] for r in rows])
    return ("﻿" + buf.getvalue()).encode("utf-8")


async def collect(db, days: int) -> dict:
    """读数据（只读）。days<=0 表示全部。"""
    since = time.time() - days * 86400 if days > 0 else 0
    q = db._db.execute_fetchall
    keys = await q("SELECT id, name, enabled, is_admin, is_test, quota_auto, created_at, last_used_at FROM api_keys")
    reg = {r[0]: r[1:] for r in await q(
        "SELECT key_id, discord_id, username, display_name FROM discord_registrations WHERE key_id IS NOT NULL")}
    usage = await q("SELECT ts, key_id, key_name, kind, model, status, images, anlas, wait_ms, dur_ms, up_status, "
                    "client, detail, reason, rid FROM usage_log WHERE ts>=? ORDER BY ts", (since,))
    tags: dict[int, list[str]] = {}
    for kid, tag, note in await q("SELECT key_id, tag, note FROM key_tags"):
        tags.setdefault(kid, []).append(tag + (f"（{note}）" if note else ""))
    return {"keys": keys, "reg": reg, "usage": usage, "tags": tags, "days": days}


def build_zip(d: dict) -> bytes:
    names = {k[0]: k[1] for k in d["keys"]}
    reg = d["reg"]
    req_rows, ok_images, clients = [], {}, {}
    for ts, kid, kname, kind, model, status, images, anlas, wait, dur, up, client, detail, reason, rid in d["usage"]:
        if status == "ok":
            ok_images[kid] = ok_images.get(kid, 0) + (images or 0)
        if client:
            clients.setdefault(kid, set()).add(client)
        r = reg.get(kid, ("", "", ""))
        req_rows.append([_ts(ts), kid, names.get(kid, kname), r[0], r[1], kind, model, status, images, anlas,
                         wait, dur, up, client, detail, reason, rid])
    key_rows = []
    for kid, name, enabled, is_admin, is_test, qa, created, last in d["keys"]:
        r = reg.get(kid, ("", "", ""))
        key_rows.append([kid, name, r[0], r[1], r[2], enabled, is_admin, is_test, qa, _ts(created), _ts(last),
                         ok_images.get(kid, 0), len(clients.get(kid, ())), "；".join(d["tags"].get(kid, []))])
    scope = f"最近 {d['days']} 天" if d["days"] > 0 else "全部"
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("README.txt", README.format(now=_ts(time.time()), scope=scope))
        z.writestr("requests.csv", _csv(req_rows, [
            "time", "key_id", "key_name", "discord_id", "discord_user", "kind", "model", "status", "images", "anlas",
            "wait_ms", "dur_ms", "up_status", "user_agent", "detail", "reason", "rid"]))
        z.writestr("keys.csv", _csv(key_rows, [
            "key_id", "key_name", "discord_id", "discord_user", "discord_display", "enabled", "is_admin", "is_test",
            "quota_auto", "created", "last_used", "ok_images", "client_kinds", "tags"]))
    return out.getvalue()
