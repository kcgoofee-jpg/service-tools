"""管理后台 API。"""

from __future__ import annotations

import base64
import hashlib
import hmac
import math
import os
import shutil
import time
from urllib.parse import urlsplit
from typing import Any, Optional

import json

from fastapi import APIRouter, HTTPException, Request, Response
from fastapi.routing import APIRoute

from .action_log import ADMIN_ACTIONS, log_action, summarize

from . import features as feature_defs
from . import token_store
from . import ops
from .policy import gen_key
from .body import read_json_body
from .allowance import SETTING, read_alert_threshold
from .reconciliation import ReconciliationError
from .state import RUNTIME_LIMIT_BOUNDS

PREFIX = "/admin/api"
_AUDIT_BODY_LIMIT = 64 * 1024


class AuditedRoute(APIRoute):
    """所有会改动数据的后台接口自动写入操作日志（谁、何时、做了什么、对象、参数摘要、结果）。

    未登录的请求不记录（防止被刷），登录本身无论成败都记录。
    """

    def get_route_handler(self):
        handler = super().get_route_handler()
        route = self

        async def audited(request: Request):
            if request.method in ("GET", "HEAD", "OPTIONS"):
                return await handler(request)
            path = route.path_format[len(PREFIX):] if route.path_format.startswith(PREFIX) else route.path_format
            label = ADMIN_ACTIONS.get((request.method, path), f"{request.method} {path}")
            is_login = path == "/login"
            body = None
            declared = request.headers.get("content-length", "")
            if declared.isdecimal() and int(declared) <= _AUDIT_BODY_LIMIT:
                try:                                    # 缓存请求体：处理函数随后仍能读到同一份内容
                    body = json.loads(await request.body() or b"null")
                except ValueError:
                    body = None
            target = await _audit_target(request, path)
            authed = is_login or check_session(request)
            try:
                response = await handler(request)
            except HTTPException as exc:
                if authed:
                    await _log(request, label, target, f"失败（{exc.status_code}）：{exc.detail}"[:200], ok=False)
                raise
            if authed:
                if path == "/keys" and isinstance(body, dict):
                    target = str(body.get("name") or "")
                if path == "/keys/{key_id}" and request.method == "DELETE" and \
                        request.query_params.get("ban") in ("1", "true", "True"):
                    label = "删除 Key 并永久禁止领取"
                await _log(request, label, target, "" if is_login else summarize(body))
            return response

        return audited


async def _log(request: Request, label: str, target: str, detail: str, ok: bool = True) -> None:
    db = getattr(getattr(request.app.state, "gate", None), "db", None)
    if db is not None:
        await log_action(db, _actor(request), label, target, detail, ok=ok)


def _actor(request: Request) -> str:
    from .state import _mask_ip
    return "后台 " + _mask_ip(_client_id(request))


async def _audit_target(request: Request, path: str) -> str:
    """操作前先记下对象名称（删除之后就查不到了）。"""
    try:
        if "{key_id}" in path:
            key = await request.app.state.gate.db.get_key(int(request.path_params["key_id"]))
            return f"Key #{request.path_params['key_id']} {key['name'] if key else '(不存在)'}"
        if "{token_id}" in path:
            return "上游 " + str(request.path_params.get("token_id", ""))[:24]
    except Exception:
        pass
    return ""


router = APIRouter(prefix=PREFIX, route_class=AuditedRoute)

COOKIE = "nai_gate_admin"


def _client_id(request: Request) -> str:
    # Let the ASGI server apply its trusted-proxy policy. Raw forwarding headers
    # are attacker-controlled on direct connections and must not select a bucket.
    return request.client.host if request.client else "unknown"


# ----------------------------------------------------------------- auth ----

def _secret(request: Request) -> str:
    s = request.app.state.gate.settings
    if s.secret_key:
        return s.secret_key
    # 未配置则自动生成并持久化
    f = s.data_dir / "secret_key"
    existing = f.read_text().strip() if f.exists() else ""
    if len(existing) >= 32:
        s.secret_key = existing
    else:
        # 不存在或损坏（空文件等）：生成新的，并先写临时文件再原子替换，崩溃也不会留下空密钥
        s.secret_key = os.urandom(32).hex()
        tmp = f.with_name(f.name + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as fh:
            fh.write(s.secret_key)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, f)
    return s.secret_key


def _sign(secret: str, payload: str) -> str:
    return hmac.new(secret.encode(), payload.encode(), hashlib.sha256).hexdigest()


def _session_key(request: Request) -> str:
    # 把管理员密码摘要混入签名密钥：修改 ADMIN_PASSWORD 即令所有旧会话失效。
    gate = request.app.state.gate
    pw = getattr(gate, "admin_pw_hash", None) or gate.settings.admin_password or ""
    return _secret(request) + ":" + hashlib.sha256(pw.encode()).hexdigest()


def _pw_hash(password: str) -> str:
    salt = os.urandom(16)
    digest = hashlib.scrypt(password.encode(), salt=salt, n=2 ** 14, r=8, p=1, dklen=32)
    return f"scrypt${salt.hex()}${digest.hex()}"


def _pw_matches(password: str, stored: str) -> bool:
    try:
        _, salt, digest = stored.split("$")
        probe = hashlib.scrypt(password.encode(), salt=bytes.fromhex(salt), n=2 ** 14, r=8, p=1, dklen=32)
        return hmac.compare_digest(probe, bytes.fromhex(digest))
    except (ValueError, TypeError):
        return False


def _password_ok(request: Request, password: str) -> bool:
    """站长在后台改过密码则以保存的哈希为准，否则使用 ADMIN_PASSWORD 环境变量。"""
    gate = request.app.state.gate
    stored = getattr(gate, "admin_pw_hash", None)
    if stored:
        return _pw_matches(password, stored)
    return hmac.compare_digest(password.encode(), gate.settings.admin_password.encode())


def make_session_cookie(request: Request) -> str:
    secret = _session_key(request)
    payload = str(int(time.time()) + 7 * 86400)
    return payload + "." + _sign(secret, payload)


def check_session(request: Request) -> bool:
    secret = _session_key(request)
    raw = request.cookies.get(COOKIE, "")
    if "." not in raw:
        return False
    payload, sig = raw.split(".", 1)
    try:
        if not hmac.compare_digest(sig.encode(), _sign(secret, payload).encode()):
            return False
    except (TypeError, ValueError):
        return False
    try:
        return int(payload) > time.time()
    except ValueError:
        return False


def _host_only(netloc: str) -> str:
    return (urlsplit("//" + netloc).hostname or "").lower()


def _origin_ok(request: Request) -> bool:
    """浏览器对跨源写请求一定会带 Origin；其主机名必须与本站 Host 一致（忽略端口，
    以兼容反向代理）。同级子域名、其他站点都会被拒。X-Forwarded-* 不被信任。
    无 Origin（curl 等非浏览器客户端）放行，它们拿不到浏览器里的 Cookie。"""
    origin = request.headers.get("origin")
    if origin is None:
        return True
    host = _host_only(urlsplit(origin).netloc)
    if not host:
        return False
    allowed = {_host_only(request.headers.get("host", ""))}
    allowed.update(_host_only(urlsplit(o).netloc) for o in request.app.state.gate.settings.admin_allowed_origins)
    allowed.discard("")
    return host in allowed


def require_admin(request: Request) -> None:
    if request.method not in ("GET", "HEAD", "OPTIONS") and not _origin_ok(request):
        raise HTTPException(403, "来源校验失败")
    if not check_session(request):
        raise HTTPException(401, "未登录或会话已过期")


# ----------------------------------------------------------------- routes ----

@router.post("/login")
async def login(request: Request, response: Response):
    if not _origin_ok(request):
        raise HTTPException(403, "来源校验失败")
    if not await request.app.state.gate.hit_login(_client_id(request)):
        raise HTTPException(429, "登录尝试过于频繁，请稍后再试")
    body = await read_json_body(request)
    password = str(body.get("password", ""))
    gate = request.app.state.gate
    if not getattr(gate, "admin_pw_hash", None):
        if not gate.settings.admin_password:
            raise HTTPException(503, "尚未设置 ADMIN_PASSWORD 环境变量，管理端已锁定")
        if gate.settings.admin_password == "changeme-please":
            raise HTTPException(503, "ADMIN_PASSWORD 仍是示例值 changeme-please，请先在 .env 中改成强密码")
    if not _password_ok(request, password):
        raise HTTPException(401, "密码错误")
    response.set_cookie(
        COOKIE, make_session_cookie(request),
        httponly=True, secure=request.app.state.gate.settings.admin_cookie_secure,
        samesite="strict", max_age=7 * 86400,
    )
    return {"ok": True}


@router.put("/password")
async def change_password(request: Request, response: Response):
    """站长在后台修改密码：需要当前密码；成功后所有已登录会话（包括当前）立即失效，需用新密码重新登录。"""
    require_admin(request)
    gate = request.app.state.gate
    if not await gate.hit_login(_client_id(request)):           # 同样受登录限流保护，防止被劫持会话后猜当前密码
        raise HTTPException(429, "尝试过于频繁，请稍后再试")
    body = await read_json_body(request)
    current, new = str(body.get("current", "")), str(body.get("new", ""))
    if not _password_ok(request, current):
        raise HTTPException(401, "当前密码不正确")
    if len(new) < 12 or new == "changeme-please" or new == current:
        raise HTTPException(422, "新密码至少 12 个字符，且不能与当前密码或示例密码相同")
    stored = _pw_hash(new)
    await gate.db.set_setting("admin_password_hash", stored)
    gate.admin_pw_hash = stored
    response.delete_cookie(COOKIE, httponly=True, secure=gate.settings.admin_cookie_secure, samesite="strict")
    return {"ok": True, "relogin": True}


@router.post("/logout")
async def logout(request: Request, response: Response):
    response.delete_cookie(
        COOKIE, httponly=True, secure=request.app.state.gate.settings.admin_cookie_secure,
        samesite="strict",
    )
    return {"ok": True}


@router.get("/me")
async def me(request: Request):
    require_admin(request)
    return {"ok": True}


@router.get("/reconciliation")
async def reconciliation_status(request: Request, response: Response):
    require_admin(request)
    response.headers["Cache-Control"] = "no-store"
    return {**await request.app.state.gate.reconciliation.status(),
            "csrf_token": _reconciliation_csrf(request)}


def _reconciliation_csrf(request: Request) -> str:
    # Use a purpose-specific HMAC of the authenticated session for CSRF checks.
    return _sign(_session_key(request), "reconciliation:" + request.cookies[COOKIE])


@router.post("/reconciliation")
async def reconcile_anlas(request: Request, response: Response):
    require_admin(request)
    # Session-bound CSRF works across TLS proxies and isolates sibling origins.
    csrf = request.headers.get("x-nai-admin-csrf", "")
    if not hmac.compare_digest(csrf.encode(), _reconciliation_csrf(request).encode()):
        raise HTTPException(403, "会话校验失败，请刷新后台后重试")
    if request.headers.get("content-type", "").split(";", 1)[0].strip().lower() != "application/json":
        raise HTTPException(415, "请使用 JSON 请求")
    if await read_json_body(request, limit=1024):
        raise HTTPException(400, "本操作不接受自定义账号或地址")
    response.headers["Cache-Control"] = "no-store"
    try:
        return {**await request.app.state.gate.reconciliation.run(),
                "csrf_token": _reconciliation_csrf(request)}
    except ReconciliationError as exc:
        headers = {"Retry-After": str(exc.retry_after)} if exc.retry_after else None
        raise HTTPException(exc.status, str(exc), headers=headers) from None


def _discord_profile(row) -> Optional[dict]:
    """成员的 Discord 资料：显示名、用户名、头像地址、个人资料链接（点开可直接私信）。"""
    if row is None:
        return None
    discord_id, username, display, avatar = str(row[1]), row[2], row[3], row[4]
    if avatar:
        avatar_url = f"https://cdn.discordapp.com/avatars/{discord_id}/{avatar}.png?size=64"
    else:
        avatar_url = f"https://cdn.discordapp.com/embed/avatars/{(int(discord_id) >> 22) % 6}.png"
    return {"id": discord_id, "username": username, "display_name": display, "avatar_url": avatar_url,
            "profile_url": f"https://discord.com/users/{discord_id}"}


def _key_json(row, counter, generated_images_total: int = 0) -> dict[str, Any]:
    return {
        "id": row["id"],
        "name": row["name"],
        "token": row["token"],
        "enabled": bool(row["enabled"]),
        "daily_images": row["daily_images"],
        "daily_anlas": row["daily_anlas"],
        "daily_v5": row["daily_v5"],
        "monthly_anlas": row["monthly_anlas"],
        "daily_text_tokens": row["daily_text_tokens"],
        "rpm": row["rpm"],
        "allow_anlas": bool(row["allow_anlas"]),
        "allow_img2img": bool(row["allow_img2img"]),
        "exclude_global_v5": bool(row["exclude_global_v5"]),
        "image_model_scope": row["image_model_scope"],
        "is_admin": bool(row["is_admin"]),
        "is_test": bool(row["is_test"]) if "is_test" in row.keys() else False,
        "features": feature_defs.key_features(row),
        "expires_at": row["expires_at"],
        "created_at": row["created_at"],
        "last_used_at": row["last_used_at"],
        "used": {
            "images": counter["images"],
            "generated_images_total": generated_images_total,
            "legacy_free_images": counter["legacy_free_images"],
            "anlas": round(float(counter["anlas"]), 2),
            "v5": counter["v5"],
            "text_tokens": counter["text_tokens"],
            "requests": counter["requests"],
        },
    }


@router.get("/keys")
async def list_keys(request: Request):
    require_admin(request)
    st = request.app.state.gate
    rows = await st.db.list_keys()
    today = st.day()
    totals = await st.db.generated_image_totals()
    sources = await st.db.key_source_summary(time.time() - 24 * 3600)
    out = []
    for r in rows:
        c = await st.db.get_counter(r["id"], today)
        item = _key_json(r, c, totals.get(r["id"], 0))
        item["sources_24h"] = sources.get(r["id"], [])
        out.append(item)
    return {"keys": out, "share_alert_nets": st.settings.key_share_alert_nets}


def _features_field(body: dict):
    """features: null/缺省 = 沿用旧行为（全局开启的都可用）；列表 = 仅允许所列功能。"""
    if "features" not in body or body["features"] is None:
        return None
    value = body["features"]
    if not isinstance(value, list) or any(not isinstance(v, str) or v not in feature_defs.FEATURES for v in value):
        raise HTTPException(422, "features 必须是功能名列表：" + ",".join(feature_defs.FEATURES))
    return feature_defs.dump(value)


def _num(value: Any, kind: type, lo, hi, name: str):
    """严格解析后台数值：空值、负数、越界、小数（整数字段）一律 422。

    这些字段里 0 代表“不限”，绝不能把空值或非法值静默变成 0，否则一次误操作就会拆掉额度保护。
    """
    if value is None or isinstance(value, bool) or (isinstance(value, str) and not value.strip()):
        raise HTTPException(422, f"{name} 不能为空")
    try:
        v = float(value)
    except (TypeError, ValueError, OverflowError):
        raise HTTPException(422, f"{name} 必须是有效数字") from None
    if not math.isfinite(v):
        raise HTTPException(422, f"{name} 必须是有效数字")
    if kind is int:
        if not v.is_integer():
            raise HTTPException(422, f"{name} 必须是整数")
        v = int(v)
    if not lo <= v <= hi:
        raise HTTPException(422, f"{name} 必须在 {lo}～{hi} 之间")
    return v


async def _new_key_features(request: Request, body: dict):
    """新建 Key 未指定功能时，沿用“新成员默认开通的功能”（默认只有文生图），而不是全部功能。"""
    if "features" in body:
        return _features_field(body)
    db = request.app.state.gate.db
    reg = await ops.registration_settings(db, getattr(request.app.state, "registrar", None))
    if reg.get("features") is None and await db.get_setting("register_features", None) == "*":
        return None                     # 站长明确把默认设为“全部已开放功能”
    return feature_defs.dump(reg.get("features") or ["image"])


@router.post("/keys")
async def create_key(request: Request):
    require_admin(request)
    st = request.app.state.gate
    body = await read_json_body(request)

    def _int_field(name: str, default: int, lo: int, hi: int) -> int:
        return _num(body.get(name, default), int, lo, hi, name)

    daily_images = _int_field("daily_images", st.settings.default_daily_images, 0, 1000000)
    monthly_anlas = _num(body.get("monthly_anlas", st.settings.default_monthly_anlas), float, 0.0, 100000.0, "monthly_anlas")
    daily_anlas = _num(body.get("daily_anlas", st.settings.default_daily_anlas), float, 0.0, 100000.0, "daily_anlas")
    daily_v5 = _int_field("daily_v5", st.settings.default_daily_v5, 0, 100000)
    daily_text = _int_field("daily_text_tokens", st.settings.default_daily_text_tokens, 0, 100_000_000)
    rpm = _int_field("rpm", st.settings.default_rpm, 1, 600)
    expires_days = _int_field("expires_days", st.settings.default_expires_days, 0, 3650)
    expires_at = (time.time() + expires_days * 86400) if expires_days > 0 else None
    image_model_scope = "all" if body.get("image_model_scope") == "all" else "legacy"

    row = await st.db.create_key({
        "name": str(body.get("name", "") or "").strip()[:60] or "未命名",
        "token": gen_key("nai"),
        "daily_images": daily_images,
        "daily_anlas": daily_anlas,
        "daily_v5": daily_v5,
        "monthly_anlas": monthly_anlas,
        "daily_text_tokens": daily_text,
        "rpm": rpm,
        "allow_anlas": bool(body.get("allow_anlas", False)),
        "allow_img2img": bool(body.get("allow_img2img", False)),
        "exclude_global_v5": bool(body.get("exclude_global_v5", False)),
        "is_test": bool(body.get("is_test", False)),
        "image_model_scope": image_model_scope,
        "features": await _new_key_features(request, body),
        "expires_at": expires_at,
    })
    c = await st.db.get_counter(row["id"], st.day())
    return {"key": _key_json(row, c, 0)}


async def _notify_member(request: Request, key_id: int, text: str) -> None:
    """后台对 Discord 成员做了会影响使用的操作时，机器人私信本人（?notify=0 / body.notify=false 可关闭）。"""
    registrar = getattr(request.app.state, "registrar", None)
    if registrar is None:
        return
    discord_id = await registrar.registration_for_key(key_id)
    if discord_id is None:
        return
    try:
        sent = await registrar.send_dm(discord_id, "🦉 猫头鹰公益站通知：" + text)
    except Exception:              # 私信失败绝不能让后台操作本身失败
        sent = False
    from .action_log import log_action
    await log_action(request.app.state.gate.db, "系统", "私信成员", f"Key #{key_id}",
                     text[:120] if sent else "私信失败（对方可能关闭了私信）", ok=sent)


def _describe_changes(before, fields: dict) -> list[str]:
    out = []
    if "enabled" in fields and bool(before["enabled"]) != fields["enabled"]:
        out.append("你的 Key 已被站长" + ("恢复，可以继续使用" if fields["enabled"] else "暂停，暂时无法使用（有疑问请到 🛠️｜问题反馈）"))
    labels = {"daily_images": "每日图片", "daily_v5": "每日 V5", "daily_anlas": "每日 Anlas"}
    for name, label in labels.items():
        if name in fields and before[name] != fields[name]:
            out.append(f"{label}额度：{before[name] or '不限'} → {fields[name] or '不限'}")
    if "allow_anlas" in fields and bool(before["allow_anlas"]) != fields["allow_anlas"]:
        out.append("已为你" + ("开通" if fields["allow_anlas"] else "关闭") + " Anlas（付费规格）")
    if "image_model_scope" in fields and before["image_model_scope"] != fields["image_model_scope"]:
        out.append("可用模型：" + ("含 V5" if fields["image_model_scope"] == "all" else "仅 V4.5 及以下"))
    old_exp, new_exp = before["expires_at"], fields.get("expires_at", before["expires_at"])
    # 编辑框每次都按「剩余天数」重算到期时间，相差不到 1 天视为没改
    if "expires_at" in fields and (old_exp is None) != (new_exp is None) or (
            old_exp and new_exp and abs(old_exp - new_exp) > 86400):
        out.append("Key 有效期已调整" + (f"，约 {max(0, round((fields['expires_at'] - time.time()) / 86400))} 天后到期" if fields["expires_at"] else "为长期有效"))
    return out


@router.post("/keys/{key_id}/regenerate")
async def regenerate_key(request: Request, response: Response, key_id: int):
    require_admin(request)
    token = gen_key("nai")
    if not await request.app.state.gate.db.rotate_key_token(key_id, token):
        raise HTTPException(404, "key 不存在")
    if request.query_params.get("notify", "1") != "0":
        await _notify_member(request, key_id, f"站长为你重置了 Key，旧 Key 已失效。新的 Key（请妥善保存）：`{token}`")
    response.headers["Cache-Control"] = "no-store"
    return {"token": token}


QUICK_V5_DAILY = 15


@router.patch("/keys/{key_id}")
async def patch_key(request: Request, key_id: int):
    require_admin(request)
    st = request.app.state.gate
    if not await st.db.get_key(key_id):
        raise HTTPException(404, "key 不存在")
    body = await read_json_body(request)
    fields: dict[str, Any] = {}
    if "name" in body:
        fields["name"] = str(body["name"] or "").strip()[:60] or "未命名"
    if "enabled" in body:
        fields["enabled"] = bool(body["enabled"])
    if "daily_images" in body:
        fields["daily_images"] = _num(body["daily_images"], int, 0, 1000000, "daily_images")
    if "daily_anlas" in body:
        fields["daily_anlas"] = _num(body["daily_anlas"], float, 0.0, 100000.0, "daily_anlas")
    if "daily_v5" in body:
        fields["daily_v5"] = _num(body["daily_v5"], int, 0, 100000, "daily_v5")
    if "monthly_anlas" in body:
        fields["monthly_anlas"] = _num(body["monthly_anlas"], float, 0.0, 100000.0, "monthly_anlas")
    if "daily_text_tokens" in body:
        fields["daily_text_tokens"] = _num(body["daily_text_tokens"], int, 0, 100_000_000, "daily_text_tokens")
    if "rpm" in body:
        fields["rpm"] = _num(body["rpm"], int, 1, 600, "rpm")
    if "allow_anlas" in body:
        fields["allow_anlas"] = bool(body["allow_anlas"])
    if "allow_img2img" in body:
        fields["allow_img2img"] = bool(body["allow_img2img"])
    if "exclude_global_v5" in body:
        fields["exclude_global_v5"] = bool(body["exclude_global_v5"])
    if "is_test" in body:
        fields["is_test"] = bool(body["is_test"])
    if "image_model_scope" in body:
        fields["image_model_scope"] = "all" if body["image_model_scope"] == "all" else "legacy"
    if "features" in body:
        fields["features"] = _features_field(body)
    mode = body.get("anlas_mode")
    if mode is not None:
        # 成员页快捷设置：off 关闭（算法也不会再开）/ auto 交给算法 / manual 手动每天 N
        if mode == "off":
            fields.update(allow_anlas=False, daily_anlas=0.0)
        elif mode == "manual":
            fields.update(allow_anlas=True, daily_anlas=_num(body.get("daily_anlas", 0), float, 1.0, 100000.0, "daily_anlas"))
        elif mode == "auto":
            fields.update(allow_anlas=False, daily_anlas=0.0)
        else:
            raise HTTPException(422, "anlas_mode 只能是 off / auto / manual")
    if body.get("image_model_scope") == "all" and "daily_v5" not in body:
        before_v5 = await st.db.get_key(key_id)
        if before_v5 and not before_v5["daily_v5"]:
            fields["daily_v5"] = QUICK_V5_DAILY     # 从「仅 V4.5」开到 V5 时给一个默认日额度，和早期成员一致
    if "expires_days" in body:
        d = _num(body["expires_days"], int, 0, 3650, "expires_days")
        fields["expires_at"] = (time.time() + d * 86400) if d > 0 else None
    before = await st.db.get_key(key_id)
    await st.db.update_key(key_id, fields)
    quota_mode = body.get("quota_mode")
    if quota_mode == "auto" or any(k in fields for k in ("daily_images", "daily_v5", "image_model_scope")):
        # 手动改了额度或模型 → 这把 Key 由站长管理（-1），动态额度算法不再覆盖；选「交给算法」恢复为 1 并立即重算
        await st.db._db.execute("UPDATE api_keys SET quota_auto=? WHERE id=?", (1 if quota_mode == "auto" else -1, key_id))
        await st.db._db.commit()
        if quota_mode == "auto":
            try:
                from . import quota_algo
                await quota_algo.run(st)
            except Exception as exc:
                st.bugs.capture("quota_algo", exc) if getattr(st, "bugs", None) else None
    if mode == "auto" or "allow_anlas" in fields or "daily_anlas" in fields:
        # 手动设置后由站长管理（-1），自动分配不会再覆盖；选「交给算法」则回到 0，下次重算时按条件分配
        await st.db._db.execute("UPDATE api_keys SET anlas_auto=? WHERE id=?", (0 if mode == "auto" else -1, key_id))
        await st.db._db.commit()
        if mode == "auto":
            try:
                from . import anlas_pool
                await anlas_pool.rebalance(st)
            except Exception as exc:
                st.bugs.capture("anlas_pool", exc) if getattr(st, "bugs", None) else None
    changes = _describe_changes(before, fields)
    if changes and body.get("notify", True) is not False:
        await _notify_member(request, key_id, "站长调整了你的 Key：\n• " + "\n• ".join(changes))
    row = await st.db.get_key(key_id)
    c = await st.db.get_counter(key_id, st.day())
    totals = await st.db.generated_image_totals(key_id)
    return {"key": _key_json(row, c, totals.get(key_id, 0))}


@router.post("/keys/{key_id}/reset-daily-image-quota")
async def reset_daily_image_quota(request: Request, key_id: int):
    """仅重置指定 Key 今日的 V5 与 Anlas 配额计数，保留审计日志。"""
    require_admin(request)
    st = request.app.state.gate
    row = await st.db.get_key(key_id)
    if not row:
        raise HTTPException(404, "key 不存在")
    await st.db.reset_daily_image_quota(key_id, st.day())
    counter = await st.db.get_counter(key_id, st.day())
    totals = await st.db.generated_image_totals(key_id)
    if request.query_params.get("notify", "1") != "0":
        await _notify_member(request, key_id, "站长已为你重置了今天的额度，可以继续使用。")
    return {"ok": True, "key": _key_json(row, counter, totals.get(key_id, 0))}


@router.delete("/keys/{key_id}")
async def delete_key(request: Request, key_id: int, ban: bool = False):
    """删除成员 / Key。Discord 自助领取的成员会同时清掉领取记录并摘除身份组；ban=true 则永久禁止该账号再次领取。"""
    require_admin(request)
    gate = request.app.state.gate
    if not await gate.db.get_key(key_id):
        raise HTTPException(404, "key 不存在")
    registrar = getattr(request.app.state, "registrar", None)
    discord_id = await registrar.registration_for_key(key_id) if registrar is not None else None
    if discord_id is not None:
        if request.query_params.get("notify", "1") != "0":
            await _notify_member(request, key_id, "你的 Key 已被站长" + (
                "删除，并且这个 Discord 账号不能再领取。" if ban else "删除。如需继续使用，可以在 🔑｜领取key 重新 /register。"))
        await (registrar.ban if ban else registrar.revoke)(discord_id)
        return {"ok": True, "discord_id": discord_id, "banned": bool(ban)}
    await gate.db.delete_key(key_id)
    return {"ok": True}


@router.get("/logs")
async def logs(request: Request, key_id: Optional[int] = None, page: int = 1,
               feature: Optional[str] = None, hide_test: bool = False, rid: str = ""):
    require_admin(request)
    per_page = 20
    page = max(1, min(int(page), 1_000_000))
    db = request.app.state.gate.db
    if feature and feature not in feature_defs.FEATURES:
        raise HTTPException(422, "未知功能")
    kinds = feature_defs.kinds_for(feature) if feature else None
    rid = "".join(c for c in rid.strip().lower() if c in "0123456789abcdef")[:16]
    total = await db.count_logs(key_id=key_id, kinds=kinds, hide_test=hide_test, rid=rid)
    pages = max(1, (total + per_page - 1) // per_page)
    page = min(page, pages)
    rows = await db.list_logs(limit=per_page, offset=(page - 1) * per_page,
                              key_id=key_id, kinds=kinds, hide_test=hide_test, rid=rid)
    return {
        "logs": [dict(r) for r in rows],
        "page": page,
        "per_page": per_page,
        "total": total,
        "pages": pages,
    }


@router.get("/quota-algo")
async def quota_algo_get(request: Request):
    """动态额度：当前结果、参数、最近 30 天的每日微调记录。"""
    require_admin(request)
    from . import quota_algo
    db = request.app.state.gate.db
    params = {k: await db.get_setting(k, v) for k, v in quota_algo.DEFAULTS.items()}
    return {"last": json.loads(await db.get_setting(quota_algo.STATE_KEY, "{}") or "{}"),
            "history": json.loads(await db.get_setting(quota_algo.HISTORY_KEY, "[]") or "[]"),
            "params": params}


@router.get("/autopilot")
async def autopilot_get(request: Request):
    """自动驾驶：每条规则的最新判断（观察模式下只记录不执行）和最近 7 天的每小时快照。"""
    require_admin(request)
    from . import autopilot
    db = request.app.state.gate.db
    return {"last": json.loads(await db.get_setting(autopilot.STATE_KEY, "{}") or "{}"),
            "history": json.loads(await db.get_setting(autopilot.HISTORY_KEY, "[]") or "[]")}


@router.put("/quota-algo")
async def quota_algo_put(request: Request):
    """修改初始值 / 步长 / 范围；保存后立即重算。"""
    require_admin(request)
    from . import quota_algo
    body = await read_json_body(request)
    st = request.app.state.gate
    bounds = {"quota_auto_enabled": (0, 1), "quota_target_avg": (10, 2000), "quota_base": (0, 2000),
              "quota_ceiling_max": (10, 5000), "quota_base_min": (0, 2000), "quota_step": (1, 500),
              "quota_base_step": (1, 500), "quota_v5_min": (0, 500), "quota_v5_max": (1, 500)}
    updates = {}
    for k, v in body.items():
        if k in bounds:
            updates[k] = _num(v, int, bounds[k][0], bounds[k][1], k)
    if updates:
        await st.db.set_settings_bulk(updates)
        # 改了初始值时，同步当前值（否则要等明天的微调）
        if "quota_target_avg" in updates:
            await st.db.set_setting("quota_ceiling", updates["quota_target_avg"])
        if "quota_base" in updates:
            await st.db.set_setting("quota_base_now", updates["quota_base"])
    result = await quota_algo.run(st)
    return {"ok": True, "last": result}


@router.get("/discord-bot")
async def discord_bot_get(request: Request):
    """Discord 机器人：配置、在线状态（心跳）、最近 50 条点赞 / 评论记录。"""
    require_admin(request)
    from . import bot_config
    return await bot_config.snapshot(request.app.state.gate.db)


@router.put("/discord-bot")
async def discord_bot_put(request: Request):
    """修改机器人配置；机器人一分钟内生效。"""
    require_admin(request)
    from . import bot_config
    try:
        values = bot_config.validate(await read_json_body(request))
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    await bot_config.save(request.app.state.gate.db, values)
    return {"ok": True, "config": await bot_config.load(request.app.state.gate.db)}


@router.get("/errors")
async def errors_list(request: Request, all: bool = False):
    """Bug 追踪：按特征归并的错误（未处理的在前）。detail 含堆栈，只在后台显示。"""
    require_admin(request)
    tracker = request.app.state.gate.bugs
    items = await tracker.list(include_resolved=all)
    keys = {r["last_key"] for r in items if r["last_key"]}
    names = {}
    for key_id in keys:
        row = await request.app.state.gate.db.get_key(key_id)
        if row:
            names[key_id] = row["name"]
    for r in items:
        r["key_name"] = names.get(r["last_key"], "")
    return {"errors": items, "open": sum(1 for r in items if r["resolved_at"] is None)}


@router.post("/errors/{sig}/resolve")
async def errors_resolve(sig: str, request: Request):
    """标记已处理；之后再出现会作为「复发」重新提醒。"""
    require_admin(request)
    if not await request.app.state.gate.bugs.resolve(sig[:12]):
        raise HTTPException(404, "没有这条未处理的错误")
    return {"ok": True}


@router.get("/perf")
async def upstream_perf(request: Request):
    """上游表现：V4.5 / V5 的生成耗时、排队、限流、成功率、产能，与过去 7 天对比。"""
    require_admin(request)
    from . import perf
    return await perf.collect(request.app.state.gate, time.time())


@router.get("/guard")
async def guard_get(request: Request):
    """账号保护与排队（P0 / P1）的当前设置和实时用量。"""
    require_admin(request)
    st = request.app.state.gate
    data = st.guard.describe()
    accounts = []
    for t in st.nai.pool:
        used = (await st.db.get_upstream_counter(t.token_id, st.day()))["images"]
        accounts.append({"position": t.position, "usable": t.usable, "today": used,
                         "this_hour": st.guard.hour_count(t.token_id)})
    data["accounts"] = accounts
    data["queued_images"] = sum(st.guard.image_inflight.values())
    return data


@router.put("/guard")
async def guard_put(request: Request):
    require_admin(request)
    body = await read_json_body(request)
    if not isinstance(body, dict):
        raise HTTPException(422, "参数必须是对象")
    try:
        await request.app.state.gate.guard.save(body)
    except ValueError as exc:
        raise HTTPException(422, str(exc)) from None
    return await guard_get(request)


@router.get("/anlas-pool")
async def anlas_pool_get(request: Request):
    """Anlas 自动分配：上次结果与参数。"""
    require_admin(request)
    from . import anlas_pool
    db = request.app.state.gate.db
    last = await db.get_setting(anlas_pool.STATE_KEY, None)
    return {"last": json.loads(last) if last else None,
            "settings": {k: await anlas_pool._setting(db, k) for k in anlas_pool.DEFAULTS}}


@router.put("/anlas-pool")
async def anlas_pool_put(request: Request):
    """修改参数并立即重算一次。"""
    require_admin(request)
    from . import anlas_pool
    body = await read_json_body(request)
    st = request.app.state.gate
    bounds = {"anlas_auto_enabled": (0, 1), "anlas_reserve": (0, 10000), "anlas_member_daily_cap": (0, 1000),
              "anlas_min_images_7d": (0, 10000), "anlas_min_key_age_days": (0, 365)}
    values = {}
    for name, (low, high) in bounds.items():
        if name in body:
            values[name] = _num(body[name], int, low, high, name)
    if values:
        await st.db.set_settings_bulk(values)
    await anlas_pool.rebalance(st)
    return await anlas_pool_get(request)


@router.get("/shadow")
async def scheduler_shadow(request: Request, hours: int = 24):
    """调度影子模式：用真实请求回放新规则，只计算不执行。hours=24 或 168。"""
    require_admin(request)
    from . import shadow
    hours = 168 if hours >= 168 else 24
    return await shadow.collect(request.app.state.gate, time.time() - hours * 3600)


@router.get("/actions")
async def admin_actions(request: Request, page: int = 1):
    require_admin(request)
    per_page = 30
    db = request.app.state.gate.db
    total = await db.count_admin_actions()
    pages = max(1, (total + per_page - 1) // per_page)
    page = min(max(1, min(int(page), 1_000_000)), pages)
    rows = await db.list_admin_actions(limit=per_page, offset=(page - 1) * per_page)
    return {"actions": rows, "page": page, "per_page": per_page, "total": total, "pages": pages}


@router.get("/overview")
async def overview(request: Request):
    require_admin(request)
    st = request.app.state.gate
    data = await st.db.overview(st.day(), st.week_days(7))
    data["pool"] = await st.nai.status()
    data["pool_configured"] = st.nai.configured
    budget = await st.db.get_setting("global_monthly_anlas", st.settings.global_monthly_anlas)
    data["anlas_budget"] = float(budget or 0)
    v5lim = await st.db.get_setting("global_daily_v5", st.settings.global_daily_v5)
    data["v5_limit"] = int(float(v5lim or 0))
    data["feature_usage"] = await _feature_usage(st)
    return data


async def _feature_usage(st) -> list[dict]:
    """每项功能今日 / 近 7 天的调用次数（成功 / 失败 / 拒绝）、图片、tokens、Anlas 与使用人数。"""
    from datetime import datetime, timedelta
    from zoneinfo import ZoneInfo
    day_start = datetime.fromisoformat(st.day()).replace(tzinfo=ZoneInfo(st.db.tz)).timestamp()
    week_start = day_start - 6 * 86400

    def empty():
        return {"ok": 0, "error": 0, "rejected": 0, "images": 0, "tokens": 0, "anlas": 0.0}

    out = {name: {"id": name, "label": label, "today": empty(), "week": empty()}
           for name, label in feature_defs.FEATURES.items()}
    for period, since in (("today", day_start), ("week", week_start)):
        for row in await st.db.usage_by_kind(since):
            name = feature_defs.KIND_FEATURE.get(row["kind"])
            if name is None:
                continue
            bucket = out[name][period]
            status = row["status"] if row["status"] in ("ok", "rejected") else "error"
            bucket[status] += int(row["n"])
            if row["status"] == "ok":
                bucket["images"] += int(row["images"])
                bucket["tokens"] += int(row["tokens"])
                bucket["anlas"] = round(bucket["anlas"] + float(row["anlas"]), 2)
    return list(out.values())


@router.get("/settings")
async def get_settings(request: Request):
    require_admin(request)
    st = request.app.state.gate
    v = await st.db.get_setting("global_monthly_anlas", st.settings.global_monthly_anlas)
    v5 = await st.db.get_setting("global_daily_v5", st.settings.global_daily_v5)
    return {"global_monthly_anlas": float(v or 0), "global_daily_v5": int(float(v5 or 0)),
            SETTING: await read_alert_threshold(st.db),
            "v5_capacity": await ops.v5_capacity(st.db, st.settings, getattr(request.app.state, "registrar", None))}


@router.get("/runtime-limits")
async def get_runtime_limits(request: Request):
    require_admin(request)
    return request.app.state.gate.runtime_limits_snapshot()


@router.put("/runtime-limits")
async def put_runtime_limits(request: Request):
    require_admin(request)
    body = await read_json_body(request)
    if not body or set(body) - set(RUNTIME_LIMIT_BOUNDS):
        raise HTTPException(422, "包含未知或空的运行限制设置")
    for name, value in body.items():
        minimum, maximum = RUNTIME_LIMIT_BOUNDS[name]
        if type(value) is not int or not minimum <= value <= maximum:
            raise HTTPException(422, f"{name} 必须是 {minimum}～{maximum} 的整数")
    return await request.app.state.gate.update_runtime_limits(body)


@router.get("/allowance")
async def allowance(request: Request):
    require_admin(request)
    return await request.app.state.gate.nai.allowance.snapshot(request.app.state.gate.nai.pool)


async def _token_payload(request: Request) -> tuple[str, dict]:
    body = await read_json_body(request, limit=4096)
    token = str(body.get("token", "")).strip()
    if not token_store.valid_token(token):
        raise HTTPException(422, "Token 格式不对：应以 pst- 开头（在 NovelAI 的 账号设置 → Get Persistent API Token 里获取）")
    return token, body


def _announce_token_change(request: Request, text: str) -> None:
    alerter = getattr(request.app.state.gate, "alerter", None)
    if alerter is not None:
        alerter.notify("upstream_token_changed", text, cooldown=0)


@router.post("/upstream-tokens")
async def add_upstream_token(request: Request):
    """后台添加一把上游 Token：先向 NovelAI 验证，通过后加入令牌池（保存到权限 600 的文件，不进数据库）。"""
    require_admin(request)
    token, body = await _token_payload(request)
    nai = request.app.state.gate.nai
    check = await nai.verify_token(token)
    if not check["ok"]:
        raise HTTPException(422, check["error"])
    try:
        await nai.add_token(token, allow_anlas=bool(body.get("allow_anlas", False)))
    except ValueError as exc:
        raise HTTPException(409, str(exc)) from None
    _announce_token_change(request, f"后台添加了一把上游 Token（订阅等级 {check['tier']}）。如果这不是你操作的，请立刻更换后台密码。")
    return {"ok": True, "tier": check["tier"], "pool": await nai.status()}


@router.put("/upstream-tokens/{token_id}")
async def replace_upstream_token(request: Request, token_id: str):
    """替换某个槽位的 Token（例如在 NovelAI 重置了 Token 后粘贴新的）；V5 日限、启停、并发和当天计数延续。"""
    require_admin(request)
    token, _ = await _token_payload(request)
    nai = request.app.state.gate.nai
    check = await nai.verify_token(token)
    if not check["ok"]:
        raise HTTPException(422, check["error"])
    try:
        await nai.replace_token(token_id, token)
    except LookupError:
        raise HTTPException(404, "上游 Token 不存在") from None
    except ValueError as exc:
        raise HTTPException(409, str(exc)) from None
    _announce_token_change(request, "后台替换了一把上游 Token。如果这不是你操作的，请立刻更换后台密码。")
    return {"ok": True, "tier": check["tier"], "pool": await nai.status()}


@router.delete("/upstream-tokens/{token_id}")
async def remove_upstream_token(request: Request, token_id: str):
    require_admin(request)
    nai = request.app.state.gate.nai
    try:
        removed = await nai.remove_token(token_id)
    except ValueError as exc:
        raise HTTPException(409, str(exc)) from None
    if not removed:
        raise HTTPException(404, "上游 Token 不存在")
    _announce_token_change(request, "后台删除了一把上游 Token。")
    return {"ok": True, "pool": await nai.status()}


@router.put("/upstream-tokens/{token_id}/v5-limit")
async def set_upstream_v5_limit(request: Request, token_id: str):
    require_admin(request)
    body = await read_json_body(request)
    limit = body.get("v5_daily_limit")
    if type(limit) is not int or not 0 <= limit <= 100000:
        raise HTTPException(422, "上游 V5 日限额必须是 0～100000 的整数（0 为不限）")
    if not await request.app.state.gate.nai.set_v5_daily_limit(token_id, limit):
        raise HTTPException(404, "上游 Token 不存在")
    return {"ok": True, "v5_daily_limit": limit}


@router.put("/upstream-tokens/{token_id}/image-concurrency")
async def set_upstream_image_concurrency(request: Request, token_id: str):
    require_admin(request)
    body = await read_json_body(request)
    limit = body.get("image_concurrency")
    if type(limit) is not int or not 1 <= limit <= 4:
        raise HTTPException(422, "上游图片并发必须是 1～4 的整数")
    if not await request.app.state.gate.nai.set_image_concurrency(token_id, limit):
        raise HTTPException(404, "上游 Token 不存在")
    return {"ok": True, "image_concurrency": limit}


@router.put("/upstream-tokens/{token_id}/enabled")
async def set_upstream_enabled(request: Request, token_id: str):
    require_admin(request)
    body = await read_json_body(request)
    enabled = body.get("enabled")
    if type(enabled) is not bool:
        raise HTTPException(422, "enabled 必须是布尔值")
    if not await request.app.state.gate.nai.set_admin_enabled(token_id, enabled):
        raise HTTPException(404, "上游 Token 不存在")
    return {"ok": True, "enabled": enabled}


@router.put("/settings")
async def put_settings(request: Request):
    require_admin(request)
    st = request.app.state.gate
    body = await read_json_body(request)
    threshold = body.get(SETTING, await read_alert_threshold(st.db))
    if type(threshold) is not int or not 1 <= threshold <= 100:
        raise HTTPException(422, "V5 告警阈值必须为 1～100 的整数百分比")
    # 缺省字段保持原值（以前缺省会被写成 0 = 不限）。
    v = _num(body.get("global_monthly_anlas", await st.db.get_setting(
        "global_monthly_anlas", st.settings.global_monthly_anlas)), float, 0.0, 1000000.0, "global_monthly_anlas")
    g5 = _num(body.get("global_daily_v5", await st.db.get_setting(
        "global_daily_v5", st.settings.global_daily_v5)), int, 0, 100000, "global_daily_v5")
    await st.db.set_setting("global_monthly_anlas", v)
    await st.db.set_setting("global_daily_v5", g5)
    await st.db.set_setting(SETTING, threshold)
    return {"ok": True, "global_monthly_anlas": v, "global_daily_v5": g5, SETTING: threshold,
            "v5_capacity": await ops.v5_capacity(st.db, st.settings, getattr(request.app.state, "registrar", None))}


@router.get("/announcement")
async def get_announcement(request: Request):
    require_admin(request)
    p = request.app.state.gate.settings.announcement_path
    if p.exists():
        return {"html": p.read_text(encoding="utf-8")}
    return {"html": ""}


@router.put("/announcement")
async def put_announcement(request: Request):
    require_admin(request)
    body = await read_json_body(request)
    html = str(body.get("html", ""))[:20000]
    request.app.state.gate.settings.announcement_path.write_text(html, encoding="utf-8")
    return {"ok": True}


# ----------------------------------------------------------- 成员与生成记录 ----

@router.get("/members")
async def members(request: Request):
    """每位成员（Key）的今日 / 近 7 天 / 累计用量，以及来源。"""
    require_admin(request)
    st = request.app.state.gate
    today = st.day()
    week = await st.db.member_usage(st.week_days(7)[0])
    totals = await st.db.generated_image_totals()
    reg = {int(r[0]): r for r in await st.db._db.execute_fetchall(
        "SELECT key_id, discord_id, username, display_name, avatar, created_at FROM discord_registrations")}
    since = time.time() - 24 * 3600
    milestones = await st.db.member_milestones(since)
    sources = await st.db.key_source_summary(since)
    out = []
    for row in await st.db.list_keys():
        counter = await st.db.get_counter(row["id"], today)
        w = week.get(row["id"], {})
        out.append({
            "id": row["id"], "name": row["name"], "enabled": bool(row["enabled"]),
            "is_admin": bool(row["is_admin"]), "is_test": bool(row["is_test"]), "discord_id": str(reg[row["id"]][1]) if row["id"] in reg else None,
            "discord": _discord_profile(reg.get(row["id"])),
            "created_at": row["created_at"], "last_used_at": row["last_used_at"],
            "expires_at": row["expires_at"],
            "daily_images": row["daily_images"], "daily_v5": row["daily_v5"],
            "allow_anlas": bool(row["allow_anlas"]), "anlas_auto": row["anlas_auto"] == 1,
            "anlas_mode": "manual" if row["anlas_auto"] == -1 and row["allow_anlas"] else "off" if row["anlas_auto"] == -1 else "auto",
            "daily_anlas": row["daily_anlas"], "image_model_scope": row["image_model_scope"],
            "quota_auto": row["quota_auto"] == 1,
            "today": {"images": counter["images"], "v5": counter["v5"], "anlas": round(float(counter["anlas"]), 2),
                      "text_tokens": counter["text_tokens"], "requests": counter["requests"]},
            "week": {"images": int(w.get("images", 0)), "v5": int(w.get("v5", 0)),
                     "anlas": round(float(w.get("anlas", 0)), 2), "requests": int(w.get("requests", 0))},
            "total_images": totals.get(row["id"], 0),
            # 筛选用：领取时间（Discord 领取优先，否则建 Key 时间）、首次 / 最近成功出图、24h 网段数与被拒次数
            "registered_at": reg[row["id"]][5] if row["id"] in reg else row["created_at"],
            "first_image_at": milestones.get(row["id"], {}).get("first_image_at"),
            "last_image_at": milestones.get(row["id"], {}).get("last_image_at"),
            "rejected_24h": milestones.get(row["id"], {}).get("rejected_24h", 0),
            "sources_24h": len(sources.get(row["id"], [])),
        })
    return {"members": out, "share_alert_nets": st.settings.key_share_alert_nets,
            "inactivity_days": st.settings.key_inactivity_delete_days}


@router.get("/audit")
async def audit_list(request: Request, key_id: Optional[int] = None, page: int = 1):
    require_admin(request)
    st = request.app.state.gate
    per_page = 24
    page = max(1, min(int(page), 1_000_000))
    rows, total = await st.db.list_audit(per_page, (page - 1) * per_page, key_id)
    return {"items": [dict(r) for r in rows], "page": page, "per_page": per_page, "total": total,
            "pages": max(1, (total + per_page - 1) // per_page)}


@router.get("/audit/{audit_id}/thumb")
async def audit_thumb(request: Request, audit_id: int):
    require_admin(request)
    data = await request.app.state.gate.db.audit_thumb(audit_id)
    if data is None:
        raise HTTPException(404, "没有缩略图")
    return Response(data, media_type="image/jpeg", headers={"Cache-Control": "private, max-age=3600"})


@router.get("/status")
async def server_status(request: Request):
    """告警与记录功能的当前状态（不含任何密钥）。"""
    require_admin(request)
    st = request.app.state.gate
    cfg = st.settings
    usage = shutil.disk_usage(cfg.data_dir)
    return {
        "alerts": {"configured": st.alerter.configured, "sent": st.alerter.sent},
        "audit": dict(zip(("prompts", "thumbs", "retention_days"), await ops.audit_flags(st.db, cfg))),
        "upstream": st.upstream_health(),
        "protection": {"auth_fail_max": cfg.auth_fail_max, "auth_fail_window": cfg.auth_fail_window,
                       "auth_block_seconds": cfg.auth_block_seconds,
                       "login_max_attempts": cfg.login_max_attempts},
        "disk": {"free_mb": usage.free // 2**20, "total_mb": usage.total // 2**20},
    }


@router.post("/alerts/test")
async def alerts_test(request: Request):
    require_admin(request)
    alerter = request.app.state.gate.alerter
    if not alerter.configured:
        raise HTTPException(409, "尚未配置告警渠道（ALERT_USER_ID / ALERT_CHANNEL_ID / ALERT_WEBHOOK_URL）")
    alerter.notify("test", "这是一条测试告警，收到说明告警渠道正常。", cooldown=0)
    return {"ok": True}


# ----------------------------------------------------------- 运行中开关 ----

async def _ops_snapshot(request: Request) -> dict:
    st = request.app.state.gate
    service = getattr(request.app.state, "registrar", None)
    reg = await ops.registration_settings(st.db, service)
    reg["active"] = await service.count_active() if service else 0
    reg["reset_at"] = service.reset_at if service else ""
    reg["v5_capacity"] = await ops.v5_capacity(st.db, st.settings, service)
    reg["waitlist"] = await service.waitlist() if service else []
    prompts, thumbs, days = await ops.audit_flags(st.db, st.settings)
    return {
        "registration": reg,
        "features": {"global": await feature_defs.global_flags(st.db), "labels": feature_defs.FEATURES,
                     # 后台“新建 Key”弹窗的默认功能，与服务端缺省逻辑一致
                     "new_key": feature_defs.parse_list(await _new_key_features(request, {}))},
        "audit": {"prompts": prompts, "thumbs": thumbs, "retention_days": days,
                  "announce_configured": st.announcer.configured},
        "upstream": st.upstream_health(),
    }


@router.get("/ops")
async def ops_get(request: Request):
    require_admin(request)
    return await _ops_snapshot(request)


@router.put("/ops/registration")
async def ops_set_registration(request: Request):
    """开放 / 关闭自助注册、名额上限、新成员默认功能与额度；机器人下一条命令即生效。"""
    require_admin(request)
    body = await read_json_body(request)
    if "features" in body and body["features"] is not None:
        if not isinstance(body["features"], list) or any(v not in feature_defs.FEATURES for v in body["features"]):
            raise HTTPException(422, "features 必须是功能名列表")
    try:
        await ops.set_registration(request.app.state.gate.db, body, request.app.state.gate,
                                   getattr(request.app.state, "registrar", None))
    except (TypeError, ValueError) as exc:
        raise HTTPException(422, str(exc) or "参数无效") from None
    return await _ops_snapshot(request)


@router.put("/ops/features")
async def ops_set_features(request: Request):
    """全局功能开关：{"flags": {"text": false, ...}}。关闭后所有成员（管理员 Key 除外）立即不可用。"""
    require_admin(request)
    body = await read_json_body(request)
    flags = body.get("flags")
    if not isinstance(flags, dict) or not flags or any(
            k not in feature_defs.FEATURES or type(v) is not bool for k, v in flags.items()):
        raise HTTPException(422, "flags 必须是 {功能名: true/false}")
    await ops.set_global_features(request.app.state.gate.db, flags)
    return await _ops_snapshot(request)


@router.put("/ops/audit")
async def ops_set_audit(request: Request):
    """开关生成记录。notify（默认 true）会向成员公告频道发出测试期声明。"""
    require_admin(request)
    body = await read_json_body(request)
    try:
        await ops.set_audit(request.app.state.gate, body)
    except (TypeError, ValueError):
        raise HTTPException(422, "参数无效") from None
    return await _ops_snapshot(request)
