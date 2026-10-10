"""风险检查（后台只读诊断）：对截图指标逐项体检，能做的做实，做不了的留空并说明原因。

两个用途：
1. 出站指纹盘点 —— 我们发给 NovelAI 的流量现在长什么样（请求头、TLS 栈、出口链路），
   站长知情即可，本模块不伪造任何一项；
2. 白嫖 / 滥用识别 —— 谁在多人共用一把 Key、谁在把站当量产机：防分享风险分、网段共用、
   用量集中度、上游限流 / 认证失败信号、当日 V5 消耗。

━━ 明确留空（前端显示「不适用」）的三类，以及为什么 ━━
- 伪装类：伪造浏览器请求头、改 TLS(JA3/JA4) 指纹、挂代理换出口 IP，把流量伪装成官方
  客户端以规避上游识别。上游风控主要看行为模式（多 Key 同源、请求节奏），伪装指纹既
  不解决根因也不改变账号共享的事实，因此不做。
- 第三方回显：主动向 JA3 / IP 回显服务发请求来「实测指纹」。这会把网关出口与查询行为
  关联起来，收益只有满足好奇心，默认不做；出口 IP 实测仅在显式配置
  RISK_CHECK_IP_ECHO_URL 时执行（自担该风险）。
- 浏览器端指标：Canvas / Audio / WebGL / 字体 / WebRTC 泄漏等，全部是浏览器运行时概念；
  本网关是纯服务端进程，没有浏览器环境，指标本身不适用。

全部只读：不写数据库、不发上游请求（出口 IP 回显除外且默认关闭）、不发 Discord 消息。
"""
from __future__ import annotations

import os
import ssl
import sys
import time
from typing import Any, Optional

HOUR = 3600
DAY = 24 * HOUR

# 状态档位：ok 正常 / warn 注意 / bad 有风险 / na 不适用（含明确留空项）
OK, WARN, BAD, NA = "ok", "warn", "bad", "na"
STATUS_LABEL = {"ok": "正常", "warn": "注意", "bad": "风险", "na": "不适用"}

# 用量集中度：单把 Key 的成功出图占全站比例达到该值时提示（一把 Key 独吃全站产能，多半是共享/转卖）
CONCENTRATION_WARN = 0.5
IP_ECHO_TIMEOUT = 8.0


def _item(id_: str, title: str, status: str, detail: str, evidence: Optional[list[str]] = None) -> dict:
    return {"id": id_, "title": title, "status": status, "label": STATUS_LABEL.get(status, status),
            "detail": detail, "evidence": evidence or []}


def _mask_token(token: str) -> str:
    token = token or ""
    if len(token) <= 12:
        return token[:4] + "…" if token else ""
    return token[:8] + "…" + token[-4:]


# ---------- 出站指纹盘点 ----------

def _outbound_headers_item(state) -> dict:
    nai = getattr(state, "nai", None)
    client = getattr(nai, "_client", None)
    evidence = ["每次上游请求实际发送："]
    if client is not None:
        headers = dict(getattr(client, "headers", {}))
        ua = headers.get("User-Agent", "")
        evidence.append(f"User-Agent: {ua or '(未设置)'}（httpx 客户端级全局头，自报的第三方客户端标识）")
    else:
        evidence.append("User-Agent: nai-gate/1.0（代码默认值；上游客户端尚未启动）")
    pool = list(getattr(nai, "pool", []) or [])
    sample = _mask_token(pool[0].token) if pool else ""
    evidence.append(f"Authorization: Bearer <上游Token>（按请求换 Token，本页打码示意：{sample}）")
    evidence.append("Accept / Content-Type: 按场景设置（application/json、x-msgpack、text/event-stream）")
    evidence.append("无浏览器指纹头（sec-ch-ua、Accept-Language、Referer、Origin 均不发送）——现状如此，仅盘点，不做伪装。")
    return _item("outbound_headers", "出站请求头", OK,
                 "上游收到的是「网关自报家门」的头。是否调整属于站长与上游的关系，本模块只如实列出。",
                 evidence)


def _tls_stack_item() -> dict:
    try:
        import httpx
        httpx_ver = httpx.__version__
    except Exception:
        httpx_ver = "未知"
    evidence = [f"Python {sys.version.split()[0]} + httpx {httpx_ver}",
                f"TLS 栈：{ssl.OPENSSL_VERSION}",
                "TLS 握手特征随 Python/OpenSSL 版本与配置变化；升级依赖后指纹会变，属正常现象。"]
    return _item("tls_stack", "TLS 栈自述", OK,
                 "本机出站 TLS 使用的软件版本。只报告，不测量、不伪装。", evidence)


def _tls_fingerprint_item() -> dict:
    return _item("tls_fingerprint", "TLS 指纹（JA3/JA4）实测与伪装", NA,
                 "留空：测 JA3 需要向第三方指纹回显服务发请求，改指纹（curl_cffi impersonate 等）"
                 "属于「伪装成真实客户端以规避上游识别」，这两件都不做。理由见模块说明。")


def _egress_ip_item() -> dict:
    proxies = {name: os.environ.get(name, "") for name in
               ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")}
    active = [f"{k}={v}" for k, v in proxies.items() if v]
    evidence = ["未配置代理：出站即服务器本机 IP，所有上游 Token 共用同一出口。" if not active
                else "检测到代理环境变量（httpx 默认信任它们）："]
    evidence += active
    return _item("proxy_config", "代理 / 出口链路", OK,
                 "网关走什么链路出站。注意：多把上游 Token 共用一个出口 IP 是客观事实，"
                 "用代理把不同 Token 分散到不同 IP 属于伪装手段，不做。",
                 evidence)


async def _egress_ip_echo_item() -> dict:
    url = os.environ.get("RISK_CHECK_IP_ECHO_URL", "").strip()
    if not url:
        return _item("egress_ip", "出口 IP 实测", NA,
                     "留空（默认关闭）：实测出口 IP 要向第三方回显服务发请求，会让对方记录到网关 IP。"
                     "确有需要时在环境变量配置 RISK_CHECK_IP_ECHO_URL（返回纯文本 IP 的 URL），本页会显示结果。")
    try:
        import httpx
        async with httpx.AsyncClient(timeout=IP_ECHO_TIMEOUT) as client:
            resp = await client.get(url)
        status, body = str(resp.status_code), resp.text.strip()[:64]
    except Exception as exc:
        return _item("egress_ip", "出口 IP 实测", WARN, f"回显服务请求失败：{exc}")
    ok = status == "200" and body
    return _item("egress_ip", "出口 IP 实测", OK if ok else WARN,
                 f"回显服务返回 HTTP {status}。",
                 [f"出口 IP：{body}" if ok else f"响应：{body}"])


def _browser_fingerprint_item() -> dict:
    return _item("browser_fingerprint", "浏览器端指纹（Canvas / Audio / WebGL / 字体 / WebRTC）", NA,
                 "留空：这些是浏览器运行时指标（截图里那类反指纹工具的对象）。本网关是纯服务端进程，"
                 "没有浏览器环境，指标不适用；成员浏览器的指纹也不经过本站。")


# ---------- 上游行为信号 ----------

async def _upstream_signals_item(state, now: float) -> dict:
    try:
        from . import perf
        data = await perf.collect(state, now)
    except Exception as exc:
        return _item("upstream_signals", "上游风控信号（429 / 401 / 成功率）", NA, f"统计暂不可用：{exc}")
    flags = data.get("flags") or []
    if any(f.get("level") == "bad" for f in flags):
        status = BAD
    elif flags:
        status = WARN
    else:
        status = OK
    empty = not data.get("families")
    return _item("upstream_signals", "上游风控信号（429 / 401 / 成功率）",
                 NA if empty and status == OK else status,
                 "最近 1 小时 / 24 小时的限流、认证失败、成功率与 7 天基线对比。"
                 "这反映的是「上游怎么对我们」，行为治理（收紧间隔、降低人数）才是改善手段。",
                 [f["text"] for f in flags])


async def _share_top_item(state, now: float) -> dict:
    share = getattr(state, "share", None)
    if share is None:
        return _item("share_top", "防分享风险分（谁在多人共用）", NA, "防分享模块未启用。")
    try:
        report = await share.report(now)
    except Exception as exc:
        return _item("share_top", "防分享风险分（谁在多人共用）", NA, f"读取失败：{exc}")
    flagged = [r for r in report if r.get("score", 0) > 0 or r.get("strikes", 0) > 0]
    if not flagged:
        return _item("share_top", "防分享风险分（谁在多人共用）", OK, "没有 Key 被记风险分。")
    status = BAD if any(r.get("strikes", 0) or r.get("score", 0) >= 60 for r in flagged) else WARN
    evidence = [f"{r['name']}：风险分 {r['score']}，违规 {r['strikes']} 次"
                + (f"，暂停至 {time.strftime('%m-%d %H:%M', time.localtime(r['paused_until']))}" if r.get("paused_until") else "")
                + (f"｜最近证据：{r['evidence'][0]['label']}" if r.get("evidence") else "")
                for r in flagged[:5]]
    return _item("share_top", "防分享风险分（谁在多人共用）", status,
                 "share_guard 的证据与风险分（网络 + 客户端双重印证才算强证据）。这才是「查白嫖」的主战场。",
                 evidence)


async def _net_overlap_item(state, now: float) -> dict:
    db = state.db
    try:
        rows = await db._db.execute_fetchall(
            "SELECT net_hash, COUNT(DISTINCT key_id) AS c FROM key_sources WHERE last_seen>=? "
            "GROUP BY net_hash HAVING c>=2 ORDER BY c DESC LIMIT 5", (now - DAY,))
    except Exception as exc:
        return _item("net_overlap", "同一网段多把 Key（24 小时）", NA, f"读取失败：{exc}")
    if not rows:
        return _item("net_overlap", "同一网段多把 Key（24 小时）", OK,
                     "近 24 小时没有多个 Key 共用同一来源网段（同一家宽带 / 校园网出现 2~3 把属正常，"
                     "数字大才有意义）。")
    evidence = []
    for net_hash, count in rows:
        try:
            name_rows = await db._db.execute_fetchall(
                "SELECT k.name FROM key_sources ks JOIN api_keys k ON k.id=ks.key_id "
                "WHERE ks.net_hash=? AND ks.last_seen>=? LIMIT 5", (net_hash, now - DAY))
            names = "、".join(r[0] for r in name_rows)
        except Exception:
            names = ""
        evidence.append(f"{count} 把 Key 共用同一网段：{names}")
    notable = any(c >= 3 for _, c in rows) or len(rows) >= 3
    return _item("net_overlap", "同一网段多把 Key（24 小时）", WARN if notable else OK,
                 "网段打码存储（IPv4 /24、IPv6 /48），不存完整 IP。仅作线索，处罚仍以 share_guard 强证据为准。",
                 evidence)


async def _concentration_item(state, now: float) -> dict:
    try:
        from .database import NOT_TEST
        rows = await state.db._db.execute_fetchall(
            "SELECT key_id, key_name, COUNT(*) AS c FROM usage_log "
            f"WHERE ts>=? AND kind IN ('image','image_stream') AND status='ok' AND {NOT_TEST} "
            "GROUP BY key_id ORDER BY c DESC LIMIT 6", (now - DAY,))
        total = sum(r[2] for r in rows)
    except Exception as exc:
        return _item("concentration", "用量集中度（24 小时）", NA, f"读取失败：{exc}")
    if not rows or not total:
        return _item("concentration", "用量集中度（24 小时）", OK, "近 24 小时没有出图请求。")
    evidence = [f"{name or ('#' + str(kid))}：{c} 张（占全站 {round(c / total * 100)}%）" for kid, name, c in rows]
    top_share = rows[0][2] / total
    status = WARN if top_share >= CONCENTRATION_WARN else OK
    return _item("concentration", "用量集中度（24 小时）", status,
                 f"成功出图按 Key 分布；单把 Key 占比 ≥ {round(CONCENTRATION_WARN * 100)}% 时提示"
                 "（一把 Key 独吃全站产能，多半在共享或量产）。", evidence)


async def _burn_today_item(state, now: float) -> dict:
    nai = getattr(state, "nai", None)
    pool = list(getattr(nai, "pool", []) or [])
    if not pool:
        return _item("burn_today", "今日上游额度消耗（V5）", NA, "令牌池为空。")
    try:
        day = state.day(now)
        evidence, total_v5 = [], 0
        for t in pool:
            counter = await state.db.get_upstream_counter(t.token_id, day)
            v5 = int(counter.get("v5", 0))
            total_v5 += v5
            evidence.append(f"Token #{t.position}：今日图片 {int(counter.get('images', 0))} 张，V5 {v5} 张"
                            + (f"（本账号日限 {t.v5_daily_limit}）" if t.v5_daily_limit else ""))
    except Exception as exc:
        return _item("burn_today", "今日上游额度消耗（V5）", NA, f"读取失败：{exc}")
    cap = int(getattr(state.settings, "global_daily_v5", 0) or 0)
    if cap > 0:
        ratio = total_v5 / cap
        status = BAD if ratio >= 1 else (WARN if ratio >= 0.8 else OK)
        evidence.append(f"全站今日 V5 合计 {total_v5} / 日额度 {cap}（{round(ratio * 100)}%）")
    else:
        status = OK
        evidence.append(f"全站今日 V5 合计 {total_v5}（未设置全站日额度）")
    return _item("burn_today", "今日上游额度消耗（V5）", status,
                 "上游免费 V5 额度恢复速率有限，冲得越快越容易撞上游限流；这里看今天冲了多少。", evidence)


# ---------- 汇总 ----------

async def collect(state, now: Optional[float] = None) -> dict:
    """返回 {items, summary, generated_at}；全部只读。"""
    now = time.time() if now is None else now
    items = [
        _outbound_headers_item(state),
        _tls_stack_item(),
        _tls_fingerprint_item(),
        _egress_ip_item(),
        await _egress_ip_echo_item(),
        _browser_fingerprint_item(),
        await _upstream_signals_item(state, now),
        await _share_top_item(state, now),
        await _net_overlap_item(state, now),
        await _concentration_item(state, now),
        await _burn_today_item(state, now),
    ]
    counts = {s: sum(1 for it in items if it["status"] == s) for s in (OK, WARN, BAD, NA)}
    overall = BAD if counts[BAD] else (WARN if counts[WARN] else OK)
    return {"items": items, "summary": {**counts, "risk": overall}, "generated_at": now}
