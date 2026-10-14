#!/usr/bin/env python3
"""旧 Python 网关 (app/) 与 Rust 网关 (crates/gate-server, 二进制 nai-gate) 的差分对拍。

用法（仓库根目录；一条命令，可反复运行）：

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/diff_gateways.py

常用选项：
    --skip-build        不跑 cargo build（用 target/release/nai-gate 现成的）
    --only A,C,B        只跑部分语料（A=成员接口 B=后台接口 C=公开页与成员网页会话 + 机器人桥接），按给定顺序跑
    --keep              保留 target/parity/work（两份数据目录、两个网关日志）方便事后排查
    --no-restart        语料之间不重启网关（默认重启：内存状态互不影响，也顺带验证优雅关停）
    --out PATH          报告路径（默认 target/parity-report.md；同时打印到 stdout，不提交进仓库）
    --list              只列出语料里的用例，不启动任何东西

做了什么：
  1. 造一份合成种子库（只含假数据，tools/ci/check-fixtures.sh 能过），复制成数据目录 A（旧 Python）和
     B（Rust），两边共用同一个 secret_key 文件和 ADMIN_PASSWORD，所以后台 / 成员会话 Cookie 可以互相验证
     （报告里“互认”几条：旧版签发的后台 Cookie 给 Rust 用，反过来也一样；成员会话由本程序按旧版格式签发）。
  2. 起两个一模一样的假上游（记录每个请求的方法、路径、查询串、按序的请求头、正文），各接一个网关；
     再起两个 Discord “黑洞”（只记录、一律拒绝：旧网关把 discord.com 写死在代码里，只能靠 HTTPS_PROXY 拦；
     Rust 经 DISCORD_API_BASE 指过来）。绝不访问真实的 NovelAI / Discord。
  3. 对两个网关依次重放同一份请求语料，逐条比较：状态码、响应头、JSON 正文（易变字段按“形状”比，不是整段丢弃）、
     上游收到的请求（路径 / 查询 / 请求头顺序 / 正文含键顺序）、发往 Discord 的尝试次数。每个语料结束后停机，
     逐表 diff 两份 SQLite（忽略时间 / 耗时 / 随机 Token，流水表按行内容比）以及 site_settings。
     语料之间重启网关（内存里的限流窗口 / 冷却 / 防爆破互不影响）；账号保护、安静时段等用“改库 + 重启”切换。
  4. 输出 Markdown 报告（stdout + target/parity-report.md）：每个接口 ✓ / ✗ / ⛔（Rust 尚未移植）；✗ 附最小 diff，
     在很多用例里重复出现的差异合并成“系统性差异”只写一次；docs/v3-deviations.md 里的有意差异列在“预期差异”，
     不算失败（匹配规则见下面的 DEVIATIONS，报告里会把被归入预期的差异原样列出来，方便人工复核）。

怎么读 ✗：先看“系统性差异”（一处根因影响很多接口），再看各接口自己的明细，最后是数据库 diff。
Rust 还没移植的写接口会让数据库 diff 里出现“只在旧版有”的行，移植完这类差异自然消失。

退出码：0 = 没有未解释的差异；1 = 有 ✗；2 = 对拍程序自身出错。

局限（报告末尾也会写）：
  - 假上游是明文 HTTP/1.1，请求头顺序是在 HTTP/1.1 下比的；线上走 TLS + HTTP/2（wreq 的 Chrome 指纹）这一层
    本程序看不到（见 docs/v3-deviations.md #3）。
  - 顺序重放：并发 / 排队 / 断连取消 / 每 Key 同时请求上限这类依赖时序的路径没有覆盖。
  - 旧版依赖版本取自 requirements.txt（未锁版本），可能与线上镜像里的版本不同；报告头部会写出实际版本。
  - Discord 交互只比“尝试了几次”；OAuth 回调签发的成员 Cookie（Rust 一侧）无法离线产生。
"""
from __future__ import annotations

import argparse
import atexit
import base64
import collections
import contextlib
import copy
import datetime as dt
import fnmatch
import hashlib
import hmac
import http.server
import io
import json
import os
import re
import shutil
import signal
import socket
import socketserver
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import urllib.parse
import zipfile
import zlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional
from zoneinfo import ZoneInfo

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

TZ_NAME = "Asia/Shanghai"
ADMIN_PASSWORD = "parity-admin-password-123"
SECRET = "0" * 16 + "parity-secret-key-not-for-production-0000"
BRIDGE_SECRET = "parity-bridge-secret-0123456789abcdefghij"
UPSTREAM_TOKEN = "pst-synthetic-parity-token"
GUILD_ID = "900000000000000100"
SEED_ANNOUNCEMENT = "<p>种子公告：欢迎来到测试站</p>"
SYNTH_DISCORD = "9000000000000000"  # 合成 Discord ID 前缀（+2 位序号）
PY, RS = "py", "rs"                  # 两个“侧”：旧 Python / 新 Rust
SETTLE = 0.08                        # 每条请求之后等后台记账任务落库的秒数


def log(msg: str) -> None:
    print(f"[parity {time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)


# ============================================================ 进程管理 ====
PROCS: list[subprocess.Popen] = []
CLEANUPS: list[Callable[[], None]] = []        # 可能阻塞（停假服务、关 httpx 连接池）：信号处理里不能跑，会和被打断的请求抢锁
FILE_CLEANUPS: list[Callable[[], None]] = []   # 删临时目录：任何时候都安全
_cleaned = False


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def kill_proc(p: subprocess.Popen, grace: float = 8.0) -> None:
    if p.poll() is not None:
        return
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.killpg(p.pid, signal.SIGTERM)
    try:
        p.wait(grace)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.killpg(p.pid, signal.SIGKILL)
        with contextlib.suppress(Exception):
            p.wait(3)


def cleanup(from_signal: bool = False) -> None:
    global _cleaned
    if _cleaned:
        return
    _cleaned = True
    for p in list(PROCS):
        kill_proc(p, 2.0 if from_signal else 8.0)
    if not from_signal:
        for fn in CLEANUPS:
            with contextlib.suppress(Exception):
                fn()
    for fn in FILE_CLEANUPS:
        with contextlib.suppress(Exception):
            fn()


def _on_signal(signum, _frame) -> None:
    log(f"收到信号 {signum}，清理子进程后退出")
    try:
        cleanup(from_signal=True)
    finally:
        os._exit(130 if signum == signal.SIGINT else 143)


def install_cleanup() -> None:
    atexit.register(cleanup)
    for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(s, _on_signal)


def spawn(argv: list[str], env: dict[str, str], logfile: Path, cwd: Path = ROOT) -> subprocess.Popen:
    fh = open(logfile, "ab")
    p = subprocess.Popen(argv, env=env, cwd=cwd, stdout=fh, stderr=subprocess.STDOUT, start_new_session=True)
    PROCS.append(p)
    return p


def wait_http(url: str, timeout: float = 60.0, proc: Optional[subprocess.Popen] = None) -> bool:
    import urllib.request
    end = time.time() + timeout
    while time.time() < end:
        if proc is not None and proc.poll() is not None:
            return False
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.2)
    return False


# ============================================================ 假图片 ====
def png_bytes(w: int, h: int, mode: str = "gradient") -> bytes:
    """手写 PNG（不依赖 Pillow）：gradient 与 Rust gate-testkit 的 tiny_png 同款，solid 是纯色（大尺寸时体积很小）。"""
    rows = bytearray()
    for y in range(h):
        rows.append(0)
        if mode == "solid":
            rows += bytes((200, 120, 60)) * w
        else:
            for x in range(w):
                rows += bytes(((x * 4) & 255, (y * 4) & 255, 128))

    def chunk(tag: bytes, data: bytes) -> bytes:
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b""))


TINY_PNG = png_bytes(64, 64)
BIG_PNG = png_bytes(1024, 1024, "solid")      # ≥ 1,011,712 像素：导演工具不会先放大（放大会让两边 PNG 编码器的输出不同）
SMALL_PNG = png_bytes(96, 96, "solid")


def b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode()


def zip_of_pngs(n: int) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for i in range(n):
            z.writestr(zipfile.ZipInfo(f"image_{i}.png", (2020, 1, 1, 0, 0, 0)), TINY_PNG)
    return buf.getvalue()


def msgpack_frame(pairs: list[tuple[str, Any]]) -> bytes:
    import msgpack
    data = msgpack.packb(dict(pairs), use_bin_type=True)
    return struct.pack(">I", len(data)) + data


def sse_event(event: str, data: dict) -> bytes:
    return f"event: {event}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n".encode()


def image_stream(n: int, wire: str) -> list[tuple[float, bytes]]:
    chunks: list[tuple[float, bytes]] = []
    enc = b64(TINY_PNG)
    for i in range(n):
        for step in range(2):
            if wire == "msgpack":
                d = msgpack_frame([("event_type", "intermediate"), ("samp_ix", i), ("step_ix", step), ("image", TINY_PNG)])
            else:
                d = sse_event("intermediate", {"event_type": "intermediate", "samp_ix": i, "step_ix": step, "image": enc})
            chunks.append((0.005, d))
        if wire == "msgpack":
            d = msgpack_frame([("event_type", "final"), ("samp_ix", i), ("image", TINY_PNG)])
        else:
            d = sse_event("final", {"event_type": "final", "samp_ix": i, "image": enc})
        chunks.append((0.005, d))
    return chunks


# ============================================================ 假上游 ====
@dataclass
class Recorded:
    method: str
    path: str
    query: str
    headers: list[tuple[str, str]]       # 原样大小写、收到的顺序
    body: bytes


@dataclass
class Reply:
    """脚本化回复。chunks 不为空时按块回（块之间等一会儿），cut=True 表示最后一块之后直接断开连接。"""
    status: int = 200
    headers: dict[str, str] = field(default_factory=dict)
    body: bytes = b""
    chunks: Optional[list[tuple[float, bytes]]] = None
    content_type: Optional[str] = None
    cut: bool = False

    @staticmethod
    def json(status: int, obj: Any, **headers: str) -> "Reply":
        return Reply(status, {"content-type": "application/json", **headers}, json.dumps(obj).encode())


class MockUpstream:
    """假 NovelAI。默认行为与 crates/gate-testkit 的 MockUpstream 一致；push(path, Reply) 脚本化下一次回复。"""

    def __init__(self, name: str):
        self.name = name
        self.lock = threading.Lock()
        self.requests: list[Recorded] = []
        self.scripts: dict[str, list[Reply]] = {}
        self.v5_percent = 80
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a, **k):  # 静音
                pass

            def _body(self) -> bytes:
                if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
                    out = b""
                    while True:
                        size = int(self.rfile.readline().strip() or b"0", 16)
                        if size == 0:
                            self.rfile.readline()
                            return out
                        out += self.rfile.read(size)
                        self.rfile.readline()
                n = int(self.headers.get("Content-Length") or 0)
                return self.rfile.read(n) if n else b""

            def _serve(self):
                body = self._body()
                parsed = urllib.parse.urlsplit(self.path)
                rec = Recorded(self.command, parsed.path, parsed.query, list(self.headers.items()), body)
                with outer.lock:
                    outer.requests.append(rec)
                    q = outer.scripts.get(parsed.path)
                    scripted = q.pop(0) if q else None
                reply = scripted or outer.default_reply(rec)
                if scripted is None and rec.method == "POST" and rec.path.startswith("/ai/"):
                    time.sleep(0.04)       # 真上游不会 1 毫秒就回；耗时统计（/admin/api/perf）靠它有个像样的量级
                try:
                    self._send(reply)
                except (BrokenPipeError, ConnectionResetError):
                    self.close_connection = True

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = _serve

            def _send(self, r: Reply):
                self.send_response(r.status)
                hs = {k.lower(): v for k, v in r.headers.items()}
                if r.content_type:
                    hs["content-type"] = r.content_type
                for k, v in hs.items():
                    self.send_header(k, v)
                if r.chunks is not None:
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    for delay, data in r.chunks:
                        time.sleep(delay)
                        self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                        self.wfile.flush()
                    if r.cut:
                        self.close_connection = True
                        self.connection.shutdown(socket.SHUT_RDWR)
                        return
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                else:
                    self.send_header("Content-Length", str(len(r.body)))
                    self.end_headers()
                    self.wfile.write(r.body)

        class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
            daemon_threads = True
            request_queue_size = 64
            allow_reuse_address = True

            def handle_error(self, request, client_address):   # 网关主动断开连接（取消 / 超时）是正常情况，不打堆栈
                pass

        self.port = free_port()
        self.server = Server(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        CLEANUPS.append(self.server.shutdown)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def push(self, path: str, reply: Reply) -> None:
        with self.lock:
            self.scripts.setdefault(path, []).append(reply)

    def mark(self) -> int:
        with self.lock:
            return len(self.requests)

    def since(self, mark: int) -> list[Recorded]:
        with self.lock:
            return list(self.requests[mark:])

    def default_reply(self, rec: Recorded) -> Reply:
        try:
            jb = json.loads(rec.body) if rec.body else {}
        except ValueError:
            jb = {}
        jb = jb if isinstance(jb, dict) else {}
        params = jb.get("parameters") if isinstance(jb.get("parameters"), dict) else {}
        n = params.get("n_samples", 1)
        n = n if isinstance(n, int) and n >= 1 else 1
        m, p = rec.method, rec.path
        if (m, p) == ("POST", "/ai/generate-image"):
            return Reply(200, {"content-type": "binary/octet-stream"}, zip_of_pngs(n))
        if (m, p) == ("POST", "/ai/generate-image-stream"):
            wire = params.get("stream") if params.get("stream") in ("sse", "msgpack") else "sse"
            ct = "application/x-msgpack" if wire == "msgpack" else "text/event-stream"
            return Reply(200, {}, chunks=image_stream(n, wire), content_type=ct)
        if (m, p) == ("POST", "/ai/generate-stream"):
            mx = min(params.get("max_length", 10) if isinstance(params.get("max_length"), int) else 10, 20)
            chunks = [(0.001, f"data: {json.dumps({'token': f' word{i} '})}\n\n".encode()) for i in range(mx)]
            chunks.append((0.0, b"data: [DONE]\n\n"))
            return Reply(200, {}, chunks=chunks, content_type="text/event-stream")
        if (m, p) == ("POST", "/ai/generate"):
            return Reply.json(200, {"output": " Once upon a time " * 3})
        if (m, p) == ("GET", "/user/subscription"):
            pct = self.v5_percent
            return Reply.json(200, {
                "tier": 3, "active": True, "expiresAt": 1893456000,
                "perks": {"maxPriorityActions": 1000, "startPriority": 10, "unlimitedMaxPriority": True},
                "trainingStepsLeft": {"fixedTrainingStepsLeft": 10000, "purchasedTrainingSteps": 0},
                "usage": {"percent": pct, "isNegative": pct <= 0, "timeUntilNextPercent": 3600}})
        if (m, p) == ("GET", "/ai/generate-image/suggest-tags"):
            return Reply.json(200, {"tags": [{"tag": "1girl", "count": 1000, "confidence": 0.9}]})
        if m == "POST" and p in ("/ai/upscale", "/ai/augment-image"):
            return Reply(200, {"content-type": "binary/octet-stream"}, zip_of_pngs(1))
        if (m, p) == ("POST", "/ai/encode-vibe"):
            return Reply(200, {"content-type": "application/octet-stream"}, bytes([7]) * 128)
        if (m, p) == ("POST", "/ai/generate-voice"):
            return Reply(200, {"content-type": "audio/mpeg"}, b"ID3-parity-voice")
        return Reply.json(404, {"statusCode": 404, "message": "Not Found"})


# ============================================================ Discord 黑洞 ====
class DiscordSink:
    """只记录、一律拒绝。旧网关把 discord.com 写死在代码里，只能靠 HTTPS_PROXY 拦（会看到 CONNECT）；
    Rust 网关用 DISCORD_API_BASE 指过来（会看到完整路径）。两边都不可能真的连到 Discord。"""

    def __init__(self, name: str):
        self.name = name
        self.lock = threading.Lock()
        self.attempts: list[str] = []
        outer = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                try:
                    line = self.rfile.readline(65536).decode("latin-1").strip()
                    while True:  # 读掉请求头
                        h = self.rfile.readline(65536)
                        if h in (b"\r\n", b"\n", b""):
                            break
                    with outer.lock:
                        outer.attempts.append(line)
                    self.wfile.write(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                except Exception:
                    pass

        class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
            daemon_threads = True
            allow_reuse_address = True

        self.port = free_port()
        self.server = Server(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        CLEANUPS.append(self.server.shutdown)

    def mark(self) -> int:
        with self.lock:
            return len(self.attempts)

    def since(self, mark: int) -> list[str]:
        with self.lock:
            return list(self.attempts[mark:])


# ============================================================ 服务日与种子 ====
def service_day(reset_hour: int, ts: Optional[float] = None) -> str:
    ts = (ts or time.time()) - reset_hour * 3600
    return dt.datetime.fromtimestamp(ts, ZoneInfo(TZ_NAME)).strftime("%Y-%m-%d")


def pick_reset_hour() -> int:
    """服务日在“现在 + 12 小时”重置：跑一整轮不会跨日，额度计数不依赖具体是几点跑的。"""
    return (dt.datetime.now(ZoneInfo(TZ_NAME)).hour + 12) % 24


def tok(name: str) -> str:
    return f"nai-parity-{name}-" + "0" * (34 - len(name))


# 别名 -> (id, 名称, 字段)。字段缺省见 build_seed。
SEED_KEYS: dict[str, dict] = {
    "member":    dict(name="成员甲", daily_images=100, daily_v5=0, rpm=600),
    "v5":        dict(name="成员乙", daily_images=100, daily_v5=20, rpm=600, image_model_scope="all"),
    "anlas":     dict(name="成员丙", daily_images=100, daily_v5=10, rpm=600, image_model_scope="all", allow_anlas=1,
                      allow_img2img=1, daily_anlas=100.0, monthly_anlas=1000.0),
    "admin":     dict(name="站长号", daily_images=100, daily_v5=10, rpm=600, image_model_scope="all", is_admin=1),
    "testkey":   dict(name="测试号", daily_images=100, daily_v5=10, rpm=600, image_model_scope="all", is_test=1),
    "disabled":  dict(name="停用号", enabled=0, rpm=600),
    "expired":   dict(name="过期号", rpm=600, expires_offset=-86400.0),
    "exhausted": dict(name="额度用尽", daily_images=5, daily_v5=3, rpm=600, image_model_scope="all",
                      daily_text_tokens=100),
    "rpm1":      dict(name="限速号", rpm=1),
    "limited":   dict(name="只开文生图", rpm=600, features="image"),
    "paced":     dict(name="注水用尽", daily_images=100, rpm=600),
    "paused":    dict(name="暂停号", rpm=600),
    "textlimit": dict(name="文本将尽", rpm=600, daily_text_tokens=5),
    "rename":    dict(name="待改名", rpm=600, daily_images=100),
    "dmtarget":  dict(name="私信对象", rpm=600, daily_images=100),
}
SEED_KEY_IDS = {alias: i + 1 for i, alias in enumerate(SEED_KEYS)}
DISCORD_OF = {alias: f"{SYNTH_DISCORD}{i + 1:02d}" for i, alias in enumerate(SEED_KEYS)}
SEED_TOKENS = {alias: tok(alias) for alias in SEED_KEYS}
KNOWN_TOKENS = set(SEED_TOKENS.values())
NO_KEY_DISCORD = f"{SYNTH_DISCORD}98"        # 在服务器里但没有 Key 的成员（会话 tag 为 "-"）


MODULES = ("capacity", "allocation", "autopilot", "registration", "registration_ready", "observation", "risk")


async def build_seed(data_dir: Path, reset_hour: int, background: bool = False) -> dict:
    """background=False（语料用）：关掉旧版启动时的动态额度算法与运行层模块——它们会在启动后几秒内改写额度 / 全站 V5 /
    公告文案，而 Rust 的对应后台任务还没移植，开着的话每个读额度的接口都会因此不一样。
    background=True（“后台任务基线”阶段用）：保持开启，专门看旧版启动期后台任务写了什么库。"""
    from app import database, status_page
    data_dir.mkdir(parents=True, exist_ok=True)
    (data_dir / "secret_key").write_text(SECRET)
    db = database.Database(str(data_dir / "nai_gate.db"), TZ_NAME)
    await db.connect()
    await status_page.ensure_schema(db)
    now = time.time()
    day = service_day(reset_hour, now)
    yday = service_day(reset_hour, now - 86400)
    for alias, spec in SEED_KEYS.items():
        exp = spec.get("expires_offset")
        row = await db.create_key({
            "name": spec["name"], "token": SEED_TOKENS[alias], "enabled": bool(spec.get("enabled", 1)),
            "daily_images": spec.get("daily_images", 100), "daily_anlas": spec.get("daily_anlas", 0.0),
            "daily_v5": spec.get("daily_v5", 0), "monthly_anlas": spec.get("monthly_anlas", 0.0),
            "daily_text_tokens": spec.get("daily_text_tokens", 100000), "rpm": spec.get("rpm", 10),
            "allow_anlas": bool(spec.get("allow_anlas")), "allow_img2img": bool(spec.get("allow_img2img")),
            "image_model_scope": spec.get("image_model_scope", "legacy"), "is_admin": bool(spec.get("is_admin")),
            "is_test": bool(spec.get("is_test")), "features": spec.get("features"),
            "expires_at": (now + exp) if exp is not None else now + 20 * 86400,
            "discord_id": DISCORD_OF[alias]})
        kid = row["id"]
        assert kid == SEED_KEY_IDS[alias], (alias, kid)
        # 手动额度（-1）：后台的动态额度算法 / Anlas 自动分配不会去改它们，两边不会因后台任务时序不同而分叉
        await db._db.execute(
            "UPDATE api_keys SET quota_auto=-1, anlas_auto=-1, last_used_at=?, last_image_at=? WHERE id=?",
            (now - 600 * kid, now - 900 * kid, kid))
        await db._db.execute(
            "INSERT INTO discord_registrations (discord_id, key_id, created_at, username, display_name, avatar) "
            "VALUES (?,?,?,?,?,?)",
            (DISCORD_OF[alias], kid, now - 86400 * (kid + 2), f"user{kid:02d}", spec["name"], ""))
    await db._db.commit()

    # 额度计数：用尽 / 注水用尽 / 文本将尽，外加一些历史
    def counter(alias: str, d: str, **kw):
        c = dict(images=0, legacy_free_images=0, anlas=0.0, text_tokens=0, requests=0, v5=0)
        c.update(kw)
        return (SEED_KEY_IDS[alias], d, c["images"], c["legacy_free_images"], c["anlas"], c["text_tokens"],
                c["requests"], c["v5"])

    rows = [
        counter("exhausted", day, images=5, legacy_free_images=5, text_tokens=100, requests=9, v5=3),
        counter("paced", day, images=90, legacy_free_images=90, requests=95),
        counter("textlimit", day, text_tokens=3, requests=1),
        counter("member", day, images=2, legacy_free_images=2, requests=3),
        counter("member", yday, images=40, legacy_free_images=30, requests=44, anlas=3.0),
        counter("v5", yday, images=10, legacy_free_images=6, requests=12, v5=4),
        counter("anlas", yday, images=7, legacy_free_images=2, requests=8, anlas=12.0, v5=1),
    ]
    await db._db.executemany(
        "INSERT INTO counters (key_id, day, images, legacy_free_images, anlas, text_tokens, requests, v5) "
        "VALUES (?,?,?,?,?,?,?,?)", rows)
    # 暂停号：share_state 里有未到期的暂停
    await db._db.execute(
        "INSERT INTO share_state (key_id, score, score_ts, strikes, paused_until, warned_ts, paused_ts) "
        "VALUES (?,?,?,?,?,?,?)",
        (SEED_KEY_IDS["paused"], 8.0, now - 3600, 2, 1893456000.0, now - 7200, now - 3600))
    # 用量日志：3 天 × 多个成员 / 种类 / 状态
    names = {a: s["name"] for a, s in SEED_KEYS.items()}
    models = ["nai-diffusion-4-5-full", "nai-diffusion-4-5-curated", "nai-diffusion-5-full", "nai-diffusion-3"]
    plan = []
    for i in range(160):
        alias = ["member", "v5", "anlas", "testkey", "rename", "limited"][i % 6]
        kind = ["image", "image", "image_stream", "tags", "text", "upscale"][i % 6]
        status = ["ok", "ok", "ok", "rejected", "error", "ok", "ok", "ok"][i % 8]
        plan.append((alias, kind, status))
    for i, (alias, kind, status) in enumerate(plan):
        ts = now - 600 - i * 1500
        await db._db.execute(
            "INSERT INTO usage_log (ts, key_id, key_name, kind, model, status, images, anlas, tokens, detail, "
            "unconfirmed_anlas, reason, wait_ms, dur_ms, client, up_status, rid, src, ver) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (ts, SEED_KEY_IDS[alias], names[alias], kind, models[i % 4], status, 1 if status == "ok" else 0,
             0.0, 12 if kind == "text" else 0,
             "" if status == "ok" else ("429 请求过于频繁" if status == "rejected" else "upstream 500"),
             0.0, "key_rate" if status == "rejected" else "", 100 + i, 3000 + i, "parity-seed",
             200 if status == "ok" else (0 if status == "rejected" else 500), f"{i:08x}", "127.0.*.*", "2.26.0"))
    # 提示词 / 原图记录（原图写成文件，放在数据目录里）
    await db.add_audit(SEED_KEY_IDS["member"], names["member"], "image", "nai-diffusion-4-5-full", "ok",
                       "1girl, seed prompt", "lowres", None, extra="seed", image=TINY_PNG, image_type="image/png")
    await db.add_audit(SEED_KEY_IDS["v5"], names["v5"], "image", "nai-diffusion-5-full", "error",
                       "2girls, seed prompt", "", None)
    # 标签 / 问卷 / 捐赠 / 反馈 / 券 / 黑名单 / Bug / 操作日志 / 状态页事件
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (SEED_KEY_IDS["member"], "观察", "种子标签", "站长", now - 5000))
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (SEED_KEY_IDS["rename"], "邀请", "站长发的邀请 Key", "站长", now - 7000))
    await db.save_survey(SEED_KEY_IDS["v5"], client="种子客户端", understand="清楚", improve="更多额度、更快",
                         improve_other="", donate="愿意")
    d1 = await db.save_donation("pst-synthetic-donation-token-0001", "只用 4.5", "contact-1")
    await db.save_donation("pst-synthetic-donation-token-0002", "周末别用", "")
    await db.handle_donation(d1, "已验证")
    await db._db.execute("INSERT INTO feedback(ts, discord_id, username, key_id, answers) VALUES (?,?,?,?,?)",
                         (now - 4000, DISCORD_OF["member"], "user01", SEED_KEY_IDS["member"],
                          json.dumps({"q1": "很好", "q2": "再快点"}, ensure_ascii=False)))
    await db.add_coupon(SEED_KEY_IDS["member"], "reset", 7.0, "种子券", "站长")
    cid = (await db.add_coupon(SEED_KEY_IDS["v5"], "reset", 7.0, "已用券", "站长"))["id"]
    await db._db.execute("UPDATE coupons SET used_at=? WHERE id=?", (now - 100, cid))
    await db.ban_discord(f"{SYNTH_DISCORD}90", reason="种子封禁", username="banned1", display_name="被封者", banned_by="站长")
    await db.ban_discord(f"{SYNTH_DISCORD}91", reason="脚本刷号", username="banned2", banned_by="系统")
    await db._db.execute(
        "INSERT INTO error_events(sig, source, level, title, detail, count, first_seen, last_seen, last_rid, last_key, "
        "last_path, resolved_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        ("seedsig00001", "request", "error", "种子 Bug：KeyError", "Traceback (synthetic)", 3, now - 9000, now - 800,
         "deadbeef", SEED_KEY_IDS["member"], "/ai/generate-image", None))
    await db._db.execute(
        "INSERT INTO error_events(sig, source, level, title, detail, count, first_seen, last_seen, last_rid, last_key, "
        "last_path, resolved_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        ("seedsig00002", "upstream", "warn", "种子 Bug：502", "", 1, now - 90000, now - 80000, "", None, "", now - 70000))
    for i, (actor, action, target, detail) in enumerate([
            ("后台 127.0.*.*", "修改 Key", "Key #1 成员甲", "{'rpm': 600}"), ("系统", "闲置计时重置", "", "种子"),
            ("Discord:" + DISCORD_OF["member"], "网页登录", "", ""), ("站长", "添加黑名单", f"Discord:{SYNTH_DISCORD}90", "种子封禁")]):
        await db._db.execute("INSERT INTO admin_actions(ts, actor, action, target, detail, ok) VALUES (?,?,?,?,?,?)",
                             (now - 3000 - i * 500, actor, action, target, detail, 1))
    for h in range(6):
        await db._db.execute("INSERT INTO status_hourly(hour, code, n) VALUES (?,?,?)",
                             (int(now // 3600) * 3600 - h * 3600, 200, 20 + h))
        await db._db.execute("INSERT INTO upstream_snapshots(ts, v5_percent, v5_rate, anlas, fresh) VALUES (?,?,?,?,?)",
                             (now - h * 3600, 80.0 - h, 5.0, 10000.0, 1))
    await db._db.commit()
    await status_page.create_incident(db, "种子事件：图片生成变慢", "minor", ["api"], "resolved",
                                      "我们已恢复正常，感谢耐心等待。", now - 40000)
    await db.set_setting("algo_notice", "种子：额度由算法每日微调")
    await db.set_setting("register_open", "1")
    if not background:
        await db.set_setting("quota_auto_enabled", "0")
        for m in MODULES:
            await db.set_setting("module_" + m, "0")
    await db.set_setting("guard_account_hourly_cap", "0")  # 0 = 不限：每小时 / 10 分钟突发上限会把后面的出图用例全挡成 429（上限用例见 A.guard.*）
    await db.set_setting("guard_interval_jitter", "0")  # 出图间隔随机抖动（默认 0~5 秒）会让每条出图请求白等好几秒
    await db.set_setting("dm_enabled", "1")            # 私信总闸门打开：任何要发的私信都会“尝试”发出（被黑洞拒绝）
    await db._db.commit()
    await db.close()
    (data_dir / "announcement.html").write_text(SEED_ANNOUNCEMENT, encoding="utf-8")
    return {"day": day, "now": now}


# ============================================================ 签发 Cookie ====
def sign(secret: str, payload: str) -> str:
    return hmac.new(secret.encode(), payload.encode(), hashlib.sha256).hexdigest()


def admin_cookie(exp: Optional[int] = None, password: str = ADMIN_PASSWORD, secret: str = SECRET) -> str:
    exp = exp if exp is not None else int(time.time()) + 7 * 86400
    key = secret + ":" + hashlib.sha256(password.encode()).hexdigest()
    return f"{exp}.{sign(key, str(exp))}"


def member_cookie(discord_id: str, token: Optional[str], exp: Optional[int] = None, secret: str = SECRET,
                  tag: Optional[str] = None) -> str:
    """成员会话。用旧 Python 的格式签发（Rust 的 sign_member 与它逐字节兼容，由此验证）。"""
    exp = exp if exp is not None else int(time.time()) + 30 * 86400
    if tag is None:
        tag = hashlib.sha256(token.encode()).hexdigest()[:16] if token else "-"
    payload = f"{discord_id}:{exp}:{tag}"
    return payload + "." + sign(secret + ":member-login", payload)


# ============================================================ 网关进程 ====
@dataclass
class Side:
    name: str                       # py / rs
    label: str                      # 报告里显示
    data_dir: Path
    port: int
    mock: MockUpstream
    sink: DiscordSink
    proc: Optional[subprocess.Popen] = None
    ctx: dict = field(default_factory=dict)

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.port}"


def base_env(side: Side, reset_hour: int, rust: bool) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "USER", "SHELL")}
    env.update({
        "DATA_DIR": str(side.data_dir), "ADMIN_PASSWORD": ADMIN_PASSWORD, "NAI_TOKENS": UPSTREAM_TOKEN,
        "NAI_IMAGE_HOST": side.mock.url, "NAI_TEXT_HOST": side.mock.url, "NAI_TEXT_HOST_LEGACY": side.mock.url,
        "KEY_IMAGE_MIN_INTERVAL": "0", "IMAGE_MIN_INTERVAL": "0",
        "POST_REQUEST_JITTER_MIN": "0", "POST_REQUEST_JITTER_MAX": "0", "QUEUE_TIMEOUT": "20",
        "IMAGE_429_COOLDOWN_SECONDS": "2",
        "ADMIN_COOKIE_SECURE": "0", "FORWARDED_ALLOW_IPS": "127.0.0.1", "TZ": TZ_NAME,
        "DAY_RESET_HOUR": str(reset_hour), "ALLOW_IMG2IMG": "1", "REGISTER_MAX_USERS": "50",
        # 假 Discord 配置（与 tools/dev/run-legacy.sh 一致）：让自助领 Key / 网页登录路由处于“已配置”状态
        "DISCORD_CLIENT_ID": "1", "DISCORD_CLIENT_SECRET": "dev", "DISCORD_BOT_TOKEN": "dev",
        "DISCORD_GUILD_ID": GUILD_ID, "SITE_URL": "http://parity-site.test", "REGISTRATION_BRIDGE_SECRET": BRIDGE_SECRET,
        "ADMIN_DISCORD_IDS": DISCORD_OF["admin"], "DISCORD_INVITE_URL": "https://discord.invalid/parity",
        "ADMIN_ALLOWED_ORIGINS": "http://localhost:5173",
        # 不让任何东西连到真实 Discord：HTTPS 一律走本地黑洞，本机地址直连
        "HTTPS_PROXY": f"http://127.0.0.1:{side.sink.port}", "HTTP_PROXY": f"http://127.0.0.1:{side.sink.port}",
        "ALL_PROXY": f"http://127.0.0.1:{side.sink.port}", "NO_PROXY": "127.0.0.1,localhost,::1",
        "https_proxy": f"http://127.0.0.1:{side.sink.port}", "http_proxy": f"http://127.0.0.1:{side.sink.port}",
        "all_proxy": f"http://127.0.0.1:{side.sink.port}", "no_proxy": "127.0.0.1,localhost,::1",
        "PYTHONUNBUFFERED": "1",
    })
    if rust:
        env.update({"DISCORD_API_BASE": f"http://127.0.0.1:{side.sink.port}/api", "RUST_LOG": "warn",
                    "BIND": f"127.0.0.1:{side.port}"})
        stub = side.data_dir.parent / "static-stub"
        if stub.is_dir():
            env["STATIC_DIR"] = str(stub)
    return env


def make_static_stub(dest: Path) -> bool:
    """web/dist 没构建时，Rust 的页面路由只会回 503「前端未构建」。用旧版静态页当替身（STATIC_DIR 环境变量，
    不改任何网关代码），这样页面路由的状态码 / content-type / 响应头仍然可以比较；页面正文本来就不比。"""
    if (ROOT / "web" / "dist" / "landing.html").exists():
        return False
    static = ROOT / "app" / "static"
    dest.mkdir(parents=True, exist_ok=True)
    for src, name in (("landing.html", "landing.html"), ("survey.html", "survey.html"), ("index.html", "admin.html"),
                      ("favicon.svg", "favicon.svg")):
        if (static / src).exists():
            shutil.copy2(static / src, dest / name)
    return True


def start_gateway(side: Side, reset_hour: int, rust_bin: Path) -> None:
    rust = side.name == RS
    env = base_env(side, reset_hour, rust)
    logf = side.data_dir.parent / f"{side.name}.log"
    if rust:
        argv = [str(rust_bin), "serve", "--bind", f"127.0.0.1:{side.port}"]
    else:
        argv = [sys.executable, "-m", "uvicorn", "app.main:app", "--host", "127.0.0.1", "--port", str(side.port),
                "--log-level", "warning"]
    side.proc = spawn(argv, env, logf)
    if not wait_http(side.base + "/healthz", 90, side.proc):
        tail = logf.read_text(errors="replace")[-2000:] if logf.exists() else ""
        raise RuntimeError(f"{side.label} 网关没有起来：\n{tail}")


def stop_gateway(side: Side) -> None:
    if side.proc is not None:
        kill_proc(side.proc, 15.0)
        if side.proc in PROCS:
            PROCS.remove(side.proc)
        side.proc = None


def cargo_build(skip: bool) -> Path:
    exe = ROOT / "target" / "release" / "nai-gate"
    if not skip:
        log("cargo build --release -p gate-server --bin nai-gate")
        r = subprocess.run(["cargo", "build", "--release", "-p", "gate-server", "--bin", "nai-gate"], cwd=ROOT)
        if r.returncode != 0:
            raise RuntimeError("cargo build 失败")
    if not exe.exists():
        raise RuntimeError(f"找不到 {exe}")
    return exe


# ============================================================ 差异模型 ====
@dataclass
class Diff:
    kind: str            # status / header / body / upstream / discord / db / schema / startup
    where: str
    a: Any = None        # 旧 Python
    b: Any = None        # Rust
    tag: str = ""        # 附加标签：transport / key-order / ...
    dev: str = ""        # 命中的有意差异编号（D4 ...）；空 = 未解释

    def render(self, width: int = 110) -> str:
        def short(v: Any) -> str:
            s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False, default=str)
            s = s.replace("\n", "\\n")
            return s if len(s) <= width else s[:width] + "…"
        tag = f" [{self.tag}]" if self.tag else ""
        return f"{self.kind} {self.where}{tag}: py={short(self.a)}  rs={short(self.b)}"


@dataclass
class Resp:
    status: int = -1
    headers: list[tuple[str, str]] = field(default_factory=list)
    body: bytes = b""
    error: str = ""
    elapsed: float = 0.0

    def hdr(self, name: str) -> list[str]:
        return [v for k, v in self.headers if k == name]

    def json(self) -> Any:
        return json.loads(self.body)


# ---------------------------------------------------------------- JSON 比较 ----
TIME_KEY = re.compile(r"(^|_)(ts|time|at|until|since|expires|seen|started)$|_at$|At$|^t$")
IGNORE_VALUE_KEY = re.compile(r"^(wait_ms|dur_ms|rid|last_rid|request_id|x-request-id|ver|avg_dur_s)$|.+_ms$|^(member_)?capacity_per_hour$")
TIME_TOL = 90.0
KEYLIKE = re.compile(r"^nai-[A-Za-z0-9_\-]{16,}$")


def npath(path: str) -> str:
    return re.sub(r"\[\d+\]", "[]", path)


class Cmp:
    """JSON 逐字段比较。易变字段按“形状”比：时间戳允许 ±TIME_TOL 秒，请求编号 / 随机 Key 只比形状，
    不会整段丢弃；单条语料还可以追加 (路径正则, 规则)。规则：ignore（不比值）/ shape（只比类型）/ time（容差）。"""

    def __init__(self, rules: Optional[list[tuple[str, str]]] = None, norm: Optional[list[tuple[str, str]]] = None,
                 check_order: bool = True):
        self.rules = [(re.compile(p), k) for p, k in (rules or [])]
        self.norm = [(re.compile(p), r) for p, r in (norm or [])]
        self.check_order = check_order
        self.out: list[Diff] = []

    def _n(self, s: str) -> str:
        s = re.sub(r"错误编号 [0-9a-f]{6,16}", "错误编号 <rid>", s)
        s = re.sub(r"nai-[A-Za-z0-9_\-]{24,}", lambda m: m.group(0) if m.group(0) in KNOWN_TOKENS else "nai-<key>", s)
        for p, r in self.norm:
            s = p.sub(r, s)
        return s

    def _rule(self, path: str) -> Optional[str]:
        npth = npath(path)
        for pat, kind in self.rules:
            if pat.search(npth):
                return kind
        return None

    def add(self, where: str, a: Any, b: Any, tag: str = "") -> None:
        self.out.append(Diff("body", where, a, b, tag))

    def diff(self, a: Any, b: Any, path: str = "$", kind: str = "body") -> list[Diff]:
        self._d(a, b, path)
        for d in self.out:
            d.kind = kind
        return self.out

    def _d(self, a: Any, b: Any, path: str) -> None:
        last = re.sub(r"\[\d+\]$", "", path.rsplit(".", 1)[-1])
        rule = self._rule(path)
        if rule == "ignore":
            return
        if rule == "shape":
            if type(a) is not type(b):
                self.add(path, a, b, "shape")
            return
        if IGNORE_VALUE_KEY.match(last) and isinstance(a, (str, int)) and isinstance(b, (str, int)):
            return
        if isinstance(a, dict) and isinstance(b, dict):
            ka, kb = list(a), list(b)
            for k in ka:
                if k not in b:
                    self.add(f"{path}.{k}", a[k], "<缺失>", "missing-in-rs")
            for k in kb:
                if k not in a:
                    self.add(f"{path}.{k}", "<缺失>", b[k], "extra-in-rs")
            common_a = [k for k in ka if k in b]
            common_b = [k for k in kb if k in a]
            if self.check_order and common_a != common_b:
                self.add(path, common_a, common_b, "key-order")
            for k in common_a:
                self._d(a[k], b[k], f"{path}.{k}")
            return
        if isinstance(a, list) and isinstance(b, list):
            if len(a) != len(b):
                self.add(f"{path}.len", len(a), len(b))
            for i, (x, y) in enumerate(zip(a, b)):
                self._d(x, y, f"{path}[{i}]")
            return
        num = (int, float)
        if isinstance(a, num) and isinstance(b, num) and not isinstance(a, bool) and not isinstance(b, bool):
            if TIME_KEY.search(last) or rule == "time":
                if abs(a - b) <= TIME_TOL:
                    return
                self.add(path, a, b, "time")
                return
            if a == b and type(a) is not type(b):
                self.add(path, a, b, "number-format")
            elif a != b:
                self.add(path, a, b)
            return
        if isinstance(a, str) and isinstance(b, str):
            if a == b:
                return
            if KEYLIKE.match(a) and KEYLIKE.match(b) and a not in KNOWN_TOKENS and b not in KNOWN_TOKENS:
                if len(a) == len(b) and a[:4] == b[:4]:
                    return                    # 服务端随机生成的 Key：只比形状
            if self._n(a) == self._n(b):
                return
            self.add(path, a, b)
            return
        if a is None and b is None:
            return
        if a != b or type(a) is not type(b):
            self.add(path, a, b, "type" if type(a) is not type(b) else "")


# ---------------------------------------------------------------- 响应头 ----
IGNORED_HEADERS = {"date", "server", "content-length", "connection", "keep-alive", "transfer-encoding"}
COOKIE_SHAPES = {
    "nai_gate_admin": re.compile(r"^\d{9,}\.[0-9a-f]{64}$"),
    "owl_member": re.compile(r"^\d+:\d+:[0-9a-f-]+\.[0-9a-f]{64}$"),
    "owl_login_state": re.compile(r"^[A-Za-z0-9_\-]{20,}$"),
}


def parse_cookie(raw: str) -> tuple[str, str, dict[str, str], list[str]]:
    parts = [p.strip() for p in raw.split(";")]
    name, _, value = parts[0].partition("=")
    attrs: dict[str, str] = {}
    order: list[str] = []
    for p in parts[1:]:
        if not p:
            continue
        k, _, v = p.partition("=")
        attrs[k.strip().lower()] = v.strip()
        order.append(k.strip().lower())
    return name.strip(), value.strip(), attrs, order


def norm_header(name: str, value: str, norm: list[tuple[re.Pattern, str]]) -> str:
    v = value
    if name == "content-type":
        v = v.lower().replace("; ", ";").replace(" ;", ";")
    if name == "x-request-id":
        v = re.sub(r"[0-9a-f]", "h", v)
    if name in ("content-disposition", "retry-after"):
        v = re.sub(r"\d{8,}-?\d*", "<n>", v)
    for p, r in norm:
        v = p.sub(r, v)
    return v


def compare_headers(ra: Resp, rb: Resp, norm: list[tuple[re.Pattern, str]], skip: set[str]) -> list[Diff]:
    out: list[Diff] = []
    names = ({k for k, _ in ra.headers} | {k for k, _ in rb.headers}) - IGNORED_HEADERS - skip
    for n in sorted(names):
        va, vb = ra.hdr(n), rb.hdr(n)
        if n == "set-cookie":
            pa = [parse_cookie(x) for x in va]
            pb = [parse_cookie(x) for x in vb]
            if [c[0] for c in pa] != [c[0] for c in pb]:
                out.append(Diff("header", "set-cookie names", [c[0] for c in pa], [c[0] for c in pb]))
                continue
            for ca, cb in zip(pa, pb):
                shape = COOKIE_SHAPES.get(ca[0])
                if shape is not None:
                    if bool(shape.match(ca[1])) != bool(shape.match(cb[1])):
                        out.append(Diff("header", f"set-cookie {ca[0]} value shape", ca[1][:40], cb[1][:40]))
                elif ca[1] != cb[1]:
                    out.append(Diff("header", f"set-cookie {ca[0]} value", ca[1], cb[1]))
                aa, ab = dict(ca[2]), dict(cb[2])
                if aa.get("max-age") == "0" and ab.get("max-age") == "0":
                    aa.pop("expires", None)       # 删除 Cookie：Max-Age=0 已经决定了结果，expires 写什么都一样
                    ab.pop("expires", None)
                if aa != ab:
                    out.append(Diff("header", f"set-cookie {ca[0]} attributes", ca[2], cb[2]))
                elif ca[3] != cb[3]:
                    out.append(Diff("header", f"set-cookie {ca[0]} attribute order", ca[3], cb[3], "order"))
            continue
        na = [norm_header(n, x, norm) for x in va]
        nb = [norm_header(n, x, norm) for x in vb]
        if na != nb:
            out.append(Diff("header", n, va if len(va) != 1 else va[0], vb if len(vb) != 1 else vb[0]))
    return out


# ---------------------------------------------------------------- 正文 ----
def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()[:16]


def sse_events(raw: bytes) -> list[tuple[str, str]]:
    out = []
    for blk in [e for e in raw.decode("utf-8", "replace").replace("\r\n", "\n").split("\n\n") if e.strip()]:
        ev, data = "", []
        for line in blk.split("\n"):
            if line.startswith("event:"):
                ev = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:][1:] if line[5:6] == " " else line[5:])
        out.append((ev, "\n".join(data)))
    return out


CSV_VOLATILE = {"time", "wait_ms", "dur_ms", "rid", "created", "last_used"}


def csv_stable(raw: bytes) -> bytes:
    """CSV 里去掉时间 / 耗时 / 请求编号这几列（按表头名认），其余照旧——这些列两边本来就不可能逐字一样。"""
    import csv
    text = raw.decode("utf-8-sig", "replace")
    rows = list(csv.reader(io.StringIO(text)))
    if not rows:
        return raw
    keep = [i for i, h in enumerate(rows[0]) if h not in CSV_VOLATILE]
    out = io.StringIO()
    csv.writer(out).writerows([[r[i] for i in keep if i < len(r)] for r in rows])
    return out.getvalue().encode()


def compare_bodies(step: "Step", ra: Resp, rb: Resp, kind: str) -> list[Diff]:
    cmp = Cmp(step.rules, step.norm)
    if kind == "skip":
        return []
    if kind == "imgdims":
        try:
            from PIL import Image
            ia, ib = Image.open(io.BytesIO(ra.body)), Image.open(io.BytesIO(rb.body))
            if (ia.format, ia.size) != (ib.format, ib.size):
                return [Diff("body", "图片格式 / 尺寸", [ia.format, ia.size], [ib.format, ib.size])]
            return []
        except Exception as exc:  # noqa: BLE001
            return [Diff("body", "图片无法解码", type(exc).__name__, len(rb.body))]
    if kind == "json":
        try:
            ja, jb = ra.json(), rb.json()
        except ValueError:
            return compare_bodies(step, ra, rb, "text")
        out = cmp.diff(ja, jb)
        if not out:
            # 内容一致时再看写法（空白 / 转义 / 浮点）。请求编号这类易变值会让字节不同，先把它们抹平再比
            def vol(b: bytes) -> bytes:
                b = re.sub(rb'("(?:request_id|rid|ver)"\s*:\s*")[^"]*"', rb'\1R"', b)
                b = re.sub(rb"\xe9\x94\x99\xe8\xaf\xaf\xe7\xbc\x96\xe5\x8f\xb7 [0-9a-f]{6,16}", b"ERR-NO", b)   # “错误编号 <rid>”
                b = re.sub(rb'("[A-Za-z0-9_]*(?:_ms|capacity_per_hour)"\s*:\s*)[-0-9.]+', rb"\1N", b)                    # 耗时类字段
                b = re.sub(rb"nai-[A-Za-z0-9_\-]{24,}", b"nai-K", b)                                                    # 服务端新发的 Key
                return re.sub(rb"(?<![\d.])\d{10}(?:\.\d+)?(?![\d])", b"T", b)                                   # 秒级时间戳
            if vol(ra.body) != vol(rb.body):
                canon = lambda v: json.dumps(v, ensure_ascii=False, separators=(",", ":")).encode()
                py_ok, rs_ok = vol(canon(ja)) == vol(ra.body.strip()), vol(canon(jb)) == vol(rb.body.strip())
                i = next((i for i, (x, y) in enumerate(zip(ra.body, rb.body)) if x != y), min(len(ra.body), len(rb.body)))
                who = "Rust 的写法与 Starlette 不同" if py_ok and not rs_ok else (
                    "旧版的写法与 Starlette 不同" if rs_ok and not py_ok else "两边写法不同")
                out.append(Diff("body", f"JSON 字节不同（{who}）@byte {i}", ra.body[max(0, i - 25):i + 35].decode("utf-8", "replace"),
                                rb.body[max(0, i - 25):i + 35].decode("utf-8", "replace"), "serialization"))
        return out
    if kind == "sse":
        ea, eb = sse_events(ra.body), sse_events(rb.body)
        out: list[Diff] = []
        if len(ea) != len(eb):
            out.append(Diff("body", "sse event count", len(ea), len(eb)))
        for i, ((na, da), (nb, db_)) in enumerate(zip(ea, eb)):
            if na != nb:
                out.append(Diff("body", f"sse[{i}].event", na, nb))
                continue
            try:
                out += Cmp(step.rules, step.norm).diff(json.loads(da), json.loads(db_), f"sse[{i}]")
            except ValueError:
                if da != db_:
                    out.append(Diff("body", f"sse[{i}].data", da, db_))
        return out[:20]
    if kind == "text":
        csvlike = "csv" in (ra.hdr("content-type") or [""])[0].lower()
        ta = (csv_stable(ra.body) if csvlike else ra.body).decode("utf-8", "replace")
        tb = (csv_stable(rb.body) if csvlike else rb.body).decode("utf-8", "replace")
        for p, r in cmp.norm:
            ta, tb = p.sub(r, ta), p.sub(r, tb)
        if ta == tb:
            return []
        i = next((i for i, (x, y) in enumerate(zip(ta, tb)) if x != y), min(len(ta), len(tb)))
        return [Diff("body", f"text@{i} (len {len(ta)} vs {len(tb)})", ta[max(0, i - 30):i + 50], tb[max(0, i - 30):i + 50])]
    # bytes
    if ra.body == rb.body:
        return []
    try:
        za, zb = zipfile.ZipFile(io.BytesIO(ra.body)), zipfile.ZipFile(io.BytesIO(rb.body))
    except Exception:  # noqa: BLE001
        return [Diff("body", f"bytes len {len(ra.body)} vs {len(rb.body)}", sha(ra.body), sha(rb.body))]
    ma = {i.filename: (csv_stable(za.read(i)) if i.filename.endswith(".csv") else za.read(i)) for i in za.infolist()}
    mb = {i.filename: (csv_stable(zb.read(i)) if i.filename.endswith(".csv") else zb.read(i)) for i in zb.infolist()}
    out: list[Diff] = []
    if list(ma) != list(mb):
        out.append(Diff("body", "zip 成员名 / 顺序", list(ma), list(mb)))
    for name in ma:
        if name in mb and ma[name] != mb[name]:
            la = ma[name].decode("utf-8", "replace").splitlines()
            lb = mb[name].decode("utf-8", "replace").splitlines()
            i = next((i for i, (x, y) in enumerate(zip(la, lb)) if x != y), min(len(la), len(lb)))
            xa, xb = (la[i] if i < len(la) else "<没有>"), (lb[i] if i < len(lb) else "<没有>")
            ca, cb = xa.split(","), xb.split(",")
            if len(ca) == len(cb):
                cols = [f"第{k + 1}列 {x!r}→{y!r}" for k, (x, y) in enumerate(zip(ca, cb)) if x != y]
                out.append(Diff("body", f"zip 成员 {name} 第 {i + 1} 行（{len(la)} 行 vs {len(lb)} 行）逐列", cols, ""))
            else:
                out.append(Diff("body", f"zip 成员 {name} 第 {i + 1} 行起不同（{len(la)} 行 vs {len(lb)} 行）", xa, xb))
    return out


def body_kind(step: "Step", ra: Resp, rb: Resp) -> str:
    if step.body_mode != "auto":
        return step.body_mode
    ct = (ra.hdr("content-type") or rb.hdr("content-type") or [""])[0].lower()
    if "json" in ct:
        return "json"
    if "event-stream" in ct:
        return "sse"
    if "html" in ct:
        return "skip" if step.html_skip else "text"
    if ct.startswith("text/") or "xml" in ct or "csv" in ct:
        return "text"
    if not ra.body and not rb.body:
        return "skip"
    return "bytes"


# ---------------------------------------------------------------- 上游请求 ----
STACK_HEADERS = {"host", "accept-encoding", "connection", "content-length", "transfer-encoding"}


def compare_upstream(step: "Step", ua: list[Recorded], ub: list[Recorded]) -> list[Diff]:
    out: list[Diff] = []

    def lbl(rs: list[Recorded]) -> list[str]:
        return [f"{r.method} {r.path}" + (f"?{r.query}" if r.query else "") for r in rs]

    if len(ua) != len(ub) or [(r.method, r.path) for r in ua] != [(r.method, r.path) for r in ub]:
        out.append(Diff("upstream", "requests", lbl(ua), lbl(ub)))
        return out
    for i, (x, y) in enumerate(zip(ua, ub)):
        w = f"[{i}] {x.method} {x.path}"
        if x.query != y.query:
            qa = urllib.parse.parse_qsl(x.query, keep_blank_values=True)
            qb = urllib.parse.parse_qsl(y.query, keep_blank_values=True)
            out.append(Diff("upstream", f"{w} query", x.query, y.query, "same-params" if qa == qb else ""))
        na, nb = [k.lower() for k, _ in x.headers], [k.lower() for k, _ in y.headers]
        if na != nb:
            core_a, core_b = [n for n in na if n not in STACK_HEADERS], [n for n in nb if n not in STACK_HEADERS]
            out.append(Diff("upstream", f"{w} header names/order", na, nb, "transport" if core_a == core_b else ""))
        da = {k.lower(): v for k, v in x.headers}
        db_ = {k.lower(): v for k, v in y.headers}
        for k in sorted(set(da) & set(db_)):
            if k == "host" or (k == "content-length" and step.up_rules):
                continue
            if da[k] != db_[k]:
                out.append(Diff("upstream", f"{w} header {k}", da[k], db_[k]))
        if x.body != y.body:
            if "json" in da.get("content-type", ""):
                try:
                    dd = Cmp(step.up_rules, step.norm).diff(json.loads(x.body), json.loads(y.body), f"{w} body")
                    if not dd and not step.up_rules:
                        dd = [Diff("upstream", f"{w} body bytes", x.body[:120].decode("utf-8", "replace"),
                                   y.body[:120].decode("utf-8", "replace"), "serialization")]
                    for d in dd:
                        d.kind = "upstream"
                    out += dd
                except ValueError:
                    out.append(Diff("upstream", f"{w} body", sha(x.body), sha(y.body)))
            else:
                out.append(Diff("upstream", f"{w} body", sha(x.body), sha(y.body)))
    return out


# ============================================================ 语料模型 ====
@dataclass
class Step:
    id: str
    method: str = "GET"
    path: str = "/"
    ep: str = ""                                  # 报告里的接口名；缺省 = 方法 + 去掉查询串的路径
    auth: Optional[str] = None                    # 种子 Key 别名 -> Authorization: Bearer
    bearer: Optional[str] = None                  # 直接给 Bearer 值
    headers: dict = field(default_factory=dict)
    cookies: dict = field(default_factory=dict)
    json: Any = None
    body: Any = None                              # bytes / str 原样发送
    ctype: Optional[str] = None
    capture: dict = field(default_factory=dict)   # 名字 -> "json:a.b" / "cookie:NAME" / "header:NAME"
    script: list = field(default_factory=list)    # [(上游路径, Reply)]：给两个假上游各排一条脚本回复
    body_mode: str = "auto"
    html_skip: bool = True                        # HTML 页面不比正文（前端整个重写了）
    rules: list = field(default_factory=list)     # 响应 JSON 的 (路径正则, 规则)
    up_rules: list = field(default_factory=list)  # 上游请求体 JSON 的规则
    norm: list = field(default_factory=list)      # 两边文本先做的 (正则, 替换)
    skip_headers: set = field(default_factory=set)
    settle: float = SETTLE
    note: str = ""
    expect_dev: str = ""                          # 这条语料本来就会撞上的有意差异编号
    fn: Optional[Callable] = None                 # 非 HTTP 的动作步骤（改假上游状态、等待……）
    timeout: float = 40.0

    def endpoint(self) -> str:
        return self.ep or f"{self.method} {self.path.split('?', 1)[0]}"


@dataclass
class StepResult:
    step: Step
    corpus: str
    ra: Resp
    rb: Resp
    diffs: list[Diff]
    cat: str = "pass"        # pass / fail / expected / notported
    req: str = ""
    ups: Optional[tuple] = None      # (旧版一侧上游收到的请求, Rust 一侧上游收到的请求)


_TPL = re.compile(r"\$\{([A-Za-z0-9_.\-]+)\}")


class World:
    """两个侧 + 模板解析。"""

    def __init__(self, sides: dict[str, Side], reset_hour: int, day: str):
        self.sides = sides
        self.reset_hour = reset_hour
        self.day = day
        self.rust_bin: Optional[Path] = None
        self.notes: list[str] = []
        self.static_stub = False
        self.ledgers: dict[str, "Ledger"] = {}
        self.reset_clients: Callable[[], None] = lambda: None

    def restart(self, edit: Optional[Callable[[sqlite3.Connection], None]] = None,
                files: Optional[dict[str, bytes]] = None, hook: Optional[Callable[["World"], None]] = None) -> None:
        """停两个网关 -> （可选）直接改两份库 / 写文件 / 自定义动作 -> 再起。用来换掉内存状态，或改开关而不依赖任何一方的缓存。"""
        for s in self.sides.values():
            stop_gateway(s)
        if hook is not None:
            hook(self)
        for s in self.sides.values():
            for name, data in (files or {}).items():
                (s.data_dir / name).write_bytes(data)
        if edit is not None:
            for s in self.sides.values():
                con = sqlite3.connect(str(s.data_dir / "nai_gate.db"), timeout=30)
                edit(con)
                con.commit()
                con.close()
        for s in self.sides.values():
            start_gateway(s, self.reset_hour, self.rust_bin)
        self.reset_clients()                 # 旧连接池里的长连接已经被对面关掉了
        time.sleep(0.5)

    def lookup(self, name: str, side: Side) -> Any:
        other = self.sides[RS if side.name == PY else PY]
        if name.startswith("tok."):
            return SEED_TOKENS[name[4:]]
        if name.startswith("kid."):
            return SEED_KEY_IDS[name[4:]]
        if name.startswith("did."):
            return DISCORD_OF[name[4:]]
        if name in ("base", "origin"):
            return side.base
        if name == "bridge":
            return BRIDGE_SECRET
        if name == "day":
            return self.day
        for pref, s in (("self.", side), ("other.", other), ("py.", self.sides[PY]), ("rs.", self.sides[RS])):
            if name.startswith(pref):
                return s.ctx[name[len(pref):]]
        return side.ctx[name]

    def subst(self, obj: Any, side: Side) -> Any:
        if isinstance(obj, str):
            m = _TPL.fullmatch(obj)
            if m:
                return self.lookup(m.group(1), side)
            return _TPL.sub(lambda mm: str(self.lookup(mm.group(1), side)), obj)
        if isinstance(obj, list):
            return [self.subst(x, side) for x in obj]
        if isinstance(obj, dict):
            return {self.subst(k, side): self.subst(v, side) for k, v in obj.items()}
        return obj


def dig(obj: Any, path: str) -> Any:
    for part in path.split("."):
        obj = obj[int(part)] if isinstance(obj, list) else obj[part]
    return obj


def send(world: World, side: Side, step: Step, client) -> tuple[Resp, str]:
    path = world.subst(step.path, side)
    headers: dict[str, str] = {"user-agent": "parity-harness/1", "accept-encoding": "identity"}
    headers.update({k.lower(): str(v) for k, v in world.subst(step.headers, side).items()})
    if step.auth:
        headers["authorization"] = "Bearer " + SEED_TOKENS[step.auth]
    if step.bearer is not None:
        headers["authorization"] = "Bearer " + str(world.subst(step.bearer, side))
    cookies = world.subst(step.cookies, side)
    if cookies:
        headers["cookie"] = "; ".join(f"{k}={v}" for k, v in cookies.items())
    content: Optional[bytes] = None
    if step.json is not None:
        content = json.dumps(world.subst(step.json, side), ensure_ascii=False, separators=(",", ":")).encode()
        headers.setdefault("content-type", "application/json")
    elif step.body is not None:
        b = world.subst(step.body, side)
        content = b if isinstance(b, bytes) else str(b).encode()
    if step.ctype:
        headers["content-type"] = step.ctype
    desc = f"{step.method} {path}" + (f" (auth={step.auth})" if step.auth else "")
    t0 = time.time()
    try:
        r = client.request(step.method, side.base + path, headers=headers, content=content, timeout=step.timeout)
        resp = Resp(r.status_code, [(k.lower(), v) for k, v in r.headers.multi_items()], r.content, "", time.time() - t0)
    except Exception as exc:  # noqa: BLE001
        resp = Resp(-1, [], b"", f"{type(exc).__name__}: {exc}", time.time() - t0)
    return resp, desc


def do_captures(step: Step, side: Side, resp: Resp) -> None:
    for name, spec in step.capture.items():
        try:
            kind, _, arg = spec.partition(":")
            if kind == "json":
                side.ctx[name] = dig(resp.json(), arg)
            elif kind == "cookie":
                for raw in resp.hdr("set-cookie"):
                    n, v, _, _ = parse_cookie(raw)
                    if n == arg:
                        side.ctx[name] = v
            elif kind == "header":
                side.ctx[name] = (resp.hdr(arg.lower()) or [""])[0]
            elif kind == "text":
                side.ctx[name] = re.search(arg, resp.body.decode("utf-8", "replace")).group(1)
        except Exception:  # noqa: BLE001
            pass
        side.ctx.setdefault(name, "<capture-failed>")


def is_router_miss(r: Resp, path: str, other: Optional[Resp] = None) -> bool:
    """Rust 没有这条路由：axum 的默认 404（空正文）、/admin 兜底的 {"detail":"Not Found"}，
    或 /admin/* 的前端兜底 GET 路由对其它方法回的空正文 405（旧版此时不是 405）。"""
    if r.status == 404 and r.body in (b"", b'{"detail":"Not Found"}'):
        return True
    return (r.status == 405 and r.body in (b"", b'{"detail":"Method Not Allowed"}') and path.startswith("/admin/")
            and any("GET" in v for v in r.hdr("allow")) and other is not None and other.status != 405)


# ============================================================ 有意差异白名单 ====
def load_deviation_doc() -> dict[int, dict[str, str]]:
    """docs/v3-deviations.md 的表格：编号 -> 位置 / 2.x 行为 / 3.0 行为 / 原因。报告里的“预期差异”一节照它列。"""
    out: dict[int, dict[str, str]] = {}
    f = ROOT / "docs" / "v3-deviations.md"
    if not f.exists():
        return out
    for line in f.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^\|\s*(\d+)\s*\|(.*)\|\s*$", line)
        if not m:
            continue
        cells = [c.strip() for c in re.split(r"(?<!\\)\|", m.group(2))]
        if len(cells) >= 4:
            out[int(m.group(1))] = {"where": cells[0], "old": cells[1], "new": cells[2], "why": cells[3]}
    return out


@dataclass
class Deviation:
    n: int                                           # docs/v3-deviations.md 里的编号
    match: Callable[[str, str, Diff], bool] = lambda sid, ep, d: False       # (用例 id, 接口名, diff) -> 是否命中
    db: Callable[[Diff], bool] = lambda d: False                             # 数据库 diff 是否命中
    np: bool = False                                 # True = 新版有意不移植这条路由（Rust 404 是预期）

    @property
    def id(self) -> str:
        return f"D{self.n}"


PAGE_EPS = ("GET /", "GET /survey", "GET /admin", "GET /admin/", "GET / (成员会话)")
PAGE_HEADERS = ("content-security-policy", "cache-control", "vary", "x-content-type-options", "referrer-policy",
                "x-frame-options", "etag", "last-modified", "accept-ranges")


def _bug_path(d: Diff, *paths: str) -> bool:
    return d.where.startswith("error_events") and "只在旧版有" in d.where and any(f'"last_path": "{p}' in json.dumps(d.a, ensure_ascii=False) for p in paths)


DEVIATIONS: list[Deviation] = [
    Deviation(1, lambda sid, ep, d: sid.startswith("A.d1."),
              lambda d: _bug_path(d, "/ai/generate-image") or (d.where.startswith("usage_log") and "OverflowError" in json.dumps(d.a, ensure_ascii=False))),
    Deviation(2, lambda sid, ep, d: "my-export" in ep and ((d.kind == "status" and d.a == 500)
                                                          or (d.kind == "body" and "服务器内部错误" in json.dumps(d.a, ensure_ascii=False)))),
    Deviation(4, lambda sid, ep, d: d.kind == "header" and ep in PAGE_EPS and d.where in PAGE_HEADERS),
    Deviation(7, lambda sid, ep, d: d.kind == "discord" and sid.startswith("A.up5xx")),
    Deviation(8, lambda sid, ep, d: any(x in ep for x in ("/public/status", "/public/live")) and d.kind == "body"
              and ("status" in d.where or "stability" in d.where)),
    Deviation(9, lambda sid, ep, d: d.kind == "discord" and sid.startswith("B.dm.")),
    Deviation(10, lambda sid, ep, d: ep in ("GET /status", "HEAD /status") and d.kind == "body"),
    Deviation(12, lambda sid, ep, d: ep in ("GET /status", "HEAD /status") and d.kind == "header"),
    Deviation(14, lambda sid, ep, d: d.kind == "ledger" and "状态页事件" in json.dumps(d.b, ensure_ascii=False),
              lambda d: d.where.startswith("admin_actions") and "状态页事件" in json.dumps(d.b, ensure_ascii=False)),
    Deviation(15, lambda sid, ep, d: ep in ("HEAD /status.json", "HEAD /status/feed.xml")),
    Deviation(18, lambda sid, ep, d: ep in ("GET /terms", "GET /privacy") and d.kind == "body"),
    Deviation(20, lambda sid, ep, d: False,
              lambda d: d.where.startswith("error_events") and "只在 Rust 有" in d.where and '"source": "web:' in json.dumps(d.b, ensure_ascii=False)),
    Deviation(21, lambda sid, ep, d: d.kind == "body" and d.tag == "number-format" and "image_min_interval" in d.where),
    Deviation(22, lambda sid, ep, d: sid.startswith("C.d22."),
              lambda d: _bug_path(d, "/announcement", "/public/live", "/public/client-error")),
    Deviation(23, lambda sid, ep, d: sid.startswith("B.d23."), np=True),
    Deviation(25, lambda sid, ep, d: d.kind == "ledger" and d.where.startswith("admin_actions")
              and any(x in ep for x in ("/home/join", "/checks/ack", "/announcement/send"))),
    Deviation(28, lambda sid, ep, d: sid.startswith("C.bridge.d28."),
              lambda d: _bug_path(d, "/self-register/ops")),
    Deviation(29, lambda sid, ep, d: sid.startswith("C.bridge.d29.")),
    Deviation(32, lambda sid, ep, d: ep == "GET /admin/api/modules" and "risk" in d.where),
    Deviation(33, lambda sid, ep, d: "kernel_last" in d.where and d.where.endswith(".error")),
    Deviation(24, lambda sid, ep, d: sid.startswith("B.d24."),
              lambda d: _bug_path(d, "/admin/api/ops/registration")),
]
DEV_DOC = load_deviation_doc()


def classify(step: Step, d: Diff) -> str:
    if step.expect_dev:
        return step.expect_dev
    for dev in DEVIATIONS:
        if dev.match(step.id, step.endpoint(), d):
            return dev.id
    return ""


BG_SETTING_KEYS = ("algo_notice", "guard_hourly_adapted_at", "quota_ceiling", "quota_base_now", "quota_algo_day",
                   "quota_algo_history", "quota_algo_last", "autopilot_last", "autopilot_history", "kernel_last",
                   "v5_period_samples", "bot_status", "register_max_users", "register_daily_images", "register_daily_v5",
                   "guard_base_daily_images", "global_daily_v5", "register_image_scope", "key_inactivity_grace_started_at")


def is_background_gap(d: Diff) -> bool:
    """旧版靠后台任务（每日额度微调 / 模块交叉校验 / 退群核对 / 机器人心跳）写的库，Rust 还没有对应的后台任务。"""
    w = d.where
    if w.startswith("site_settings") and any(f"[key='{k}']" in w for k in BG_SETTING_KEYS):
        return True
    if w.startswith("error_events") and '"source": "check:' in json.dumps(d.a, ensure_ascii=False):
        return True
    if w.startswith("admin_actions") and (isinstance(d.a, dict) and d.a.get("actor") == "系统"):
        return True
    return w.startswith("discord_registrations") and ".member_checked_at" in w


def classify_db(d: Diff) -> str:
    for dev in DEVIATIONS:
        if dev.db(d):
            return dev.id
    return "BG" if is_background_gap(d) else ""


# ============================================================ SQLite 逐表 diff ====
DB_TIME_COL = re.compile(r"(^|_)(ts|at|seen|until|started)$|_at$|^ts$|_ts$")
DB_IGNORE_COL = {"wait_ms", "dur_ms", "rid", "last_rid", "ver"}      # ver = 网关版本号（2.26.0 vs 3.0.0），本来就不同
DB_TIME_TOL = 120.0
DB_IGNORE_TABLES = {"sqlite_sequence"}
LOG_TABLES = {"admin_actions", "usage_log", "generation_audit"}     # 流水表按行内容比（id 会因为一边多写一行而整体错位）


def dump_db(data_dir: Path, scratch: Path) -> dict:
    """复制数据库文件（含 -wal / -shm）到临时目录再读，不碰原件。"""
    scratch.mkdir(parents=True, exist_ok=True)
    for suffix in ("", "-wal", "-shm"):
        src = data_dir / f"nai_gate.db{suffix}"
        if src.exists():
            shutil.copy2(src, scratch / src.name)
    con = sqlite3.connect(str(scratch / "nai_gate.db"))
    out: dict = {"tables": {}, "schema": {}, "objects": {}}
    for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"):
        info = con.execute(f'PRAGMA table_info("{t}")').fetchall()
        cols = [r[1] for r in info]
        pk = [r[1] for r in sorted(info, key=lambda r: r[5]) if r[5] > 0]
        rows = con.execute(f'SELECT * FROM "{t}"').fetchall()
        out["tables"][t] = {"cols": cols, "pk": pk, "rows": rows}
        out["schema"][t] = [(r[1], (r[2] or "").upper(), r[3], r[4], r[5]) for r in info]
    for name, typ, tbl, sql in con.execute("SELECT name, type, tbl_name, sql FROM sqlite_master WHERE type IN ('index','trigger') "
                                          "AND name NOT LIKE 'sqlite_%'"):
        out["objects"][f"{typ}:{name}"] = (tbl, re.sub(r"\s+", " ", sql or "").strip())
    con.close()
    return out


def _norm_cell(table: str, col: str, v: Any) -> Any:
    if isinstance(v, (bytes, bytearray)):
        return ("blob", len(v), sha(bytes(v)))
    if isinstance(v, str) and (table, col) in (("site_settings", "value"), ("anlas_reconciliations", "snapshot"),
                                                  ("feedback", "answers"), ("incidents", "components")):
        try:
            return ("json", json.loads(v))
        except ValueError:
            return v
    return v


def _cell_equal(col: str, a: Any, b: Any) -> bool:
    if isinstance(a, tuple) and isinstance(b, tuple) and a[:1] == ("json",) and b[:1] == ("json",) \
            and isinstance(a[1], (int, float)) and isinstance(b[1], (int, float)) and min(a[1], b[1]) > 1e9:
        return abs(a[1] - b[1]) <= DB_TIME_TOL      # site_settings 里单个时间戳
    if isinstance(a, tuple) and a and a[0] == "json" and isinstance(b, tuple) and b and b[0] == "json":
        return not Cmp().diff(a[1], b[1])
    if DB_TIME_COL.search(col) and isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return abs(a - b) <= DB_TIME_TOL
    if isinstance(a, tuple) and isinstance(b, tuple) and a[:1] == ("json",) and b[:1] == ("json",) \
            and isinstance(a[1], (int, float)) and isinstance(b[1], (int, float)) and min(a[1], b[1]) > 1e9:
        return abs(a[1] - b[1]) <= DB_TIME_TOL      # site_settings 里单个时间戳
    if isinstance(a, str) and isinstance(b, str) and KEYLIKE.match(a) and KEYLIKE.match(b) \
            and a not in KNOWN_TOKENS and b not in KNOWN_TOKENS:
        return len(a) == len(b)
    return a == b


def diff_dbs(da: dict, db_: dict, seen: set[str], skip: tuple = ()) -> list[Diff]:
    out: list[Diff] = []

    def add(kind: str, where: str, a: Any, b: Any, ident: str, tag: str = "") -> None:
        key = f"{ident}|{json.dumps(a, default=str, sort_keys=True)}|{json.dumps(b, default=str, sort_keys=True)}"
        if key in seen:
            return
        seen.add(key)
        out.append(Diff(kind, where, a, b, tag))

    for t in sorted(set(da["tables"]) | set(db_["tables"])):
        if t not in da["tables"] or t not in db_["tables"]:
            add("schema", f"table {t}", t in da["tables"], t in db_["tables"], f"tbl:{t}")
    for t in sorted(set(da["schema"]) & set(db_["schema"])):
        if da["schema"][t] != db_["schema"][t]:
            sa, sb = {c[0]: c for c in da["schema"][t]}, {c[0]: c for c in db_["schema"][t]}
            for c in sorted(set(sa) | set(sb)):
                if sa.get(c) != sb.get(c):
                    add("schema", f"column {t}.{c}", sa.get(c), sb.get(c), f"col:{t}.{c}")
            if [c[0] for c in da["schema"][t]] != [c[0] for c in db_["schema"][t]] and set(sa) == set(sb):
                add("schema", f"column order {t}", [c[0] for c in da["schema"][t]], [c[0] for c in db_["schema"][t]], f"colorder:{t}", "order")
    for o in sorted(set(da["objects"]) | set(db_["objects"])):
        if da["objects"].get(o) != db_["objects"].get(o):
            add("schema", f"{o}", da["objects"].get(o), db_["objects"].get(o), f"obj:{o}")
    for t in sorted(set(da["tables"]) & set(db_["tables"])):
        if t in skip:
            continue
        ta, tb = da["tables"][t], db_["tables"][t]
        if ta["cols"] != tb["cols"]:
            continue                                   # 列不同已在 schema 里报告
        cols, pk = ta["cols"], ta["pk"]
        if t in LOG_TABLES:
            # 只追加的流水表：按行内容（去掉 id / 时间 / 耗时）比多重集。一边多写一行（比如旧版启动时的后台任务）
            # 不会让后面所有行的 id 错位、变成几百条无意义的差异
            def bag(tt: dict) -> "collections.Counter":
                c: "collections.Counter" = collections.Counter()
                for r in tt["rows"]:
                    c[tuple((col, ("blob", sha(bytes(v))) if isinstance(v, (bytes, bytearray)) else v)
                            for col, v in zip(cols, r) if col not in pk and col not in DB_IGNORE_COL
                            and not DB_TIME_COL.search(col))] += 1
                return c
            ba, bb = bag(ta), bag(tb)
            only_a, only_b = list((ba - bb).elements()), list((bb - ba).elements())
            da_, db2 = [dict(k) for k in only_a], [dict(k) for k in only_b]
            used: set[int] = set()
            used_a: set[int] = set()
            groups: dict[tuple, list] = {}
            costs = sorted(((len(dc), i, j, dc) for i, ra_ in enumerate(da_) for j, rb_ in enumerate(db2)
                            for dc in [[c for c in ra_ if ra_[c] != rb_.get(c)]] if len(dc) <= 3), key=lambda x: x[0])
            for _, i, j, dc in costs:               # 差异列最少的配对优先，一对一
                if i in used_a or j in used:
                    continue
                used_a.add(i)
                used.add(j)
                sig = tuple((c, da_[i][c], db2[j][c]) for c in dc)
                groups.setdefault(sig, [0, da_[i]])[0] += 1
            left_a = [r for i, r in enumerate(da_) if i not in used_a]
            for sig, (n, ex) in groups.items():
                ident = f"{ex.get('kind', ex.get('action', ''))}/{ex.get('key_name', ex.get('actor', ''))}"
                add("db", f"{t} 同一请求的记账行 ×{n} [{ident}]", {c: a for c, a, _ in sig}, {c: b for c, _, b in sig}, f"{t}|pair|{sig}")
            for ra_ in left_a:
                add("db", f"{t} 只在旧版有 ×1", ra_, "<缺失>", f"{t}|py|{sorted(ra_.items())}")
            for j, rb_ in enumerate(db2):
                if j not in used:
                    add("db", f"{t} 只在 Rust 有 ×1", "<缺失>", rb_, f"{t}|rs|{sorted(rb_.items())}")
            continue
        key_cols = pk or cols
        idx = [cols.index(c) for c in key_cols]

        def rows_of(tt: dict) -> dict:
            res = {}
            for r in tt["rows"]:
                k = tuple(("blob", sha(x)) if isinstance(x, (bytes, bytearray)) else x for x in (r[i] for i in idx))
                if k in res and not pk:
                    k = k + (len(res),)
                res[k] = {c: _norm_cell(t, c, v) for c, v in zip(cols, r) if c not in DB_IGNORE_COL}
            return res

        ra, rb = rows_of(ta), rows_of(tb)
        n_before = len(out)
        for k in list(ra) + [k for k in rb if k not in ra]:
            pkd = ",".join(f"{c}={v!r}" for c, v in zip(key_cols, k)) if pk else ""
            if k not in rb:
                add("db", f"{t} [{pkd or 'row'}] 只在旧版有", ra[k], "<缺失>", f"{t}|{k}")
            elif k not in ra:
                add("db", f"{t} [{pkd or 'row'}] 只在 Rust 有", "<缺失>", rb[k], f"{t}|{k}")
            else:
                for c in cols:
                    if c in DB_IGNORE_COL:
                        continue
                    if not _cell_equal(c, ra[k][c], rb[k][c]):
                        a_, b_ = ra[k][c], rb[k][c]
                        a_ = a_[1] if isinstance(a_, tuple) and a_ and a_[0] == "json" else a_
                        b_ = b_[1] if isinstance(b_, tuple) and b_ and b_[0] == "json" else b_
                        add("db", f"{t} [{pkd or 'row'}].{c}", a_, b_, f"{t}|{k}|{c}")
        if len(out) > n_before and pk:
            # 只是顺序不同？去掉主键后做多重集比较
            def bag(rows: dict) -> list:
                return sorted(json.dumps({c: v for c, v in r.items() if c not in pk and not DB_TIME_COL.search(c)},
                                         default=str, sort_keys=True) for r in rows.values())
            if bag(ra) == bag(rb) and len(ra) == len(rb):
                for d in out[n_before:]:
                    d.tag = (d.tag + " order-only").strip()
    return out


# ============================================================ 逐请求的记账行 ====
LEDGER_COLS = {
    "usage_log": ["key_id", "key_name", "kind", "model", "status", "images", "anlas", "tokens", "unconfirmed_anlas", "detail",
                  "reason", "client", "up_status", "src"],
    "admin_actions": ["actor", "action", "target", "detail", "ok"],
}


COUNTER_TABLES = {
    "counters": (("key_id", "day"), ["images", "legacy_free_images", "anlas", "text_tokens", "requests", "v5"]),
    "upstream_token_counters": (("token_id", "day"), ["images", "v5", "units"]),
}


class Ledger:
    """每条请求之后，读两份库里新增的 usage_log / admin_actions 行、以及 counters / upstream_token_counters 的增量并逐项比较——
    哪条请求的记账不一样，一眼就能对上号（语料结束后的整库 diff 只能告诉你“有一行不一样”）。"""

    def __init__(self, side: "Side"):
        self.side = side
        self.last: dict[str, int] = {}
        self.counters: dict[str, dict] = {}

    def _con(self) -> sqlite3.Connection:
        con = sqlite3.connect(f"file:{self.side.data_dir / 'nai_gate.db'}?mode=ro", uri=True, timeout=3)
        con.row_factory = sqlite3.Row
        return con

    def _max_ids(self) -> dict[str, int]:
        con = self._con()
        try:
            return {t: (con.execute(f"SELECT COALESCE(MAX(id), 0) FROM {t}").fetchone()[0]) for t in LEDGER_COLS}
        finally:
            con.close()

    def _counter_snapshot(self, con: sqlite3.Connection) -> dict[str, dict]:
        snap: dict[str, dict] = {}
        for t, (keys, cols) in COUNTER_TABLES.items():
            have = [r[1] for r in con.execute(f'PRAGMA table_info("{t}")')]
            use = [c for c in cols if c in have]
            snap[t] = {tuple(r[:len(keys)]): dict(zip(use, r[len(keys):]))
                       for r in con.execute(f"SELECT {', '.join(list(keys) + use)} FROM {t}")}
        return snap

    def reset(self) -> None:
        with contextlib.suppress(Exception):
            self.last = self._max_ids()
            con = self._con()
            try:
                self.counters = self._counter_snapshot(con)
            finally:
                con.close()

    def settle(self, limit: float = 0.8) -> None:
        """后台记账任务是 fire-and-forget：等库里的最大 id 连续两次不变再读。"""
        end, prev = time.time() + limit, None
        while time.time() < end:
            try:
                cur = self._max_ids()
            except Exception:  # noqa: BLE001
                return
            if cur == prev:
                return
            prev = cur
            time.sleep(0.04)

    def new_rows(self) -> dict[str, list[dict]]:
        out: dict[str, list[dict]] = {}
        try:
            con = self._con()
        except Exception:  # noqa: BLE001
            return out
        try:
            for t, cols in LEDGER_COLS.items():
                rows = con.execute(f"SELECT id, {', '.join(cols)} FROM {t} WHERE id > ? ORDER BY id", (self.last.get(t, 0),)).fetchall()
                out[t] = [dict(r) for r in rows]
                if rows:
                    self.last[t] = rows[-1]["id"]
            snap = self._counter_snapshot(con)
            for t in COUNTER_TABLES:
                old, new = self.counters.get(t, {}), snap[t]
                delta = {}
                for k, v in new.items():
                    d = {c: round(x - old.get(k, {}).get(c, 0), 6) for c, x in v.items()}
                    d = {c: x for c, x in d.items() if x}
                    if d:
                        delta[" / ".join(map(str, k))] = d
                out[t] = [delta] if delta else []
            self.counters = snap
        except Exception:  # noqa: BLE001
            pass
        finally:
            con.close()
        return out


def compare_ledger(a: dict[str, list[dict]], b: dict[str, list[dict]]) -> list[Diff]:
    out: list[Diff] = []
    for t in COUNTER_TABLES:
        da, db_ = (a.get(t) or [{}])[0], (b.get(t) or [{}])[0]
        if da != db_:
            keys = sorted(set(da) | set(db_))
            out.append(Diff("ledger", f"{t} 本次增量", {k: da.get(k) for k in keys if da.get(k) != db_.get(k)},
                            {k: db_.get(k) for k in keys if da.get(k) != db_.get(k)}))
    for t in LEDGER_COLS:
        ra, rb = a.get(t, []), b.get(t, [])
        key = lambda r: json.dumps({k: v for k, v in r.items() if k != "id"}, sort_keys=True, ensure_ascii=False, default=str)
        ra, rb = sorted(ra, key=key), sorted(rb, key=key)
        pairs = list(zip(ra, rb))
        for x, y in pairs:
            dc = {c: (x[c], y[c]) for c in LEDGER_COLS[t] if x[c] != y[c]
                  and not (isinstance(x[c], (int, float)) and isinstance(y[c], (int, float)) and abs(x[c] - y[c]) < 1e-9)}
            if dc:
                ident = x.get("kind") or x.get("action") or ""
                out.append(Diff("ledger", f"{t} 行（{ident}）", {c: v[0] for c, v in dc.items()}, {c: v[1] for c, v in dc.items()}))
        for x in ra[len(rb):]:
            out.append(Diff("ledger", f"{t} 只在旧版写了一行", {k: x[k] for k in LEDGER_COLS[t]}, "<没写>"))
        for y in rb[len(ra):]:
            out.append(Diff("ledger", f"{t} 只在 Rust 写了一行", "<没写>", {k: y[k] for k in LEDGER_COLS[t]}))
    return out


# ============================================================ 运行语料 ====
@dataclass
class Collected:
    results: list[StepResult] = field(default_factory=list)
    db_diffs: dict[str, list[Diff]] = field(default_factory=dict)       # 语料 -> diff
    startup: list[StepResult] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def ensure_alive(world: World) -> list[str]:
    """网关被外部杀掉（共用机器上别人的 pkill）/ 崩溃时，记下退出码并重启，免得后面整段都是连接被拒。"""
    notes = []
    for s in world.sides.values():
        if s.proc is not None and s.proc.poll() is not None:
            rc = s.proc.returncode
            notes.append(f"{s.label} 网关意外退出（returncode={rc}），已重启；此后的内存状态（限流窗口 / 冷却）从零开始")
            log(notes[-1])
            if s.proc in PROCS:
                PROCS.remove(s.proc)
            s.proc = None
            start_gateway(s, world.reset_hour, world.rust_bin)
    return notes


def run_step(world: World, clients: dict, corpus: str, step: Step) -> Optional[StepResult]:
    py, rs = world.sides[PY], world.sides[RS]
    if step.fn is not None:
        step.fn(world)
        return None
    for note in ensure_alive(world):
        world.notes.append(f"{step.id} 之前：{note}")
    for path, reply in step.script:
        for s in (py, rs):
            s.mock.push(path, copy.deepcopy(reply))
    marks = {n: (s.mock.mark(), s.sink.mark()) for n, s in world.sides.items()}
    rpath = world.subst(step.path, rs).split("?", 1)[0]
    ra, desc = send(world, py, step, clients[PY])
    rb, _ = send(world, rs, step, clients[RS])
    if max(ra.elapsed, rb.elapsed) > 4.0:
        log(f"  慢请求 {step.id}: 旧版 {ra.elapsed:.1f}s / Rust {rb.elapsed:.1f}s")
    time.sleep(step.settle)
    led = {}
    if world.ledgers:
        for lg in world.ledgers.values():
            lg.settle()
        led = {n: lg.new_rows() for n, lg in world.ledgers.items()}
    ups = {n: s.mock.since(marks[n][0]) for n, s in world.sides.items()}
    sinks = {n: s.sink.since(marks[n][1]) for n, s in world.sides.items()}
    do_captures(step, py, ra)
    do_captures(step, rs, rb)
    diffs: list[Diff] = []
    cat = "pass"
    if ra.error or rb.error:
        diffs.append(Diff("status", "transport-error", ra.error or ra.status, rb.error or rb.status))
    elif is_router_miss(rb, rpath, ra) and not is_router_miss(ra, rpath):
        cat = "notported"
        diffs.append(Diff("status", "code", ra.status, rb.status, "not-ported"))
    else:
        if ra.status != rb.status:
            diffs.append(Diff("status", "code", ra.status, rb.status))
            diffs.append(Diff("body", "preview (status differs)", ra.body[:140].decode("utf-8", "replace"),
                              rb.body[:140].decode("utf-8", "replace"), "preview"))
        else:
            diffs += compare_bodies(step, ra, rb, body_kind(step, ra, rb))
        norm = [(re.compile(p), r) for p, r in step.norm]
        diffs += compare_headers(ra, rb, norm, step.skip_headers)
        diffs += compare_upstream(step, ups[PY], ups[RS])
        if led:
            diffs += compare_ledger(led[PY], led[RS])
        if len(sinks[PY]) != len(sinks[RS]):
            diffs.append(Diff("discord", "outbound attempts (all blocked)", sinks[PY], sinks[RS]))
    for d in diffs:
        d.dev = "" if cat == "notported" else classify(step, d)
    if cat == "notported":
        intended = [dv.id for dv in DEVIATIONS if dv.np and dv.match(step.id, step.endpoint(), diffs[0])]
        if intended:
            cat = "expected"
            diffs[0].dev = intended[0]
    if cat not in ("notported",) and diffs:
        cat = "expected" if all(d.dev for d in diffs if d.tag != "preview") else "fail"
    return StepResult(step, corpus, ra, rb, diffs, cat, desc, (ups[PY], ups[RS]))


def db_phase(world: World, work: Path, corpus: str, col: Collected, seen: set[str], skip: tuple = ()) -> None:
    da = dump_db(world.sides[PY].data_dir, work / "scratch" / "py")
    dbb = dump_db(world.sides[RS].data_dir, work / "scratch" / "rs")
    ds = diff_dbs(da, dbb, seen, skip)
    for d in ds:
        d.dev = classify_db(d)
    col.db_diffs[corpus] = ds


def start_all(world: World, rust_bin: Path, col: Collected, label: str, compare: bool = True) -> None:
    """起两个网关，并把“启动期间上游收到的请求”当作一条语料比较（旧版启动时会探一次订阅）。"""
    for s in world.sides.values():
        mk = s.mock.mark()
        s.ctx["_startup_mark"] = mk
        start_gateway(s, world.reset_hour, rust_bin)
    time.sleep(1.0)
    world.reset_clients()
    if not compare:
        return
    py, rs = world.sides[PY], world.sides[RS]
    ua, ub = py.mock.since(py.ctx["_startup_mark"]), rs.mock.since(rs.ctx["_startup_mark"])
    st = Step(f"S.startup.{label}", "GET", "/healthz", ep=f"启动期间的上游请求（{label}）")
    d = compare_upstream(st, ua, ub)
    for x in d:
        x.dev = classify(st, x)
    col.startup.append(StepResult(st, "S", Resp(200), Resp(200), d,
                                  "pass" if not d else ("expected" if all(x.dev for x in d) else "fail"), "startup"))


def run_all(world: World, rust_bin: Path, corpora: dict[str, list[Step]], order: list[str], work: Path,
            restart: bool) -> Collected:
    import httpx
    col = Collected()
    seen: set[str] = set()
    world.rust_bin = rust_bin
    def new_client() -> "httpx.Client":
        return httpx.Client(timeout=40, follow_redirects=False, trust_env=False,
                            limits=httpx.Limits(max_keepalive_connections=4))

    clients = {n: new_client() for n in world.sides}

    world.ledgers = {n: Ledger(sd) for n, sd in world.sides.items()}

    def reset_clients() -> None:
        for n in list(clients):
            with contextlib.suppress(Exception):
                clients[n].close()
            clients[n] = new_client()
        for lg in world.ledgers.values():     # 每次（重）启动之后，旧的记账行不算
            if lg.side.data_dir.name in ("A", "B"):
                lg.reset()

    world.reset_clients = reset_clients
    CLEANUPS.append(lambda: [c.close() for c in clients.values()])
    # 后台任务基线：旧版启动期的后台任务（动态额度每日微调、模块交叉校验、退群核对……）开着，不发任何请求，只看写了什么库
    if (work / "A0").is_dir():
        mains = {n: sd.data_dir for n, sd in world.sides.items()}
        for n, sd in world.sides.items():
            sd.data_dir = work / ("A0" if n == PY else "B0")
        start_all(world, rust_bin, col, "后台任务基线", compare=False)
        time.sleep(5.0)
        stop_all(world)
        db_phase(world, work, "后台任务基线（不发请求，启动后约 6 秒）", col, set())
        for n, sd in world.sides.items():
            sd.data_dir = mains[n]
    # 启动后先比较一次 schema（迁移是否一致）
    start_all(world, rust_bin, col, "初始")
    stop_all(world)
    db_phase(world, work, "启动后（迁移）", col, seen)
    start_all(world, rust_bin, col, "语料 " + order[0])
    for i, name in enumerate(order):
        steps = corpora[name]
        log(f"语料 {name}：{sum(1 for s in steps if s.fn is None)} 条请求")
        for n_done, step in enumerate(steps):
            if n_done and n_done % 60 == 0:
                log(f"  …{name} {n_done}/{len(steps)}")
            try:
                r = run_step(world, clients, name, step)
            except Exception as exc:  # noqa: BLE001
                col.errors.append(f"{step.id}: {type(exc).__name__}: {exc}")
                continue
            if r is not None:
                col.results.append(r)
        time.sleep(0.5)
        if restart or i == len(order) - 1:
            stop_all(world)
            db_phase(world, work, name, col, seen, skip=("usage_log", "admin_actions"))   # 这两张表每条请求后已逐行比过
            if i < len(order) - 1:
                start_all(world, rust_bin, col, "语料 " + order[i + 1])
        else:
            time.sleep(0.5)
    col.errors += world.notes
    return col


def stop_all(world: World) -> None:
    for s in world.sides.values():
        stop_gateway(s)


# ============================================================ 报告 ====
CORPUS_TITLE = {"A": "A · 成员接口（出图 / 文本 / 兼容接口）", "B": "B · 后台接口（/admin/api/*）",
                "C": "C · 公开页 / 状态页 / 成员网页会话 / 机器人桥接"}
SYMBOL = {"pass": "✓", "fail": "✗", "expected": "≈", "notported": "⛔"}


def md_escape(s: str) -> str:
    return s.replace("|", "\\|").replace("\n", " ")


def group_results(results: list[StepResult]) -> dict[tuple[str, str], list[StepResult]]:
    g: dict[tuple[str, str], list[StepResult]] = {}
    for r in results:
        g.setdefault((r.corpus, r.step.endpoint()), []).append(r)
    return g


def only_systemic(rs: list[StepResult], sys_sigs: set) -> bool:
    """这个接口的 ✗ 全部是系统性差异（接口自己的行为没有差异）。"""
    fails = [r for r in rs if r.cat == "fail"]
    return bool(fails) and all(all(dsig_of(d) in sys_sigs for d in r.diffs if not d.dev and d.tag != "preview") for r in fails)


def dsig_of(d: Diff) -> tuple:
    short = lambda v: json.dumps(v, ensure_ascii=False, default=str)[:90]
    return (d.kind, re.sub(r"\[\d+\]", "[]", d.where), short(d.a), short(d.b), d.tag)


def systemic_sigs(res: list[StepResult], threshold: int = 6) -> dict[tuple, list[str]]:
    sig_steps: dict[tuple, list[str]] = {}
    for r in res:
        if r.cat == "fail":
            for sg in {dsig_of(d) for d in r.diffs if not d.dev and d.tag != "preview"}:
                sig_steps.setdefault(sg, []).append(r.step.id)
    return {sg: ids for sg, ids in sig_steps.items() if len(ids) >= threshold}


def verdict(rs: list[StepResult]) -> str:
    cats = [r.cat for r in rs]
    if "fail" in cats:
        return "fail"
    if "notported" in cats:
        return "notported" if all(c == "notported" for c in cats) else "fail"
    if "expected" in cats:
        return "expected"
    return "pass"


def render_report(col: Collected, meta: dict) -> str:
    L: list[str] = []
    res = col.results
    groups = group_results(res)
    cnt = {k: sum(1 for r in res if r.cat == k) for k in SYMBOL}
    ep_v = {k: verdict(v) for k, v in groups.items()}
    systemic = systemic_sigs(res)
    ep_sys = {k for k, v in groups.items() if ep_v[k] == "fail" and only_systemic(v, set(systemic))}
    ep_cnt = {k: sum(1 for v in ep_v.values() if v == k) for k in SYMBOL}
    ep_cnt["sys"] = len(ep_sys)
    db_unexpected = sum(1 for v in col.db_diffs.values() for d in v if not d.dev)
    startup_bad = [r for r in col.startup if r.cat == "fail"]
    L += ["# owl-v3 网关对拍报告（旧 Python ↔ Rust）", ""]
    L += [f"- 时间：{meta['when']}　分支：`{meta['branch']}` @ `{meta['rev']}`　Rust 二进制：`{meta['bin']}`",
          f"- 语料：{'、'.join(meta['corpora'])}；请求 {len(res)} 条（{cnt['pass']} ✓ / {cnt['fail']} ✗ / {cnt['expected']} ≈ 预期差异 / "
          f"{cnt['notported']} ⛔ Rust 尚未移植）",
          f"- 接口 {len(groups)} 个：{ep_cnt['pass']} ✓ / {ep_cnt['fail'] - ep_cnt['sys']} ✗ 自身有差异 / {ep_cnt['sys']} ✗* 只有系统性差异 / "
          f"{ep_cnt['expected']} ≈ / {ep_cnt['notported']} ⛔",
          f"- 数据库 diff：{db_unexpected} 处未解释；启动期上游请求：{'有差异' if startup_bad else '一致'}",
          f"- 服务日重置钟点 {meta['reset_hour']}（现在 +12 小时，保证一轮内不跨日）；时区 {TZ_NAME}；"
          "上游为明文 HTTP/1.1 假服务（请求头顺序在 HTTP/1.1 下比较，线上 TLS/h2 指纹这层看不到）",
          f"- 旧网关依赖版本（requirements.txt 未锁版本）：{meta['pkgs']}",
          ("- Rust 的页面路由用旧版静态页当替身（web/dist 未构建，STATIC_DIR）：页面只比状态码 / content-type / 响应头，不比正文"
           if meta.get("stub") else "- Rust 页面用二进制内嵌的 web/dist"), ""]
    if col.errors:
        L += ["## 对拍程序自身的错误", ""] + [f"- {e}" for e in col.errors] + [""]
    failing = bool(ep_cnt["fail"] or db_unexpected or startup_bad)
    L += [f"**结论：{'存在未解释的差异（见下）' if failing else '没有未解释的差异'}**"
          + (f"；另有 {ep_cnt['notported']} 个接口 Rust 尚未移植（上线前必须清零）" if ep_cnt["notported"] else ""), ""]

    # ---- 逐语料的接口表 ----
    for c in ("A", "C", "B"):
        keys = [k for k in groups if k[0] == c]
        if not keys:
            continue
        L += [f"## {CORPUS_TITLE.get(c, c)}", "", "（✗* = 这个接口自己没有差异，只有下面“系统性差异”里列的那几处）", "",
              "| 接口 | 结果 | 用例 | ✓ | ✗ | ≈ | ⛔ |", "|---|:-:|--:|--:|--:|--:|--:|"]
        for k in keys:
            rs = groups[k]
            n = {s: sum(1 for r in rs if r.cat == s) for s in SYMBOL}
            L.append(f"| `{md_escape(k[1])}` | {SYMBOL[ep_v[k]] + ('*' if k in ep_sys else '')} | {len(rs)} | {n['pass']} | {n['fail']} | {n['expected']} | {n['notported']} |")
        L.append("")

    # ---- ✗ 详情 ----
    bad = [(k, rs) for k, rs in groups.items() if ep_v[k] == "fail"]
    # 系统性差异：同一条差异（同类、同位置、同两边的值）出现在很多条用例里——只在这里写一次
    dsig = dsig_of
    sys_id = {sg: f"S{i}" for i, sg in enumerate(sorted(systemic, key=lambda g: -len(systemic[g])), 1)}
    if systemic:
        L += ["## 跨用例的系统性差异（同一处差异在 ≥ 6 条用例里重复出现；下面各接口的明细里不再重复）", "",
              "| 编号 | 次数 | 差异 | 例 |", "|---|--:|---|---|"]
        for sg, sid_ in sorted(sys_id.items(), key=lambda kv: kv[1][1:].zfill(4)):
            ids = systemic[sg]
            kind, where, a, b, tag = sg
            L.append(f"| {sid_} | {len(ids)} | `{md_escape(f'{kind} {where}' + (f' [{tag}]' if tag else '') + f': py={a}  rs={b}')}` | "
                     f"{', '.join(ids[:3])} |")
        L.append("")
    L += ["## ✗ 未解释的差异", ""]
    if not bad and not db_unexpected and not startup_bad:
        L += ["（无）", ""]
    for (c, ep), rs in bad:
        L += [f"### [{c}] `{ep}`", ""]
        seen_sig: set[str] = set()
        shown = 0
        for r in rs:
            if r.cat not in ("fail", "notported"):
                continue
            ds = [d for d in r.diffs if not d.dev]
            own = [d for d in ds if dsig(d) not in sys_id]
            sys_here = sorted({sys_id[dsig(d)] for d in ds if dsig(d) in sys_id}, key=lambda x: int(x[1:]))
            sig = "|".join(f"{d.kind}:{d.where}:{d.tag}" for d in own) + "#" + ",".join(sys_here)
            if sig in seen_sig:
                continue
            seen_sig.add(sig)
            shown += 1
            if shown > 6:
                L.append(f"- …另有 {sum(1 for x in rs if x.cat == 'fail') - 6} 条类似失败用例")
                break
            L.append(f"- **{r.step.id}** `{md_escape(r.req)}`" + (f" — {r.step.note}" if r.step.note else "")
                     + (f" — 另含系统性差异 {', '.join(sys_here)}" if sys_here and own else ""))
            if not own and sys_here:
                L.append(f"  - 只有系统性差异：{', '.join(sys_here)}")
            for d in own[:8]:
                L.append(f"  - `{md_escape(d.render())}`")
            if len(own) > 8:
                L.append(f"  - …另有 {len(own) - 8} 处差异")
        L.append("")
    for r in startup_bad:
        L += [f"### 启动期间的上游请求：{r.step.ep}", ""] + [f"- `{md_escape(d.render())}`" for d in r.diffs if not d.dev] + [""]
    mixed = [(k, rs) for k, rs in groups.items() if ep_v[k] == "fail" and any(r.cat == "notported" for r in rs)]
    if mixed:
        L += ["> 注：以下接口一部分用例通过、一部分在 Rust 上 404：" + "、".join(f"`{k[1]}`" for k, _ in mixed), ""]

    # ---- 数据库 ----
    writes_np = sum(1 for r in res if r.cat == "notported" and r.step.method in ("POST", "PUT", "PATCH", "DELETE"))
    L += ["## 数据库 diff（逐表；忽略 ts / *_at / rid / wait_ms / dur_ms，随机 Key 只比形状；site_settings 的 JSON 值逐字段比；"
          "usage_log / admin_actions 在每条请求之后就逐行比过（见各接口明细里的“ledger”），这里只在启动阶段整表比；generation_audit 按行内容比，不看 id）", ""]
    if writes_np:
        L += [f"> 有 {writes_np} 条写请求在 Rust 上是 404（尚未移植），它们本该写进库的行必然表现为“只在旧版有”——这类差异会随路由移植而消失。"
              "“启动后（迁移）”一节是不发任何请求、只启动再停止时两份库的结构 / 迁移差异。旧版启动期后台任务写的库见“后台任务基线”和下面的“后台任务尚未移植”一节。", ""]
    any_db = False
    for name, all_ds in col.db_diffs.items():
        ds = [d for d in all_ds if not d.dev]
        if not ds:
            continue
        any_db = True
        L += [f"### {name}", ""]
        by_table: dict[str, list[Diff]] = {}
        for d in ds:
            by_table.setdefault(d.where.split(" ")[0] if d.kind != "schema" else "schema", []).append(d)
        for t, items in by_table.items():
            L.append(f"- **{t}**：{len(items)} 处")
            for d in items[:6]:
                L.append(f"  - `{md_escape(d.render())}`")
            if len(items) > 6:
                L.append(f"  - …另有 {len(items) - 6} 处")
        L.append("")
    if not any_db:
        L += ["（各表一致）", ""]

    # ---- 后台任务 ----
    bg_by: dict[str, list[tuple[str, Diff]]] = {}
    for name, all_ds in col.db_diffs.items():
        for d in all_ds:
            if d.dev == "BG":
                bg_by.setdefault(name, []).append((name, d))
    L += ["## ⛔ 旧版后台任务写的库，Rust 还没有对应的后台任务（不算 ✗；上线前必须清零）", ""]
    if not bg_by:
        L += ["（无）", ""]
    else:
        L += ["旧版启动后会跑动态额度每日微调、模块交叉校验（写 Bug 追踪）、退群核对、机器人心跳、小时上限自适应等；"
              "下面是这些后台任务在两份库里留下的差异。语料阶段已经把动态额度 / 模块关掉，所以这里主要来自“后台任务基线”那一阶段。", ""]
        for name, items in bg_by.items():
            L.append(f"### {name}：{len(items)} 处")
            seen_w: set[str] = set()
            for _, d in items:
                key = d.where.split("[")[0] + (d.where.split("]")[-1] if "]" in d.where else "")
                if key in seen_w:
                    continue
                seen_w.add(key)
                L.append(f"- `{md_escape(d.render(100))}`")
                if len(seen_w) >= 12:
                    break
            L.append("")

    # ---- 预期差异 ----
    L += ["## ≈ 预期差异（docs/v3-deviations.md；不算失败）", ""]
    exp: dict[str, list[tuple[str, Diff]]] = {}
    for r in res + col.startup:
        for d in r.diffs:
            if d.dev:
                exp.setdefault(d.dev, []).append((r.step.id, d))
    for name, all_ds in col.db_diffs.items():
        for d in all_ds:
            if d.dev:
                exp.setdefault(d.dev, []).append((f"db:{name}", d))
    have = {dv.n for dv in DEVIATIONS}
    L += ["| # | 位置 | 本次 | 说明 |", "|--:|---|---|---|"]
    for n in sorted(set(DEV_DOC) | have):
        doc = DEV_DOC.get(n, {})
        items = exp.get(f"D{n}", [])
        state = (f"触发 {len(items)} 处" if items else ("未触发" if n in have else "语料未覆盖（无自动匹配）"))
        L.append(f"| {n} | {md_escape(doc.get('where', '(文档里没有这一条)'))} | {state} | {md_escape((doc.get('new') or '')[:120])} |")
    L.append("")
    for n in sorted(have):
        items = exp.get(f"D{n}", [])
        if not items:
            continue
        doc = DEV_DOC.get(n, {})
        L += [f"### D{n} {doc.get('where', '')}", ""]
        steps = sorted({sid for sid, _ in items})
        L.append(f"触发 {len(items)} 处（{len(steps)} 个来源：{', '.join(steps[:8])}{'…' if len(steps) > 8 else ''}）。被归入预期的差异示例（请扫一眼，确认真的是这条在起作用）：")
        shown: set[str] = set()
        for sid, d in items:
            line = md_escape(d.render(90))
            if line in shown:
                continue
            shown.add(line)
            L.append(f"- {sid}：`{line}`")
            if len(shown) >= 4:
                break
        L.append("")
    forced = sorted({(r.step.id, r.step.expect_dev) for r in res if r.step.expect_dev})
    if forced:
        L += ["> 以下用例声明了预期差异：" + "、".join(f"{a}→{b}" for a, b in forced), ""]

    # ---- 上游请求样例 ----
    sample = next((r for r in res if r.ups and r.ups[0] and r.ups[1] and r.ups[0][0].path == "/ai/generate-image"), None)
    if sample:
        L += ["## 附：上游收到的出图请求样例（对拍的是这样的东西；明文 HTTP/1.1 下按收到的顺序）", "",
              f"用例 {sample.step.id}：", "", "| # | 旧版 | Rust |", "|--:|---|---|"]
        ha, hb = sample.ups[0][0].headers, sample.ups[1][0].headers
        for i in range(max(len(ha), len(hb))):
            cell = lambda hs: md_escape(f"{hs[i][0]}: {hs[i][1][:70]}") if i < len(hs) else ""
            L.append(f"| {i + 1} | `{cell(ha)}` | `{cell(hb)}` |")
        L += ["", f"请求体（旧版 {len(sample.ups[0][0].body)} 字节 / Rust {len(sample.ups[1][0].body)} 字节）："
              f"`{md_escape(sample.ups[0][0].body[:200].decode('utf-8', 'replace'))}`", ""]

    # ---- 尚未移植 ----
    L += ["## ⛔ Rust 尚未移植（404）", ""]
    np_groups = [(k, rs) for k, rs in groups.items() if ep_v[k] == "notported"]
    if not np_groups:
        L += ["（无）", ""]
    else:
        L += ["| 语料 | 接口 | 用例数 |", "|---|---|--:|"]
        for (c, ep), rs in np_groups:
            L.append(f"| {c} | `{md_escape(ep)}` | {len(rs)} |")
        L.append("")
    L += ["## 没有覆盖的东西", "",
          "- 并发 / 排队 / 断连取消 / 每 Key 同时请求上限 / 计时窗口（RPM 之外）；单次对拍是顺序重放。",
          "- 线上 TLS + HTTP/2（wreq Chrome 指纹）这层：假上游是明文 HTTP/1.1。",
          "- Discord 真实交互（OAuth 回调、私信、身份组）：只比较“尝试了几次”，且全部被黑洞拒绝；Rust 签发的成员会话 Cookie "
          "只能由 OAuth 回调产生，这里只验证 Rust 接受 Python 签发的成员会话与双向通用的后台会话。",
          "- 页面 HTML 正文（前端整个重写），只比状态码 / content-type / 非 CSP 响应头。", ""]
    return "\n".join(L)


# ============================================================ 主程序 ====
def pkg_versions() -> str:
    """旧网关实际跑在哪些依赖版本上（requirements.txt 没有锁版本，线上镜像里的版本可能不同）。"""
    import importlib.metadata as md
    out = []
    for name in ("fastapi", "starlette", "uvicorn", "httpx", "h11", "aiosqlite", "Pillow", "msgpack"):
        with contextlib.suppress(Exception):
            out.append(f"{name} {md.version(name)}")
    return "，".join(out)


def git_info() -> tuple[str, str]:
    def run(*a: str) -> str:
        try:
            return subprocess.run(["git", *a], cwd=ROOT, capture_output=True, text=True).stdout.strip()
        except Exception:  # noqa: BLE001
            return "?"
    return run("rev-parse", "--abbrev-ref", "HEAD"), run("rev-parse", "--short", "HEAD")


def main() -> int:
    ap = argparse.ArgumentParser(description="旧 Python 网关 ↔ Rust 网关 差分对拍")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--only", default="A,C,B", help="语料，逗号分隔，按给定顺序跑（默认 A,C,B）")
    ap.add_argument("--keep", action="store_true", help="保留 target/parity/work")
    ap.add_argument("--no-restart", action="store_true")
    ap.add_argument("--out", default=str(ROOT / "target" / "parity-report.md"))
    ap.add_argument("--rust-bin", default="")
    ap.add_argument("--list", action="store_true", help="只列出语料里的用例，不启动任何东西")
    args = ap.parse_args()
    install_cleanup()
    order = [x.strip().upper() for x in args.only.split(",") if x.strip()]
    corpora = {"A": corpus_a(), "B": corpus_b(), "C": corpus_c()}
    if args.list:
        for n in order:
            for s in corpora[n]:
                if s.fn is None:
                    print(f"{s.id:40} {s.endpoint()}")
        return 0
    work = ROOT / "target" / "parity" / "work"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    rust_bin = Path(args.rust_bin) if args.rust_bin else cargo_build(args.skip_build)
    reset_hour = pick_reset_hour()
    import asyncio
    log("生成合成种子库")
    info = asyncio.run(build_seed(work / "seed", reset_hour))
    asyncio.run(build_seed(work / "seed-bg", reset_hour, background=True))
    for n in ("A", "B"):
        shutil.copytree(work / "seed", work / n)
    for n in ("A0", "B0"):
        shutil.copytree(work / "seed-bg", work / n)
    sides = {
        PY: Side(PY, "旧 Python", work / "A", free_port(), MockUpstream(PY), DiscordSink(PY)),
        RS: Side(RS, "Rust", work / "B", free_port(), MockUpstream(RS), DiscordSink(RS)),
    }
    world = World(sides, reset_hour, info["day"])
    world.static_stub = make_static_stub(work / "static-stub")
    if not args.keep:
        FILE_CLEANUPS.append(lambda: shutil.rmtree(work, ignore_errors=True))
    try:
        col = run_all(world, rust_bin, corpora, order, work, restart=not args.no_restart)
    except Exception as exc:  # noqa: BLE001
        log(f"对拍程序出错：{type(exc).__name__}: {exc}")
        import traceback
        traceback.print_exc()
        cleanup()
        return 2
    branch, rev = git_info()
    meta = {"when": time.strftime("%Y-%m-%d %H:%M:%S"), "branch": branch, "rev": rev, "bin": str(rust_bin.relative_to(ROOT)
            if rust_bin.is_relative_to(ROOT) else rust_bin), "corpora": [CORPUS_TITLE[o][:1] for o in order],
            "reset_hour": reset_hour, "pkgs": pkg_versions(), "stub": world.static_stub}
    report = render_report(col, meta)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(report, encoding="utf-8")
    print(report)
    log(f"报告已写入 {out}")
    cleanup()
    bad = (any(r.cat == "fail" for r in col.results) or any(not d.dev for v in col.db_diffs.values() for d in v)
           or any(r.cat == "fail" for r in col.startup))
    return 1 if bad else 0


# ============================================================ 语料 A：成员接口 ====
DEL = object()          # 请求体里“删掉这个字段”的标记
DIGITS = [(r"\d+", "N")]
MINUTES = [(r"约 \d+ 分钟", "约 N 分钟"), (r"请 \d+ 分钟", "请 N 分钟"), (r"约 \d+ 秒", "约 N 秒")]
XFF = lambda ip: {"x-forwarded-for": ip}


def img(model: str = "nai-diffusion-4-5-full", n: int = 1, action: str = "generate", inp: Any = "1girl, solo",
        **params: Any) -> dict:
    p = {"width": 832, "height": 1216, "steps": 28, "scale": 5, "sampler": "k_euler_ancestral", "n_samples": n,
         "seed": 1234, "negative_prompt": "lowres"}
    p.update(params)
    p = {k: v for k, v in p.items() if v is not DEL}
    body = {"input": inp, "model": model, "action": action, "parameters": p}
    return {k: v for k, v in body.items() if v is not DEL}


def corpus_a() -> list[Step]:
    A: list[Step] = []
    add = A.append
    G = "/ai/generate-image"

    def gen(sid: str, payload: Any = None, auth: str = "member", path: str = G, note: str = "", **kw: Any) -> None:
        add(Step(f"A.gen.{sid}", "POST", path, auth=auth, json=payload, note=note, ep=kw.pop("ep", f"POST {path}"), **kw))

    # ---- 基础：未知路径 / 方法不对 / CORS ----
    add(Step("A.basic.unknown-path", "GET", "/no-such-path", ep="GET <未知路径>"))
    add(Step("A.basic.405-get", "GET", G, ep="GET /ai/generate-image (405)"))
    add(Step("A.basic.405-post-models", "POST", "/v1/models", ep="POST /v1/models (405)"))
    add(Step("A.cors.preflight", "OPTIONS", G, ep="OPTIONS /ai/generate-image (CORS)",
             headers={"origin": "https://app.example.test", "access-control-request-method": "POST",
                      "access-control-request-headers": "authorization,content-type"}))
    add(Step("A.cors.simple", "POST", G, ep="POST /ai/generate-image (CORS 简单请求)", json=img(),
             headers={"origin": "https://app.example.test"}))
    add(Step("A.cors.get-models", "GET", "/v1/models", ep="GET /v1/models", headers={"origin": "https://app.example.test"}))

    # ---- 鉴权 ----
    gen("noauth", img(), auth=None)
    add(Step("A.gen.badkey", "POST", G, bearer="nai-wrong-key-000000000000000000", json=img(), ep=f"POST {G}"))
    add(Step("A.gen.bearer-no-token", "POST", G, headers={"authorization": "Bearer"}, json=img(), ep=f"POST {G}"))
    add(Step("A.gen.basic-scheme", "POST", G, headers={"authorization": "Basic abc"}, json=img(), ep=f"POST {G}"))
    add(Step("A.gen.lower-bearer", "POST", G, headers={"authorization": "bearer ${tok.member}"}, json=img(), ep=f"POST {G}"))
    gen("disabled", img(), auth="disabled")
    gen("expired", img(), auth="expired")
    gen("paused", img(), auth="paused")
    # 同一出口反复提交无效 Key -> 防爆破封锁（用 XFF 隔离，避免波及其它用例）
    for i in range(1, 23):
        add(Step(f"A.auth.brute.{i:02d}", "POST", G, bearer=f"nai-wrong-{i:02d}", json=img(), headers=XFF("203.0.113.50"),
                 ep="POST /ai/generate-image (无效 Key 防爆破)", norm=MINUTES, settle=0.0))
    add(Step("A.auth.blocked-valid", "GET", "/v1/me", auth="member", headers=XFF("203.0.113.50"), ep="GET /v1/me (被封 IP 持有效 Key)"))
    add(Step("A.auth.blocked-nokey", "POST", G, json=img(), headers=XFF("203.0.113.50"), norm=MINUTES,
             ep="POST /ai/generate-image (无效 Key 防爆破)"))

    # ---- 出图：成功路径与上游请求 ----
    gen("ok-v45-full", img())
    gen("ok-v45-curated", img("nai-diffusion-4-5-curated"))
    gen("ok-v4-full", img("nai-diffusion-4-full"))
    gen("ok-v3", img("nai-diffusion-3", sm=True, sm_dyn=False, noise_schedule="native"))
    gen("ok-model-case", img(" NAI-Diffusion-4-5-Full "), note="模型名会被小写去空白后回写")
    gen("ok-rich-params", img(qualityToggle=True, ucPreset=0, params_version=3, legacy=False, add_original_image=True,
                              cfg_rescale=0.0, noise_schedule="karras", skip_cfg_above_sigma=None, dynamic_thresholding=False,
                              prefer_brownian=True, autoSmea=False, controlnet_strength=1.0, deliberate_euler_ancestral_bug=False))
    centers = [{"x": 0.5, "y": 0.5}]
    v4p = {"caption": {"base_caption": "1girl, solo", "char_captions": [{"char_caption": "girl, red hair", "centers": centers}]},
           "use_coords": False, "use_order": True, "legacy_uc": False}
    v4n = {"caption": {"base_caption": "lowres", "char_captions": [{"char_caption": "bad hands", "centers": centers}]},
           "legacy_uc": False}
    gen("ok-with-v4-prompt", img(v4_prompt=v4p, v4_negative_prompt=v4n), note="已带结构化提示词：fill_v4_prompt 不应再改")
    gen("ok-characterprompts", img(characterPrompts=[{"prompt": "girl, red hair", "uc": "bad", "center": {"x": 0.5, "y": 0.5},
                                                      "enabled": True}], use_coords=True))
    gen("ok-name-digit-weight", img(inp="1girl, bm94199::, solo"), note="bm94199:: -> bm94199 ::")
    gen("ok-webui-weight", img(inp="1girl, (red hair:1.2), solo, ::", negative_prompt="(lowres:0.8), , ,bad"),
        note="WebUI 语法转译 + 孤立 :: 清理")
    gen("ok-unicode-prompt", img(inp="一个女孩，红色头发，✨ é\U0001F984"), note="非 ASCII：上游请求体转义写法")
    gen("ok-n2", img(n=2))
    gen("ok-seed-neg1", img(seed=-1), note="seed=-1 由网关随机化", up_rules=[(r"parameters\.seed$", "ignore")])
    gen("ok-seed-str", img(seed="abc"), up_rules=[(r"parameters\.seed$", "ignore")])
    gen("ok-size-snapped", img(width=1024, height=1024))
    gen("ok-clamp-steps", img(steps=50), note="免费 Key 步数被钳到 28")
    gen("ok-clamp-size", img(width=2048, height=2048), note="免费 Key 尺寸被钳到 100 万像素内")
    gen("ok-float-int", img(width=832.0, height=1216.0, steps=28.0), note="整数值浮点被转成整数")
    gen("ok-extra-fields", {**img(), "unknown_top": 1, "parameters_extra": {}}, note="未知顶层字段原样转发")
    gen("ok-admin", img("nai-diffusion-5-full"), auth="admin")
    gen("ok-testkey", img(), auth="testkey")
    # ---- 出图：参数 / 模型 / 权限拒绝 ----
    gen("unknown-model", img("not-a-model"))
    gen("model-missing", img(model=DEL))
    gen("model-number", {**img(), "model": 5})
    gen("model-empty", img(""))
    gen("params-missing", {"input": "x", "model": "nai-diffusion-4-5-full"})
    gen("params-list", {"input": "x", "model": "nai-diffusion-4-5-full", "parameters": [1]})
    add(Step("A.gen.body-bad-json", "POST", G, auth="member", body='{"input": "x",', ctype="application/json", ep=f"POST {G}"))
    add(Step("A.gen.body-list", "POST", G, auth="member", body="[1,2]", ctype="application/json", ep=f"POST {G}"))
    add(Step("A.gen.body-empty", "POST", G, auth="member", body="", ctype="application/json", ep=f"POST {G}"))
    add(Step("A.gen.body-text-plain", "POST", G, auth="member", body='{"a":1}', ctype="text/plain", ep=f"POST {G}"))
    add(Step("A.gen.body-nan", "POST", G, auth="member", ctype="application/json", ep=f"POST {G}",
             body='{"input":"x","model":"nai-diffusion-4-5-full","parameters":{"width":832,"height":1216,"steps":28,"scale":NaN}}',
             note="Python json 接受 NaN"))
    add(Step("A.gen.body-dup-keys", "POST", G, auth="member", ctype="application/json", ep=f"POST {G}",
             body='{"input":"a","input":"b","model":"nai-diffusion-4-5-full","parameters":{"width":832,"height":1216,"steps":28,"n_samples":1,"seed":1}}',
             note="重复键：后者覆盖"))
    gen("width-not-64", img(width=833))
    gen("height-not-64", img(height=1000))
    gen("width-str", img(width="832"))
    gen("width-neg", img(width=-64))
    gen("width-huge", img(width=70000))
    gen("steps-bool", img(steps=True))
    gen("n-zero", img(n=0))
    gen("n-str", img(n="2"))
    gen("n-float", img(n=2.5))
    gen("n-huge", img(n=65536))
    gen("n-four", img(n=4), note="多张：免费 Key 的 Anlas 权限")
    gen("scale-big", img(scale=51))
    gen("scale-str", img(scale="5"))
    gen("sampler-ddim-v4", img(sampler="ddim"))
    gen("sampler-bad-chars", img(sampler="k euler!"))
    gen("sampler-nonstr", img(sampler=5))
    gen("noise-schedule-long", img(noise_schedule="x" * 40))
    gen("prompt-too-long", img(inp="a" * 51300))
    gen("prompt-weight-huge", img(inp="1girl, artist94199::, solo"), note="异常权重 94199::")
    gen("prompt-weight-100", img(inp="1girl, 101::red hair::"))
    gen("seven-characters", img(characterPrompts=[{"prompt": f"c{i}", "uc": "", "center": {"x": 0.5, "y": 0.5}, "enabled": True}
                                                  for i in range(7)]))
    gen("top-level-image", {**img(), "image": b64(SMALL_PNG)})
    gen("infill-no-mask", img(action="infill", image=b64(SMALL_PNG)), auth="anlas")
    gen("img2img-member", img(action="img2img", image=b64(SMALL_PNG), strength=0.5, noise=0.0), note="无 img2img 权限")
    gen("img2img-anlas", img(action="img2img", image=b64(SMALL_PNG), strength=0.5, noise=0.0, sm=True, sm_dyn=True),
        auth="anlas", note="SMEA 被置 false")
    gen("img2img-admin", img(action="img2img", image=b64(SMALL_PNG), strength=0.7, noise=0.1), auth="admin")
    gen("infill-mask", img(action="infill", image=b64(SMALL_PNG), mask=b64(SMALL_PNG), add_original_image=False), auth="admin")
    gen("v5-legacy-scope", img("nai-diffusion-5-full"), note="V4.5 范围的 Key 用 V5")
    gen("v5-ok", img("nai-diffusion-5-full"), auth="v5", note="V5 免费额度")
    gen("v5-medium", img("nai-diffusion-5-medium"), auth="v5")
    gen("v5-big", img("nai-diffusion-5-full", width=1472, height=1472, steps=40), auth="v5")
    gen("v5-anlas", img("nai-diffusion-5-full", width=1472, height=1472, steps=40), auth="anlas")
    gen("anlas-big-v45", img(width=1472, height=1472, steps=40, n_samples=2), auth="anlas")
    gen("anlas-v4-curated-multi", img("nai-diffusion-4-5-curated", n=3), auth="anlas")
    gen("vibe-bad-cache-key", img(reference_image_multiple_cached=[{"cache_secret_key": "xyz", "data": b64(TINY_PNG)}],
                                 reference_strength_multiple=[0.6], reference_information_extracted_multiple=[1.0]), auth="anlas")
    gen("vibe-limited-feature", img(reference_image_multiple=[b64(TINY_PNG)], reference_strength_multiple=[0.6]), auth="limited")
    gen("vibe-both-arrays", img(reference_image_multiple=[b64(TINY_PNG)], reference_image_multiple_cached=[
        {"cache_secret_key": "a" * 64, "data": b64(TINY_PNG)}]), auth="anlas")
    gen("vibe-v3-raw", img("nai-diffusion-3", reference_image_multiple=[b64(SMALL_PNG)], reference_strength_multiple=[0.6],
                           reference_information_extracted_multiple=[1.0]), auth="anlas")
    gen("precise-ref-bad", img(director_reference_images_cached=[{"cache_secret_key": "b" * 64, "data": b64(TINY_PNG)}]), auth="anlas")
    gen("old-reference-field", img(reference_image=b64(TINY_PNG)), auth="anlas")
    # ---- 出图：功能开关 / 额度 / 限速 ----
    gen("limited-image-ok", img(), auth="limited", note="只开了文生图的 Key 仍可出图")
    gen("quota-legacy-exhausted", img(), auth="exhausted")
    gen("quota-v5-exhausted", img("nai-diffusion-5-full"), auth="exhausted")
    gen("quota-paced", img(), auth="paced", norm=MINUTES)
    gen("quota-admin-bypass", img(), auth="admin")
    add(Step("A.rpm.first", "POST", G, auth="rpm1", body="{bad", ctype="application/json", ep="POST /ai/generate-image (RPM)"))
    add(Step("A.rpm.second", "POST", G, auth="rpm1", body="{bad", ctype="application/json", ep="POST /ai/generate-image (RPM)"))
    # ---- 出图：路径别名 ----
    gen("alias-nai", img(), path="/nai/ai/generate-image")
    gen("alias-root", img(), path="/", ep="POST /")
    gen("alias-v1", img(), path="/v1/ai/generate-image", ep="POST /v1/ai/generate-image (去 /v1)")
    gen("alias-v1-nai", img(), path="/v1/nai/ai/generate-image", ep="POST /v1/nai/ai/generate-image (不在别名里)")
    add(Step("A.gen.multipart", "POST", G, auth="member", ep="POST /ai/generate-image (multipart)",
             ctype="multipart/form-data; boundary=parityboundary",
             body=("--parityboundary\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n"
                   + json.dumps(img(), separators=(",", ":")) + "\r\n--parityboundary--\r\n")))

    # ---- 流式出图 ----
    S_ = "/ai/generate-image-stream"

    def stream(sid: str, payload: Any, auth: str = "member", path: str = S_, **kw: Any) -> None:
        add(Step(f"A.stream.{sid}", "POST", path, auth=auth, json=payload, ep=kw.pop("ep", f"POST {path}"), **kw))

    stream("default", img())
    stream("sse", img(stream="sse"))
    stream("msgpack", img(stream="msgpack"))
    stream("msgpack-n2", img(n=2, stream="msgpack"))
    stream("bool-true", img(stream=True))
    stream("bool-false", img(stream=False))
    stream("alias-token", img(stream="MessagePack"))
    stream("bogus", img(stream="ws"))
    stream("noauth", img(), auth=None)
    stream("alias-nai", img(), path="/nai/ai/generate-image-stream")
    stream("unknown-model", img("not-a-model"))
    stream("v5", img("nai-diffusion-5-full", stream="sse"), auth="v5")
    stream("quota-exhausted", img(), auth="exhausted")
    stream("quota-paced", img(), auth="paced", norm=MINUTES)
    stream("v5-legacy-scope", img("nai-diffusion-5-full"))

    # ---- Vibe 编码 ----
    E = "/ai/encode-vibe"
    enc = lambda **k: {"image": b64(SMALL_PNG), "model": "nai-diffusion-4-5-full", "informationExtracted": 0.7, **k}
    for sid, body, auth in [
            ("ok", enc(), "anlas"), ("ok-snake", {"image": b64(SMALL_PNG), "model": "nai-diffusion-4-5-full",
                                                  "information_extracted": 0.5}, "anlas"),
            ("both-fields-equal", {**enc(), "information_extracted": 0.7}, "anlas"),
            ("both-fields-differ", {**enc(), "information_extracted": 0.1}, "anlas"),
            ("v5-model", enc(model="nai-diffusion-5-full"), "anlas"), ("v3-model", enc(model="nai-diffusion-3"), "anlas"),
            ("bad-image", enc(image="not base64!!"), "anlas"), ("data-url", enc(image="data:image/png;base64," + b64(SMALL_PNG)), "anlas"),
            ("info-high", enc(informationExtracted=1.5), "anlas"), ("info-str", enc(informationExtracted="0.5"), "anlas"),
            ("no-anlas", enc(), "member"), ("limited", enc(), "limited"), ("admin", enc(), "admin"),
            ("noauth", enc(), None), ("exhausted", enc(), "exhausted")]:
        add(Step(f"A.vibe.{sid}", "POST", E, auth=auth, json=body, ep=f"POST {E}"))
    add(Step("A.vibe.alias-nai", "POST", "/nai/ai/encode-vibe", auth="anlas", json=enc(), ep="POST /nai/ai/encode-vibe"))

    # ---- 放大 / 导演工具 ----
    for sid, path, body, auth in [
            ("upscale-ok", "/ai/upscale", {"image": b64(BIG_PNG), "width": 1024, "height": 1024, "scale": 2}, "anlas"),
            ("upscale-member", "/ai/upscale", {"image": b64(BIG_PNG)}, "member"),
            ("upscale-limited", "/ai/upscale", {"image": b64(BIG_PNG)}, "limited"),
            ("upscale-bad-image", "/ai/upscale", {"image": "!!"}, "anlas"),
            ("upscale-tiny", "/ai/upscale", {"image": b64(TINY_PNG)}, "anlas"),
            ("upscale-bad-scale", "/ai/upscale", {"image": b64(BIG_PNG), "scale": 4}, "anlas"),
            ("upscale-wrong-dims", "/ai/upscale", {"image": b64(BIG_PNG), "width": 100, "height": 100}, "anlas"),
            ("upscale-noauth", "/ai/upscale", {"image": b64(BIG_PNG)}, None),
            ("upscale-admin", "/ai/upscale", {"image": b64(BIG_PNG)}, "admin"),
            ("augment-lineart", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "lineart", "width": 1024, "height": 1024}, "anlas"),
            ("augment-colorize", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "colorize", "prompt": "red", "defry": 3}, "anlas"),
            ("augment-bgremoval", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "bg-removal"}, "anlas"),
            ("augment-bad-tool", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "nope"}, "anlas"),
            ("augment-bad-defry", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "emotion", "defry": 9}, "anlas"),
            ("augment-member", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "lineart"}, "member"),
            ("augment-noauth", "/ai/augment-image", {"image": b64(BIG_PNG), "req_type": "lineart"}, None)]:
        add(Step(f"A.tool.{sid}", "POST", path, auth=auth, json=body, ep=f"POST {path}", settle=0.15))
    add(Step("A.tool.augment-small-resize", "POST", "/ai/augment-image", auth="anlas", ep="POST /ai/augment-image (小图先放大)",
             json={"image": b64(SMALL_PNG), "req_type": "sketch"}, up_rules=[(r"body\.image$", "ignore")],
             note="先放大到 ~100 万像素：两边 PNG 编码器不同，image 内容不比，宽高要一致"))
    add(Step("A.tool.alias-nai", "POST", "/nai/ai/upscale", auth="anlas", json={"image": b64(BIG_PNG)}, ep="POST /nai/ai/upscale"))

    # ---- 标签补全 ----
    T = "/ai/generate-image/suggest-tags"
    add(Step("A.tags.get", "GET", T + "?prompt=1girl&model=nai-diffusion-4-5-full", auth="member", ep=f"GET {T}"))
    add(Step("A.tags.get-encoded", "GET", T + "?prompt=red%20hair%2C+blue%26eyes%E2%9C%A8&model=nai-diffusion-3", auth="member", ep=f"GET {T}"))
    add(Step("A.tags.get-empty", "GET", T, auth="member", ep=f"GET {T}"))
    add(Step("A.tags.post", "POST", T, auth="member", json={"prompt": "1girl", "model": "nai-diffusion-4-5-full"}, ep=f"POST {T}"))
    add(Step("A.tags.post-nonstring", "POST", T, auth="member", json={"prompt": 5, "model": None}, ep=f"POST {T}"))
    add(Step("A.tags.post-bad-json", "POST", T, auth="member", body="{x", ctype="application/json", ep=f"POST {T}"))
    add(Step("A.tags.noauth", "GET", T + "?prompt=a", ep=f"GET {T}"))
    add(Step("A.tags.limited", "GET", T + "?prompt=a", auth="limited", ep=f"GET {T}"))
    add(Step("A.tags.disabled", "GET", T + "?prompt=a", auth="disabled", ep=f"GET {T}"))
    add(Step("A.tags.alias-nai", "GET", "/nai" + T + "?prompt=a", auth="member", ep=f"GET /nai{T}"))
    add(Step("A.tags.upstream-500", "GET", T + "?prompt=boom", auth="member", ep=f"GET {T}",
             script=[("/ai/generate-image/suggest-tags", Reply.json(500, {"message": "internal"}))]))

    # ---- 文本 ----
    def text(sid: str, path: str, payload: Any, auth: str = "member", **kw: Any) -> None:
        add(Step(f"A.text.{sid}", "POST", path, auth=auth, json=payload, ep=kw.pop("ep", f"POST {path}"), **kw))

    tbody = lambda model="kayra-v1", **p: {"input": "Once upon a time", "model": model,
                                           "parameters": {"max_length": 8, "min_length": 1, "temperature": 0.9, "use_string": True, **p}}
    for path in ("/ai/generate", "/ai/generate-stream"):
        k = path.rsplit("/", 1)[1]
        text(f"{k}-kayra", path, tbody())
        text(f"{k}-erato", path, tbody("llama-3-erato-v1"))
        text(f"{k}-retired", path, tbody("clio-v1"))
        text(f"{k}-unknown", path, tbody("some-other"))
        text(f"{k}-unknown-admin", path, tbody("some-other"), auth="admin")
        text(f"{k}-clamp", path, tbody(max_length=900, temperature=7))
        text(f"{k}-long-input", path, {**tbody(), "input": "x" * 25000})
        text(f"{k}-no-input", path, {"model": "kayra-v1", "parameters": {}})
        text(f"{k}-bad-json", path, None, body="{x", ctype="application/json")
        text(f"{k}-noauth", path, tbody(), auth=None)
        text(f"{k}-limited", path, tbody(), auth="limited")
        text(f"{k}-exhausted", path, tbody(), auth="exhausted")
        text(f"{k}-alias-nai", "/nai" + path, tbody(), ep=f"POST /nai{path}")
    text("stream-textlimit", "/ai/generate-stream", tbody(), auth="textlimit", note="每日上限中途截断")
    text("generate-upstream-500", "/ai/generate", tbody(), script=[("/ai/generate", Reply.json(500, {"message": "x"}))])
    text("voice-member", "/ai/generate-voice", {"text": "hi", "voice": -1, "seed": "x", "opus": False, "version": "v2"})
    text("voice-admin", "/ai/generate-voice", {"text": "hi", "voice": -1, "seed": "x", "opus": False, "version": "v2"}, auth="admin")
    text("voice-limited", "/ai/generate-voice", {"text": "hi"}, auth="limited")
    chat = lambda **k: {"model": "kayra-v1", "messages": [{"role": "user", "content": "你好"}], "max_tokens": 8, **k}
    C_ = "/v1/chat/completions"
    ctime = [(r'"created":\s*\d+', '"created":0')]
    text("chat", C_, chat(), rules=[(r"\.created$", "time")])
    text("chat-stream", C_, chat(stream=True), norm=ctime)
    text("chat-noauth", C_, chat(), auth=None)
    text("chat-retired", C_, chat(model="clio-v1"))
    text("chat-unknown-model", C_, chat(model="gpt-4"), rules=[(r"\.created$", "time")])
    text("chat-no-messages", C_, {"model": "kayra-v1"}, rules=[(r"\.created$", "time")])
    text("chat-max-tokens-str", C_, chat(max_tokens="abc"))
    text("chat-exhausted", C_, chat(), auth="exhausted")
    text("chat-limited", C_, chat(), auth="limited")
    text("chat-long", C_, {"messages": [{"role": "user", "content": "x" * 25000}]})
    text("chat-stream-textlimit", C_, chat(stream=True), auth="textlimit", norm=ctime)
    add(Step("A.text.models", "GET", "/v1/models", ep="GET /v1/models"))
    add(Step("A.text.models-root", "GET", "/v1", ep="GET /v1"))
    for alias in ("member", "v5", "anlas", "admin", "limited", "exhausted", "paced", "testkey"):
        add(Step(f"A.me.{alias}", "GET", "/v1/me", auth=alias, ep="GET /v1/me"))
    add(Step("A.me.noauth", "GET", "/v1/me", ep="GET /v1/me"))
    add(Step("A.me.disabled", "GET", "/v1/me", auth="disabled", ep="GET /v1/me"))
    add(Step("A.me.paused", "GET", "/v1/me", auth="paused", ep="GET /v1/me"))
    add(Step("A.me.expired", "GET", "/v1/me", auth="expired", ep="GET /v1/me"))

    # ---- /user/* 兼容接口（原生 NovelAI 客户端会探）----
    for leaf in ("subscription", "data", "priority", "keystore", "information"):
        for prefix in ("/user/", "/ai/user/", "/v1/user/", "/v1/ai/user/"):
            for alias in (("member", "v5") if prefix == "/user/" else ("member",)):
                add(Step(f"A.user.{leaf}{prefix.replace('/', '-')}{alias}", "GET", prefix + leaf, auth=alias,
                         ep=f"GET {prefix}{leaf}"))
        add(Step(f"A.user.{leaf}-noauth", "GET", "/user/" + leaf, ep=f"GET /user/{leaf}"))
    add(Step("A.user.subscription-admin", "GET", "/user/subscription", auth="admin", ep="GET /user/subscription"))
    add(Step("A.user.subscription-exhausted", "GET", "/user/subscription", auth="exhausted", ep="GET /user/subscription"))
    add(Step("A.user.subscription-disabled", "GET", "/user/subscription", auth="disabled", ep="GET /user/subscription"))

    # ---- 上游故障注入（放在最后：5xx / 429 会影响账号健康度与全站冷却）----
    gp = "/ai/generate-image"
    add(Step("A.up5xx.1", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 500)",
             script=[(gp, Reply.json(500, {"statusCode": 500, "message": "Internal Server Error"}))], settle=0.3))
    add(Step("A.up5xx.2", "POST", G, auth="v5", json=img(), ep="POST /ai/generate-image (上游 500)",
             script=[(gp, Reply(500, {"content-type": "text/plain"}, b"boom"))], settle=0.3))
    add(Step("A.up4xx.model403", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 403 模型不存在)",
             script=[(gp, Reply.json(403, {"statusCode": 403, "message": "model 'x' doesn't exist"}))], settle=0.3))
    add(Step("A.up4xx.400", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 400)",
             script=[(gp, Reply.json(400, {"statusCode": 400, "message": "Invalid parameters"}))], settle=0.3))
    add(Step("A.up4xx.402", "POST", G, auth="anlas", json=img(), ep="POST /ai/generate-image (上游 402)",
             script=[(gp, Reply.json(402, {"statusCode": 402, "message": "insufficient"}))], settle=0.3))
    add(Step("A.upbad.garbage-200", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 200 非 zip)",
             script=[(gp, Reply(200, {"content-type": "binary/octet-stream"}, b"not-a-zip"))], settle=0.3))
    add(Step("A.upbad.empty-200", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 200 空体)",
             script=[(gp, Reply(200, {"content-type": "application/json"}, b""))], settle=0.3))
    add(Step("A.upstream.stream-error-event", "POST", S_, auth="member", json=img(stream="sse"),
             ep="POST /ai/generate-image-stream (上游错误事件)", settle=0.3,
             script=[("/ai/generate-image-stream", Reply(200, {}, chunks=[(0.01, sse_event("intermediate", {
                 "event_type": "intermediate", "samp_ix": 0, "step_ix": 0, "image": b64(TINY_PNG)})),
                 (0.01, sse_event("error", {"event_type": "error", "message": "boom"}))], content_type="text/event-stream"))]))
    add(Step("A.upstream.stream-truncated", "POST", S_, auth="member", json=img(n=2, stream="msgpack"),
             ep="POST /ai/generate-image-stream (上游提前结束)", settle=0.3,
             script=[("/ai/generate-image-stream", Reply(200, {}, chunks=image_stream(1, "msgpack"),
                                                       content_type="application/x-msgpack"))]))
    add(Step("A.upstream.stream-500", "POST", S_, auth="member", json=img(stream="sse"), ep="POST /ai/generate-image-stream (上游 500)",
             settle=0.3, script=[("/ai/generate-image-stream", Reply.json(500, {"message": "x"}))]))
    add(Step("A.upstream.tool-500", "POST", "/ai/upscale", auth="anlas", json={"image": b64(BIG_PNG)}, ep="POST /ai/upscale (上游 500)",
             settle=0.3, script=[("/ai/upscale", Reply.json(500, {"message": "x"}))]))
    add(Step("A.upstream.vibe-json", "POST", E, auth="anlas", json=enc(), ep="POST /ai/encode-vibe (上游返回 JSON)", settle=0.3,
             script=[(E, Reply.json(200, {"oops": 1}))]))
    # 429：上游限流 -> 全站图片冷却（env IMAGE_429_COOLDOWN_SECONDS=2，Retry-After=3 -> 约 3 秒）
    add(Step("A.up429.1", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (上游 429)", settle=0.3,
             script=[(gp, Reply(429, {"retry-after": "3", "content-type": "application/json"},
                                b'{"statusCode":429,"message":"Too many requests"}'))]))
    add(Step("A.up429.cooldown-gen", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (冷却中)", norm=MINUTES))
    add(Step("A.up429.cooldown-stream", "POST", S_, auth="v5", json=img(), ep="POST /ai/generate-image-stream (冷却中)", norm=MINUTES))
    add(Step("A.up429.cooldown-tags", "GET", T + "?prompt=a", auth="member", ep="GET /ai/generate-image/suggest-tags (冷却中)", norm=MINUTES))
    add(Step("A.up429.cooldown-vibe", "POST", E, auth="anlas", json=enc(), ep="POST /ai/encode-vibe (冷却中)", norm=MINUTES))
    add(Step("A.up429.cooldown-text", "POST", "/ai/generate", auth="member", json=tbody(), ep="POST /ai/generate (图片冷却中，文本不受影响)"))
    add(Step("A.up429.queue-status", "GET", "/queue-status", ep="GET /queue-status", rules=[(r"image_cooldown_remaining$", "time")]))
    add(Step("A.up429.wait", fn=lambda w: time.sleep(4.0)))
    add(Step("A.up429.recovered", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (冷却结束)"))
    # 账号保护：改 site_settings 后重启两个网关再试（直接写库 + 重启，不依赖任何一方的内存缓存）
    hour_now = dt.datetime.now(ZoneInfo(TZ_NAME)).hour
    phases = [
        ("hourly", {"guard_account_hourly_cap": "5"}),
        ("quiet", {"guard_account_hourly_cap": "0", "guard_quiet_start": str((hour_now - 1) % 24),
                   "guard_quiet_end": str((hour_now + 2) % 24), "guard_quiet_hourly_cap": "0"}),
        ("quiet-capped", {"guard_quiet_hourly_cap": "5"}),
        ("daily", {"guard_quiet_start": "0", "guard_quiet_end": "0", "guard_account_daily_cap": "3"}),
        ("normal", {"guard_account_daily_cap": "1000", "guard_account_hourly_cap": "0", "guard_quiet_start": "0",
                    "guard_quiet_end": "0", "guard_quiet_hourly_cap": "20"}),
    ]
    for name, settings in phases:
        add(Step(f"A.guard.{name}.restart", fn=site_edit(settings)))
        if name == "normal":
            break
        for sid, path, body, auth in [("gen", G, img(), "member"), ("stream", S_, img(stream="sse"), "v5"),
                                      ("vibe", E, enc(), "anlas"), ("upscale", "/ai/upscale", {"image": b64(BIG_PNG)}, "anlas")]:
            add(Step(f"A.guard.{name}.{sid}", "POST", path, auth=auth, json=body, ep=f"POST {path} (账号保护：{name})", norm=MINUTES))
        add(Step(f"A.guard.{name}.admin", "POST", G, auth="admin", json=img(), ep=f"POST {G} (账号保护：{name}，站长号)", norm=MINUTES))
        add(Step(f"A.guard.{name}.tags", "GET", T + "?prompt=a", auth="member", ep=f"GET {T} (账号保护：{name})"))
        add(Step(f"A.guard.{name}.text", "POST", "/ai/generate", auth="member", json=tbody(), ep=f"POST /ai/generate (账号保护：{name})"))
    # D1：Retry-After 是 5000 位数字 -> 旧版解析抛异常（500、不冷却），新版按默认 20 秒冷却。
    # 这两条本来就会让两边的记账 / 冷却分叉，所以前后各存一次库，跑完整份库回滚，免得污染后面的语料。
    add(Step("A.d1.snapshot", fn=lambda w: w.restart(hook=snapshot_dbs)))
    add(Step("A.d1.huge-retry-after", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (Retry-After 超长数字)",
             settle=0.3, script=[(gp, Reply(429, {"retry-after": "9" * 5000, "content-type": "application/json"},
                                           b'{"statusCode":429,"message":"Too many requests"}'))]))
    add(Step("A.d1.after", "POST", G, auth="member", json=img(), ep="POST /ai/generate-image (Retry-After 超长数字之后)"))
    add(Step("A.d1.rollback", fn=lambda w: w.restart(hook=restore_dbs)))
    return A


# ============================================================ 语料 C：公开页 / 成员网页会话 / 机器人桥接 ====
TS_NORM = [(r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?", "<ts>"),
           (r"\d{2}-\d{2} \d{2}:\d{2}", "<md-hm>")]


def _db_files(side: "Side") -> list[Path]:
    return [side.data_dir / f"nai_gate.db{x}" for x in ("", "-wal", "-shm")]


def snapshot_dbs(world: "World") -> None:
    """（网关已停）把两份库备份下来。"""
    for sd in world.sides.values():
        snap = sd.data_dir.parent / f"snap-{sd.name}"
        snap.mkdir(exist_ok=True)
        for f in _db_files(sd):
            if f.exists():
                shutil.copy2(f, snap / f.name)


def restore_dbs(world: "World") -> None:
    """（网关已停）把两份库恢复到备份时的样子：D1 这类“两边本来就会不一样”的用例留下的记账、冷却不要带进后面的语料。"""
    for sd in world.sides.values():
        snap = sd.data_dir.parent / f"snap-{sd.name}"
        for f in _db_files(sd):
            f.unlink(missing_ok=True)
        for f in snap.iterdir():
            shutil.copy2(f, sd.data_dir / f.name)


def site_edit(settings: dict, files: Optional[dict[str, bytes]] = None) -> Callable:
    """停机 -> 直接改两份库里的 site_settings（/ 写数据目录里的文件）-> 再起。"""
    def fn(world: "World") -> None:
        def edit(con: sqlite3.Connection) -> None:
            for k, v in settings.items():
                con.execute("INSERT INTO site_settings(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (k, v))
        world.restart(edit, files)
    return fn


def web_flag_edit(value: str) -> Callable:
    """停机 -> 直接改两份库里的 site_settings -> 再起：不依赖任何一方的内存缓存。"""
    def fn(world: "World") -> None:
        world.restart(lambda con: con.execute(
            "INSERT INTO site_settings(key, value) VALUES('web_login_paused', ?) "
            "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (value,)))
    return fn


def corpus_c() -> list[Step]:
    C: list[Step] = []
    add = C.append
    ck = lambda alias, **kw: {"owl_member": member_cookie(DISCORD_OF[alias], SEED_TOKENS[alias], **kw)}
    cookies = {
        "ok": ck("member"), "v5": ck("v5"),
        "nokey": {"owl_member": member_cookie(NO_KEY_DISCORD, None)},
        "stale": {"owl_member": member_cookie(DISCORD_OF["member"], "an-old-rotated-key")},
        "expired": ck("member", exp=int(time.time()) - 10),
        "badsig": {"owl_member": member_cookie(DISCORD_OF["member"], SEED_TOKENS["member"])[:-6] + "000000"},
        "legacy": {"owl_member": "key:5:123.deadbeef"},
        "disabled": ck("disabled"), "admin": ck("admin"), "garbage": {"owl_member": "xyz"},
        "other-secret": {"owl_member": member_cookie(DISCORD_OF["member"], SEED_TOKENS["member"], secret="another-secret-key-another-secret-key-00")},
    }

    # ---- 静态 / 页面 ----
    add(Step("C.page.healthz", "GET", "/healthz"))
    for sid, path in [("landing", "/"), ("survey", "/survey"), ("admin", "/admin"), ("admin-slash", "/admin/"),
                      ("admin-spa", "/admin/members")]:
        add(Step(f"C.page.{sid}", "GET", path, ep=f"GET {path}"))
    add(Step("C.page.landing-member", "GET", "/", cookies=cookies["ok"], ep="GET / (成员会话)"))
    add(Step("C.page.landing-nokey", "GET", "/", cookies=cookies["nokey"], ep="GET / (成员会话)"))
    add(Step("C.page.landing-stale", "GET", "/", cookies=cookies["stale"], ep="GET / (成员会话)"))
    add(Step("C.page.favicon-svg", "GET", "/favicon.svg"))
    add(Step("C.page.favicon-ico", "GET", "/favicon.ico"))
    add(Step("C.page.terms", "GET", "/terms", html_skip=False))
    add(Step("C.page.privacy", "GET", "/privacy", html_skip=False))
    add(Step("C.page.announcement", "GET", "/announcement", html_skip=False))
    # ---- 状态页 ----
    add(Step("C.status.json", "GET", "/status.json"))
    add(Step("C.status.feed", "GET", "/status/feed.xml", norm=TS_NORM))
    add(Step("C.status.page", "GET", "/status", ep="GET /status"))
    add(Step("C.status.json-head", "HEAD", "/status.json", ep="HEAD /status.json"))
    add(Step("C.status.feed-head", "HEAD", "/status/feed.xml", ep="HEAD /status/feed.xml"))
    add(Step("C.status.page-head", "HEAD", "/status", ep="HEAD /status"))
    # ---- 公开实时数据 ----
    live_rules = [(r"\.(t|next_in)$", "time"), (r"image_cooldown", "time")]
    add(Step("C.live.anon", "GET", "/public/live", rules=live_rules))
    add(Step("C.live.member", "GET", "/public/live", cookies=cookies["ok"], rules=live_rules, ep="GET /public/live (成员会话)"))
    add(Step("C.live.nokey", "GET", "/public/live", cookies=cookies["nokey"], rules=live_rules, ep="GET /public/live (成员会话)"))
    add(Step("C.live.badcookie", "GET", "/public/live", cookies=cookies["badsig"], rules=live_rules, ep="GET /public/live (成员会话)"))
    add(Step("C.status.public", "GET", "/public/status", rules=[(r"image_cooldown", "time")]))
    add(Step("C.queue-status", "GET", "/queue-status", rules=[(r"image_cooldown_remaining$", "time")]))
    add(Step("C.live-me.ok", "GET", "/v1/live/me", auth="member", ep="GET /v1/live/me"))
    add(Step("C.live-me.noauth", "GET", "/v1/live/me", ep="GET /v1/live/me"))
    add(Step("C.live-me.disabled", "GET", "/v1/live/me", auth="disabled", ep="GET /v1/live/me"))
    add(Step("C.live-me.paused", "GET", "/v1/live/me", auth="paused", ep="GET /v1/live/me"))
    # ---- 前端错误上报 ----
    CE = "/public/client-error"
    host = {"host": "${base}"}
    add(Step("C.clienterr.foreign-origin", "POST", CE, json={"page": "landing", "msg": "x"}, headers={"origin": "https://evil.example.test"}))
    add(Step("C.clienterr.ok", "POST", CE, json={"page": "landing", "msg": "TypeError: x is undefined", "src": "/_owl/app.js",
                                                 "line": 3, "stack": "at f (http://127.0.0.1/_owl/app.js:3:1)"}))
    add(Step("C.clienterr.injected", "POST", CE, json={"page": "admin", "msg": "Script error."}))
    add(Step("C.clienterr.bad-json", "POST", CE, body="{x", ctype="application/json"))
    add(Step("C.clienterr.not-dict", "POST", CE, body="[1]", ctype="application/json"))
    add(Step("C.clienterr.owl-path", "POST", CE, json={"page": "landing", "msg": "boom2", "stack": "at h (/_owl/app-abc.js:1:1)"},
             note="堆栈只有 /_owl/ 文件：旧版当“注入脚本”丢掉，新版算本站的错（D20）"))
    add(Step("C.clienterr.other-page", "POST", CE, json={"page": "zzz", "msg": "boom", "stack": "at g (/_owl/x.js:1:1)"}))
    # ---- 问卷 / 捐赠 ----
    MS = "/member/survey"
    add(Step("C.survey.ok", "POST", MS, auth="member", json={"understand": "清楚", "donate": "愿意", "improve": ["更快", "更多额度"],
                                                            "client": "测试客户端", "improve_other": "无"}, ep=f"POST {MS}"))
    add(Step("C.survey.missing", "POST", MS, auth="member", json={"understand": "清楚"}, ep=f"POST {MS}"))
    add(Step("C.survey.bad-improve", "POST", MS, auth="member", json={"understand": "a", "donate": "b", "improve": "x"}, ep=f"POST {MS}"))
    add(Step("C.survey.bad-json", "POST", MS, auth="member", body="{x", ctype="application/json", ep=f"POST {MS}"))
    add(Step("C.survey.not-dict", "POST", MS, auth="member", body="[1]", ctype="application/json", ep=f"POST {MS}"))
    add(Step("C.survey.noauth", "POST", MS, json={}, ep=f"POST {MS}"))
    add(Step("C.survey.disabled", "POST", MS, auth="disabled", json={}, ep=f"POST {MS}"))
    DN = "/public/donate"
    tokn = "pst-synthetic-donate-token-" + "z" * 12
    add(Step("C.donate.ok", "POST", DN, json={"token": tokn, "message": "只用 4.5", "contact": "c@example.test"}, ep=f"POST {DN}"))
    add(Step("C.donate.foreign-origin", "POST", DN, json={"token": tokn, "message": "x"}, headers={"origin": "https://evil.example.test"}, ep=f"POST {DN}"))
    add(Step("C.donate.same-origin", "POST", DN, json={"token": tokn, "message": "同源", "contact": ""}, headers={"origin": "${base}"}, ep=f"POST {DN}"))
    add(Step("C.donate.short-token", "POST", DN, json={"token": "short", "message": "x"}, ep=f"POST {DN}"))
    add(Step("C.donate.no-message", "POST", DN, json={"token": tokn, "message": "  "}, ep=f"POST {DN}"))
    add(Step("C.donate.long-contact", "POST", DN, json={"token": tokn, "message": "x", "contact": "c" * 101}, ep=f"POST {DN}"))
    add(Step("C.donate.missing-field", "POST", DN, json={"token": tokn}, ep=f"POST {DN}"))
    for i in range(1, 4):
        add(Step(f"C.donate.rate.{i}", "POST", DN, json={"token": tokn, "message": f"第{i}次"}, ep=f"POST {DN}"))
    # ---- 成员网页会话 ----
    for name in ("ok", "v5", "nokey", "stale", "expired", "badsig", "legacy", "disabled", "admin", "garbage", "other-secret"):
        add(Step(f"C.me.{name}", "GET", "/public/me", cookies=cookies[name], ep="GET /public/me"))
    add(Step("C.me.anon", "GET", "/public/me", ep="GET /public/me"))
    add(Step("C.me.bearer-not-cookie", "GET", "/public/me", auth="member", ep="GET /public/me"))
    add(Step("C.coupon.noauth", "POST", "/public/me/coupons/1/use", ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.nokey", "POST", "/public/me/coupons/1/use", cookies=cookies["nokey"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.not-mine", "POST", "/public/me/coupons/2/use", cookies=cookies["ok"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.use", "POST", "/public/me/coupons/1/use", cookies=cookies["ok"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.reuse", "POST", "/public/me/coupons/1/use", cookies=cookies["ok"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.missing", "POST", "/public/me/coupons/9999/use", cookies=cookies["ok"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.coupon.bad-id", "POST", "/public/me/coupons/abc/use", cookies=cookies["ok"], ep="POST /public/me/coupons/{id}/use"))
    add(Step("C.me.after-coupon", "GET", "/public/me", cookies=cookies["ok"], ep="GET /public/me"))
    for fmt in ("zip", "epub", "rar"):
        add(Step(f"C.export.{fmt}", "GET", f"/public/my-export?format={fmt}", cookies=cookies["ok"], ep="GET /public/my-export", settle=0.2))
    add(Step("C.export.noauth", "GET", "/public/my-export", ep="GET /public/my-export"))
    add(Step("C.export.nokey", "GET", "/public/my-export", cookies=cookies["nokey"], ep="GET /public/my-export"))
    add(Step("C.logout", "POST", "/logout", cookies=cookies["ok"], ep="POST /logout"))
    # ---- 网页登录（先暂停，再放开）----
    add(Step("C.login.paused", "GET", "/login", ep="GET /login"))
    add(Step("C.login.callback-paused", "GET", "/login/callback?code=abc&state=zzz", ep="GET /login/callback"))
    add(Step("C.login.restart-open", fn=web_flag_edit("0")))
    add(Step("C.login.start", "GET", "/login", ep="GET /login", capture={"login_state": "cookie:owl_login_state"},
             norm=[(r"state=[A-Za-z0-9_\-]+", "state=<s>"), (r"redirect_uri=[^&]+", "redirect_uri=<r>")]))
    add(Step("C.login.callback-nostate", "GET", "/login/callback?code=abc&state=zzz", ep="GET /login/callback"))
    add(Step("C.login.callback-denied", "GET", "/login/callback?error=access_denied", ep="GET /login/callback"))
    add(Step("C.login.callback-badstate", "GET", "/login/callback?code=abc&state=zzz", cookies={"owl_login_state": "other"}, ep="GET /login/callback"))
    add(Step("C.login.callback-good-state", "GET", "/login/callback?code=abc&state=${login_state}", cookies={"owl_login_state": "${login_state}"},
             ep="GET /login/callback", note="换 Token 会请求 Discord：被黑洞拒绝 -> 登录失败重定向"))
    add(Step("C.live.web-login-open", "GET", "/public/status", ep="GET /public/status (网页登录开放)", rules=[(r"image_cooldown", "time")]))
    # ---- 机器人桥接 /self-register/* ----
    SR = "/self-register"
    who = lambda alias="member", **k: {"discord_id": "${did.%s}" % alias, "guild_id": GUILD_ID, **k}
    adm = lambda alias="member", **k: who(alias, actor_id="${did.admin}", **k)

    def bridge(sid: str, leaf: str, payload: Any, bearer: Optional[str] = "${bridge}", **kw: Any) -> None:
        add(Step(f"C.bridge.{sid}", "POST", f"{SR}/{leaf}", bearer=bearer, json=payload, ep=kw.pop("ep", f"POST {SR}/{leaf}"), **kw))

    bridge("info-ok", "info", who())
    bridge("info-noauth", "info", who(), bearer=None)
    bridge("info-badsecret", "info", who(), bearer="wrong-secret")
    bridge("info-wrong-guild", "info", {"discord_id": "${did.member}", "guild_id": "1"})
    bridge("info-nonnumeric", "info", {"discord_id": "abc", "guild_id": GUILD_ID})
    bridge("info-missing-field", "info", {"discord_id": "${did.member}"})
    bridge("quota-ok", "quota", who("v5"))
    bridge("quota-nokey", "quota", {"discord_id": NO_KEY_DISCORD, "guild_id": GUILD_ID})
    bridge("quota-paced", "quota", who("paced"))
    bridge("slots-admin", "slots", adm())
    bridge("slots-nonadmin", "slots", who())
    bridge("ops-open-off", "ops", adm(action="open", value="off"))
    bridge("ops-open-on", "ops", adm(action="open", value="on"))
    bridge("ops-limit", "ops", adm(action="limit", value="40"))
    bridge("ops-limit-bad", "ops", adm(action="limit", value="abc"))
    add(Step("C.bridge.d28.snapshot", fn=lambda w: w.restart(hook=snapshot_dbs)))
    bridge("d28.ops-limit-over", "ops", adm(action="limit", value="1001"), note="名额超过 1000：旧版抛未捕获的 ValueError（500），新版 422 并记操作日志（D28）")
    add(Step("C.bridge.d28.rollback", fn=lambda w: w.restart(hook=restore_dbs)))
    bridge("ops-limit-restore", "ops", adm(action="limit", value="40"))
    bridge("d29.big-body", "info", {**who(), "pad": "x" * (1024 * 1024 + 1024)}, note="请求体超过 1 MiB：旧版没有上限，新版 413（D29）")
    bridge("ops-grant", "ops", adm(action="grant", feature="vibe", value="on", target="${did.limited}"))
    bridge("ops-grant-off", "ops", adm(action="grant", feature="vibe", value="off", target="${did.limited}"))
    bridge("ops-grant-bad-feature", "ops", adm(action="grant", feature="zzz", value="on", target="${did.limited}"))
    bridge("ops-grant-nokey", "ops", adm(action="grant", feature="vibe", value="on", target=NO_KEY_DISCORD))
    bridge("ops-ban", "ops", adm(action="ban", target=f"{SYNTH_DISCORD}92"))
    bridge("ops-unban", "ops", adm(action="unban", target=f"{SYNTH_DISCORD}92"))
    bridge("ops-unban-again", "ops", adm(action="unban", target=f"{SYNTH_DISCORD}92"))
    bridge("ops-unknown", "ops", adm(action="explode"))
    bridge("ops-nonadmin", "ops", who(action="open", value="on"))
    bridge("feedback-q", "feedback/questions", {})
    bridge("feedback-ok", "feedback", {**who("member"), "username": "user01", "answers": {"q1": "不错", "q2": "再快点"}})
    bridge("feedback-nokey", "feedback", {"discord_id": NO_KEY_DISCORD, "guild_id": GUILD_ID, "username": "x", "answers": {"q1": "ok"}})
    bridge("feedback-bad", "feedback", {**who("member"), "answers": {}})
    add(Step("C.bridge.bot-config", "GET", f"{SR}/bot/config", bearer="${bridge}", ep=f"GET {SR}/bot/config"))
    add(Step("C.bridge.bot-config-noauth", "GET", f"{SR}/bot/config", ep=f"GET {SR}/bot/config"))
    bridge("bot-report", "bot/report", {"status": {"online": True, "guilds": 1}, "event": {"type": "like", "user": "user01"}})
    bridge("issue-new", "issue", {"discord_id": f"{SYNTH_DISCORD}93", "guild_id": GUILD_ID, "name": "newcomer", "username": "newcomer",
                                  "global_name": "Newcomer", "avatar": ""}, settle=0.3)
    bridge("issue-existing", "issue", {"discord_id": "${did.member}", "guild_id": GUILD_ID, "name": "user01"}, settle=0.3)
    bridge("issue-wrong-guild", "issue", {"discord_id": f"{SYNTH_DISCORD}94", "guild_id": "1"})
    bridge("issue-invite-bad", "issue", {"discord_id": f"{SYNTH_DISCORD}95", "guild_id": GUILD_ID, "invite_key": "nai-no-such-invite"}, settle=0.3)
    bridge("issue-banned", "issue", {"discord_id": f"{SYNTH_DISCORD}90", "guild_id": GUILD_ID, "name": "banned1"}, settle=0.3)
    bridge("resetkey", "resetkey", who("rename"), rules=[(r"\.key$", "shape")])
    bridge("resetkey-nokey", "resetkey", {"discord_id": NO_KEY_DISCORD, "guild_id": GUILD_ID})
    bridge("revoke-nonadmin", "revoke", who("rename"))
    bridge("revoke", "revoke", adm("rename"))
    bridge("revoke-again", "revoke", adm("rename"))
    # D22：刻意构造的输入，旧版会 500
    bad_utf8 = b"<p>\xff\xfe broken announcement</p>"
    add(Step("C.d22.setup-bad-announcement", fn=site_edit({}, {"announcement.html": bad_utf8})))
    add(Step("C.d22.announcement-bad-utf8", "GET", "/announcement", ep="GET /announcement (文件不是 UTF-8)", html_skip=False))
    add(Step("C.d22.live-bad-announcement", "GET", "/public/live", ep="GET /public/live (公告文件不是 UTF-8)", rules=live_rules))
    add(Step("C.d22.setup-bad-quota-last", fn=site_edit({"quota_algo_last": "[1]"}, {"announcement.html": SEED_ANNOUNCEMENT.encode()})))
    add(Step("C.d22.live-bad-quota-last", "GET", "/public/live", ep="GET /public/live (quota_algo_last 不是对象)", rules=live_rules))
    add(Step("C.d22.clienterr-unparsable-origin", "POST", CE, json={"page": "landing", "msg": "x"},
             headers={"origin": "http://[abc"}, ep="POST /public/client-error (无法解析的 Origin)"))
    add(Step("C.d22.restore", fn=site_edit({"quota_algo_last": "{}"})))
    return C


# ============================================================ 语料 B：后台接口 ====
def corpus_b() -> list[Step]:
    B: list[Step] = []
    add = B.append
    P = "/admin/api"
    ADM = {"nai_gate_admin": "${admin_cookie}"}
    NO = "notify=0"

    def adm(sid: str, method: str, leaf: str, payload: Any = None, ep: str = "", **kw: Any) -> None:
        add(Step(f"B.{sid}", method, f"{P}{leaf}", json=payload, cookies=ADM, ep=ep or f"{method} {P}{leaf.split('?')[0]}", **kw))

    # 前面语料里“旧版 500、新版不是”的有意差异（D1 / D2 / D22）会在旧版多留 Bug 记录：后台从干净的 Bug 列表起步，
    # 免得 /admin/api/errors 把那些已解释的差异再报一遍
    add(Step("B.setup-clean-bugs", fn=lambda w: w.restart(lambda con: con.execute("DELETE FROM error_events WHERE sig NOT LIKE 'seedsig%'"))))
    # ---- 未登录 / 会话 ----
    add(Step("B.unauth.me", "GET", f"{P}/me", ep=f"GET {P}/me"))
    add(Step("B.unauth.keys", "GET", f"{P}/keys", ep=f"GET {P}/keys"))
    add(Step("B.unauth.create", "POST", f"{P}/keys", json={"name": "x"}, ep=f"POST {P}/keys"))
    add(Step("B.unauth.garbage-cookie", "GET", f"{P}/me", cookies={"nai_gate_admin": "garbage"}, ep=f"GET {P}/me"))
    add(Step("B.unauth.expired-cookie", "GET", f"{P}/me", cookies={"nai_gate_admin": admin_cookie(int(time.time()) - 10)}, ep=f"GET {P}/me"))
    add(Step("B.unauth.wrong-password-cookie", "GET", f"{P}/me", cookies={"nai_gate_admin": admin_cookie(password="not-the-password")}, ep=f"GET {P}/me"))
    add(Step("B.unauth.member-cookie", "GET", f"{P}/me", cookies={"nai_gate_admin": member_cookie(DISCORD_OF['member'], SEED_TOKENS['member'])}, ep=f"GET {P}/me"))
    L = f"{P}/login"
    add(Step("B.login.bad-origin", "POST", L, json={"password": ADMIN_PASSWORD}, headers={"origin": "https://evil.example.test"}, ep=f"POST {L}"))
    add(Step("B.login.wrong", "POST", L, json={"password": "nope"}, ep=f"POST {L}"))
    add(Step("B.login.no-password", "POST", L, json={}, ep=f"POST {L}"))
    add(Step("B.login.bad-json", "POST", L, body="{x", ctype="application/json", ep=f"POST {L}"))
    add(Step("B.login.ok", "POST", L, json={"password": ADMIN_PASSWORD}, ep=f"POST {L}",
             capture={"admin_cookie": "cookie:nai_gate_admin"}))
    add(Step("B.login.ok-same-origin", "POST", L, json={"password": ADMIN_PASSWORD}, headers={"origin": "${base}"}, ep=f"POST {L}",
             capture={"admin_cookie2": "cookie:nai_gate_admin"}))
    add(Step("B.login.ok-allowed-origin", "POST", L, json={"password": ADMIN_PASSWORD}, headers={"origin": "http://localhost:5173"},
             ep=f"POST {L}"))
    # 跨实现互认：旧版签发的会话 Cookie 给 Rust 用，反之亦然（两边共用 secret_key 与 ADMIN_PASSWORD）
    add(Step("B.cross.py-cookie-on-rs", "GET", f"{P}/me", cookies={"nai_gate_admin": "${py.admin_cookie}"}, ep=f"GET {P}/me (互认)"))
    add(Step("B.cross.rs-cookie-on-py", "GET", f"{P}/me", cookies={"nai_gate_admin": "${rs.admin_cookie}"}, ep=f"GET {P}/me (互认)"))
    add(Step("B.cross.harness-signed", "GET", f"{P}/me", cookies={"nai_gate_admin": admin_cookie()}, ep=f"GET {P}/me (互认)"))
    add(Step("B.me", "GET", f"{P}/me", cookies=ADM, ep=f"GET {P}/me"))
    add(Step("B.me.head", "HEAD", f"{P}/me", cookies=ADM, ep=f"HEAD {P}/me"))
    add(Step("B.cors.admin-preflight", "OPTIONS", f"{P}/keys", ep=f"OPTIONS {P}/keys (CORS)",
             headers={"origin": "http://localhost:5173", "access-control-request-method": "POST"}))
    # ---- 只读接口 ----
    gets = ["/reconciliation", "/keys", "/bans", "/bans?q=%E5%B0%81", "/logs", "/logs?page=2&per_page=20", "/logs?per_page=50&key_id=1",
            "/logs?feature=image", "/logs?feature=zzz", "/logs?hide_test=true", "/logs?rid=000000a0", "/logs?page=9999",
            "/logs?per_page=7", "/quota-algo", "/autopilot", "/discord-bot", "/modules", "/home/join", "/errors", "/errors?all=true",
            "/perf", "/guard", "/quota-shadow", "/shadow", "/shadow?hours=168", "/actions", "/actions?page=2&actor=system",
            "/actions?actor=admin", "/actions?actor=member", "/actions?action=%E4%BF%AE%E6%94%B9%20Key%7C%E6%B7%BB%E5%8A%A0%E9%BB%91%E5%90%8D%E5%8D%95",
            "/overview", "/settings", "/runtime-limits", "/allowance", "/announcement", "/members", "/audit", "/audit?key_id=1&page=1",
            "/audit?key_id=1,2&per_page=48", "/audit?page=99", "/audit/1/thumb", "/audit/1/image", "/audit/2/image", "/audit/999/image",
            "/audit/export?key_id=1", "/audit/export", "/feedback", "/status", "/ops", "/survey", "/donations", "/donations/count",
            "/incidents", "/keys/1/coupons", "/keys/9999/coupons", "/status-codes", "/status-codes?hours=1", "/status-codes?hours=24",
            "/status-codes?hours=168", "/status-codes?hours=abc", "/status-codes?hours=0"]
    for g in gets:
        adm(("d23." if g.startswith(("/quota-shadow", "/shadow")) else "get.") + re.sub(r"[^a-z0-9]+", "-", g.lower()).strip("-"), "GET", g, ep=f"GET {P}{re.sub(r'/[0-9]+(/|$)', r'/{id}\1', g.split('?')[0])}",
            norm=TS_NORM, settle=0.05, rules=[(r"csrf_token$", "shape")],
            **({"body_mode": "imgdims", "note": "两边缩图编码器不同（Pillow vs image crate）：只比格式与尺寸"} if g.endswith("/thumb") else {}))
    adm("get.feedback-export", "GET", "/feedback/export", ep=f"GET {P}/feedback/export", norm=[(r"\d{8}-\d{4}", "<stamp>")])
    adm("get.logs-export", "GET", "/logs/export?days=7", ep=f"GET {P}/logs/export", norm=[(r"\d{8}-\d{4}", "<stamp>")])
    adm("get.logs-export-throttled", "GET", "/logs/export?days=7", ep=f"GET {P}/logs/export", norm=[(r"\d{8}-\d{4}", "<stamp>")])
    # ---- 写入：Key ----
    adm("keys.create", "POST", "/keys", {"name": "对拍新成员", "daily_images": 50, "daily_v5": 5, "rpm": 20, "daily_text_tokens": 1000,
                                         "expires_days": 10, "features": ["image", "tags"]}, ep=f"POST {P}/keys",
        capture={"new_id": "json:key.id", "new_token": "json:key.token"})
    adm("keys.create-defaults", "POST", "/keys", {}, ep=f"POST {P}/keys")
    adm("keys.create-anlas", "POST", "/keys", {"name": "有 Anlas", "allow_anlas": True, "daily_anlas": 30, "monthly_anlas": 0,
                                               "image_model_scope": "all", "is_test": True, "role": "regular"}, ep=f"POST {P}/keys")
    adm("keys.create-anlas-no-cap", "POST", "/keys", {"name": "x", "allow_anlas": True, "daily_anlas": 0, "monthly_anlas": 0}, ep=f"POST {P}/keys")
    adm("keys.create-neg", "POST", "/keys", {"daily_images": -1}, ep=f"POST {P}/keys")
    adm("keys.create-bad-type", "POST", "/keys", {"rpm": "abc"}, ep=f"POST {P}/keys")
    adm("keys.create-bad-features", "POST", "/keys", {"features": ["zzz"]}, ep=f"POST {P}/keys")
    adm("keys.create-bad-json", "POST", "/keys", None, body="{x", ctype="application/json", ep=f"POST {P}/keys")
    adm("keys.create-bad-origin", "POST", "/keys", {"name": "x"}, ep=f"POST {P}/keys", headers={"origin": "https://evil.example.test"})
    adm("keys.invite", "POST", "/keys/invite", {}, ep=f"POST {P}/keys/invite")
    adm("keys.invite2", "POST", "/keys/invite", {}, ep=f"POST {P}/keys/invite")
    adm("keys.list-after", "GET", "/keys", ep=f"GET {P}/keys")
    pk = "/keys/${new_id}"
    adm("keys.patch", "PATCH", pk, {"name": "改名后", "rpm": 33, "daily_images": 60, "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-quota-manual", "PATCH", pk, {"quota": "manual", "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-quota-auto", "PATCH", pk, {"quota": "auto", "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-bad-quota", "PATCH", pk, {"quota": "weird"}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-bad-rpm", "PATCH", pk, {"rpm": 0}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-bad-role", "PATCH", pk, {"role": "root"}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-role-test", "PATCH", pk, {"role": "test", "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-anlas-mode", "PATCH", pk, {"anlas_mode": "manual", "daily_anlas": 20, "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-scope-all", "PATCH", pk, {"image_model_scope": "all", "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-expires", "PATCH", pk, {"expires_days": 5, "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-features", "PATCH", pk, {"features": ["image"], "notify": False}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.patch-missing", "PATCH", "/keys/99999", {"name": "x"}, ep=f"PATCH {P}/keys/{{id}}")
    adm("keys.role", "POST", pk + "/role", {"role": "invite"}, ep=f"POST {P}/keys/{{id}}/role")
    adm("keys.role-bad", "POST", pk + "/role", {"role": "boss"}, ep=f"POST {P}/keys/{{id}}/role")
    adm("keys.role-missing", "POST", "/keys/99999/role", {"role": "regular"}, ep=f"POST {P}/keys/{{id}}/role")
    adm("keys.reset-quota", "POST", pk + f"/reset-daily-image-quota?{NO}", {}, ep=f"POST {P}/keys/{{id}}/reset-daily-image-quota")
    adm("keys.reset-quota-missing", "POST", f"/keys/99999/reset-daily-image-quota?{NO}", {}, ep=f"POST {P}/keys/{{id}}/reset-daily-image-quota")
    adm("keys.share-clear", "POST", pk + "/share-clear", {}, ep=f"POST {P}/keys/{{id}}/share-clear")
    adm("keys.bind-discord", "POST", pk + "/bind-discord", {"discord_id": f"{SYNTH_DISCORD}60", "username": "bound1"},
        ep=f"POST {P}/keys/{{id}}/bind-discord")
    adm("keys.bind-discord-conflict", "POST", pk + "/bind-discord", {"discord_id": "${did.member}"}, ep=f"POST {P}/keys/{{id}}/bind-discord")
    adm("keys.bind-discord-bad", "POST", pk + "/bind-discord", {"discord_id": "abc"}, ep=f"POST {P}/keys/{{id}}/bind-discord")
    adm("keys.tag-put", "PUT", pk + "/tags", {"tag": "  观察   对象 ", "note": "对拍备注", "by": "测试"}, ep=f"PUT {P}/keys/{{id}}/tags")
    adm("keys.tag-put-empty", "PUT", pk + "/tags", {"tag": "  "}, ep=f"PUT {P}/keys/{{id}}/tags")
    adm("keys.tag-put-missing-key", "PUT", "/keys/99999/tags", {"tag": "x"}, ep=f"PUT {P}/keys/{{id}}/tags")
    adm("keys.tag-delete", "DELETE", pk + "/tags/%E8%A7%82%E5%AF%9F%20%E5%AF%B9%E8%B1%A1", ep=f"DELETE {P}/keys/{{id}}/tags/{{tag}}")
    adm("keys.tag-delete-missing", "DELETE", pk + "/tags/nope", ep=f"DELETE {P}/keys/{{id}}/tags/{{tag}}")
    adm("keys.coupon-give", "POST", pk + "/coupons", {"kind": "reset", "days": 3, "note": "对拍券", "by": "测试"},
        ep=f"POST {P}/keys/{{id}}/coupons")
    adm("keys.coupon-bad-kind", "POST", pk + "/coupons", {"kind": "gold"}, ep=f"POST {P}/keys/{{id}}/coupons")
    adm("keys.coupon-bad-days", "POST", pk + "/coupons", {"days": 99}, ep=f"POST {P}/keys/{{id}}/coupons")
    adm("keys.coupon-missing-key", "POST", "/keys/99999/coupons", {}, ep=f"POST {P}/keys/{{id}}/coupons")
    adm("keys.coupon-list", "GET", pk + "/coupons", ep=f"GET {P}/keys/{{id}}/coupons")
    adm("keys.regenerate", "POST", pk + f"/regenerate?{NO}", {}, ep=f"POST {P}/keys/{{id}}/regenerate")
    adm("keys.regenerate-missing", "POST", f"/keys/99999/regenerate?{NO}", {}, ep=f"POST {P}/keys/{{id}}/regenerate")
    # 默认会私信成员（旧版）/ 默认不私信（Rust，deviations #9）：dmtarget 有 Discord 绑定
    adm("dm.patch-default-notify", "PATCH", "/keys/${kid.dmtarget}", {"daily_images": 77}, ep=f"PATCH {P}/keys/{{id}} (默认私信)")
    adm("dm.regenerate-default-notify", "POST", "/keys/${kid.dmtarget}/regenerate", {}, ep=f"POST {P}/keys/{{id}}/regenerate (默认私信)")
    adm("dm.reset-quota-default-notify", "POST", "/keys/${kid.dmtarget}/reset-daily-image-quota", {},
        ep=f"POST {P}/keys/{{id}}/reset-daily-image-quota (默认私信)")
    adm("keys.delete", "DELETE", pk + f"?{NO}", ep=f"DELETE {P}/keys/{{id}}")
    adm("keys.delete-again", "DELETE", pk + f"?{NO}", ep=f"DELETE {P}/keys/{{id}}")
    # ---- 写入：黑名单 / 设置 / 保护 / 运行期 ----
    adm("bans.create", "POST", "/bans", {"discord_id": f"{SYNTH_DISCORD}61", "reason": "对拍封禁"}, ep=f"POST {P}/bans")
    adm("bans.create-default-reason", "POST", "/bans", {"discord_id": f"{SYNTH_DISCORD}62"}, ep=f"POST {P}/bans")
    adm("bans.create-bad", "POST", "/bans", {"discord_id": "abc"}, ep=f"POST {P}/bans")
    adm("bans.list", "GET", "/bans", ep=f"GET {P}/bans")
    adm("bans.delete", "DELETE", f"/bans/{SYNTH_DISCORD}61", ep=f"DELETE {P}/bans/{{id}}")
    adm("bans.delete-missing", "DELETE", f"/bans/{SYNTH_DISCORD}61", ep=f"DELETE {P}/bans/{{id}}")
    adm("settings.put", "PUT", "/settings", {"global_monthly_anlas": 9000, "global_daily_v5": 120, "member_daily_anlas": 40},
        ep=f"PUT {P}/settings")
    adm("settings.put-bad-threshold", "PUT", "/settings", {"v5_alert_threshold": 500}, ep=f"PUT {P}/settings")
    adm("settings.put-bad-number", "PUT", "/settings", {"global_daily_v5": -5}, ep=f"PUT {P}/settings")
    adm("settings.get-after", "GET", "/settings", ep=f"GET {P}/settings")
    adm("guard.put", "PUT", "/guard", {"account_daily_cap": 900, "key_image_queue": 2, "interval_jitter": 0}, ep=f"PUT {P}/guard")
    adm("guard.put-bad-range", "PUT", "/guard", {"account_hourly_cap": 9999}, ep=f"PUT {P}/guard")
    adm("guard.put-bad-type", "PUT", "/guard", {"account_daily_cap": "x"}, ep=f"PUT {P}/guard")
    adm("guard.put-not-object", "PUT", "/guard", [1], ep=f"PUT {P}/guard")
    adm("guard.restore", "PUT", "/guard", {"account_daily_cap": 1000, "key_image_queue": 1, "interval_jitter": 0}, ep=f"PUT {P}/guard")
    adm("runtime.put", "PUT", "/runtime-limits", {"queue_timeout": 60}, ep=f"PUT {P}/runtime-limits")
    adm("runtime.put-interval", "PUT", "/runtime-limits", {"image_min_interval": 20}, ep=f"PUT {P}/runtime-limits")
    add(Step("B.runtime.queue-status", "GET", "/queue-status", ep="GET /queue-status (运行期限制改过之后)",
             rules=[(r"image_cooldown_remaining$", "time")]))
    adm("runtime.put-interval-restore", "PUT", "/runtime-limits", {"image_min_interval": 15}, ep=f"PUT {P}/runtime-limits")
    adm("runtime.put-bad", "PUT", "/runtime-limits", {"queue_timeout": 1}, ep=f"PUT {P}/runtime-limits")
    adm("runtime.put-unknown", "PUT", "/runtime-limits", {"nope": 1}, ep=f"PUT {P}/runtime-limits")
    adm("runtime.put-empty", "PUT", "/runtime-limits", {}, ep=f"PUT {P}/runtime-limits")
    adm("ops.features", "PUT", "/ops/features", {"flags": {"voice": False, "tags": True}}, ep=f"PUT {P}/ops/features")
    adm("ops.features-bad", "PUT", "/ops/features", {"flags": {"zzz": True}}, ep=f"PUT {P}/ops/features")
    adm("ops.features-bad-type", "PUT", "/ops/features", {"flags": {"voice": "no"}}, ep=f"PUT {P}/ops/features")
    adm("ops.registration", "PUT", "/ops/registration", {"open": True, "max_users": 45, "notify": False}, ep=f"PUT {P}/ops/registration")
    adm("ops.registration-bad-features", "PUT", "/ops/registration", {"features": "image"}, ep=f"PUT {P}/ops/registration")
    adm("ops.audit", "PUT", "/ops/audit", {"prompts": True, "thumbs": False, "notify": False}, ep=f"PUT {P}/ops/audit")
    adm("ops.audit-bad", "PUT", "/ops/audit", {"prompts": "yes", "notify": False}, ep=f"PUT {P}/ops/audit")
    adm("ops.get-after", "GET", "/ops", ep=f"GET {P}/ops")
    adm("ops.reset-all-refused", "POST", "/ops/reset-all-keys", {"confirm": "no"}, ep=f"POST {P}/ops/reset-all-keys")
    adm("home.join", "POST", "/home/join", {"open": False}, ep=f"POST {P}/home/join")
    adm("home.join-restore", "POST", "/home/join", {"open": True}, ep=f"POST {P}/home/join")
    adm("checks.ack", "POST", "/checks/ack", {"key": "v5_oversell", "on": True}, ep=f"POST {P}/checks/ack")
    adm("checks.ack-bad", "POST", "/checks/ack", {"key": "zzz"}, ep=f"POST {P}/checks/ack")
    adm("quota-algo.put", "PUT", "/quota-algo", {"quota_step": 3, "quota_target_avg": 90}, ep=f"PUT {P}/quota-algo",
        rules=[(r"\.last\.", "ignore")], note="立即重算：结果里的时间 / 统计量不比")
    adm("quota-algo.put-bad", "PUT", "/quota-algo", {"quota_step": 0}, ep=f"PUT {P}/quota-algo")
    adm("discord-bot.put", "PUT", "/discord-bot", {"reaction_enabled": True}, ep=f"PUT {P}/discord-bot")
    adm("discord-bot.put-bad", "PUT", "/discord-bot", {"reaction_enabled": "maybe", "zzz": 1}, ep=f"PUT {P}/discord-bot")
    adm("modules.put", "PUT", "/modules/capacity", {"enabled": True}, ep=f"PUT {P}/modules/{{name}}", rules=[(r"modules", "shape")])
    adm("modules.put-unknown", "PUT", "/modules/zzz", {"enabled": True}, ep=f"PUT {P}/modules/{{name}}")
    adm("modules.put-bad", "PUT", "/modules/capacity", {"enabled": "yes"}, ep=f"PUT {P}/modules/{{name}}")
    adm("modules.run", "POST", "/modules/run", {}, ep=f"POST {P}/modules/run", rules=[(r"modules", "shape")])
    # ---- 写入：公告 / Bug / 事件 / 捐赠 ----
    adm("announcement.put", "PUT", "/announcement", {"html": "<p>对拍公告 ✨</p>"}, ep=f"PUT {P}/announcement")
    adm("announcement.get", "GET", "/announcement", ep=f"GET {P}/announcement")
    add(Step("B.announcement.public", "GET", "/announcement", ep="GET /announcement", html_skip=False))
    adm("announcement.clear", "PUT", "/announcement", {"html": ""}, ep=f"PUT {P}/announcement")
    add(Step("B.announcement.public-empty", "GET", "/announcement", ep="GET /announcement"))
    adm("announcement.send-unconfigured", "POST", "/announcement/send", {"text": "hi"}, ep=f"POST {P}/announcement/send")
    adm("announcement.send-empty", "POST", "/announcement/send", {"text": " "}, ep=f"POST {P}/announcement/send")
    adm("alerts.test", "POST", "/alerts/test", {}, ep=f"POST {P}/alerts/test")
    adm("errors.resolve", "POST", "/errors/seedsig00001/resolve", {}, ep=f"POST {P}/errors/{{sig}}/resolve")
    adm("errors.resolve-missing", "POST", "/errors/nope/resolve", {}, ep=f"POST {P}/errors/{{sig}}/resolve")
    adm("errors.resolve-all", "POST", "/errors/resolve-all", {}, ep=f"POST {P}/errors/resolve-all")
    adm("incidents.create", "POST", "/incidents", {"title": "对拍事件", "impact": "minor", "components": ["api"], "status": "investigating"},
        ep=f"POST {P}/incidents", capture={"incident_id": "json:id"})
    adm("incidents.create-bad", "POST", "/incidents", {"title": "", "status": "investigating"}, ep=f"POST {P}/incidents")
    adm("incidents.create-bad-status", "POST", "/incidents", {"title": "x", "status": "weird"}, ep=f"POST {P}/incidents")
    adm("incidents.create-bad-at", "POST", "/incidents", {"title": "x", "status": "resolved", "at": 12345}, ep=f"POST {P}/incidents")
    adm("incidents.update", "POST", "/incidents/${incident_id}/updates", {"status": "resolved", "body": ""}, ep=f"POST {P}/incidents/{{id}}/updates")
    adm("incidents.update-missing", "POST", "/incidents/9999/updates", {"status": "update"}, ep=f"POST {P}/incidents/{{id}}/updates")
    adm("incidents.list", "GET", "/incidents", ep=f"GET {P}/incidents")
    add(Step("B.incidents.public", "GET", "/status.json", ep="GET /status.json (事件之后)"))
    adm("donations.handle", "POST", "/donations/2/handle", {"note": "对拍"}, ep=f"POST {P}/donations/{{id}}/handle")
    adm("donations.handle-again", "POST", "/donations/2/handle", {"note": "对拍"}, ep=f"POST {P}/donations/{{id}}/handle")
    adm("donations.list", "GET", "/donations", ep=f"GET {P}/donations")
    adm("gift", "POST", "/gift", {"discord_id": f"{SYNTH_DISCORD}70", "reason": "对拍奖励", "coupon_days": 3}, ep=f"POST {P}/gift", settle=0.3,
        rules=[(r"\.key$", "shape")])
    adm("gift-bad", "POST", "/gift", {"discord_id": "abc"}, ep=f"POST {P}/gift")
    adm("recon.get", "GET", "/reconciliation", ep=f"GET {P}/reconciliation", capture={"csrf": "json:csrf_token"}, rules=[(r"csrf_token$", "shape")])
    adm("recon.post-no-csrf", "POST", "/reconciliation", {}, ep=f"POST {P}/reconciliation")
    add(Step("B.recon.post", "POST", f"{P}/reconciliation", json={}, cookies=ADM, headers={"x-nai-admin-csrf": "${csrf}"},
             ep=f"POST {P}/reconciliation", settle=0.3, rules=[(r"csrf_token$", "shape")], norm=TS_NORM))
    adm("audit.purge-refused", "POST", "/audit/purge-images", {"confirm": "no"}, ep=f"POST {P}/audit/purge-images")
    adm("audit.purge", "POST", "/audit/purge-images", {"confirm": "清空"}, ep=f"POST {P}/audit/purge-images")
    adm("audit.list-after-purge", "GET", "/audit", ep=f"GET {P}/audit")
    # 上游 Token 管理（只测校验 / 404 路径，不真的增删上游账号）
    adm("tokens.add-bad-format", "POST", "/upstream-tokens", {"token": "abc"}, ep=f"POST {P}/upstream-tokens")
    adm("tokens.replace-missing", "PUT", "/upstream-tokens/nope", {"token": "abc"}, ep=f"PUT {P}/upstream-tokens/{{id}}")
    adm("tokens.v5-limit-bad", "PUT", "/upstream-tokens/nope/v5-limit", {"v5_daily_limit": "x"}, ep=f"PUT {P}/upstream-tokens/{{id}}/v5-limit")
    adm("tokens.v5-limit-missing", "PUT", "/upstream-tokens/nope/v5-limit", {"v5_daily_limit": 5}, ep=f"PUT {P}/upstream-tokens/{{id}}/v5-limit")
    adm("tokens.concurrency-bad", "PUT", "/upstream-tokens/nope/image-concurrency", {"image_concurrency": 9}, ep=f"PUT {P}/upstream-tokens/{{id}}/image-concurrency")
    adm("tokens.enabled-bad", "PUT", "/upstream-tokens/nope/enabled", {"enabled": "yes"}, ep=f"PUT {P}/upstream-tokens/{{id}}/enabled")
    adm("tokens.enabled-missing", "PUT", "/upstream-tokens/nope/enabled", {"enabled": True}, ep=f"PUT {P}/upstream-tokens/{{id}}/enabled")
    adm("tokens.delete-last", "DELETE", "/upstream-tokens/nope", ep=f"DELETE {P}/upstream-tokens/{{id}}")
    # 改密码：只测校验分支，不真的改（改了所有会话立刻失效）
    adm("password.wrong-current", "PUT", "/password", {"current": "nope", "new": "a-very-long-new-password"}, ep=f"PUT {P}/password")
    adm("password.too-short", "PUT", "/password", {"current": ADMIN_PASSWORD, "new": "short"}, ep=f"PUT {P}/password")
    adm("password.same", "PUT", "/password", {"current": ADMIN_PASSWORD, "new": ADMIN_PASSWORD}, ep=f"PUT {P}/password")
    adm("d24.registration-nested-list", "PUT", "/ops/registration", {"features": [["a"]]}, ep=f"PUT {P}/ops/registration")
    # ---- 登出 / 登录限流 ----
    add(Step("B.logout", "POST", f"{P}/logout", cookies=ADM, ep=f"POST {P}/logout"))
    add(Step("B.after-logout-cookie-still-valid", "GET", f"{P}/me", cookies=ADM, ep=f"GET {P}/me (登出后旧 Cookie)"))
    for i in range(1, 8):
        add(Step(f"B.login.brute.{i}", "POST", L, json={"password": f"wrong-{i}"}, headers=XFF("203.0.113.77"),
                 ep=f"POST {L} (限流)", settle=0.0))
    add(Step("B.login.brute-then-correct", "POST", L, json={"password": ADMIN_PASSWORD}, headers=XFF("203.0.113.77"), ep=f"POST {L} (限流)"))
    return B


# @@CORPORA-END@@


if __name__ == "__main__":
    sys.exit(main())
