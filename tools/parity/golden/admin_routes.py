"""app/admin.py「成员 / 记录」接口的路由级 golden 夹具（合成数据，时间冻结）。

做法：在旧 Python 网关上（不走 lifespan：无后台循环、无 Discord）灌一份合成数据，按顺序发一串后台请求，
记录每一步的状态码、响应原文（逐字节）、下载响应头、zip 内容、（假）注册服务收到的调用，以及写操作之后关键表的内容。
Rust 端 `crates/gate-server/tests/admin_golden.rs` 用同一份种子和同一个冻结时间重放同样的请求，逐步比较。

输出（crates/gate-server/tests/fixtures/）：
  admin_routes_seed.sql   —— 种子数据（两边都用这个文件灌库）
  admin_routes.jsonl.gz   —— 步骤与旧版结果；第一行是元信息（冻结的时间、场景名）

用法（仓库根目录，Python 3.12，与线上一致）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/admin_routes.py

约定与取舍：
- 时间冻结在 T（time.time / datetime.now 都打桩），所以时间戳逐位可比；新建 Key 的随机串（nai-……32 位）在响应原文里统一换成 nai-<rand>。
- 动态额度算法用站点设置 quota_auto_enabled=0 关掉（种子里写了这一行）：两边的 `quota_algo.run` 都真的被调用，但立刻返回；
  算法本身的对拍在内核任务里做，这里只比后台接口自己的行为（什么时候调、调之前写了什么）。
- 不比 `api_keys.anlas_auto` / `v5_pinned` 两列：这是 3.0 已删除功能的列，Rust 端不再读写（见 docs/v3-deviations.md）。
- 有注册服务的场景（reg）用旧版真实的 RegistrationService：Discord REST 换成本机假服务（Python 这边是 httpx.MockTransport，
  Rust 那边是本机 HTTP 服务），逐步记录「发给 Discord 的请求」（方法、路径、JSON 体）。私信有总闸门和限速（10 分钟 3 条、
  24 小时 20 条），所以每个有注册服务的步骤之前先让冻结的时钟走 700 秒（记在步骤里的 at 字段，Rust 端照着冻结）；
  要测限速的几步把 advance 设成 0。
- 非 ASCII 字符一律直接写（本文件是 UTF-8），但 Windows 风格转义 \\uXXXX 不要写。
"""
from __future__ import annotations

import asyncio
import datetime as _dt
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
import time
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))
OUT = ROOT / "crates" / "gate-server" / "tests" / "fixtures"

T = 1790040000.0                 # 2026-09-22 09:20 (Asia/Shanghai)，服务日 2026-09-22，重置点 07:00 已过
DAY, YDAY = "2026-09-22", "2026-09-21"
H, D = 3600.0, 86400.0
ADMIN_PASSWORD = "dev-admin-password-123"

# 先冻结时间、再导入旧应用
_real_time, _real_localtime, _real_strftime = time.time, time.localtime, time.strftime
time.time = lambda: T
os.environ["TZ"] = "Asia/Shanghai"
time.tzset()
# time.strftime(fmt) / time.localtime() 不带参数时读的是 C 库的真实时钟，不受 time.time 打桩影响：一并冻结
time.localtime = lambda secs=None: _real_localtime(T if secs is None else secs)
time.strftime = lambda fmt, t=None: _real_strftime(fmt, _real_localtime(T) if t is None else t)


class _FrozenDatetime(_dt.datetime):
    @classmethod
    def now(cls, tz=None):
        return cls.fromtimestamp(T, tz)


_dt.datetime = _FrozenDatetime

DATA = tempfile.mkdtemp(prefix="owl-routes-")
os.environ.update({
    "DATA_DIR": DATA, "ADMIN_PASSWORD": ADMIN_PASSWORD, "NAI_TOKENS": "pst-synthetic-upstream-token",
    "SECRET_KEY": "test-secret-key-test-secret-key-0123456789", "ADMIN_COOKIE_SECURE": "0",
})
for _k in list(os.environ):
    if _k.startswith("DISCORD_") or _k in ("SITE_URL", "REGISTRATION_BRIDGE_SECRET"):
        del os.environ[_k]

import httpx  # noqa: E402
from app import main, ops, quota_algo, status_page  # noqa: E402
from app.policy import gen_key  # noqa: E402
from app.registration import configured_service  # noqa: E402
from app.state import GateState  # noqa: E402

PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000080000000808020000004b6d29dc0000006c4944415478da0dc94101003008033194a0a44a"
    "aa84e7a940094aaa68cb375545172a5c4cb1c51529aa9a6ed4b899669b6bd23f440b098b112b4e443f4c1b199b316bcec43f861e34789861"
    "871b323f965eb4789965975bb23f8e3e74f898638f3b723f42070587091b2e243cb02c5481f9a60b790000000049454e44ae426082")
FAKE_JPEG = b"\xff\xd8\xff\xe0fake-thumb"


# ------------------------------------------------------------------ 种子数据
def lit(v) -> str:
    if v is None:
        return "NULL"
    if isinstance(v, bytes):
        return "X'" + v.hex() + "'"
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return repr(v)
    return "'" + str(v).replace("'", "''") + "'"


def ins(table: str, **cols) -> str:
    return f"INSERT INTO {table} ({', '.join(cols)}) VALUES ({', '.join(lit(v) for v in cols.values())});"


def seed_sql(variant: str = "main") -> str:
    q: list[str] = ["DELETE FROM api_keys;", "DELETE FROM sqlite_sequence WHERE name='api_keys';",
                    "DELETE FROM deleted_key_usage_flags;", "DELETE FROM daily_quota_offsets;",
                    "INSERT OR REPLACE INTO site_settings(key, value) VALUES ('quota_auto_enabled', '0');"]
    keys = [
        # id, name, 其它列
        (1, "月见草", dict(daily_images=104, daily_anlas=50.0, daily_v5=10, rpm=5, image_model_scope="all", quota_auto=1,
                           expires_at=T + 20 * D, created_at=T - 5 * D, last_used_at=T - 600, last_image_at=T - 900,
                           features="image,vibe,tags", discord_id="900000000000000001")),
        (2, "白鸟", dict(daily_images=60, quota_auto=-1, created_at=T - 3 * D, last_used_at=T - 2 * H,
                         discord_id="900000000000000002", expires_at=T + 5 * D)),
        (3, "nightjar", dict(is_test=1, quota_auto=-1, created_at=T - 2 * D, last_used_at=T - 5 * H)),
        (4, "雾岛", dict(enabled=0, created_at=T - 9 * D, last_used_at=T - 2 * D, discord_id="900000000000000004")),
        (5, "柚子", dict(is_admin=1, created_at=T - 30 * D, last_used_at=T - 60)),
        (6, "Kestrel", dict(allow_anlas=1, daily_anlas=30.0, monthly_anlas=0.0, created_at=T - 1 * D)),
        (7, "邀请-2", dict(created_at=T - 1 * D, quota_auto=1, expires_at=T + 29 * D)),
        (8, "长尾山雀", dict(allow_img2img=1, created_at=T - 7 * D, last_used_at=T - 3 * H,
                             discord_id="900000000000000008")),
        (9, "", dict(enabled=0, created_at=T - 40 * D)),
        (10, "邀请-٩", dict(enabled=0, created_at=T - 41 * D)),         # 阿拉伯-印度数字 9：旧版 \\d 认，邀请编号要接着它往下排
    ]
    for kid, name, cols in keys:
        base = dict(id=kid, name=name, token=f"nai-seed-{kid:04d}", monthly_anlas=500.0, created_at=T - D)
        base.update(cols)
        q.append(ins("api_keys", **base))
    regs = [
        ("900000000000000001", 1, T - 5 * D, "user1", "月见草", "abc123"),
        ("900000000000000002", 2, T - 3 * D, "user2", "白鸟", None),
        ("900000000000000004", 4, T - 9 * D, "user4", "雾岛", ""),
        ("900000000000000008", 8, T - 7 * D, "user8", "长尾山雀", None),
    ]
    if variant == "merge":
        # 同一个 Discord 账号留下两把 Key（线上表是 discord_id 主键，这条路径只在旧数据里出现过）
        q.append("DROP TABLE discord_registrations;")
        q.append("CREATE TABLE discord_registrations (discord_id TEXT, key_id INTEGER NOT NULL UNIQUE, "
                 "created_at REAL NOT NULL, role_granted INTEGER NOT NULL DEFAULT 0, username TEXT, display_name TEXT, "
                 "avatar TEXT, member_checked_at REAL NOT NULL DEFAULT 0);")
        regs = [
            ("900000000000000001", 1, T - 5 * D, "user1", "月见草", "abc123"),
            ("900000000000000001", 6, T - 1 * D, "user1", "月见草", "abc123"),
            ("900000000000000001", 7, T - 1 * D, "user1", "月见草", "abc123"),
            ("900000000000000008", 8, T - 7 * D, "user8", "长尾山雀", None),
        ]
    for did, kid, ts, un, dn, av in regs:
        q.append(ins("discord_registrations", discord_id=did, key_id=kid, created_at=ts, username=un,
                     display_name=dn, avatar=av))
    tags = [
        (2, "邀请", "站长发的邀请 Key：成员用 /register 填它完成绑定；只由站长手动删除", "站长", T - 3 * D),
        (7, "邀请", "站长发的邀请 Key：成员用 /register 填它完成绑定；只由站长手动删除", "站长", T - 1 * D),
        (1, "常驻", "备注A", "站长", T - D),
        (1, "观察", "备注B", "运维", T - H),
        (6, "待确认", "", "站长", T - 100),
        (4, "旧账号", "删除时会一起清掉", "站长", T - 2 * D),
    ]
    for kid, tag, note, by, ts in tags:
        q.append(ins("key_tags", key_id=kid, tag=tag, note=note, by=by, ts=ts))
    counters = [
        (1, DAY, 12, 2, 7.5, 100, 20, 3), (2, DAY, 5, 0, 0.0, 0, 6, 0), (6, DAY, 1, 0, 2.25, 0, 1, 0),
        (1, YDAY, 30, 4, 12.5, 500, 41, 5), (2, YDAY, 9, 0, 0.0, 0, 9, 0), (3, YDAY, 4, 0, 1.0, 0, 4, 1),
        (8, "2026-09-17", 20, 0, 0.0, 0, 22, 0),
    ]
    for kid, day, im, lf, an, tok, rq, v5 in counters:
        q.append(ins("counters", key_id=kid, day=day, images=im, legacy_free_images=lf, anlas=an, text_tokens=tok,
                     requests=rq, v5=v5))
    q.append(ins("daily_quota_offsets", key_id=1, day=DAY, anlas=2.5, v5=1))

    def log(ts, kid, kind="image", status="ok", images=0, anlas=0.0, tokens=0, detail="", reason="", wait=0, dur=0,
            client="", up=200, rid="", model="nai-diffusion-4-5-full", name=None):
        nm = name if name is not None else dict((k[0], k[1]) for k in keys).get(kid, "")
        q.append(ins("usage_log", ts=ts, key_id=kid, key_name=nm, kind=kind, model=model, status=status, images=images,
                     anlas=anlas, tokens=tokens, unconfirmed_anlas=0.0, detail=detail, reason=reason, wait_ms=wait,
                     dur_ms=dur, client=client, up_status=up, rid=rid, src="", ver="3.0.0"))

    # 月见草：最近 24 小时内的出图（小时条）、流式、放大、导演、Vibe、标签、聊天
    for i, hrs in enumerate([0.5, 1.5, 2.5, 5, 10, 23]):
        log(T - hrs * H, 1, images=1, wait=100 * i, dur=4000 + i, client="柏宝绘", rid=f"a1b2c3d4e5f6{i:04x}")
    log(T - 30 * H, 1, images=1, client="柏宝绘", rid="0000000000000030")
    log(T - 3 * H, 1, kind="image_stream", images=2, client="柏宝绘")
    log(T - 4 * H, 1, kind="image_stream", images=2, client="SillyTavern/1.12")
    log(T - 6 * H, 1, kind="upscale", images=1, client="柏宝绘")
    log(T - 7 * H, 1, kind="augment-image", images=1, client="柏宝绘")
    log(T - 8 * H, 1, kind="vibe_encode", images=0, anlas=2.0, client="柏宝绘")
    log(T - 9 * H, 1, kind="tags", client="柏宝绘")
    log(T - 11 * H, 1, kind="chat", model="glm-4-6", tokens=120, client="SillyTavern/1.12")
    log(T - 2 * H, 1, status="rejected", reason="key_rate", detail="请求过于频繁（上限 5 次/分钟），请稍后再试", up=0,
        rid="deadbeef00000001")
    log(T - 3 * H, 1, status="rejected", detail="含 img2img 的请求不允许", up=0)
    log(T - 4 * H, 1, status="error", detail="画师串里有 NaN 权重", up=400, rid="deadbeef00000002")
    log(T - 5 * H, 1, status="rejected", reason="param", detail="角色太多：最多 6 个", up=0)
    # 白鸟
    log(T - 2 * D, 2, images=1, client="柏宝绘")
    log(T - 20 * H, 2, images=1, client="ComfyUI")
    log(T - 1 * H, 2, status="rejected", reason="queue_full", detail="人多，请稍后再试", up=0)
    # 测试 Key
    for i in range(3):
        log(T - (i + 1) * H, 3, images=1, client="测试脚本")
    # Kestrel：只被拒
    log(T - 1 * H, 6, status="rejected", detail="角色太多：最多 6 个", up=0)
    log(T - 2 * H, 6, status="rejected", detail="角色太多：最多 6 个", up=0)
    log(T - 5 * H, 6, kind="text", status="rejected", detail="不支持调用文本生成接口", up=0, model="")
    # 没有 Key 的拒绝
    log(T - 1 * H, None, status="rejected", detail="Key 无效", up=0, name="")
    # 长尾山雀、测试
    log(T - 3 * D, 8, images=1, client="手机")
    log(T - 12 * H, 4, images=1, client="柏宝绘")

    def audit(ts, kid, kname, prompt, negative="", extra="", thumb=None, image=None, image_type=""):
        q.append(ins("generation_audit", ts=ts, key_id=kid, key_name=kname, kind="image", model="nai-diffusion-4-5-full",
                     status="ok", prompt=prompt, negative=negative, extra=extra, thumb=thumb, image=image,
                     image_type=image_type, image_size=len(image) if image else None))

    p1 = '{"params":{"width":832,"height":1216,"seed":123,"sampler":"k_euler","steps":28,"scale":5.0}}'
    audit(T - 2 * H, 1, "月见草", "cat girl, masterpiece", "lowres", p1, image=PNG, image_type="image/png")
    audit(T - 3 * H, 1, "月见草", "owl, night", "", '{"params":{"seed":0,"sampler":"","steps":0,"scale":0}}', image=PNG,
          image_type="image/png")
    audit(T - 5 * H, 1, "月见草", "只有缩略图", "", "", thumb=FAKE_JPEG)
    audit(T - 6 * H, 2, "白鸟", "moon", "bad hands", "not json", image=PNG, image_type="image/webp")
    audit(T - 7 * H, 3, "nightjar", "只有提示词")
    audit(T - 30 * H, 1, "月见草", "老记录", "", p1, image=PNG, image_type="")

    errs = [
        ("aaaaaaaaaaaa", "request", "error", "KeyError: x", "Traceback...", 3, T - 2 * D, T - H, "r0000001", 1, "/ai/generate-image", None),
        ("bbbbbbbbbbbb", "upstream", "error", "上游 502", "详情", 1, T - D, T - 2 * H, "r0000002", 99, "/ai/generate-image", None),
        ("cccccccccccc", "maintenance:cleanup", "warn", "清理失败", "详情", 7, T - 3 * D, T - 3 * H, "", None, "", None),
        ("dddddddddddd", "request", "error", "已处理的", "详情", 2, T - 5 * D, T - 4 * D, "r0000004", 6, "/ai/x", T - 3 * D),
        ("eeeeeeeeeeee", "request", "error", "空名字的 Key", "详情", 1, T - D, T - 10 * H, "r0000005", 9, "/ai/y", None),
    ]
    for e in errs:
        q.append(ins("error_events", sig=e[0], source=e[1], level=e[2], title=e[3], detail=e[4], count=e[5],
                     first_seen=e[6], last_seen=e[7], last_rid=e[8], last_key=e[9], last_path=e[10], resolved_at=e[11]))
    bans = [
        ("900000000000000010", T - 3 * D, "刷量", "spam10", "刷", "", "站长"),
        ("900000000000000011", T - 2 * D, "转卖 Key", "seller11", "卖家", "av11", "系统"),
        ("900000000000000012", T - 1 * D, "", "", "", "", "站长"),
    ]
    for b in bans:
        q.append(ins("discord_bans", discord_id=b[0], created_at=b[1], reason=b[2], username=b[3], display_name=b[4],
                     avatar=b[5], banned_by=b[6]))
    fb = [
        (T - 2 * D, "900000000000000001", "user1", 1,
         {"score": "满意", "pain": "出图失败或报错", "wish": "多给一些 V5", "client": "手机 App", "freq": "每天都用"}),
        (T - 1 * D, "900000000000000002", '=cmd,"x"', 2,
         {"score": "一般", "pain": "没什么困扰", "wish": "保持现状就好", "client": "其他", "freq": "偶尔用"}),
        (T - 3 * H, "900000000000000099", "gone", 99,
         {"score": "非常满意", "pain": "没什么困扰", "wish": "更清楚的教程", "client": "手机 App", "freq": "刚来还没怎么用"}),
    ]
    for ts, did, un, kid, ans in fb:
        q.append(ins("feedback", ts=ts, discord_id=did, username=un, key_id=kid,
                     answers=json.dumps(ans, ensure_ascii=False)))
    sv = [
        (1, "柏宝绘", "懂了", "多给 V5、出图更快、更清楚的教程", "", "愿意", T - 2 * D),
        (2, "ComfyUI", "大概懂", "多给 V5", "希望有公告", "再想想", T - D),
        (6, "手机", "懂了", "出图更快、多给 V5", "", "愿意", T - H),
    ]
    for kid, cl, un, im, imo, do, ts in sv:
        q.append(ins("member_survey", key_id=kid, client=cl, understand=un, improve=im, improve_other=imo, donate=do, ts=ts))
    for ts, token, msg, contact, handled, hat, hnote in [
        (T - 2 * D, "pst-donate-aaaa", "愿意长期提供", "x@example.test", 0, None, ""),
        (T - D, "pst-donate-bbbb", "试试看", "", 0, None, ""),
        (T - 3 * D, "pst-donate-cccc", "旧的", "", 1, T - 2 * D, "已接入"),
    ]:
        q.append(ins("key_donations", ts=ts, token=token, message=msg, contact=contact, handled=handled, handled_at=hat,
                     handled_note=hnote))
    q.append(ins("coupons", key_id=1, kind="reset", note="唱得真好听", by="站长", created_at=T - 2 * D, expires_at=T + 5 * D,
                 used_at=None))
    q.append(ins("coupons", key_id=1, kind="reset", note="已用的", by="奶妹", created_at=T - 6 * D, expires_at=T - D,
                 used_at=T - 3 * D))
    q.append(ins("share_state", key_id=6, score=1.0, score_ts=T - 100, strikes=1, paused_until=T + H, warned_ts=0,
                 paused_ts=T - 100, pause_reason="客户端死循环重试"))
    q.append(ins("share_state", key_id=4, score=0.0, score_ts=0, strikes=0, paused_until=T - 100, warned_ts=0, paused_ts=0,
                 pause_reason=""))
    acts = [
        (T - 20000, "后台 *.*.0.1", "重置今日额度", "Key #1 月见草", "", 1),     # 在今天 07:00 之前，不计入
        (T - 3000, "后台 *.*.0.1", "重置今日额度", "Key #1 月见草", "", 1),
        (T - 1000, "站长", "重置今日额度", "Key #1 月见草", "", 1),
        (T - 900, "站长", "重置今日额度", "Key #6 Kestrel", "", 0),            # 失败的不计入
        (T - 800, "站长", "重置今日额度", "Key #6 Kestrel", "", 1),
        (T - 7000, "后台 *.*.0.1", "登录后台", "", "", 1),
        (T - 6000, "后台 *.*.0.1", "新建 Key", "测试", '{"name":"测试"}', 1),
        (T - 5000, "系统", "闲置计时重置", "", "网关停机约 2.0 小时", 1),
        (T - 4000, "系统", "私信成员", "Key #2", "已私信：你好", 1),
        (T - 2000, "Discord:900000000000000001", "成员自助重置", "Key #1", "", 1),
        (T - 500, "站长", "添加黑名单", "Discord:900000000000000010", "刷量", 1),
        (T - 400, "后台 *.*.9.9", "修改 Key", "Key #3 nightjar", '{"enabled":false}', 1),
    ]
    for ts, actor, action, target, detail, ok in acts:
        q.append(ins("admin_actions", ts=ts, actor=actor, action=action, target=target, detail=detail, ok=ok))
    return "\n".join(q) + "\n"


# ------------------------------------------------------------------ 假 Discord（Python 端用 httpx.MockTransport）
class Discord:
    """记录每个发给 Discord 的请求；closed = 对方关了私信（创建私信频道回 403）。"""

    def __init__(self):
        self.calls: list = []
        self.closed = False

    def handler(self, request: httpx.Request) -> httpx.Response:
        path = re.sub(r"^/api(/v10)?", "", request.url.path)
        body = json.loads(request.content) if request.content else None
        self.calls.append([request.method, path, body])
        if request.method == "POST" and path == "/users/@me/channels":
            return httpx.Response(403, json={}) if self.closed else httpx.Response(200, json={"id": "dm-1"})
        return httpx.Response(200, json={})


# ------------------------------------------------------------------ 请求脚本
TOKEN_RE = re.compile(r"nai-[A-Za-z0-9_-]{32}(?![A-Za-z0-9_-])")
SNAP_TABLES = ["api_keys", "key_tags", "discord_registrations", "discord_bans", "coupons", "admin_actions",
               "share_state", "site_settings", "daily_quota_offsets", "counters", "key_donations"]
# error_events 只在 Bug 页相关的步骤里比：Rust 端记 Bug 是后台任务，写库的时机晚于响应
ERRORS_TABLE = "error_events"
SKIP_COLS = {"api_keys": {"anlas_auto", "v5_pinned"}}


def norm_text(s: str) -> str:
    return TOKEN_RE.sub("nai-<rand>", s)


def step(name, method, path, *, json_body=None, raw=None, headers=None, anon=False, registrar=None, snap=False,
         ctype="json", sql=None, advance=None):
    """registrar 不为空 = 这一步属于「有注册服务」场景（reg），内容是私信的状态：on 正常 / closed 对方关了私信 / off 总闸门关。"""
    return dict(name=name, method=method, path=path, json=json_body, raw=raw, headers=headers or {}, anon=anon,
                registrar=registrar, snap=snap, ctype=ctype, sql=sql,
                advance=(700 if registrar is not None else 0) if advance is None else advance)


A = "/admin/api"
FAKE = dict(dm="on")
FAKE_NODM = dict(dm="closed")
FAKE_BLOCK = dict(dm="off")


def build_steps(variant: str) -> list[dict]:
    S = step
    if variant == "merge":
        return [S("merge: members", "GET", f"{A}/members")]
    if variant == "early":
        # 服务日 07:00 重置，现在是 05:20：旧版「今日重置次数」的起点算成了今天 07:00（在未来），统计为空——照抄
        return [S("early: members", "GET", f"{A}/members"), S("early: keys", "GET", f"{A}/keys")]
    s: list[dict] = []
    add = s.append
    if variant == "reg":
        return [x for x in build_steps("all") if x["registrar"] is not None]
    # --- 鉴权与参数校验（校验先于鉴权）
    add(S("anon: keys", "GET", f"{A}/keys", anon=True))
    add(S("anon: regenerate", "POST", f"{A}/keys/1/regenerate", anon=True))
    add(S("anon: bad id", "GET", f"{A}/keys/abc/coupons", anon=True))
    add(S("anon: bad query", "GET", f"{A}/logs?page=x&hide_test=maybe", anon=True))
    add(S("cross origin write", "POST", f"{A}/keys/1/regenerate", headers={"origin": "https://evil.example"}))
    add(S("bad id write", "POST", f"{A}/keys/abc/regenerate", headers={"origin": "https://evil.example"}))
    # --- Key 列表
    add(S("keys: list", "GET", f"{A}/keys"))
    # --- 新建 Key：校验
    for nm, body in [
        ("daily_images 负数", {"daily_images": -1}), ("daily_images 字符串", {"daily_images": "abc"}),
        ("daily_images 空串", {"daily_images": " "}), ("daily_images null", {"daily_images": None}),
        ("daily_images 布尔", {"daily_images": True}), ("daily_images 小数", {"daily_images": 1.5}),
        ("daily_images 超大", {"daily_images": 1000001}), ("daily_images 列表", {"daily_images": [1]}),
        ("daily_images 字符串数字", {"daily_images": "12"}), ("monthly_anlas 越界", {"monthly_anlas": 100001}),
        ("daily_anlas 无穷", {"daily_anlas": "inf"}), ("daily_v5 越界", {"daily_v5": 100001}),
        ("daily_text 越界", {"daily_text_tokens": 100000001}), ("rpm 0", {"rpm": 0}), ("rpm 601", {"rpm": 601}),
        ("expires_days 越界", {"expires_days": 3651}),
        ("Anlas 上限都是 0", {"allow_anlas": True, "daily_anlas": 0, "monthly_anlas": 0}),
        ("features 不是列表", {"features": "image"}), ("features 含未知", {"features": ["image", "nope"]}),
        ("features 含非字符串", {"features": [1]}),
    ]:
        add(S(f"create: {nm}", "POST", f"{A}/keys", json_body=body))
    add(S("create: 非 JSON", "POST", f"{A}/keys", raw="{bad", headers={"content-type": "application/json"}))
    add(S("create: 列表体", "POST", f"{A}/keys", raw="[1]", headers={"content-type": "application/json"}))
    # --- 新建 Key：成功
    add(S("create: 默认", "POST", f"{A}/keys", json_body={}, snap=True))
    add(S("create: 全字段", "POST", f"{A}/keys", json_body={
        "name": "  新成员甲  ", "daily_images": 77, "monthly_anlas": 123.5, "daily_anlas": 10, "daily_v5": 9,
        "daily_text_tokens": 5000, "rpm": 7, "expires_days": 3, "image_model_scope": "all", "allow_anlas": True,
        "allow_img2img": True, "exclude_global_v5": True, "is_test": True, "role": "TEST ",
        "features": ["vibe", "image", "image"]}))
    add(S("create: features null", "POST", f"{A}/keys", json_body={"name": "全功能", "features": None}))
    add(S("create: 长名字 + 数字名", "POST", f"{A}/keys", json_body={"name": "名" * 70}))
    add(S("create: 名字是数字", "POST", f"{A}/keys", json_body={"name": 12345, "role": "invite"}))
    add(S("create: 字符串数字 + 管理员", "POST", f"{A}/keys", json_body={"name": "x", "daily_images": "12", "rpm": "3", "role": "admin"}))
    add(S("create: 坏 role 忽略", "POST", f"{A}/keys", json_body={"name": "r", "role": "boss"}))
    add(S("create: 名字 null / false / 列表 / 制表符", "POST", f"{A}/keys", json_body={"name": None}))
    add(S("create: 名字 false", "POST", f"{A}/keys", json_body={"name": False}))
    add(S("create: 名字是浮点 + role 是数字", "POST", f"{A}/keys", json_body={"name": 1.50, "role": 5}))
    add(S("create: 名字含空白", "POST", f"{A}/keys", json_body={"name": "\t 两  端\u3000", "daily_anlas": "1e2", "monthly_anlas": " 3_0 "}))
    add(S("create: is_test 为 0 / role 大小写", "POST", f"{A}/keys", json_body={"name": "t", "is_test": 0, "role": "ADMIN"}))
    add(S("create: 默认功能 = 全部（设置 *）", "POST", f"{A}/keys", json_body={"name": "star"}, sql="INSERT OR REPLACE INTO site_settings(key,value) VALUES('register_features','*')"))
    add(S("create: 默认功能 = 设置的列表", "POST", f"{A}/keys", json_body={"name": "lst"}, sql="INSERT OR REPLACE INTO site_settings(key,value) VALUES('register_features','vibe,text')"))
    add(S("create: 默认功能（有注册服务时取服务默认）", "POST", f"{A}/keys", json_body={"name": "reg"}, registrar=FAKE, sql="DELETE FROM site_settings WHERE key='register_features'"))
    # --- 邀请 Key
    add(S("invite: 无注册服务", "POST", f"{A}/keys/invite", json_body={}, snap=True))
    add(S("invite: 有注册服务", "POST", f"{A}/keys/invite", json_body={}, registrar=FAKE, sql="INSERT OR REPLACE INTO site_settings(key,value) VALUES('register_daily_images','44'),('register_daily_v5','3'),('register_image_scope','all'),('register_expires_days','0'),('register_features','image,tags')"))
    add(S("invite: 设置值不合法", "POST", f"{A}/keys/invite", json_body={}, registrar=FAKE, sql="INSERT OR REPLACE INTO site_settings(key,value) VALUES('register_daily_images','abc'),('register_daily_v5','-5'),('register_expires_days','2.9'),('register_features','')"))
    # --- 重新生成
    add(S("regenerate: 不存在", "POST", f"{A}/keys/999/regenerate"))
    add(S("regenerate: 无注册服务", "POST", f"{A}/keys/6/regenerate", snap=True))
    add(S("regenerate: 有注册服务（私信）", "POST", f"{A}/keys/2/regenerate", registrar=FAKE))
    add(S("regenerate: 私信被拦", "POST", f"{A}/keys/4/regenerate", registrar=FAKE_BLOCK))
    add(S("regenerate: 私信失败无原因", "POST", f"{A}/keys/8/regenerate", registrar=FAKE_NODM))
    add(S("regenerate: notify=0", "POST", f"{A}/keys/1/regenerate?notify=0", registrar=FAKE))
    add(S("regenerate: 没有绑定 Discord 的不私信", "POST", f"{A}/keys/6/regenerate", registrar=FAKE))
    # --- PATCH
    add(S("patch: 不存在", "PATCH", f"{A}/keys/999", json_body={"enabled": False}))
    add(S("patch: quota 非法", "PATCH", f"{A}/keys/1", json_body={"quota": "x"}))
    add(S("patch: quota 布尔", "PATCH", f"{A}/keys/1", json_body={"quota": False}))
    add(S("patch: 测试 Key 不归算法", "PATCH", f"{A}/keys/3", json_body={"quota": "auto"}))
    add(S("patch: 非 JSON", "PATCH", f"{A}/keys/1", raw="nope", headers={"content-type": "application/json"}))
    add(S("patch: 空对象", "PATCH", f"{A}/keys/1", json_body={}))
    add(S("patch: 校验失败不进操作日志", "PATCH", f"{A}/keys/abc", json_body={"enabled": False}))
    add(S("patch: 名字空 -> 未命名 + enabled", "PATCH", f"{A}/keys/6", json_body={"name": "", "enabled": 0}))
    add(S("patch: 名字改回", "PATCH", f"{A}/keys/6", json_body={"name": "  Kestrel ", "enabled": True}))
    for nm, body in [
        ("daily_images 非法", {"daily_images": "x"}), ("daily_anlas 非法", {"daily_anlas": -0.5}),
        ("daily_v5 非法", {"daily_v5": 1.5}), ("monthly_anlas 非法", {"monthly_anlas": None}),
        ("daily_text 非法", {"daily_text_tokens": -1}), ("rpm 非法", {"rpm": 0}), ("expires_days 非法", {"expires_days": "z"}),
        ("role 非法", {"role": "boss"}), ("features 非法", {"features": ["nope"]}),
        ("开 Anlas 但上限都是 0", {"allow_anlas": True, "daily_anlas": 0, "monthly_anlas": 0}),
    ]:
        add(S(f"patch: {nm}", "PATCH", f"{A}/keys/2", json_body=body))
    add(S("patch: 开 Anlas（沿用已有上限）", "PATCH", f"{A}/keys/1", json_body={"allow_anlas": True}, registrar=FAKE))
    add(S("patch: 改额度", "PATCH", f"{A}/keys/1", json_body={"daily_images": 200, "daily_anlas": 60, "daily_v5": 0,
                                                                  "image_model_scope": "legacy", "notify": False}, registrar=FAKE, snap=True))
    add(S("patch: 改额度（未变，不改归属）", "PATCH", f"{A}/keys/2", json_body={"daily_images": 60, "image_model_scope": "legacy"}))
    add(S("patch: 模型开 V5 自动给 15", "PATCH", f"{A}/keys/2", json_body={"image_model_scope": "all"}, registrar=FAKE))
    add(S("patch: 模型开 V5（已有 V5 额度）", "PATCH", f"{A}/keys/1", json_body={"image_model_scope": "all"}))
    add(S("patch: 有效期 + 无限", "PATCH", f"{A}/keys/2", json_body={"expires_days": 0}, registrar=FAKE))
    add(S("patch: 有效期 30 天", "PATCH", f"{A}/keys/2", json_body={"expires_days": 30}, registrar=FAKE))
    add(S("patch: 有效期（相差不到 1 天不提示）", "PATCH", f"{A}/keys/2", json_body={"expires_days": 30}, registrar=FAKE))
    add(S("patch: features + 开关 + 可用模型提示", "PATCH", f"{A}/keys/8", json_body={"features": ["image", "text"], "allow_img2img": False,
                                                                          "exclude_global_v5": True, "image_model_scope": "all"}, registrar=FAKE))
    add(S("patch: features null", "PATCH", f"{A}/keys/8", json_body={"features": None}))
    add(S("patch: 关闭 Key（私信）", "PATCH", f"{A}/keys/4", json_body={"enabled": False}, registrar=FAKE))
    add(S("patch: 启用 Key（私信）", "PATCH", f"{A}/keys/4", json_body={"enabled": True}, registrar=FAKE))
    add(S("patch: 启用 Key（notify=false 不私信）", "PATCH", f"{A}/keys/4", json_body={"enabled": True, "notify": False}, registrar=FAKE))
    # 10 分钟内最多 3 条私信：连着 4 次改动，第 4 次被限速拦下（日志里记原因）
    for i in range(4):
        add(S(f"patch: 连续改动 {i + 1}（私信限速）", "PATCH", f"{A}/keys/8", json_body={"enabled": i % 2 == 1}, registrar=FAKE,
              advance=700 if i == 0 else 0, snap=(i == 3)))
    add(S("patch: role 换身份组", "PATCH", f"{A}/keys/6", json_body={"role": "Invite", "is_test": True, "is_admin": True}, snap=True))
    add(S("patch: is_test / is_admin 单独传（is_admin 被忽略）", "PATCH", f"{A}/keys/6", json_body={"is_test": True, "is_admin": True}))
    add(S("patch: 交给算法（普通 Key，其它额度字段被忽略）", "PATCH", f"{A}/keys/8", json_body={"quota": "auto", "daily_images": 5, "features": ["image"], "enabled": True, "anlas_mode": "bad"}, snap=True))
    add(S("patch: 交给算法（测试 Key 但同时取消测试）", "PATCH", f"{A}/keys/3", json_body={"quota": "auto", "is_test": False}, snap=True))
    add(S("patch: 旧参数 quota_mode=auto", "PATCH", f"{A}/keys/2", json_body={"quota_mode": "auto", "daily_images": 61}, registrar=FAKE, snap=True))
    add(S("patch: 手动", "PATCH", f"{A}/keys/7", json_body={"quota": "manual", "daily_images": 15, "anlas_mode": "manual", "v5_pinned": 5}, snap=True))
    add(S("patch: quota=manual 不动其它", "PATCH", f"{A}/keys/7", json_body={"quota": "manual", "quota_mode": "auto"}))
    # --- 身份组
    add(S("role: 不存在", "POST", f"{A}/keys/999/role", json_body={"role": "test"}))
    add(S("role: 非法", "POST", f"{A}/keys/1/role", json_body={"role": "boss"}))
    add(S("role: 空", "POST", f"{A}/keys/1/role", json_body={}))
    add(S("role: 非 JSON", "POST", f"{A}/keys/1/role", raw="x"))
    add(S("role: test", "POST", f"{A}/keys/1/role", json_body={"role": " Test "}, snap=True))
    add(S("role: invite", "POST", f"{A}/keys/1/role", json_body={"role": "invite"}))
    add(S("role: regular", "POST", f"{A}/keys/1/role", json_body={"role": "regular"}))
    add(S("role: admin", "POST", f"{A}/keys/1/role", json_body={"role": "admin"}))
    add(S("role: 还原 regular", "POST", f"{A}/keys/1/role", json_body={"role": "regular"}))
    # --- 重置今日额度
    add(S("reset-quota: 不存在", "POST", f"{A}/keys/999/reset-daily-image-quota"))
    add(S("reset-quota: 成功", "POST", f"{A}/keys/1/reset-daily-image-quota", registrar=FAKE, snap=True))
    add(S("reset-quota: 没有计数的 Key", "POST", f"{A}/keys/6/reset-daily-image-quota?notify=0", registrar=FAKE))
    # --- 绑定 Discord
    add(S("bind: 非数字", "POST", f"{A}/keys/6/bind-discord", json_body={"discord_id": "12a"}))
    add(S("bind: 空", "POST", f"{A}/keys/6/bind-discord", json_body={}))
    add(S("bind: 不存在的 Key", "POST", f"{A}/keys/999/bind-discord", json_body={"discord_id": "900000000000000050"}))
    add(S("bind: 已被占用", "POST", f"{A}/keys/6/bind-discord", json_body={"discord_id": "900000000000000001"}))
    add(S("bind: 新绑定", "POST", f"{A}/keys/6/bind-discord", json_body={"discord_id": " 900000000000000050 ", "username": " u50 "}, snap=True))
    add(S("bind: 换绑（显示名默认用户名）", "POST", f"{A}/keys/6/bind-discord", json_body={"discord_id": "900000000000000051", "username": "u51", "display_name": "五十一"}))
    add(S("bind: 数字 ID 当数字", "POST", f"{A}/keys/3/bind-discord", json_body={"discord_id": 900000000000000052}))
    add(S("bind: 用户名超长 + 显示名空串", "POST", f"{A}/keys/5/bind-discord", json_body={"discord_id": "900000000000000054", "username": "名" * 100, "display_name": ""}))
    add(S("bind: 显示名超长", "POST", f"{A}/keys/5/bind-discord", json_body={"discord_id": "900000000000000054", "username": "u", "display_name": "d" * 100}))
    add(S("bind: 同账号重复绑定同一把 Key", "POST", f"{A}/keys/3/bind-discord", json_body={"discord_id": "900000000000000052", "username": "again"}))
    add(S("bind: 非 ASCII 数字", "POST", f"{A}/keys/5/bind-discord", json_body={"discord_id": "٩٠٠٠٠٠٠٠٠٠٠٠٠٠٠٠٠٥٣", "username": "u"}))
    # --- 券
    add(S("coupons: 列表", "GET", f"{A}/keys/1/coupons"))
    add(S("coupons: 无券", "GET", f"{A}/keys/2/coupons"))
    add(S("coupon: kind 非法", "POST", f"{A}/keys/1/coupons", json_body={"kind": "gold"}))
    add(S("coupon: days 0", "POST", f"{A}/keys/1/coupons", json_body={"days": 0}))
    add(S("coupon: days 31", "POST", f"{A}/keys/1/coupons", json_body={"days": 31}))
    add(S("coupon: days 字符串", "POST", f"{A}/keys/1/coupons", json_body={"days": "7"}))
    add(S("coupon: days 布尔", "POST", f"{A}/keys/1/coupons", json_body={"days": True}))
    add(S("coupon: 不存在的 Key", "POST", f"{A}/keys/999/coupons", json_body={}))
    add(S("coupon: days 0.5（>0 就行，文案是 1～30）", "POST", f"{A}/keys/2/coupons", json_body={"days": 0.5, "note": None, "by": None}))
    add(S("coupon: days 30.0", "POST", f"{A}/keys/2/coupons", json_body={"days": 30.0}))
    add(S("coupon: kind null", "POST", f"{A}/keys/2/coupons", json_body={"kind": None}))
    add(S("coupon: 默认", "POST", f"{A}/keys/2/coupons", json_body={}, snap=True))
    add(S("coupon: 全字段", "POST", f"{A}/keys/2/coupons", json_body={"kind": "reset", "days": 12.5, "note": "备注" * 120, "by": "奶妹" * 30}))
    add(S("coupons: 之后列表", "GET", f"{A}/keys/2/coupons"))
    # --- 标签
    add(S("tag: 不存在", "PUT", f"{A}/keys/999/tags", json_body={"tag": "x"}))
    add(S("tag: 空", "PUT", f"{A}/keys/1/tags", json_body={"tag": "  "}))
    add(S("tag: 默认来源", "PUT", f"{A}/keys/1/tags", json_body={"tag": "  多  空白\t标签很长很长很长很长  ", "note": "  备注  "}, snap=True))
    add(S("tag: 覆盖", "PUT", f"{A}/keys/1/tags", json_body={"tag": "常驻", "note": "新备注", "by": "  运维二号运维二号运维二号运维二号  "}))
    add(S("tag: by 空白", "PUT", f"{A}/keys/1/tags", json_body={"tag": "常驻", "by": "   "}))
    add(S("tag: 全角空格 / 换行折叠", "PUT", f"{A}/keys/1/tags", json_body={"tag": "a\u3000\u3000b\n\tc", "note": 123, "by": 7}))
    add(S("tag: tag 是数字", "PUT", f"{A}/keys/1/tags", json_body={"tag": 12345678901234567}))
    add(S("tag: 删除", "DELETE", f"{A}/keys/1/tags/%E8%A7%82%E5%AF%9F"))
    add(S("tag: 删除不存在", "DELETE", f"{A}/keys/1/tags/%E8%A7%82%E5%AF%9F"))
    add(S("tag: 删除 Key 不存在", "DELETE", f"{A}/keys/999/tags/x"))
    # --- 防分享
    add(S("share-clear: 有暂停", "POST", f"{A}/keys/6/share-clear", snap=True))
    add(S("share-clear: 没有暂停", "POST", f"{A}/keys/999/share-clear"))
    # --- 黑名单
    add(S("bans: 列表", "GET", f"{A}/bans"))
    add(S("bans: 搜索 ID", "GET", f"{A}/bans?q=%2011%20"))
    add(S("bans: 搜索原因", "GET", f"{A}/bans?q=%E5%88%B7"))
    add(S("bans: 搜索 + 号等于空格", "GET", f"{A}/bans?q=%E8%BD%AC%E5%8D%96+Key"))
    add(S("bans: 搜索无结果", "GET", f"{A}/bans?q=zzz"))
    add(S("ban: 非数字", "POST", f"{A}/bans", json_body={"discord_id": "abc"}))
    add(S("ban: 空", "POST", f"{A}/bans", json_body={}))
    add(S("ban: 无注册服务", "POST", f"{A}/bans", json_body={"discord_id": " 900000000000000060 ", "reason": "  " + "长" * 120}, snap=True))
    add(S("ban: 默认原因", "POST", f"{A}/bans", json_body={"discord_id": "900000000000000061"}))
    add(S("ban: 有注册服务（会撤销 Key）", "POST", f"{A}/bans", json_body={"discord_id": "900000000000000008", "reason": "测试封禁"}, registrar=FAKE, snap=True))
    add(S("unban: 不在名单", "DELETE", f"{A}/bans/900000000000000099"))
    add(S("unban: 成功", "DELETE", f"{A}/bans/900000000000000060", snap=True))
    add(S("unban: 带空白", "DELETE", f"{A}/bans/%20900000000000000061%20"))
    # --- 奖励
    add(S("gift: 缺 ID", "POST", f"{A}/gift", json_body={}))
    add(S("gift: ID 非数字", "POST", f"{A}/gift", json_body={"discord_id": "x"}))
    add(S("gift: ID 为 null", "POST", f"{A}/gift", json_body={"discord_id": None}))
    add(S("gift: 没有注册服务", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000070"}))
    add(S("gift: 被停用的账号", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000010"}, registrar=FAKE))
    add(S("gift: 已有 Key 的账号", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000001", "reason": "夸夸"}, registrar=FAKE))
    add(S("gift: 新账号（私信成功）", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000070", "username": "u70",
                                                                 "global_name": "七十", "reason": "唱得好听"}, registrar=FAKE, snap=True))
    add(S("gift: 新账号（私信被拦，返回 Key）", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000071", "username": "u71",
                                                                      "coupon_days": 3}, registrar=FAKE_BLOCK))
    add(S("gift: 新账号（私信失败无原因，不发券，带回复）", "POST", f"{A}/gift", json_body={
        "discord_id": "900000000000000072", "coupon_days": 0, "reply": {"channel_id": "900000000000000041", "message_id": 5, "text": "好耶"}},
        registrar=FAKE_NODM))
    add(S("gift: 回复缺字段不回复", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000001", "coupon_days": "2.5",
                                                                  "reply": {"channel_id": "1", "message_id": "2"}}, registrar=FAKE))
    add(S("gift: 名额调整", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000073", "reason": "r"}, registrar=FAKE,
          sql="INSERT OR REPLACE INTO site_settings(key,value) VALUES('register_max_users','1')"))
    add(S("gift: 名额已调整后再发", "POST", f"{A}/gift", json_body={"discord_id": "900000000000000074"}, registrar=FAKE, snap=True))
    # --- 成员页
    add(S("members: 列表", "GET", f"{A}/members"))
    # --- 删除 Key
    add(S("delete: 不存在", "DELETE", f"{A}/keys/999"))
    add(S("delete: ban 参数非法", "DELETE", f"{A}/keys/5?ban=zz"))
    add(S("delete: 无注册服务（ban 被忽略）", "DELETE", f"{A}/keys/9?ban=true", snap=True))
    add(S("delete: 有注册服务无绑定", "DELETE", f"{A}/keys/5", registrar=FAKE))
    add(S("delete: 有注册服务（revoke + 私信）", "DELETE", f"{A}/keys/2?ban=0", registrar=FAKE, snap=True))
    add(S("delete: 有注册服务 + ban + 原因", "DELETE", f"{A}/keys/4?ban=1&reason=%E8%BD%AC%E5%8D%96", registrar=FAKE_BLOCK))
    add(S("delete: ban 默认原因 notify=0", "DELETE", f"{A}/keys/{{kid:900000000000000070}}?ban=yes&notify=0", registrar=FAKE))
    # --- 日志
    add(S("logs: 默认", "GET", f"{A}/logs"))
    add(S("logs: per_page=50", "GET", f"{A}/logs?per_page=50&page=1"))
    add(S("logs: per_page 非法", "GET", f"{A}/logs?per_page=7"))
    add(S("logs: 分页", "GET", f"{A}/logs?page=2"))
    add(S("logs: 页码越界", "GET", f"{A}/logs?page=99"))
    add(S("logs: 页码 0 / 负", "GET", f"{A}/logs?page=-3"))
    add(S("logs: key_id", "GET", f"{A}/logs?key_id=1"))
    add(S("logs: key_id=0", "GET", f"{A}/logs?key_id=0"))
    add(S("logs: feature=image", "GET", f"{A}/logs?feature=image"))
    add(S("logs: feature=upscale", "GET", f"{A}/logs?feature=upscale&per_page=100"))
    add(S("logs: feature=text", "GET", f"{A}/logs?feature=text"))
    add(S("logs: 未知 feature", "GET", f"{A}/logs?feature=nope"))
    add(S("logs: feature 空", "GET", f"{A}/logs?feature="))
    add(S("logs: hide_test", "GET", f"{A}/logs?hide_test=true&per_page=100"))
    add(S("logs: rid", "GET", f"{A}/logs?rid=%20DEADBEEF00000001zz%20"))
    add(S("logs: rid 全非法", "GET", f"{A}/logs?rid=xyz"))
    add(S("logs: 组合", "GET", f"{A}/logs?key_id=1&feature=image&hide_test=1&per_page=20&page=2"))
    add(S("logs: 非法 page", "GET", f"{A}/logs?page=x"))
    add(S("logs: 非法 key_id", "GET", f"{A}/logs?key_id="))
    add(S("logs: 多个错误", "GET", f"{A}/logs?key_id=a&page=b&hide_test=c&per_page=d"))
    add(S("logs: 重复参数取最后", "GET", f"{A}/logs?per_page=20&per_page=50"))
    add(S("logs: 浮点写法的整数", "GET", f"{A}/logs?page=1.0&per_page=50.0"))
    add(S("logs: 超大页码", "GET", f"{A}/logs?page=99999999999999999999999"))
    # --- 导出（客户端日志：先成功，再被限频）
    add(S("logs-export: 非法 days", "GET", f"{A}/logs/export?days=abc"))
    add(S("logs-export: 成功", "GET", f"{A}/logs/export?days=7", ctype="zip"))
    add(S("logs-export: 限频", "GET", f"{A}/logs/export"))
    # --- 操作日志
    add(S("actions: 默认", "GET", f"{A}/actions"))
    add(S("actions: 重复参数取最后", "GET", f"{A}/actions?actor=admin&actor=system"))
    add(S("actions: 站长", "GET", f"{A}/actions?actor=admin"))
    add(S("actions: 系统", "GET", f"{A}/actions?actor=system"))
    add(S("actions: 成员", "GET", f"{A}/actions?actor=member"))
    add(S("actions: 未知类别不筛", "GET", f"{A}/actions?actor=zzz"))
    add(S("actions: 按操作名", "GET", f"{A}/actions?action=%E9%87%8D%E7%BD%AE%E4%BB%8A%E6%97%A5%E9%A2%9D%E5%BA%A6%7C%20%E7%99%BB%E5%BD%95%E5%90%8E%E5%8F%B0%20%7C%7C"))
    add(S("actions: 组合", "GET", f"{A}/actions?actor=admin&action=%E9%87%8D%E7%BD%AE%E4%BB%8A%E6%97%A5%E9%A2%9D%E5%BA%A6"))
    add(S("actions: 页码越界", "GET", f"{A}/actions?page=50"))
    add(S("actions: 非法页码", "GET", f"{A}/actions?page=q"))
    # --- Bug
    add(S("errors: 未处理", "GET", f"{A}/errors"))
    add(S("errors: 全部", "GET", f"{A}/errors?all=true"))
    add(S("errors: all 非法", "GET", f"{A}/errors?all=zzz"))
    add(S("errors: resolve 不存在", "POST", f"{A}/errors/nonexistent/resolve"))
    add(S("errors: resolve", "POST", f"{A}/errors/aaaaaaaaaaaaZZZZ/resolve", snap="errors"))
    add(S("errors: resolve 已处理的", "POST", f"{A}/errors/aaaaaaaaaaaa/resolve"))
    add(S("errors: resolve-all", "POST", f"{A}/errors/resolve-all", snap="errors"))
    add(S("errors: resolve-all 再来", "POST", f"{A}/errors/resolve-all"))
    add(S("errors: 全部（之后）", "GET", f"{A}/errors?all=1"))
    # --- 生成记录
    add(S("audit: 默认", "GET", f"{A}/audit"))
    add(S("audit: key_id=1", "GET", f"{A}/audit?key_id=1"))
    add(S("audit: key_id 文本", "GET", f"{A}/audit?key_id=1.0"))
    add(S("audit: key_id 非数字", "GET", f"{A}/audit?key_id=abc"))
    add(S("audit: key_id 空", "GET", f"{A}/audit?key_id="))
    add(S("audit: 多把 Key", "GET", f"{A}/audit?key_id=1,2"))
    add(S("audit: 多把 Key 含垃圾", "GET", f"{A}/audit?key_id=2,x,,3"))
    add(S("audit: 全是垃圾", "GET", f"{A}/audit?key_id=a,b"))
    add(S("audit: 重复 key_id 取最后", "GET", f"{A}/audit?key_id=1&key_id=2"))
    add(S("audit: per_page=96", "GET", f"{A}/audit?per_page=96"))
    add(S("audit: per_page=48", "GET", f"{A}/audit?per_page=48&page=1"))
    add(S("audit: 页码越界", "GET", f"{A}/audit?page=9&per_page=24"))
    add(S("audit: 非法页码", "GET", f"{A}/audit?page=x"))
    add(S("audit: 缩略图（从原图现缩）", "GET", f"{A}/audit/1/thumb", ctype="image"))
    add(S("audit: 缩略图（已存）", "GET", f"{A}/audit/3/thumb", ctype="image"))
    add(S("audit: 缩略图（没有）", "GET", f"{A}/audit/5/thumb"))
    add(S("audit: 缩略图（不存在）", "GET", f"{A}/audit/999/thumb"))
    add(S("audit: 缩略图非法 id", "GET", f"{A}/audit/x/thumb"))
    add(S("audit: 原图", "GET", f"{A}/audit/1/image", ctype="image"))
    add(S("audit: 原图（类型 webp）", "GET", f"{A}/audit/4/image", ctype="image"))
    add(S("audit: 原图（类型空 -> png）", "GET", f"{A}/audit/6/image", ctype="image"))
    add(S("audit: 原图回退缩略图", "GET", f"{A}/audit/3/image", ctype="image"))
    add(S("audit: 原图没有", "GET", f"{A}/audit/5/image"))
    add(S("audit: 导出缺 key_id", "GET", f"{A}/audit/export"))
    add(S("audit: 导出", "GET", f"{A}/audit/export?key_id=1", ctype="zip"))
    add(S("audit: 导出（名字含特殊字符）", "GET", f"{A}/audit/export?key_id=2", ctype="zip"))
    add(S("audit: 导出（多 Key 查不到）", "GET", f"{A}/audit/export?key_id=1,2"))
    add(S("audit: 导出（没有原图）", "GET", f"{A}/audit/export?key_id=3"))
    add(S("audit: 导出（Key 不存在）", "GET", f"{A}/audit/export?key_id=999"))
    add(S("audit: 导出（文本编号）", "GET", f"{A}/audit/export?key_id=%201.0", ctype="zip"))
    add(S("audit: 清空缺确认", "POST", f"{A}/audit/purge-images", json_body={}))
    add(S("audit: 清空确认错误", "POST", f"{A}/audit/purge-images", json_body={"confirm": "yes"}))
    add(S("audit: 清空", "POST", f"{A}/audit/purge-images", json_body={"confirm": "清空"}, snap=True))
    add(S("audit: 清空后列表", "GET", f"{A}/audit"))
    add(S("audit: 清空后缩略图", "GET", f"{A}/audit/1/thumb", ctype="image"))
    add(S("audit: 清空后原图", "GET", f"{A}/audit/1/image"))
    # --- 反馈 / 问卷 / 捐赠
    add(S("feedback: 列表", "GET", f"{A}/feedback"))
    add(S("feedback: 导出", "GET", f"{A}/feedback/export", ctype="text"))
    add(S("survey: 汇总", "GET", f"{A}/survey"))
    add(S("donations: 列表", "GET", f"{A}/donations"))
    add(S("donations: 数量", "GET", f"{A}/donations/count"))
    add(S("donations: 非法 id", "POST", f"{A}/donations/x/handle", json_body={}))
    add(S("donations: 不存在", "POST", f"{A}/donations/99/handle", json_body={}))
    add(S("donations: 处理", "POST", f"{A}/donations/1/handle", json_body={"note": "  已联系  " + "长" * 300}, snap=True))
    add(S("donations: 重复处理", "POST", f"{A}/donations/1/handle", json_body={}))
    add(S("donations: 处理（无备注）", "POST", f"{A}/donations/2/handle", json_body={"note": None}))
    add(S("donations: 数量（之后）", "GET", f"{A}/donations/count"))
    # --- 全站重置
    add(S("reset-all: 缺确认", "POST", f"{A}/ops/reset-all-keys", json_body={}))
    add(S("reset-all: 确认错误", "POST", f"{A}/ops/reset-all-keys", json_body={"confirm": "确认"}))
    add(S("reset-all: 成功", "POST", f"{A}/ops/reset-all-keys", json_body={"confirm": "确认重置"}, snap=True))
    add(S("reset-all: 之后 Keys", "GET", f"{A}/keys"))
    add(S("reset-all: 之后 members", "GET", f"{A}/members"))
    add(S("reset-all: 操作日志", "GET", f"{A}/actions?page=1"))
    if variant == "main":
        return [x for x in s if x["registrar"] is None]
    return s


# ------------------------------------------------------------------ 运行
async def resolve(st, path: str) -> str:
    """路径里的 {kid:<discord_id>} 换成该账号当前绑定的 Key 编号。"""
    def one(m):
        return m.group(0)
    for m in re.finditer(r"\{kid:(\d+)\}", path):
        rows = await st.db._db.execute_fetchall("SELECT key_id FROM discord_registrations WHERE discord_id=?", (m.group(1),))
        path = path.replace(m.group(0), str(rows[0][0]) if rows else "0")
    return path


async def snapshot(st, errors: bool = False) -> dict:
    out = {}
    for t in SNAP_TABLES + ([ERRORS_TABLE] if errors else []):
        rows = await st.db._db.execute_fetchall(f"SELECT * FROM {t} ORDER BY rowid")
        skip = SKIP_COLS.get(t, set())
        out[t] = [{k: r[k] for k in r.keys() if k not in skip} for r in rows]
    return out


def zip_view(blob: bytes) -> dict:
    z = zipfile.ZipFile(io.BytesIO(blob))
    files = {}
    for n in z.namelist():
        data = z.read(n)
        if n.endswith((".csv", ".txt")):
            files[n] = norm_text(data.decode("utf-8"))
        else:
            files[n] = {"sha256": hashlib.sha256(data).hexdigest(), "len": len(data)}
    return files


async def run_variant(variant: str, out_lines: list[str]) -> None:
    global DATA, T
    shutil.rmtree(DATA, ignore_errors=True)
    DATA = tempfile.mkdtemp(prefix="owl-routes-")
    os.environ["DATA_DIR"] = DATA
    main.SETTINGS = main.load_settings()
    st = GateState(main.SETTINGS)
    await st.db.connect()
    await status_page.ensure_schema(st.db)
    main.STATE = st
    main.app.state.gate = st
    main.app.state.registrar = None
    discord = Discord()
    if variant == "reg":
        # 旧版真实的 RegistrationService；Discord REST 换成本机假服务（只记录、不联网）
        os.environ.update({"DISCORD_CLIENT_ID": "client-id", "DISCORD_CLIENT_SECRET": "client-secret",
                           "DISCORD_BOT_TOKEN": "fake-bot-token", "DISCORD_GUILD_ID": "900000000000000001",
                           "REGISTRATION_BRIDGE_SECRET": "contract-secret-contract-secret-0001",
                           "SITE_URL": "https://gate.test"})
        mock = httpx.AsyncClient(transport=httpx.MockTransport(discord.handler))
        main.app.state.registrar = configured_service(st.db, mock)
        assert main.app.state.registrar is not None
    sql = seed_sql("main" if variant == "reg" else variant)
    if variant in ("main", "reg"):
        (OUT / "admin_routes_seed.sql").write_text(sql, encoding="utf-8")
    else:
        (OUT / f"admin_routes_seed_{variant}.sql").write_text(sql, encoding="utf-8")
    await st.db._db.executescript(sql)
    await st.db._db.commit()
    http = httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://owl.example")
    r = await http.post("/admin/api/login", json={"password": ADMIN_PASSWORD}, headers={"origin": "https://owl.example"})
    assert r.status_code == 200, r.text
    cookie = r.cookies.get("nai_gate_admin")
    http.cookies.clear()                       # 匿名步骤不能带上登录时留下的 Cookie
    out_lines.append(json.dumps({"kind": "meta", "variant": variant, "now": T}, ensure_ascii=False))
    for stp in build_steps(variant):
        T += stp["advance"]
        reg = stp["registrar"]
        if reg is not None:
            # 私信总闸门 / 对方关了私信：写进步骤的 sql，两种语言都从夹具里执行同一条
            flag = "0" if reg["dm"] == "off" else "1"
            gate = f"INSERT OR REPLACE INTO site_settings(key,value) VALUES('dm_enabled','{flag}')"
            stp["sql"] = gate + (";" + stp["sql"] if stp["sql"] else "")
            discord.closed = reg["dm"] == "closed"
        if stp["sql"]:
            await st.db._db.executescript(stp["sql"])
            await st.db._db.commit()
        stp["path"] = await resolve(st, stp["path"])
        discord.calls.clear()
        headers = {"origin": "https://owl.example", **stp["headers"]}
        try:
            if not stp["anon"]:
                headers["cookie"] = f"nai_gate_admin={cookie}"
            resp = await http.request(stp["method"], stp["path"], json=stp["json"], content=stp["raw"], headers=headers)
            exc = None
        except Exception as e:   # noqa: BLE001 —— 真服务器会回 500
            resp, exc = None, f"{type(e).__name__}"
        rec: dict = {"kind": "step", "name": stp["name"], "method": stp["method"], "path": stp["path"],
                     "json": stp["json"], "raw": stp["raw"], "headers": stp["headers"], "anon": stp["anon"],
                     "registrar": reg, "sql": stp["sql"], "at": T}
        if resp is None:
            rec.update(status=500, internal=True)
        else:
            rec["status"] = resp.status_code
            ct = resp.headers.get("content-type", "")
            if "json" in ct:
                rec["text"] = norm_text(resp.text)
            elif ct.startswith("application/zip"):
                rec["zip"] = zip_view(resp.content)
            elif ct.startswith("text/csv"):
                rec["text"] = norm_text(resp.content.decode("utf-8"))
            elif ct.startswith("image/"):
                rec["image"] = {"sha256": hashlib.sha256(resp.content).hexdigest(), "len": len(resp.content)} \
                    if resp.content[:2] != b"\xff\xd8" or len(resp.content) < 40 else {"jpeg": True}
            else:
                rec["text"] = norm_text(resp.text)
            rec["ctype"] = ct
            rec["disposition"] = resp.headers.get("content-disposition")
        if reg is not None:
            rec["calls"] = json.loads(norm_text(json.dumps(discord.calls, ensure_ascii=False)))
        if stp["snap"]:
            rec["db"] = json.loads(norm_text(json.dumps(await snapshot(st, stp["snap"] == "errors"), ensure_ascii=False)))
        out_lines.append(json.dumps(rec, ensure_ascii=False))
    out_lines.append(json.dumps({"kind": "final", "db": json.loads(norm_text(json.dumps(await snapshot(st, True), ensure_ascii=False)))},
                                ensure_ascii=False))
    await http.aclose()
    await st.db.close()


def child(variant: str, out: Path) -> None:
    """一个场景一个进程：旧应用的设置按环境变量缓存，同进程里换数据目录不干净。"""
    global T, DAY, YDAY
    if variant == "early":
        T, DAY, YDAY = 1790025600.0, "2026-09-21", "2026-09-20"       # 2026-09-22 05:20，服务日还是 09-21
    lines: list[str] = []
    code = 0
    try:
        asyncio.new_event_loop().run_until_complete(run_variant(variant, lines))
    except BaseException:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        code = 1
    finally:
        out.write_text("\n".join(lines) + "\n", encoding="utf-8")
        sys.stdout.flush()
        sys.stderr.flush()
        os._exit(code)


def main_() -> None:
    import subprocess
    OUT.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []
    for variant in ("main", "reg", "merge", "early"):
        with tempfile.TemporaryDirectory() as td:
            part = Path(td) / "part.jsonl"
            r = subprocess.run([sys.executable, __file__, "--variant", variant, str(part)], capture_output=True, text=True)
            if r.returncode != 0:
                sys.stderr.write(r.stderr)
                raise SystemExit(f"场景 {variant} 失败")
            lines += part.read_text(encoding="utf-8").splitlines()
    data = ("\n".join(lines) + "\n").encode("utf-8")
    path = OUT / "admin_routes.jsonl.gz"
    path.write_bytes(gzip.compress(data, compresslevel=9, mtime=0))
    print(f"{path.relative_to(ROOT)}: {len(lines)} 条（{len(data) // 1024} KB 未压缩）")


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "--variant":
        child(sys.argv[2], Path(sys.argv[3]))
    else:
        main_()
