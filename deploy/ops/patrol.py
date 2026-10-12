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
from pathlib import Path

DB = os.environ.get("NAI_GATE_DB", "/opt/service-tools/data/nai_gate.db")
ENV_FILE = os.environ.get("NAI_GATE_ENV", "/opt/service-tools/.env")
HEALTH = os.environ.get("NAI_GATE_HEALTH", "http://127.0.0.1:3003/healthz")
STATE_DIR = Path(os.environ.get("PATROL_STATE_DIR", "/var/lib/nai-patrol"))
COOLDOWN = int(os.environ.get("PATROL_COOLDOWN", "1800"))
NZ = "+8 hours"                       # 站点按 Asia/Shanghai 记「天」

OK, WARN, RED = "ok", "warn", "red"
RANK = {OK: 0, WARN: 1, RED: 2}


class Report:
    def __init__(self):
        self.rows: list[tuple[str, str, str]] = []      # (severity, 区域, 文本)
        self.data: dict = {}

    def add(self, severity: str, area: str, text: str) -> None:
        self.rows.append((severity, area, text))

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
    rep.add(OK if same else WARN, "数据", f"今日计数：成员 {counters} · 上游 {upstream} · 日志 {logged} 张")
    bugs = dict(cur.execute("SELECT level, COUNT(*) FROM error_events WHERE resolved_at IS NULL GROUP BY level"))
    rep.data["bugs_open"] = bugs
    rep.add(RED if bugs.get("error") else WARN if num(bugs.get("warn")) > 8 else OK, "数据",
            f"未解决 Bug：error {bugs.get('error', 0)} · warn {bugs.get('warn', 0)}")
    recent = cur.execute("SELECT level, substr(title,1,58), count FROM error_events WHERE resolved_at IS NULL "
                         "ORDER BY last_seen DESC LIMIT 3").fetchall()
    for level, title, count in recent:
        rep.add(WARN if level in ("warn", "error") else OK, "数据", f"[{level}] {title}（{count} 次）")
    evidence = cur.execute("SELECT kind, COUNT(*) FROM share_evidence WHERE ts>? GROUP BY kind",
                           (time.time() - 86400,)).fetchall()
    if evidence:
        rep.add(OK, "数据", "防分享 24 小时证据：" + "、".join(f"{k} {c}" for k, c in evidence))


# ---------------------------------------------------------------- 告警
def send_alert(text: str) -> bool:
    token, user, channel = env_value("DISCORD_BOT_TOKEN"), env_value("ALERT_USER_ID"), env_value("ALERT_CHANNEL_ID")
    if not token or not (user or channel):
        print("[patrol] 未配置告警通道（ALERT_USER_ID / ALERT_CHANNEL_ID），只记日志", file=sys.stderr)
        return False
    base = f"https://discord.com/api/v10/{'users/@me/channels' if not channel else 'channels'}"
    try:
        if channel:
            url, method, body = f"{base}/{channel}/messages", "POST", {"content": text[:1900]}
        else:
            req = urllib.request.Request("https://discord.com/api/v10/users/@me/channels",
                                         data=json.dumps({"recipient_id": user}).encode(),
                                         headers={"Authorization": "Bot " + token, "Content-Type": "application/json"},
                                         method="POST")
            with urllib.request.urlopen(req, timeout=10) as response:
                channel_id = json.loads(response.read())["id"]
            url, method, body = f"https://discord.com/api/v10/channels/{channel_id}/messages", "POST", {"content": text[:1900]}
        request = urllib.request.Request(url, data=json.dumps(body).encode(),
                                         headers={"Authorization": "Bot " + token, "Content-Type": "application/json"},
                                         method=method)
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status in (200, 201)
    except Exception as exc:              # noqa: BLE001
        print(f"[patrol] 告警发送失败：{type(exc).__name__}", file=sys.stderr)
        return False


def maybe_alert(rep: Report, text: str) -> None:
    """同一问题在冷却期内只发一次（用问题文本的哈希去重）。"""
    digest = hashlib.sha1("|".join(f"{s}{a}{t}" for s, a, t in rep.problems()).encode()).hexdigest()[:16]
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
    finally:
        conn.close()

    worst = rep.worst
    if args.json:
        print(json.dumps({"worst": worst, "data": rep.data,
                          "rows": [{"severity": s, "area": a, "text": t} for s, a, t in rep.rows]},
                         ensure_ascii=False, indent=2))
    elif args.oneline:
        icon = {OK: "OK", WARN: "WARN", RED: "RED"}[worst]
        head = "；".join(f"{a}: {t}" for s, a, t in rep.problems()[:3]) or "全部正常"
        print(f"[patrol {icon}] {time.strftime('%m-%d %H:%M')} {head}")
    else:
        print(f"=== 巡检 {time.strftime('%Y-%m-%d %H:%M:%S')} → {worst.upper()} ===")
        for area in ("系统", "上游", "注册", "成员", "数据"):
            for severity, row_area, text in rep.rows:
                if row_area == area:
                    print(f"  [{'ok  ' if severity == OK else 'warn' if severity == WARN else 'RED '}] {text}")
        problems = rep.problems()
        print(f"=== 结论：{worst.upper()}（{len(problems)} 项需要关注）===")
    if args.alert and rep.problems():
        body = "🔎 **巡检** " + time.strftime("%m-%d %H:%M") + "\n" + "\n".join(
            f"{'❗' if s == RED else '⚠'} {a}：{t}" for s, a, t in rep.problems()[:12])
        maybe_alert(rep, body)
    return RANK[worst]


if __name__ == "__main__":
    sys.exit(main())
