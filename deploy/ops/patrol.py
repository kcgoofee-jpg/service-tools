#!/usr/bin/env python3
"""7×24 巡检：只读地检查站点数据、上游容量、成员出图请求与「开注册」的就绪度。

和 nai-watch.sh 的分工：watch 是给人看的实时事件流（前台/常驻跑），patrol 是给机器判断的体检
（退出码 + 可选告警），适合每 5～10 分钟跑一次（systemd timer / cron）。

用法：
    python3 deploy/ops/patrol.py              # 人读报告，退出码 0 正常 / 1 注意 / 2 危险
    python3 deploy/ops/patrol.py --oneline    # 一行摘要（写日志用）
    python3 deploy/ops/patrol.py --json       # 机器可读
    python3 deploy/ops/patrol.py --alert      # 有问题时给站长发 Discord 告警（同一问题 30 分钟只发一次）

只读：数据库用 mode=ro 打开，不写任何业务表；告警冷却状态写在 /var/lib/nai-patrol/。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path

DB = os.environ.get("NAI_GATE_DB", "/opt/service-tools/data/nai_gate.db")
ENV_FILE = os.environ.get("NAI_GATE_ENV", "/opt/service-tools/.env")
HEALTH = os.environ.get("NAI_GATE_HEALTH", "http://127.0.0.1:3003/healthz")
STATE_DIR = Path(os.environ.get("PATROL_STATE_DIR", "/var/lib/nai-patrol"))
COOLDOWN = int(os.environ.get("PATROL_COOLDOWN", "1800"))
STALE_SECONDS = int(os.environ.get("PATROL_STALE", "2700"))    # 45 分钟没再出现 = 旧记录，不告警
NZ = "+8 hours"                       # 站点按 Asia/Shanghai 记「天」

# 已知 / 按设计的事项：照常显示、不告警。加进来之前先问一句「这是不是要站长改决定」——
# 是的话就在后台挂一条说明，而不是每分钟响一次（今天真出问题的那次就是这么被淹没的）。
KNOWN_SIGNATURES = (
    ("V5：每人额度 × 人数 ≤ 全站上限", "按设计：名额超卖，已在首页写明「先到先得」，改分配结果需站长点头"),
    ("每人承诺 ≤ 全站日容量", "按设计：开注册就绪度提示，是否收紧名额/额度由站长决定"),
    ("后台额度与环境变量一致", "配置漂移提示：只在后台值与环境变量不一致时出现，盯 .env 即可"),
)

OK, WARN, RED = "ok", "warn", "red"
RANK = {OK: 0, WARN: 1, RED: 2}

# Discord 前面有 Cloudflare：用 python 默认的 User-Agent（Python-urllib/3.x）会被判成机器人直接 403 + error code 1010。
# 2026-10-13 实测：默认 UA → 403；换成下面这组 → 200。别删。
BROWSER_HEADERS = {"User-Agent": "DiscordBot (https://gate.davidzhao.top, 1.0)", "Accept": "application/json"}


class Report:
    def __init__(self):
        self.rows: list[tuple[str, str, str]] = []      # (severity, 区域, 文本)
        self.keys: list[str] = []                       # 非 ok 行的稳定标识（告警去重用，不含会变的数字）
        self.data: dict = {}

    def add(self, severity: str, area: str, text: str, *, key: str = "") -> None:
        self.rows.append((severity, area, text))
        if severity != OK:
            self.keys.append(key or text)

    def known(self, area: str, text: str) -> None:
        """已知 / 按设计 / 已不再复现的事项：照常显示给人看，但不参与告警。

        2026-10-13 的教训：三条「按设计」的交叉校验（V5 名额超卖、注册承诺、配置漂移）
        每 10 分钟告警一次，站长只能把告警当背景音 —— 真出问题（局部重绘集体 500）反而被淹没。
        """
        self.rows.append((OK, area, "（已知，不告警）" + text))

    @property
    def worst(self) -> str:
        return max((s for s, _, _ in self.rows), key=lambda s: RANK[s], default=OK)

    def problems(self) -> list[tuple[str, str, str]]:
        return [r for r in self.rows if r[0] != OK]


def q1(cur, sql, args=()):
    row = cur.execute(sql, args).fetchone()
    return row[0] if row else None


def http_code(url: str, timeout: float = 5) -> int:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return response.status
    except urllib.error.HTTPError as exc:
        return exc.code
    except Exception:                     # noqa: BLE001  连不上就是连不上，交给调用方判断
        return 0


def http_json(url: str, timeout: float = 5) -> dict:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8", "replace"))
    except Exception:                     # noqa: BLE001
        return {}


def docker_running() -> dict[str, bool]:
    out: dict[str, bool] = {}
    try:
        names = subprocess.run(["docker", "ps", "--format", "{{.Names}}"], capture_output=True,
                               text=True, timeout=10).stdout.split()
        for name in ("nai-gate", "nai-gate-discord", "caddy"):
            out[name] = name in names
    except Exception:                     # noqa: BLE001
        pass
    return out


_DOCKER_TIME = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:?\d{2})$")


def parse_docker_time(text: str) -> float:
    """docker 的 StartedAt 是 RFC3339Nano（最多 9 位小数纳秒）。

    2026-10-13 踩到两次：Python 3.10 的 fromisoformat 既不认 'Z'，也只吃 6 位小数；
    3.11+ 才放宽。线上巡检跑的是系统 python3（3.10），直接解析抛 ValueError →
    重启历史永远为空 → 部署窗口里的前端失败还是被当成故障。
    """
    text = (text or "").strip()
    if not text:
        return 0.0
    match = _DOCKER_TIME.match(text)
    if not match:
        return 0.0
    seconds, frac, offset = match.group(1), match.group(2) or "", match.group(3)
    stamp = seconds + ("." + frac[:6] if frac else "")          # 纳秒截到微秒
    try:
        return datetime.fromisoformat(stamp + ("+00:00" if offset == "Z" else offset)).timestamp()
    except ValueError:
        return 0.0


def container_started_at(name: str = "nai-gate") -> float:
    """网关容器的启动时间：用来把「部署重启那十几秒的前端失败」和真故障分开。"""
    try:
        out = subprocess.run(["docker", "inspect", "-f", "{{.State.StartedAt}}", name],
                             capture_output=True, text=True, timeout=10).stdout.strip()
        return parse_docker_time(out)
    except Exception:                     # noqa: BLE001
        return 0.0


def restart_history(name: str = "nai-gate") -> list[float]:
    """网关重启时间的历史（最近 20 次，存在 STATE_DIR）。

    只认「最近一次重启」不够用：今天连发三次版，上一轮窗口里失败的前端请求，
    到下一轮巡检时已经不算「重启后 10 分钟内」了，于是又被当成故障。
    """
    started = container_started_at(name)
    path = STATE_DIR / "restarts.json"
    try:
        hist = [float(x) for x in json.loads(path.read_text())]
    except Exception:                     # noqa: BLE001
        hist = []
    if started and (not hist or started > hist[-1] + 1):
        hist.append(started)
    hist = sorted(set(hist))[-20:]
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(hist))
    except OSError:
        pass
    return hist


def env_value(key: str) -> str:
    try:
        for line in Path(ENV_FILE).read_text(encoding="utf-8", errors="replace").splitlines():
            line = line.strip()
            if line.startswith(key + "="):
                return line.split("=", 1)[1].strip().strip('"').strip("'")
    except OSError:
        pass
    return ""


def read_setting(cur, key: str):
    return q1(cur, "SELECT value FROM site_settings WHERE key=?", (key,))


def num(raw, default=0):
    try:
        return int(float(raw))
    except (TypeError, ValueError):
        return default


# ---------------------------------------------------------------- 各项检查
def check_infra(rep: Report, cur) -> None:
    running = docker_running()
    if not running:
        rep.add(WARN, "系统", "读不到容器状态（docker 不可用？）")
    for name, up in running.items():
        rep.add(OK if up else RED, "系统", f"{name} 容器 {'运行中' if up else '没有运行'}")
    code = http_code(HEALTH)
    rep.add(OK if code == 200 else RED, "系统", f"/healthz → {code or '无响应'}")
    rep.add(*status_of_queue(http_json(f"http://127.0.0.1:3003/queue-status")))
    try:
        mem = subprocess.run(["free", "-m"], capture_output=True, text=True, timeout=5).stdout
        avail = int([line for line in mem.splitlines() if line.startswith("Mem:")][0].split()[6])
        disk = int(subprocess.run(["df", "/"], capture_output=True, text=True, timeout=5)
                   .stdout.splitlines()[1].split()[4].rstrip("%"))
        rep.data.update(mem_available_mb=avail, disk_used_pct=disk)
        rep.add(RED if avail < 200 else WARN if avail < 350 else OK, "系统", f"可用内存 {avail}MB · 磁盘 {disk}%")
        rep.add(RED if disk > 90 else WARN if disk > 80 else OK, "系统", f"磁盘已用 {disk}%")
    except Exception as exc:              # noqa: BLE001
        rep.add(WARN, "系统", f"资源读取失败：{type(exc).__name__}")
    if os.path.exists(DB):
        rep.data["db_mb"] = round(os.path.getsize(DB) / 1048576, 1)
        rep.add(OK, "系统", f"数据库 {rep.data['db_mb']}MB")


def status_of_queue(queue: dict) -> tuple[str, str, str]:
    if not queue:
        return WARN, "系统", "队列状态读取失败"
    waiting = num((queue.get("global") or {}).get("waiting"))
    active = num((queue.get("global") or {}).get("active"))
    cool = num(queue.get("image_cooldown_remaining"))
    severity = OK if waiting <= 20 and cool == 0 else WARN
    text = f"队列 active {active} / waiting {waiting}" + (f" · 上游冷却 {cool}s" if cool else "")
    return severity, "系统", text


def check_upstream(rep: Report, cur, per_person: int, active_members: int) -> None:
    now = time.time()
    day = q1(cur, "SELECT date('now',?)", (NZ,))
    used = num(q1(cur, "SELECT COALESCE(SUM(images),0) FROM upstream_token_counters WHERE day=?", (day,)))
    daily_cap = num(read_setting(cur, "guard_account_daily_cap"), 1000)
    # 账号数按最近两天取（当天 0 点后还没有计数行时也能知道有几个账号）
    accounts = num(q1(cur, "SELECT COUNT(DISTINCT token_id) FROM upstream_token_counters "
                           "WHERE day>=?", (q1(cur, "SELECT date('now',?,'-1 day')", (NZ,)),))) or 1
    site_daily = daily_cap * max(1, accounts)
    share = used / site_daily if site_daily else 0
    rep.data.update(today_images=used, site_daily=site_daily, capacity_used_pct=round(share * 100, 1))
    rep.add(WARN if share >= 0.9 else OK, "上游",
            f"今日已出图 {used} / {site_daily} 张（{share * 100:.0f}%）· {accounts} 个账号 × {daily_cap} 张")

    algo = {}
    try:
        algo = json.loads(read_setting(cur, "quota_algo_last") or "{}")
    except (TypeError, ValueError):
        pass
    v5 = algo.get("v5") or {}
    pct = v5.get("percent")
    if pct is not None:
        rep.data["v5_percent"] = pct
        rep.add(WARN if float(pct) <= 2 else OK, "上游", f"V5 剩余 {pct}%（恢复 {v5.get('rate')}%/天）")
    # 名额承诺 vs 日容量：开注册前最关键的一条
    if per_person and active_members:
        promise = per_person * active_members
        rep.data.update(promise_images=promise, per_person=per_person, active_members=active_members)
        rep.add(WARN if promise > site_daily else OK, "注册",
                f"每人 {per_person} 张 × {active_members} 人 = {promise} 张 vs 日容量 {site_daily} 张："
                f"平分每人约 {site_daily // max(1, active_members)} 张，只有前约 {site_daily // max(1, per_person)} 人能拿满")


def check_members(rep: Report, cur) -> None:
    """成员生图请求：成功率、等待、拒绝结构、每 Key 出图间隔。"""
    now = time.time()
    hour = cur.execute(
        "SELECT COUNT(*), COALESCE(SUM(images),0), COALESCE(MAX(wait_ms),0), COALESCE(CAST(AVG(wait_ms) AS INT),0) "
        "FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'", (now - 3600,)).fetchone()
    ok_n, ok_img, worst_wait, avg_wait = hour[0], hour[1], hour[2], hour[3]
    rep.data.update(hour_ok=ok_n, hour_images=ok_img, hour_worst_wait_ms=worst_wait)
    rep.add(WARN if worst_wait > 120_000 else OK, "成员",
            f"近 1 小时成功 {ok_n} 次 / {ok_img} 张 · 平均等待 {avg_wait / 1000:.0f}s · 最长 {worst_wait / 1000:.0f}s")

    stat = dict(cur.execute(
        "SELECT status, COUNT(*) FROM usage_log WHERE ts>? AND kind LIKE 'image%' GROUP BY status", (now - 86400,)))
    total = sum(stat.values()) or 1
    rep.data["day_status"] = stat
    ok_rate = stat.get("ok", 0) / total
    rep.add(WARN if ok_rate < 0.85 else OK, "成员",
            f"24 小时：成功 {stat.get('ok', 0)} · 拒绝 {stat.get('rejected', 0)} · 出错 {stat.get('error', 0)}"
            f"（成功率 {ok_rate * 100:.0f}%）")
    reasons = cur.execute(
        "SELECT COALESCE(reason,'(空)'), COUNT(*) FROM usage_log WHERE ts>? AND status='rejected' "
        "GROUP BY 1 ORDER BY 2 DESC LIMIT 5", (now - 86400,)).fetchall()
    rep.data["day_reasons"] = list(reasons)
    if reasons:
        top = "、".join(f"{name} {count}" for name, count in reasons)
        capacity_reasons = sum(count for name, count in reasons if name in ("paced", "daily_cap", "hourly_cap"))
        rep.add(WARN if capacity_reasons / max(1, sum(c for _, c in reasons)) > 0.7 else OK,
                "成员", f"24 小时拒绝原因：{top}")
    # 每 Key 出图间隔应 ≥ 15 秒（网关硬限制）；出现连续两次 < 12 秒说明间隔失效
    pairs = cur.execute(
        """SELECT COUNT(*) FROM (
             SELECT key_id, ts - LAG(ts) OVER (PARTITION BY key_id ORDER BY ts) AS gap
             FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%' AND COALESCE(key_id,0)>0)
           WHERE gap IS NOT NULL AND gap BETWEEN 0 AND 12""", (now - 3600,)).fetchone()[0]
    rep.add(WARN if pairs > 3 else OK, "成员", f"近 1 小时间隔疑似被绕过（同 Key <12 秒连发）：{pairs} 次")

    active = q1(cur, "SELECT COUNT(DISTINCT key_id) FROM usage_log WHERE ts>? AND status='ok' "
                     "AND kind LIKE 'image%' AND COALESCE(key_id,0) NOT IN (SELECT id FROM api_keys WHERE is_test=1)",
                (now - 86400,))
    rep.data["active_members_24h"] = num(active)

    # 客户端分布：能认出来的（小白盒 / 酒馆 / 启动器 / DSH…）与只有浏览器 UA 的分别有多少
    clients = cur.execute(
        "SELECT substr(COALESCE(NULLIF(client,''),'(未记录)'),1,34) c, COUNT(*) FROM usage_log "
        "WHERE ts>? AND kind LIKE 'image%' GROUP BY 1 ORDER BY 2 DESC LIMIT 6", (now - 86400,)).fetchall()
    if clients:
        rep.data["clients_24h"] = list(clients)
        rep.add(OK, "成员", "24 小时客户端：" + "、".join(f"{name} ×{count}" for name, count in clients))


def check_registration(rep: Report, cur) -> tuple[int, int]:
    """注册就绪度：开关、名额、链路、领取速度。返回 (每人额度, 报名人数)。"""
    open_now = (read_setting(cur, "register_open") or "0") in ("1", "true", "yes", "on")
    per = num(read_setting(cur, "register_daily_images"), 30)
    v5_per = num(read_setting(cur, "register_daily_v5"), 0)
    max_users = num(read_setting(cur, "register_max_users"), 0)
    hourly = num(read_setting(cur, "issue_hourly_cap"), 12)
    registered = num(q1(cur, "SELECT COUNT(*) FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id "
                            "WHERE k.expires_at IS NULL OR k.expires_at>?", (time.time(),)))
    new24 = num(q1(cur, "SELECT COUNT(*) FROM discord_registrations WHERE created_at>?", (time.time() - 86400,)))
    new1h = num(q1(cur, "SELECT COUNT(*) FROM discord_registrations WHERE created_at>?", (time.time() - 3600,)))
    waitlist = num(q1(cur, "SELECT COUNT(*) FROM waitlist"))
    rep.data.update(register_open=open_now, registered=registered, register_daily_images=per,
                    register_daily_v5=v5_per, register_max_users=max_users, new_24h=new24, new_1h=new1h,
                    waitlist=waitlist, issue_hourly_cap=hourly)
    rep.add(OK, "注册",
            f"{'开放' if open_now else '关闭'} · 已领 {registered} 人 / 名额 {max_users or '不限'} · "
            f"每人 {per} 张（V5 {v5_per}）· 每小时最多发 {hourly} 个 · 候补 {waitlist}")
    rep.add(OK if new24 <= 288 else WARN, "注册", f"新注册：24 小时 {new24} 人 · 1 小时 {new1h} 人")
    if open_now and new1h >= hourly:
        rep.add(WARN, "注册", f"本小时领取已达上限（{new1h}/{hourly}），成员会看到限流提示")
    if not env_value("REGISTRATION_BRIDGE_SECRET"):
        rep.add(RED if open_now else WARN, "注册", "网关没有 REGISTRATION_BRIDGE_SECRET：/register 会直接失败")
    # 后台额度 vs 环境变量（后台值被清掉会跳回环境变量）
    env_per = num(env_value("REGISTER_DAILY_IMAGES"), 0)
    env_quota = num(env_value("REGISTER_MAX_USERS"), 0)
    if env_per and env_per != per:
        rep.add(WARN, "注册", f"每人额度：后台 {per} / 环境变量 {env_per}（后台值被清掉会跳到 {env_per}）")
    if env_quota and env_quota != max_users:
        rep.add(WARN, "注册", f"名额：后台 {max_users or '不限'} / 环境变量 {env_quota}")
    return per, registered


def check_data(rep: Report, cur) -> None:
    """记账一致性 + Bug 追踪 + 防分享信号。"""
    day = q1(cur, "SELECT date('now',?)", (NZ,))
    counters = num(q1(cur, "SELECT COALESCE(SUM(images),0) FROM counters WHERE day=?", (day,)))
    upstream = num(q1(cur, "SELECT COALESCE(SUM(images),0) FROM upstream_token_counters WHERE day=?", (day,)))
    logged = num(q1(cur, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE status='ok' AND kind LIKE 'image%' "
                          "AND ts>=strftime('%s','now','+8 hours','start of day','-8 hours')"))
    rep.data.update(today_counters=counters, today_logged=logged)
    same = abs(counters - upstream) <= max(2, counters * 0.02)
    rep.add(OK if same else WARN, "数据", f"今日计数：成员 {counters} · 上游 {upstream} · 日志 {logged} 张",
            key="counts")
    bugs = dict(cur.execute("SELECT level, COUNT(*) FROM error_events WHERE resolved_at IS NULL GROUP BY level"))
    rep.data["bugs_open"] = bugs
    # 只有 error 级才告警：warn 级多为「按设计」的交叉校验提示，会长期挂着（见下面 KNOWN_SIGNATURES）
    rep.add(RED if num(bugs.get("error")) else OK, "数据",
            f"未解决 Bug：error {bugs.get('error', 0)} · warn {bugs.get('warn', 0)}", key="bug-count")
    now = time.time()
    restarts = restart_history()
    recent = cur.execute("SELECT level, substr(title,1,58), count, last_seen FROM error_events "
                         "WHERE resolved_at IS NULL ORDER BY last_seen DESC LIMIT 6").fetchall()
    for level, title, count, last_seen in recent:
        known_why = next((why for sig, why in KNOWN_SIGNATURES if sig in title), "")
        hit = next((r for r in restarts if r <= num(last_seen) < r + 180), 0.0)
        if not known_why and title.startswith("live:") and hit:
            # 前端每秒轮询 /public/live，网关重启那十几秒必然失败 —— 我们自己的部署不算故障
            rep.known("数据", f"{title}（{count} 次）— 最近一次在网关重启（{time.strftime('%H:%M', time.localtime(hit))}）"
                              "后 3 分钟内，部署窗口的正常失败")
            continue
        if known_why:
            rep.known("数据", f"[{level}] {title}（{count} 次）— {known_why}")
        elif num(last_seen) < now - STALE_SECONDS:
            # 已不再出现的旧记录：留着给站长看，但不该每 10 分钟吵一次
            rep.known("数据", f"[{level}] {title}（{count} 次，最近 {int((now - num(last_seen)) / 60)} 分钟没再出现）")
        else:
            rep.add(WARN if level in ("warn", "error") else OK, "数据", f"[{level}] {title}（{count} 次）",
                    key=f"bug:{title}")
    evidence = cur.execute("SELECT kind, COUNT(*) FROM share_evidence WHERE ts>? GROUP BY kind",
                           (time.time() - 86400,)).fetchall()
    if evidence:
        rep.add(OK, "数据", "防分享 24 小时证据：" + "、".join(f"{k} {c}" for k, c in evidence))


def check_channels(rep: Report, cur) -> None:
    """各条业务通道「现在真的能用吗」——今天连续修好文本与局部重绘，回归要立刻看得见。

    - 文本 / 聊天：近 30 分钟成功率（修好之前是 0%，近 7 天一次都没成功过）
    - 出图上游 500：局部重绘缺蒙版 / 带 SMEA 就是这个签名（上游不给可读原因）
    - 局部重绘：近 1 小时的成功与失败，重绘坏了要单独一眼看到
    """
    now = time.time()
    row = cur.execute("SELECT COUNT(*), SUM(status='ok') FROM usage_log WHERE ts>? "
                      "AND kind IN ('chat','text','text_stream','voice')", (now - 1800,)).fetchone()
    total, ok = num(row[0]), num(row[1])
    rep.data.update(text_30m=total, text_ok_30m=ok)
    if total:
        rate = ok / total
        rep.add(OK if rate >= 0.9 else WARN if rate >= 0.5 else RED, "通道",
                f"文本/聊天 30 分钟：{ok}/{total} 成功（{rate:.0%}）", key="text-channel")
        if rate < 0.9:
            last = cur.execute("SELECT COALESCE(detail,''), datetime(ts,'unixepoch','localtime'), COALESCE(rid,'') "
                               "FROM usage_log WHERE ts>? AND kind IN ('chat','text','text_stream') "
                               "AND status!='ok' ORDER BY ts DESC LIMIT 1", (now - 1800,)).fetchone()
            if last:
                rep.add(WARN, "通道", f"最近一条文本失败：{last[1]} {last[0][:50]}（rid {last[2] or '-'}）",
                        key="text-last-error")
    five = num(q1(cur, "SELECT COUNT(*) FROM usage_log WHERE ts>? AND kind LIKE 'image%' "
                       "AND (detail LIKE '%upstream 500%' OR up_status=500)", (now - 1800,)))
    if five:
        rep.add(WARN if five < 5 else RED, "通道",
                f"近 30 分钟出图上游 500 × {five}（局部重绘缺蒙版 / 图生图带 SMEA 会这样，查 rid）",
                key="image-500")
    ip = cur.execute("SELECT COUNT(*), SUM(status='ok') FROM usage_log WHERE ts>? AND model LIKE '%inpainting%'",
                     (now - 3600,)).fetchone()
    ip_total, ip_ok = num(ip[0]), num(ip[1])
    if ip_total:
        rep.data.update(inpaint_1h=ip_total, inpaint_ok_1h=ip_ok)
        rep.add(OK if ip_ok == ip_total else WARN, "通道",
                f"局部重绘 1 小时：{ip_ok}/{ip_total} 成功", key="inpaint")

    # 上游 429 = 账号级红线（2026-10-13 01:30 第一次遇到）：一定要让站长立刻知道。
    # 我们的限流器只能挡住自己放行出去的量，挡不住「绕过网关直连上游」的突发。
    last_429 = num(read_setting(cur, "guard_last_upstream_429"))
    if last_429 and now - last_429 < 3600:
        rep.add(RED, "通道", f"上游 {max(1, int((now - last_429) / 60))} 分钟前返回 429（限流）："
                             "上限已自动减半并进入冷却，请确认没有额外突发打到上游", key="upstream-429")
    n429 = num(q1(cur, "SELECT COUNT(*) FROM usage_log WHERE ts>? AND up_status=429", (now - 3600,)))
    if n429:
        rep.add(RED, "通道", f"近 60 分钟上游 429 × {n429}（账号级限流，必须立刻降速）", key="upstream-429-count")
    # 余量：离当前每小时上限还有多少 —— 决定要不要再往下降一档
    cap = num(read_setting(cur, "guard_account_hourly_cap"), 0)
    used = num(q1(cur, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' "
                       "AND kind LIKE 'image%'", (now - 3600,)))
    rep.data.update(hourly_cap=cap, hourly_used=used)
    if cap:
        rep.add(OK if used < cap * 0.9 else WARN, "通道",
                f"近 60 分钟 {used} 张 / 每小时上限 {cap} 张（余量 {max(0, cap - used)}）", key="hourly-headroom")


# ---------------------------------------------------------------- 告警
def send_alert(text: str) -> bool:
    token, user, channel = env_value("DISCORD_BOT_TOKEN"), env_value("ALERT_USER_ID"), env_value("ALERT_CHANNEL_ID")
    if not token or not (user or channel):
        print("[patrol] 未配置告警通道（ALERT_USER_ID / ALERT_CHANNEL_ID），只记日志", file=sys.stderr)
        return False
    headers = {"Authorization": "Bot " + token, "Content-Type": "application/json", **BROWSER_HEADERS}
    try:
        if channel:
            url, body = f"https://discord.com/api/v10/channels/{channel}/messages", {"content": text[:1900]}
        else:
            request = urllib.request.Request("https://discord.com/api/v10/users/@me/channels",
                                             data=json.dumps({"recipient_id": user}).encode(),
                                             headers=headers, method="POST")
            with urllib.request.urlopen(request, timeout=10) as response:
                channel_id = json.loads(response.read())["id"]
            url, body = f"https://discord.com/api/v10/channels/{channel_id}/messages", {"content": text[:1900]}
        request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status in (200, 201)
    except urllib.error.HTTPError as exc:                       # 403 + Cloudflare 1010 = UA 不合规
        hint = "（Cloudflare 1010：User-Agent 被拦）" if exc.code == 403 and "1010" in _safe_read(exc) else ""
        print(f"[patrol] 告警发送失败：HTTP {exc.code}{hint}", file=sys.stderr)
        return False
    except Exception as exc:              # noqa: BLE001
        print(f"[patrol] 告警发送失败：{type(exc).__name__}", file=sys.stderr)
        return False


def _safe_read(exc: urllib.error.HTTPError) -> str:
    try:
        return exc.read().decode("utf-8", "replace")[:200]
    except Exception:                     # noqa: BLE001
        return ""


def maybe_alert(rep: Report, text: str) -> None:
    """同一批问题在冷却期内只发一次。

    去重用**检查项标识**（不含次数、张数这些每分钟都在变的数字）：以前用整段文本做哈希，
    数字一变就被当成新问题，于是「V5 超卖」每 10 分钟响一次。
    """
    digest = hashlib.sha1("|".join(sorted(rep.keys)).encode()).hexdigest()[:16]
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    mark = STATE_DIR / "last_alert.json"
    now = time.time()
    try:
        last = json.loads(mark.read_text())
    except Exception:                     # noqa: BLE001
        last = {}
    if last.get("digest") == digest and now - float(last.get("at", 0)) < COOLDOWN:
        return
    if send_alert(text):
        mark.write_text(json.dumps({"digest": digest, "at": now}))
        print("[patrol] 已发送告警", file=sys.stderr)


# ---------------------------------------------------------------- 主流程
def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--alert", action="store_true", help="有问题时给站长发 Discord 告警")
    parser.add_argument("--json", action="store_true", help="输出 JSON")
    parser.add_argument("--oneline", action="store_true", help="只输出一行摘要")
    args = parser.parse_args()

    rep = Report()
    per_person = active_members = 0
    try:
        conn = sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=10)
    except sqlite3.Error as exc:
        print(f"[patrol] 打不开数据库 {DB}：{exc}")
        return 2
    try:
        cur = conn.cursor()
        check_infra(rep, cur)
        check_data(rep, cur)
        per_person, active_members = check_registration(rep, cur)
        check_upstream(rep, cur, per_person, active_members)
        check_members(rep, cur)
        check_channels(rep, cur)
    finally:
        conn.close()

    worst = rep.worst
    if args.json:
        print(json.dumps({"worst": worst, "data": rep.data,
                          "rows": [{"severity": s, "area": a, "text": t} for s, a, t in rep.rows]},
                         ensure_ascii=False, indent=2))
    elif args.oneline:
        icon = {OK: "OK", WARN: "WARN", RED: "RED"}[worst]
        # 按严重度排序再截取：RED 不能被排在前面的 warn 挤出头条（2026-10-13 凌晨，429 的 RED 就被挤掉了）
        head = "；".join(f"{a}: {t}" for s, a, t in sorted(rep.problems(), key=lambda r: -RANK[r[0]])[:3]) or "全部正常"
        print(f"[patrol {icon}] {time.strftime('%m-%d %H:%M')} {head}")
    else:
        print(f"=== 巡检 {time.strftime('%Y-%m-%d %H:%M:%S')} → {worst.upper()} ===")
        for area in ("系统", "上游", "通道", "注册", "成员", "数据"):
            for severity, row_area, text in rep.rows:
                if row_area == area:
                    print(f"  [{'ok  ' if severity == OK else 'warn' if severity == WARN else 'RED '}] {text}")
        problems = rep.problems()
        print(f"=== 结论：{worst.upper()}（{len(problems)} 项需要关注）===")
    if args.alert and rep.problems():
        ranked = sorted(rep.problems(), key=lambda r: -RANK[r[0]])
        body = "🔎 **巡检** " + time.strftime("%m-%d %H:%M") + "\n" + "\n".join(
            f"{'❗' if s == RED else '⚠'} {a}：{t}" for s, a, t in ranked[:12])
        maybe_alert(rep, body)
    return RANK[worst]


if __name__ == "__main__":
    sys.exit(main())
