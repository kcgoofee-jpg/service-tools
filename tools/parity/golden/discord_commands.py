"""奶妹 / 燕双鹰（crates/gate-discord）的 golden 夹具：在旧 Python（integration/*.py + discord.py）上生成。

怎么生成才「最可靠」：不解析装饰器，而是**真的跑旧代码**——
  1. `build_client()` 建出真正的 CommandTree，`tree.sync()` 走真实的 HTTP 层，把「Discord 实际会收到的」
     `PUT /applications/{app}/guilds/{guild}/commands` 整个请求体截下来；
  2. 用真实的 `discord.Interaction` 对象 + 真实的 CommandTree 分发 + 真实的 `backend()` / `handle_register()`，
     只把最底层的网络换成假的：httpx（网关桥接）与 aiohttp session（Discord REST），逐条记录发出去的请求；
  3. `@奶妹` 回复、`sync_loop` 心跳、燕双鹰的在线状态也同样截取。
Rust 侧（tests/*.rs）用同样的输入跑一遍，要求「发给网关的请求」与「发给 Discord 的请求」逐项一致。

产物（crates/gate-discord/tests/golden/）：
  - discord_commands.json   Discord 收到的命令定义 + Intent 位 + 燕双鹰的在线状态载荷
  - discord_flows.jsonl     每行一个场景：输入（交互载荷 / 网关脚本）+ 期望（桥接请求 / Discord 请求）
  - discord_copy.json       三个 Python 源文件里出现过的全部字符串常量（含 f-string 的固定片段），用来核对文案

运行（仓库根目录；discord.py>=2.6 才有弹窗里的 Label 组件）：
    uv run --python 3.12 --with "discord.py>=2.6" --with "httpx>=0.27" python tools/parity/golden/discord_commands.py

夹具只含合成数据：ID 一律 9000000000000000xx 段，密钥 / 地址是 test-secret / bridge.test。
"""
from __future__ import annotations

import ast
import asyncio
import contextlib
import copy
import gzip
import io
import json
import os
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
INTEGRATION = ROOT / "integration"
OUT = ROOT / "crates" / "gate-discord" / "tests" / "golden"

# ---------- 合成环境（必须在 import 旧代码之前设置） ----------
GUILD = "900000000000000001"
OWNER = "900000000000000002"
APP = "900000000000000003"
CHANNEL = "900000000000000004"
ALICE_ID = "900000000000000005"
BOB_ID = "900000000000000006"
TARGET_ID = "900000000000000007"
BOT_ID = "900000000000000099"
OTHER_GUILD = "900000000000000008"
SECRET = "test-secret"
BACKEND = "http://bridge.test"
SITE = "https://site.test"
NOW = 1_760_000_000.0           # 固定的 time.time()
READY_AT = 1_759_999_000.5      # 固定的 sync_loop 起始时间

os.environ["DISCORD_GUILD_ID"] = GUILD
os.environ["REGISTRATION_BRIDGE_SECRET"] = SECRET
os.environ["REGISTRATION_BACKEND_URL"] = BACKEND + "/"
os.environ["SITE_URL"] = SITE + "/"
os.environ.pop("PRAISE_API_KEY", None)
sys.path.insert(0, str(INTEGRATION))

import discord  # noqa: E402
import httpx  # noqa: E402

import discord_bot  # noqa: E402
import discord_registration  # noqa: E402
import warden_bot  # noqa: E402

API_BASE = "https://discord.com/api/v10"


class Mono:
    t = 0.0


discord_bot.time = types.SimpleNamespace(time=lambda: NOW, monotonic=lambda: Mono.t)


# ---------- 记录器与假网络 ----------

class Rec:
    def __init__(self) -> None:
        self.bridge: list[dict] = []
        self.discord: list[dict] = []


REC = Rec()
SCRIPT: dict = {"bridge": {}, "discord": {}}


def _norm_query(params) -> dict:
    return {str(k): str(v) for k, v in (params or {}).items()}


class FakeHttpxResponse:
    def __init__(self, spec: dict) -> None:
        self.status_code = spec["status"]
        self._spec = spec

    def json(self):
        if "json" in self._spec:
            return copy.deepcopy(self._spec["json"])
        return json.loads(self._spec["text"])     # 非 JSON 文本：抛 ValueError，与 httpx 一致


def _bridge_spec(path: str) -> dict:
    spec = SCRIPT["bridge"].get(path, SCRIPT["bridge"].get("*"))
    assert spec is not None, f"场景没有给 {path} 配置桥接响应"
    return spec


class FakeAsyncClient:
    def __init__(self, timeout=None, **_kw) -> None:
        self.timeout = timeout

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_a):
        return False

    async def _do(self, method: str, url: str, json_body, headers):
        path = url.removeprefix(BACKEND)
        assert url.startswith(BACKEND), url
        REC.bridge.append({"method": method, "path": path, "authorization": (headers or {}).get("Authorization"),
                           "timeout": self.timeout, "json": copy.deepcopy(json_body)})
        spec = _bridge_spec(path)
        if "error" in spec:
            raise httpx.ConnectError("synthetic connect error")
        return FakeHttpxResponse(spec)

    async def post(self, url, json=None, headers=None, **_kw):
        return await self._do("POST", url, json, headers)

    async def request(self, method, url, json=None, headers=None, **_kw):
        return await self._do(method, url, json, headers)


httpx.AsyncClient = FakeAsyncClient


class FakeDiscordResponse:
    def __init__(self, status: int, body) -> None:
        self.status = status
        self.reason = "synthetic"
        self.headers = {"content-type": "application/json"}
        self._body = body

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_a):
        return False

    async def text(self, encoding="utf-8"):
        return json.dumps(self._body)

    async def json(self):
        return self._body


MESSAGE_STUB = {"id": "900000000000000090", "channel_id": CHANNEL, "type": 0, "content": "", "attachments": [],
                "embeds": [], "mentions": [], "mention_roles": [], "pinned": False, "mention_everyone": False,
                "tts": False, "timestamp": "2024-01-01T00:00:00+00:00", "edited_timestamp": None, "flags": 64,
                "author": {"id": BOT_ID, "username": "naimei", "discriminator": "0", "avatar": None}}


class FakeSession:
    """替代 aiohttp.ClientSession：discord.py 的交互回调 / 跟进消息走 webhook 适配器，直接调 session.request。"""

    def request(self, method, url, data=None, headers=None, params=None, **_kw):
        kind = "callback" if url.endswith("/callback") else "followup"
        REC.discord.append({"method": method, "path": url.removeprefix(API_BASE), "query": _norm_query(params),
                            "json": json.loads(data) if isinstance(data, str) else data})
        fail = SCRIPT["discord"].get(kind)
        if fail:
            return FakeDiscordResponse(fail["status"], fail["json"])
        if kind == "callback":
            return FakeDiscordResponse(200, {"interaction": {"id": "900000000000000010", "type": 2,
                                                             "response_message_id": None,
                                                             "response_message_loading": False,
                                                             "response_message_ephemeral": False},
                                             "resource": {"type": 5}})
        return FakeDiscordResponse(200, MESSAGE_STUB)


async def fake_http_request(route, *, files=None, form=None, **kwargs):
    """替代 HTTPClient.request：@奶妹回复（POST messages）与 tree.sync（PUT commands）走这里。"""
    body = copy.deepcopy(kwargs.get("json"))
    if isinstance(body, dict) and "nonce" in body:
        body["nonce"] = "<nonce>"        # secrets.randbits(64)：每次不同，夹具里换成占位符
    REC.discord.append({"method": route.method, "path": route.url.removeprefix(API_BASE),
                        "query": _norm_query(kwargs.get("params")), "json": body})
    fail = SCRIPT["discord"].get("message")
    if fail:
        raise discord.HTTPException(FakeDiscordResponse(fail["status"], fail["json"]), fail["json"])
    if route.method == "POST":
        out = dict(MESSAGE_STUB)
        out["content"] = (kwargs.get("json") or {}).get("content", "")
        return out
    return []


# ---------- 客户端 / 状态 ----------

CLIENT, TREE, GUILD_OBJ = discord_bot.build_client()
STATE = CLIENT._connection
STATE.http._HTTPClient__session = FakeSession()
STATE.http.request = fake_http_request
STATE.application_id = int(APP)
STATE.user = discord.ClientUser(state=STATE, data={
    "id": BOT_ID, "username": "naimei", "discriminator": "0", "avatar": None, "bot": True, "global_name": None,
    "verified": True, "mfa_enabled": False})
GUILD_DATA = {"id": GUILD, "name": "测试服", "owner_id": OWNER, "roles": [], "emojis": [], "stickers": [],
              "features": [], "members": [],
              "channels": [{"id": CHANNEL, "type": 0, "name": "general", "position": 0, "permission_overwrites": []}]}
STATE._add_guild(discord.Guild(data=GUILD_DATA, state=STATE))


def user_obj(uid: str, name: str, global_name=None, avatar=None) -> dict:
    return {"id": uid, "username": name, "global_name": global_name, "avatar": avatar, "discriminator": "0"}


ALICE = user_obj(ALICE_ID, "alice", "Alice", "a1b2c3")
BOB = user_obj(BOB_ID, "bob", None, None)
TARGET = user_obj(TARGET_ID, "carol", "Carol", None)


def member_obj(user: dict, nick=None) -> dict:
    return {"user": user, "nick": nick, "roles": [], "joined_at": "2024-01-01T00:00:00+00:00", "deaf": False,
            "mute": False, "flags": 0, "permissions": "0"}


_counter = [10]


def next_ids() -> tuple[str, str]:
    _counter[0] += 1
    n = _counter[0]
    return f"9000000000000000{n:02d}", f"tok-{n:02d}"


def command_interaction(command: str, options=None, user=ALICE, nick=None, guild_id=GUILD, resolved=None) -> dict:
    """`guild_id` 是交互本身所在的服务器；命令表按 `data.guild_id` 查（公会级命令只会从本服务器递过来），
    所以这里 `data.guild_id` 固定是本服务器——「别的服务器」只用来触发 /register 里的服务器校验分支。"""
    iid, token = next_ids()
    data = {"id": "900000000000000050", "name": command, "type": 1, "guild_id": GUILD}
    if options:
        data["options"] = options
    if resolved:
        data["resolved"] = resolved
    return {"id": iid, "application_id": APP, "type": 2, "token": token, "version": 1, "guild_id": guild_id,
            "channel_id": CHANNEL, "app_permissions": "0", "attachment_size_limit": 10485760, "locale": "zh-CN",
            "guild_locale": "zh-CN", "member": member_obj(user, nick), "data": data}


def modal_interaction(custom_id: str, components: list, user=ALICE) -> dict:
    iid, token = next_ids()
    return {"id": iid, "application_id": APP, "type": 5, "token": token, "version": 1, "guild_id": GUILD,
            "channel_id": CHANNEL, "app_permissions": "0", "attachment_size_limit": 10485760, "locale": "zh-CN",
            "guild_locale": "zh-CN", "member": member_obj(user),
            "data": {"custom_id": custom_id, "components": components}}


def member_options(uid=TARGET_ID, user=TARGET) -> tuple[list, dict]:
    return ([{"name": "member", "type": 6, "value": uid}],
            {"users": {uid: user}, "members": {uid: {"roles": [], "joined_at": "2024-01-01T00:00:00+00:00",
                                                       "deaf": False, "mute": False, "flags": 0,
                                                       "permissions": "0"}}})


# ---------- 运行一个步骤 ----------

async def settle_tasks() -> None:
    """等后台任务（弹窗提交走 create_task）把活干完：Discord / 桥接请求数连续几轮不再变化。"""
    last, stable = -1, 0
    for _ in range(400):
        await asyncio.sleep(0.005)
        n = len(REC.discord) + len(REC.bridge)
        stable = stable + 1 if n == last else 0
        last = n
        if stable >= 8:
            return


@contextlib.contextmanager
def env_override(cfg: dict):
    saved = {k: os.environ.get(k) for k in ("REGISTRATION_BRIDGE_SECRET",)}
    mode = cfg.get("secret", "set")
    if mode == "unset":
        os.environ.pop("REGISTRATION_BRIDGE_SECRET", None)
    elif mode == "empty":
        os.environ["REGISTRATION_BRIDGE_SECRET"] = ""
    try:
        yield
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def deep_replace(obj, mapping: dict):
    if isinstance(obj, str):
        return mapping.get(obj, obj)
    if isinstance(obj, list):
        return [deep_replace(x, mapping) for x in obj]
    if isinstance(obj, dict):
        return {k: deep_replace(v, mapping) for k, v in obj.items()}
    return obj


class Scenario:
    """一个场景 = 若干步骤，共享同一个 cfg（环境）。"""

    def __init__(self, name: str, cfg: dict | None = None) -> None:
        self.name = name
        self.cfg = cfg or {}
        self.steps: list[dict] = []
        self.modal_ids: dict[str, str] = {}      # 真实 custom_id → 占位符

    async def command(self, payload: dict, bridge: dict, discord_script: dict | None = None) -> None:
        step = {"kind": "command", "interaction": payload, "bridge": bridge, "discord": discord_script or {}}
        with env_override(self.cfg):
            await self._run(step, lambda: self._call_command(payload))

    async def _call_command(self, payload: dict) -> None:
        interaction = discord.Interaction(data=copy.deepcopy(payload), state=STATE)
        # 与 CommandTree._from_interaction 里的 wrapper 一致：命令没找到之类的 AppCommandError 也会走 tree.error
        try:
            await TREE._call(interaction)
        except discord.app_commands.AppCommandError as e:
            await TREE._dispatch_error(interaction, e)

    async def modal_submit(self, custom_id_placeholder: str, selects: list[str | None],
                           bridge: dict, discord_script: dict | None = None, user=ALICE) -> None:
        """`selects`：每个下拉框选中的值；None = 没有值（提交里 values 为空）。"""
        reverse = {v: k for k, v in self.modal_ids.items()}
        real_modal = reverse.get(custom_id_placeholder, custom_id_placeholder)
        comps_real, comps_ph = [], []
        for i, value in enumerate(selects):
            ph = f"<sel{i}>"
            real = reverse.get(ph, ph)
            for target, cid in ((comps_real, real), (comps_ph, ph)):
                target.append({"type": 18, "id": 2 * i + 1, "component": {
                    "type": 3, "id": 2 * i + 2, "custom_id": cid, "values": [] if value is None else [value]}})
        real_payload = modal_interaction(real_modal, comps_real, user)
        ph_payload = copy.deepcopy(real_payload)
        ph_payload["data"] = {"custom_id": custom_id_placeholder, "components": comps_ph}
        step = {"kind": "modal_submit", "interaction": ph_payload, "bridge": bridge, "discord": discord_script or {}}
        with env_override(self.cfg):
            await self._run(step, lambda: self._call_modal(real_payload), settle=True)

    async def _call_modal(self, payload: dict) -> None:
        interaction = discord.Interaction(data=copy.deepcopy(payload), state=STATE)
        data = payload["data"]
        STATE._view_store.dispatch_modal(data["custom_id"], interaction, data["components"], data.get("resolved", {}))

    async def _run(self, step: dict, action, settle: bool = False) -> None:
        REC.bridge, REC.discord = [], []
        SCRIPT["bridge"], SCRIPT["discord"] = step["bridge"], step["discord"]
        sink = io.StringIO()
        with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
            await action()
            if settle:
                await settle_tasks()
        # 弹窗 custom_id 是随机的：换成稳定的占位符，Rust 侧按同样规则还原
        for call in REC.discord:
            body = call["json"] or {}
            if body.get("type") == 9:
                modal = body["data"]
                self.modal_ids[modal["custom_id"]] = "<modal>"
                for i, comp in enumerate(modal["components"]):
                    self.modal_ids[comp["component"]["custom_id"]] = f"<sel{i}>"
        step["expect"] = {"bridge_requests": deep_replace(REC.bridge, self.modal_ids),
                          "discord_calls": deep_replace(REC.discord, self.modal_ids)}
        self.steps.append(step)

    def record(self) -> dict:
        return {"name": self.name, "cfg": self.cfg, "steps": self.steps}


# ---------- 场景 ----------

def ok(json_body, status=200) -> dict:
    return {"status": status, "json": json_body}


def err(detail, status) -> dict:
    return {"status": status, "json": {"detail": detail}}


def text(raw: str, status=200) -> dict:
    return {"status": status, "text": raw}


CONNECT_ERROR = {"error": "connect"}

QUOTA_BASE = {
    "enabled": True, "expires_at": None, "daily_images": 20, "images": 5, "legacy": 4, "anlas": 0,
    "daily_anlas": 0, "daily_v5": 0, "v5": 0, "image_model_scope": "all", "reset_hour": 7, "quiet_now": False,
    "quiet_window": "",
    "features": [{"id": "image", "label": "文生图", "on": True}, {"id": "upscale", "label": "放大", "on": False}],
}
INFO_BASE = {"open": True, "active": 12, "max": 50, "default_features": [], "daily_images": 20, "daily_v5": 3,
             "notice": "", "upstream": {"status": "ok", "recent": 0, "failed": 0, "image_cooldown_seconds": 0}}


def with_(base: dict, **kw) -> dict:
    out = copy.deepcopy(base)
    out.update(kw)
    return out


def info_up(**up) -> dict:
    out = copy.deepcopy(INFO_BASE)
    out["upstream"].update(up)
    return out


QUESTIONS = [
    {"id": "score", "label": "整体用下来感觉怎么样？", "options": ["非常满意", "满意", "一般", "不太满意", "很不满意"]},
    {"id": "pain", "label": "目前最困扰你的是？", "options": ["排队太久 / 经常提示人多", "每天张数不够用", "没什么困扰"]},
    {"id": "freq", "label": "你多久用一次公益站？", "options": ["每天都用", "每周几次", "偶尔用", "刚来还没怎么用"]},
]
LONG_QUESTIONS = [
    {"id": f"q{i}", "label": "题" * 60, "options": [f"选项{j}" + "x" * 120 for j in range(30)]} for i in range(7)
]


async def build_command_scenarios() -> list[dict]:
    out: list[dict] = []

    async def one(name, payload, bridge, cfg=None, discord_script=None):
        s = Scenario(name, cfg)
        await s.command(payload, bridge, discord_script)
        out.append(s.record())

    # ---- /register ----
    reg = lambda **kw: command_interaction("register", **kw)  # noqa: E731
    await one("register_ok_plain", reg(), {"/self-register/issue": ok({"message": "领取成功！你的 Key：nai-abc"})})
    await one("register_ok_with_invite_key",
              reg(options=[{"name": "key", "type": 3, "value": "  nai-invite-xyz  "}]),
              {"/self-register/issue": ok({"message": "已绑定邀请 Key。"})})
    await one("register_ok_long_invite_key_truncated",
              reg(options=[{"name": "key", "type": 3, "value": "k" * 120}]),
              {"/self-register/issue": ok({"message": "已绑定。"})})
    await one("register_ok_without_message", reg(), {"/self-register/issue": ok({})})
    await one("register_ok_empty_message", reg(), {"/self-register/issue": ok({"message": ""})})
    await one("register_ok_numeric_message", reg(), {"/self-register/issue": ok({"message": 42})})
    await one("register_user_without_global_name_uses_nick", reg(user=BOB, nick="Bobby"),
              {"/self-register/issue": ok({"message": "ok"})})
    await one("register_user_without_global_name_or_nick", reg(user=BOB),
              {"/self-register/issue": ok({"message": "ok"})})
    await one("register_long_names", reg(user=user_obj(ALICE_ID, "名" * 100, "G" * 100, "h" * 200)),
              {"/self-register/issue": ok({"message": "ok"})})
    await one("register_403_detail", reg(), {"/self-register/issue": err("你的 Discord 账号注册时间太短。", 403)})
    await one("register_403_no_detail", reg(), {"/self-register/issue": ok({}, 403)})
    await one("register_403_not_json", reg(), {"/self-register/issue": text("<html>", 403)})
    await one("register_403_detail_list", reg(), {"/self-register/issue": err([{"msg": "x"}], 403)})
    await one("register_401", reg(), {"/self-register/issue": err("未授权", 401)})
    await one("register_500", reg(), {"/self-register/issue": err("boom", 500)})
    await one("register_200_not_json", reg(), {"/self-register/issue": text("oops")})
    await one("register_200_json_list_is_error", reg(), {"/self-register/issue": ok([1, 2])})
    await one("register_503", reg(), {"/self-register/issue": err("自助领 Key 尚未配置", 503)})
    await one("register_connect_error", reg(), {"/self-register/issue": CONNECT_ERROR})
    await one("register_wrong_guild", reg(guild_id=OTHER_GUILD), {})
    await one("register_secret_empty", reg(), {}, cfg={"secret": "empty"})
    await one("register_secret_unset", reg(), {}, cfg={"secret": "unset"})
    await one("register_defer_fails", reg(), {"/self-register/issue": ok({"message": "ok"})},
              discord_script={"callback": {"status": 404, "json": {"message": "Unknown interaction", "code": 10062}}})
    await one("register_followup_fails", reg(), {"/self-register/issue": ok({"message": "ok"})},
              discord_script={"followup": {"status": 400, "json": {"message": "Invalid Form Body", "code": 50035}}})

    # ---- /quota ----
    q = lambda: command_interaction("quota")  # noqa: E731
    await one("quota_minimal", q(), {"/self-register/quota": ok(QUOTA_BASE), "/self-register/info": ok(INFO_BASE)})
    await one("quota_all_branches", q(), {
        "/self-register/quota": ok(with_(QUOTA_BASE, daily_v5=3, v5=1, daily_anlas=100, anlas=42, quiet_now=True,
                                        quiet_window="2:00–6:00", image_model_scope="legacy",
                                        expires_at=NOW + 86400 * 3.5, enabled=False)),
        "/self-register/info": ok(info_up(status="degraded", recent=12, failed=7, image_cooldown_seconds=33))})
    await one("quota_legacy_missing_and_reset_hour_null", q(), {
        "/self-register/quota": ok({k: v for k, v in with_(QUOTA_BASE, reset_hour=None).items() if k != "legacy"}),
        "/self-register/info": ok(info_up(status="idle"))})
    await one("quota_anlas_key_missing", q(), {
        "/self-register/quota": ok({k: v for k, v in with_(QUOTA_BASE, daily_anlas=9).items() if k != "anlas"}),
        "/self-register/info": ok(INFO_BASE)})
    await one("quota_expired_and_no_open_features", q(), {
        "/self-register/quota": ok(with_(QUOTA_BASE, expires_at=NOW - 86400 * 10,
                                        features=[{"id": "image", "label": "文生图", "on": False}])),
        "/self-register/info": ok(info_up(status="ok", recent=3))})
    await one("quota_unknown_upstream_status", q(), {
        "/self-register/quota": ok(QUOTA_BASE), "/self-register/info": ok(info_up(status="weird"))})
    await one("quota_expires_int", q(), {
        "/self-register/quota": ok(with_(QUOTA_BASE, expires_at=int(NOW) + 86400 * 30)),
        "/self-register/info": ok(INFO_BASE)})
    await one("quota_info_unavailable", q(), {
        "/self-register/quota": ok(QUOTA_BASE), "/self-register/info": err("boom", 500)})
    await one("quota_info_connect_error", q(), {
        "/self-register/quota": ok(QUOTA_BASE), "/self-register/info": CONNECT_ERROR})
    await one("quota_not_claimed", q(), {"/self-register/quota": err("你还没有领取 Key，请先使用 /register。", 404)})
    await one("quota_forbidden_wrong_guild", q(), {"/self-register/quota": err("请在指定服务器使用该命令。", 403)})
    await one("quota_detail_missing", q(), {"/self-register/quota": ok({}, 500)})
    await one("quota_non_json_body", q(), {"/self-register/quota": text("<html>bad gateway</html>", 502)})
    await one("quota_422_validation_list", q(), {"/self-register/quota": err([{"loc": ["body", "x"], "msg": "bad"}], 422)})
    await one("quota_non200_json_list", q(), {"/self-register/quota": ok([1], 500)})
    await one("quota_connect_error", q(), {"/self-register/quota": CONNECT_ERROR})
    await one("quota_missing_daily_images_is_error", q(), {
        "/self-register/quota": ok({k: v for k, v in QUOTA_BASE.items() if k != "daily_images"})})
    await one("quota_missing_v5_when_daily_v5_set_is_error", q(), {
        "/self-register/quota": ok({k: v for k, v in with_(QUOTA_BASE, daily_v5=3).items() if k != "v5"})})
    await one("quota_info_missing_upstream_is_error", q(), {
        "/self-register/quota": ok(QUOTA_BASE), "/self-register/info": ok({"open": True})})
    await one("quota_secret_unset", q(), {}, cfg={"secret": "unset"})
    await one("quota_defer_fails", q(), {"/self-register/quota": ok(QUOTA_BASE), "/self-register/info": ok(INFO_BASE)},
              discord_script={"callback": {"status": 404, "json": {"message": "Unknown interaction", "code": 10062}}})

    # ---- /resetkey /help ----
    await one("resetkey_ok", command_interaction("resetkey"), {"/self-register/resetkey": ok({"key": "nai-newkey123"})})
    await one("resetkey_not_claimed", command_interaction("resetkey"),
              {"/self-register/resetkey": err("你还没有领取 Key，请先使用 /register。", 404)})
    await one("resetkey_missing_key_is_error", command_interaction("resetkey"), {"/self-register/resetkey": ok({})})
    await one("help_open_no_notice", command_interaction("help"), {"/self-register/info": ok(INFO_BASE)})
    await one("help_closed_with_notice", command_interaction("help"),
              {"/self-register/info": ok(with_(INFO_BASE, open=False, notice="今晚 22:00 维护"))})
    await one("help_info_unavailable", command_interaction("help"), {"/self-register/info": err("boom", 500)})
    await one("help_info_connect_error", command_interaction("help"), {"/self-register/info": CONNECT_ERROR})
    await one("help_notice_not_string_is_error", command_interaction("help"),
              {"/self-register/info": ok(with_(INFO_BASE, notice=5))})
    await one("help_open_missing_is_error", command_interaction("help"), {"/self-register/info": ok({"notice": ""})})

    # ---- 管理命令 ----
    for state, label in (("on", "on"), ("off", "off")):
        await one(f"open_{label}", command_interaction("open", options=[{"name": "state", "type": 3, "value": state}]),
                  {"/self-register/ops": ok({"message": "已开放领 Key。" if state == "on" else "已关闭领 Key。"})})
    await one("open_not_admin", command_interaction("open", options=[{"name": "state", "type": 3, "value": "on"}]),
              {"/self-register/ops": err("没有管理员权限。", 403)})
    await one("open_invalid_choice", command_interaction("open", options=[{"name": "state", "type": 3, "value": "maybe"}]), {})
    await one("open_message_missing_is_error",
              command_interaction("open", options=[{"name": "state", "type": 3, "value": "on"}]),
              {"/self-register/ops": ok({})})
    await one("limit_ok", command_interaction("limit", options=[{"name": "count", "type": 4, "value": 25}]),
              {"/self-register/ops": ok({"message": "名额上限已设为 25。"})})
    await one("limit_zero", command_interaction("limit", options=[{"name": "count", "type": 4, "value": 0}]),
              {"/self-register/ops": ok({"message": "名额上限已设为 不限。"})})
    # discord.py 的 Range 在服务端不再校验范围（只有 Discord 客户端限制），5000 会原样转给网关
    await one("limit_out_of_range_is_forwarded_as_is",
              command_interaction("limit", options=[{"name": "count", "type": 4, "value": 5000}]),
              {"/self-register/ops": ok({"message": "名额上限已设为 5000。"})})
    await one("limit_422", command_interaction("limit", options=[{"name": "count", "type": 4, "value": 3}]),
              {"/self-register/ops": err("请输入数字", 422)})
    opts, res = member_options()
    await one("ban_ok", command_interaction("ban", options=opts, resolved=res),
              {"/self-register/ops": ok({"message": "已永久禁止该账号领取，并撤销其 Key 与身份组。"})})
    await one("unban_ok", command_interaction("unban", options=opts, resolved=res),
              {"/self-register/ops": ok({"message": "已解除禁止。"})})
    await one("unban_not_admin", command_interaction("unban", options=opts, resolved=res),
              {"/self-register/ops": err("没有管理员权限。", 403)})
    await one("ops_connect_error", command_interaction("ban", options=opts, resolved=res),
              {"/self-register/ops": CONNECT_ERROR})
    await one("slots_ok", command_interaction("slots"),
              {"/self-register/slots": ok({"active": 12, "max": 50, "open": True, "reset_at": "07:00"})})
    await one("slots_unlimited_closed", command_interaction("slots"),
              {"/self-register/slots": ok({"active": 3, "max": 0, "open": False, "reset_at": ""})})
    await one("slots_open_missing", command_interaction("slots"),
              {"/self-register/slots": ok({"active": 3, "max": 9, "reset_at": None})})
    await one("slots_not_admin", command_interaction("slots"), {"/self-register/slots": err("没有管理员权限。", 403)})
    await one("slots_missing_active_is_error", command_interaction("slots"),
              {"/self-register/slots": ok({"max": 9, "reset_at": ""})})
    await one("revoke_ok", command_interaction("revoke", options=opts, resolved=res),
              {"/self-register/revoke": ok({"ok": True})})
    await one("revoke_nothing_to_revoke", command_interaction("revoke", options=opts, resolved=res),
              {"/self-register/revoke": err("该用户没有已领取的 Key。", 404)})
    await one("revoke_not_admin", command_interaction("revoke", options=opts, resolved=res),
              {"/self-register/revoke": err("没有管理员权限。", 403)})

    # ---- 未知命令 ----
    await one("unknown_command", command_interaction("nope"), {})
    return out


async def build_feedback_scenarios() -> list[dict]:
    out: list[dict] = []

    async def opened(name: str, questions_resp: dict, discord_script=None) -> Scenario:
        s = Scenario(name)
        await s.command(command_interaction("反馈"), {"/self-register/feedback/questions": questions_resp}, discord_script)
        return s

    s = await opened("feedback_open_ok", ok({"questions": QUESTIONS}))
    out.append(s.record())

    s = await opened("feedback_open_truncates_like_python", ok({"questions": LONG_QUESTIONS}))
    out.append(s.record())

    for name, resp in (("feedback_open_backend_error", err("boom", 500)),
                       ("feedback_open_no_questions", ok({"questions": []})),
                       ("feedback_open_questions_missing", ok({})),
                       ("feedback_open_not_json", text("<html>")),
                       ("feedback_open_connect_error", CONNECT_ERROR),
                       ("feedback_open_question_missing_options_is_error",
                        ok({"questions": [{"id": "a", "label": "b"}]}))):
        s = await opened(name, resp)
        out.append(s.record())

    s = await opened("feedback_open_modal_rejected", ok({"questions": [{"id": "a", "label": "b", "options": []}]}),
                     discord_script={"callback": {"status": 400, "json": {"message": "Invalid Form Body", "code": 50035}}})
    out.append(s.record())

    receipt = "收到啦，谢谢你的反馈～奶妹会转给站长 ✨"
    for name, selects, resp in (
        ("feedback_submit_ok", ["满意", "每天张数不够用", "每周几次"], ok({"message": receipt})),
        ("feedback_submit_missing_selection", ["满意", None, "每周几次"], ok({"message": receipt})),
        ("feedback_submit_cooldown", ["满意", "没什么困扰", "偶尔用"], err("刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。", 400)),
        ("feedback_submit_no_message_key", ["满意", "没什么困扰", "偶尔用"], ok({})),
        ("feedback_submit_json_list", ["满意", "没什么困扰", "偶尔用"], ok([1])),
        ("feedback_submit_connect_error", ["满意", "没什么困扰", "偶尔用"], CONNECT_ERROR),
        ("feedback_submit_forbidden", ["满意", "没什么困扰", "偶尔用"], err("请在指定服务器使用该命令。", 403)),
    ):
        s = await opened(name, ok({"questions": QUESTIONS}))
        await s.modal_submit("<modal>", selects, {"/self-register/feedback": resp})
        out.append(s.record())

    s = await opened("feedback_submit_user_without_global_name", ok({"questions": QUESTIONS}))
    await s.modal_submit("<modal>", ["满意", "没什么困扰", "偶尔用"], {"/self-register/feedback": ok({"message": receipt})}, user=BOB)
    out.append(s.record())

    # 提交失败（defer 被拒）后弹窗仍然有效：可以再提交一次
    s = await opened("feedback_submit_defer_fails_then_retry", ok({"questions": QUESTIONS}))
    await s.modal_submit("<modal>", ["满意", "没什么困扰", "偶尔用"], {"/self-register/feedback": ok({"message": receipt})},
                         discord_script={"callback": {"status": 404, "json": {"message": "Unknown interaction", "code": 10062}}})
    await s.modal_submit("<modal>", ["满意", "没什么困扰", "偶尔用"], {"/self-register/feedback": ok({"message": receipt})})
    out.append(s.record())

    # 成功提交后弹窗作废（stop()）：同一个弹窗再提交会被忽略
    s = await opened("feedback_submit_twice_second_ignored", ok({"questions": QUESTIONS}))
    await s.modal_submit("<modal>", ["满意", "没什么困扰", "偶尔用"], {"/self-register/feedback": ok({"message": receipt})})
    await s.modal_submit("<modal>", ["满意", "没什么困扰", "偶尔用"], {"/self-register/feedback": ok({"message": receipt})})
    out.append(s.record())

    # 不认识的弹窗：什么都不发
    s = Scenario("feedback_submit_unknown_modal")
    await s.modal_submit("unknown-modal-id", ["满意"], {})
    out.append(s.record())
    return out


# ---------- @奶妹 ----------

def message_payload(mid: str, author_id: str, content: str, bot=False, guild_id=GUILD) -> dict:
    return {"id": mid, "channel_id": CHANNEL, "guild_id": guild_id, "type": 0, "content": content,
            "timestamp": "2024-01-01T00:00:00+00:00", "edited_timestamp": None, "tts": False,
            "mention_everyone": False, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
            "pinned": False, "flags": 0,
            "author": {"id": author_id, "username": "u" + author_id[-2:], "discriminator": "0", "avatar": None,
                       "bot": bot},
            "member": {"roles": [], "joined_at": "2024-01-01T00:00:00+00:00", "deaf": False, "mute": False,
                       "flags": 0}}


async def run_message(payload: dict, mono: float, discord_script=None) -> list[dict]:
    REC.bridge, REC.discord = [], []
    SCRIPT["bridge"], SCRIPT["discord"] = {}, discord_script or {}
    Mono.t = mono
    sink = io.StringIO()
    with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        channel_state = STATE._get_guild(int(GUILD))
        message = discord.Message(state=STATE, channel=channel_state._resolve_channel(int(CHANNEL)), data=payload)
        await CLIENT.on_message(message)
    return copy.deepcopy(REC.discord)


async def build_message_scenarios() -> list[dict]:
    out: list[dict] = []
    tag = f"<@{BOT_ID}>"
    tag_bang = f"<@!{BOT_ID}>"
    CH2 = "900000000000000012"

    def seq(name: str, events: list[tuple]) -> dict:
        return {"name": name, "events": events}

    async def run_seq(name: str, specs: list[dict]) -> None:
        # 每个场景从全新的限流表开始：on_message 的 mention_last 在 build_client 闭包里，重建客户端最干净
        global CLIENT, TREE, STATE
        CLIENT, TREE, _g = discord_bot.build_client()
        STATE = CLIENT._connection
        STATE.http._HTTPClient__session = FakeSession()
        STATE.http.request = fake_http_request
        STATE.application_id = int(APP)
        STATE.user = discord.ClientUser(state=STATE, data={
            "id": BOT_ID, "username": "naimei", "discriminator": "0", "avatar": None, "bot": True,
            "global_name": None, "verified": True, "mfa_enabled": False})
        STATE._add_guild(discord.Guild(data=GUILD_DATA, state=STATE))
        events = []
        for i, spec in enumerate(specs):
            calls = await run_message(spec["payload"], spec["mono"], spec.get("discord"))
            events.append({"mono": spec["mono"], "message": spec["payload"], "discord": spec.get("discord", {}),
                           "expect": {"discord_calls": calls}})
        out.append({"name": name, "events": events})

    def m(i: int, author: str, content: str, mono: float, **kw) -> dict:
        return {"payload": message_payload(f"9000000000000000{40 + i}", author, content, **kw), "mono": mono}

    await run_seq("mention_replies_to_owner", [m(0, ALICE_ID, f"hi {tag}", 1000.0)])
    await run_seq("mention_with_bang_form", [m(0, ALICE_ID, f"{tag_bang} 在吗", 1000.0)])
    await run_seq("no_mention_no_reply", [m(0, ALICE_ID, "hello world", 1000.0)])
    await run_seq("other_user_mention_no_reply", [m(0, ALICE_ID, "<@900000000000000098> hi", 1000.0)])
    await run_seq("bot_author_ignored", [m(0, ALICE_ID, f"hi {tag}", 1000.0, bot=True)])
    await run_seq("rate_limit_windows", [
        m(0, ALICE_ID, tag, 1000.0),                # 回复
        m(1, BOB_ID, tag, 1010.0),                   # 全服 20 秒内：不回
        m(2, BOB_ID, tag, 1021.0),                   # 20 秒后换人：回（同频道 60 秒窗口：不回）
        m(3, BOB_ID, tag, 1062.0),                   # 频道 60 秒后：回
        m(4, ALICE_ID, tag, 1090.0),                 # Alice 的 5 分钟窗口：不回
        m(5, ALICE_ID, tag, 1301.0),                 # 5 分钟后：回
    ])
    await run_seq("reply_failure_still_counts_against_limits", [
        {**m(0, ALICE_ID, tag, 1000.0), "discord": {"message": {"status": 403, "json": {"message": "Missing Access", "code": 50001}}}},
        m(1, BOB_ID, tag, 1005.0),                   # 全服 20 秒窗口仍然生效
    ])
    return out


# ---------- 心跳 / 同步 ----------

class StopLoop(Exception):
    pass


async def build_sync_scenarios() -> list[dict]:
    out: list[dict] = []

    class StubClient:
        user = "naimei"
        latency = 0.0421

        def __init__(self, guild_name, latency=0.0421) -> None:
            self._g = guild_name
            self.latency = latency

        def is_closed(self):
            return False

        def get_guild(self, _gid):
            return types.SimpleNamespace(name=self._g) if self._g is not None else None

    async def stop_sleep(_secs):
        raise StopLoop

    discord_bot.asyncio = types.SimpleNamespace(sleep=stop_sleep, create_task=asyncio.create_task)
    try:
        for name, cfg_resp, guild_name, latency, script in (
            ("sync_report_basic", ok({"gallery_forum": "跑图", "gallery_like": 1, "gallery_ai": 1,
                                      "gallery_ai_daily": 5, "gallery_ai_model": "m", "unknown_key": 9}),
             "测试服", 0.0421, None),
            ("sync_report_guild_missing_and_slow_latency", ok({"gallery_like": 0}), None, 0.2566, None),
            ("sync_config_unavailable", err("boom", 500), "测试服", 0.0, None),
            ("sync_config_not_object", ok([1]), "测试服", 0.0421, None),
            ("sync_config_connect_error", CONNECT_ERROR, "测试服", 0.0421, None),
        ):
            REC.bridge, REC.discord = [], []
            SCRIPT["bridge"] = {"/self-register/bot/config": cfg_resp, "/self-register/bot/report": ok({"ok": True})}
            if cfg_resp is CONNECT_ERROR:
                SCRIPT["bridge"]["/self-register/bot/report"] = CONNECT_ERROR
            discord_bot.CONFIG.update({"gallery_forum": "跑图分享", "gallery_like": 0, "gallery_ai": 0,
                                       "gallery_ai_daily": 30, "gallery_ai_model": "LongCat-2.5-Preview"})
            sink = io.StringIO()
            with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
                try:
                    await discord_bot.sync_loop(StubClient(guild_name, latency))
                except StopLoop:
                    pass
            out.append({"name": name, "ready_at": NOW, "latency_s": latency, "guild_name": guild_name,
                        "bridge": SCRIPT["bridge"], "expect": {"bridge_requests": copy.deepcopy(REC.bridge),
                                                              "config_after": copy.deepcopy(discord_bot.CONFIG)}})
    finally:
        import asyncio as real_asyncio
        discord_bot.asyncio = real_asyncio
    return out


# ---------- 燕双鹰 / 命令同步 / 文案 ----------

async def capture_commands() -> dict:
    REC.bridge, REC.discord = [], []
    SCRIPT["bridge"], SCRIPT["discord"] = {}, {}
    await TREE.sync(guild=GUILD_OBJ)
    assert len(REC.discord) == 1, REC.discord
    call = REC.discord[0]
    return {"discord_py": discord.__version__, "sync_request": call,
            "intents": CLIENT.intents.value, "warden_intents": discord.Intents.none().value}


async def capture_warden_presence() -> dict:
    from discord.gateway import DiscordWebSocket
    sent: list = []
    warden = warden_bot.Warden(intents=discord.Intents.none())
    ws = DiscordWebSocket.__new__(DiscordWebSocket)

    async def fake_send(data):
        sent.append(json.loads(data))
    ws.send = fake_send
    warden.ws = ws
    await warden.change_presence(status=discord.Status.online,
                                 activity=discord.Activity(type=discord.ActivityType.watching, name="一人一 Key"))
    assert len(sent) == 1, sent
    return {"payload": sent[0]}


def string_constants() -> list[str]:
    found: set[str] = set()
    for name in ("discord_bot.py", "discord_registration.py", "warden_bot.py"):
        tree = ast.parse((INTEGRATION / name).read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                found.add(node.value)
    return sorted(found)


def dump(path: Path, obj) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"{path.relative_to(ROOT)}")


async def main() -> None:
    commands = await capture_commands()
    commands["warden_presence"] = await capture_warden_presence()
    dump(OUT / "discord_commands.json", commands)

    flows: list[dict] = []
    for build in (build_command_scenarios, build_feedback_scenarios, build_message_scenarios, build_sync_scenarios):
        got = await build()
        for rec in got:
            rec["group"] = build.__name__.removeprefix("build_").removesuffix("_scenarios")
        flows.extend(got)
    OUT.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps(f, ensure_ascii=False, separators=(",", ":")) for f in flows]
    data = ("\n".join(lines) + "\n").encode("utf-8")
    # 与 _golden.Writer.save 同一约定：超过 256 KB 存成 .jsonl.gz（mtime=0，重复生成逐字节一致）
    plain, gz = OUT / "discord_flows.jsonl", OUT / "discord_flows.jsonl.gz"
    for stale in (plain, gz):
        stale.unlink(missing_ok=True)
    target = gz if len(data) > 256 * 1024 else plain
    target.write_bytes(gzip.compress(data, compresslevel=9, mtime=0) if target is gz else data)
    print(f"{target.relative_to(ROOT)}: {len(flows)} 个场景")

    dump(OUT / "discord_copy.json", {"strings": string_constants()})


if __name__ == "__main__":
    asyncio.run(main())
