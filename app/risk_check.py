"""风险检查（后台只读诊断）：对截图指标逐项体检，能做的做实，做不了的留空并说明原因。

两个用途：
1. 出站指纹盘点 —— 我们发给 NovelAI 的流量现在长什么样（请求头、TLS 栈、出口链路），
   站长知情即可，本模块不伪造任何一项；
2. 白嫖 / 滥用识别 —— 谁在多人共用一把 Key、谁在把站当量产机：防分享风险分、网段共用、
   用量集中度、上游限流 / 认证失败信号、当日 V5 消耗。

━━ 出站伪装的现状（2026-10-10 站长批准，见 nai.py「anti-ban」）━━
- 请求头：已模拟 Chrome 浏览器（UA、sec-ch-ua、Origin、Referer 等），每把 Token 固定一套 profile；
  查额度、对账、查 Anlas 也用同一套。
- HTTP/2：装了 h2 就启用；请求后 1~3 秒随机间隔；连续 3 次 403 冷却 5 分钟。
- 代理：支持 UPSTREAM_PROXY，未配置时直连。
- TLS(JA3/JA4) 指纹：默认没有改，仍是 Python/OpenSSL 的原生指纹（页面如实标出这一不一致）；
  配置 UPSTREAM_TLS_IMPERSONATE 后改走 curl_cffi 模拟 Chrome，页面显示目标与请求头版本是否对齐。
- 浏览器端指标（Canvas / WebGL 等）对纯服务端进程不适用。

全部只读：不写数据库、不发上游请求（出口 IP 回显除外且默认关闭）、不发 Discord 消息。
"""
from __future__ import annotations

import os
import ssl
import sys
import time
from typing import Optional

DAY = 24 * 3600

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
    """只露后 4 位：上游 Token 是全站最敏感的凭据，后台页面也不必多露。"""
    token = token or ""
    return "…" + token[-4:] if len(token) > 8 else ("…" if token else "")


def _redact_proxy(value: str) -> str:
    """代理地址里的账号密码（scheme://user:pass@host）打码后再显示。"""
    import re
    return re.sub(r"(://)[^/@\s]+@", r"\1***@", value)


# ---------- 出站指纹盘点 ----------

def _outbound_headers_item(state) -> dict:
    """如实列出生图请求真正发出去的头（来自 nai.default_browser_headers + 每把 Token 的 profile）。"""
    from .nai import default_browser_headers
    nai = getattr(state, "nai", None)
    pool = list(getattr(nai, "pool", []) or [])
    if not pool:
        return _item("outbound_headers", "出站请求头", NA, "上游客户端尚未启动，没有 Token。")
    evidence = []
    for ts in pool:
        h = default_browser_headers("x", profile=getattr(ts, "browser_profile", None))
        h.pop("Authorization", None)
        evidence.append(f"Token #{ts.position}（{_mask_token(ts.token)}）：")
        evidence += [f"  {k}: {v}" for k, v in h.items()]
    client = getattr(nai, "_client", None)
    http2 = bool(getattr(nai, "_http2", False))
    try:
        import h2  # noqa: F401
        h2_ok = True
    except ImportError:
        h2_ok = False
    evidence.append(f"HTTP/2：{'启用' if http2 and h2_ok else '未启用（缺 h2 包，已降级 HTTP/1.1）' if http2 else '关闭'}")
    evidence.append(f"请求后随机间隔：{getattr(nai, '_post_jitter_min', '?')}~{getattr(nai, '_post_jitter_max', '?')} 秒；"
                    "连续 3 次 403 冷却 5 分钟")
    status = OK if client is not None and (h2_ok or not http2) else WARN
    return _item("outbound_headers", "出站请求头", status,
                 "生图、查额度、对账、查 Anlas 都发同一套 Chrome 浏览器请求头（站长 10/10 批准）。", evidence)


def _tls_stack_item(state=None) -> dict:
    try:
        import httpx
        httpx_ver = httpx.__version__
    except Exception:
        httpx_ver = "未知"
    evidence = [f"Python {sys.version.split()[0]} + httpx {httpx_ver}",
                f"TLS 栈：{ssl.OPENSSL_VERSION}",
                "TLS 握手特征随 Python/OpenSSL 版本与配置变化；升级依赖后指纹会变，属正常现象。"]
    target = getattr(getattr(state, "nai", None), "tls_target", None)
    if target:
        evidence.append(f"上游 NovelAI 请求不走上面的 OpenSSL：已改用 curl_cffi（BoringSSL）模拟 {target}")
    return _item("tls_stack", "TLS 栈自述", OK,
                 "本机出站 TLS 使用的软件版本。只报告，不测量、不伪装。", evidence)


def _tls_fingerprint_item(state=None) -> dict:
    nai = getattr(state, "nai", None)
    target = getattr(nai, "tls_target", None)
    if target:
        import re
        from .nai import BROWSER_PROFILES
        from .tls_impersonate import chrome_major
        try:
            from curl_cffi import __version__ as cc_ver
        except Exception:
            cc_ver = "未知"
        major = chrome_major(target)
        uas = sorted({getattr(ts, "browser_profile", {}).get("user_agent", "") for ts in getattr(nai, "pool", []) or []}
                     or {p["user_agent"] for p in BROWSER_PROFILES})
        ua_majors = {m.group(1) for ua in uas for m in [re.search(r"Chrome/(\d+)", ua)] if m}
        aligned = major is not None and ua_majors == {str(major)}
        evidence = [f"curl_cffi {cc_ver}，impersonate={target}",
                    "请求头 UA 自称：" + ("、".join(f"Chrome {v}" for v in sorted(ua_majors)) or "非 Chrome")]
        if not aligned:
            evidence.append("请求头里的 Chrome 版本与 TLS 目标不一致（自定义了 UPSTREAM_USER_AGENT，或目标不是桌面 Chrome）")
        return _item("tls_fingerprint", "TLS 指纹（JA3/JA4）", OK if aligned else WARN,
                     f"已开启：上游请求经 curl_cffi 发出，TLS 与 HTTP/2 握手模拟 {target}。", evidence)
    error = getattr(nai, "tls_error", None)
    if error:
        return _item("tls_fingerprint", "TLS 指纹（JA3/JA4）", WARN,
                     f"配置了 UPSTREAM_TLS_IMPERSONATE={getattr(nai, 'tls_requested', '')}，但没有生效，"
                     "已回退 httpx：TLS 握手仍是 Python/OpenSSL 的原生指纹，和请求头里自称的 Chrome 不一致。", [error])
    return _item("tls_fingerprint", "TLS 指纹（JA3/JA4）", WARN,
                 "没有改：TLS 握手仍是 Python/OpenSSL 的原生指纹，和请求头里自称的 Chrome 不一致。"
                 "实测要向第三方回显服务发请求，默认不做。是否改 TLS 指纹（如 curl_cffi）由站长决定。")


def _egress_ip_item() -> dict:
    proxies = {name: os.environ.get(name, "") for name in
               ("UPSTREAM_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")}
    active = [f"{k}={_redact_proxy(v)}" for k, v in proxies.items() if v]
    evidence = ["未配置代理：出站即服务器本机 IP，所有上游 Token 共用同一出口。" if not active
                else "检测到代理环境变量（httpx 默认信任它们）："]
    evidence += active
    return _item("proxy_config", "代理 / 出口链路", OK,
                 "网关走什么链路出站。生图支持 UPSTREAM_PROXY；未配置时直连本机 IP。",
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
    cap = 0
    try:   # 优先用额度算法当前给的全站 V5 日额度（动态），没有再退回环境变量的静态值
        import json
        cap = int(((json.loads(await state.db.get_setting("quota_algo_last", None) or "{}").get("v5") or {})
                   .get("global")) or 0)
    except (TypeError, ValueError, AttributeError):
        cap = 0
    cap = cap or int(getattr(state.settings, "global_daily_v5", 0) or 0)
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
        _tls_stack_item(state),
        _tls_fingerprint_item(state),
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
