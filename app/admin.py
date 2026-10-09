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

from fastapi import APIRouter, HTTPException, Request, Response

from . import features as feature_defs
from . import ops
from .policy import gen_key
from .body import read_json_body
from .allowance import SETTING, read_alert_threshold
from .reconciliation import ReconciliationError
from .state import RUNTIME_LIMIT_BOUNDS

router = APIRouter(prefix="/admin/api")

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
    if f.exists():
        s.secret_key = f.read_text().strip()
    else:
        s.secret_key = os.urandom(32).hex()
        fd = os.open(f, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as fh:
            fh.write(s.secret_key)
    return s.secret_key


def _sign(secret: str, payload: str) -> str:
    return hmac.new(secret.encode(), payload.encode(), hashlib.sha256).hexdigest()


def _session_key(request: Request) -> str:
    # 把管理员密码摘要混入签名密钥：修改 ADMIN_PASSWORD 即令所有旧会话失效。
    pw = request.app.state.gate.settings.admin_password or ""
    return _secret(request) + ":" + hashlib.sha256(pw.encode()).hexdigest()


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
    if not request.app.state.gate.settings.admin_password:
        raise HTTPException(503, "尚未设置 ADMIN_PASSWORD 环境变量，管理端已锁定")
    if request.app.state.gate.settings.admin_password == "changeme-please":
        raise HTTPException(503, "ADMIN_PASSWORD 仍是示例值 changeme-please，请先在 .env 中改成强密码")
    if not hmac.compare_digest(password, request.app.state.gate.settings.admin_password):
        raise HTTPException(401, "密码错误")
    response.set_cookie(
        COOKIE, make_session_cookie(request),
        httponly=True, secure=request.app.state.gate.settings.admin_cookie_secure,
        samesite="strict", max_age=7 * 86400,
    )
    return {"ok": True}


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
    out = []
    for r in rows:
        c = await st.db.get_counter(r["id"], today)
        out.append(_key_json(r, c, totals.get(r["id"], 0)))
    return {"keys": out}


def _features_field(body: dict):
    """features: null/缺省 = 沿用旧行为（全局开启的都可用）；列表 = 仅允许所列功能。"""
    if "features" not in body or body["features"] is None:
        return None
    value = body["features"]
    if not isinstance(value, list) or any(not isinstance(v, str) or v not in feature_defs.FEATURES for v in value):
        raise HTTPException(422, "features 必须是功能名列表：" + ",".join(feature_defs.FEATURES))
    return feature_defs.dump(value)


def _num(value: Any, kind: type, lo, hi, name: str):
    """把后台输入解析为有限数值并钳制范围；非法输入返回 422 而不是 500。"""
    try:
        v = kind(value if value not in (None, "") else 0)
        if kind is float and not math.isfinite(v):
            raise ValueError
    except (TypeError, ValueError, OverflowError):
        raise HTTPException(422, f"{name} 必须是有效数字") from None
    return max(lo, min(hi, v))


@router.post("/keys")
async def create_key(request: Request):
    require_admin(request)
    st = request.app.state.gate
    body = await read_json_body(request)

    def _int_field(name: str, default: int, lo: int, hi: int) -> int:
        try:
            v = int(body.get(name, default))
        except (TypeError, ValueError):
            v = default
        return max(lo, min(hi, v))

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
        "image_model_scope": image_model_scope,
        "features": _features_field(body),
        "expires_at": expires_at,
    })
    c = await st.db.get_counter(row["id"], st.day())
    return {"key": _key_json(row, c, 0)}


@router.post("/keys/{key_id}/regenerate")
async def regenerate_key(request: Request, response: Response, key_id: int):
    require_admin(request)
    token = gen_key("nai")
    if not await request.app.state.gate.db.rotate_key_token(key_id, token):
        raise HTTPException(404, "key 不存在")
    response.headers["Cache-Control"] = "no-store"
    return {"token": token}


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
    if "image_model_scope" in body:
        fields["image_model_scope"] = "all" if body["image_model_scope"] == "all" else "legacy"
    if "features" in body:
        fields["features"] = _features_field(body)
    if "expires_days" in body:
        d = _num(body["expires_days"], int, 0, 3650, "expires_days")
        fields["expires_at"] = (time.time() + d * 86400) if d > 0 else None
    await st.db.update_key(key_id, fields)
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
    return {"ok": True, "key": _key_json(row, counter, totals.get(key_id, 0))}


@router.delete("/keys/{key_id}")
async def delete_key(request: Request, key_id: int):
    require_admin(request)
    if not await request.app.state.gate.db.get_key(key_id):
        raise HTTPException(404, "key 不存在")
    await request.app.state.gate.db.delete_key(key_id)
    return {"ok": True}


@router.get("/logs")
async def logs(request: Request, key_id: Optional[int] = None, page: int = 1):
    require_admin(request)
    per_page = 20
    page = max(1, min(int(page), 1_000_000))
    db = request.app.state.gate.db
    total = await db.count_logs(key_id=key_id)
    pages = max(1, (total + per_page - 1) // per_page)
    page = min(page, pages)
    rows = await db.list_logs(limit=per_page, offset=(page - 1) * per_page,
                              key_id=key_id)
    return {
        "logs": [dict(r) for r in rows],
        "page": page,
        "per_page": per_page,
        "total": total,
        "pages": pages,
    }


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
    return data


@router.get("/settings")
async def get_settings(request: Request):
    require_admin(request)
    st = request.app.state.gate
    v = await st.db.get_setting("global_monthly_anlas", st.settings.global_monthly_anlas)
    v5 = await st.db.get_setting("global_daily_v5", st.settings.global_daily_v5)
    return {"global_monthly_anlas": float(v or 0), "global_daily_v5": int(float(v5 or 0)),
            SETTING: await read_alert_threshold(st.db)}


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
    v = _num(body.get("global_monthly_anlas", 0), float, 0.0, 1000000.0, "global_monthly_anlas")
    await st.db.set_setting("global_monthly_anlas", v)
    g5 = _num(body.get("global_daily_v5", 0), int, 0, 100000, "global_daily_v5")
    await st.db.set_setting("global_daily_v5", g5)
    await st.db.set_setting(SETTING, threshold)
    return {"ok": True, "global_monthly_anlas": v, "global_daily_v5": g5, SETTING: threshold}


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
    reg = {int(r[0]): str(r[1]) for r in await st.db._db.execute_fetchall(
        "SELECT key_id, discord_id FROM discord_registrations")}
    out = []
    for row in await st.db.list_keys():
        counter = await st.db.get_counter(row["id"], today)
        w = week.get(row["id"], {})
        out.append({
            "id": row["id"], "name": row["name"], "enabled": bool(row["enabled"]),
            "is_admin": bool(row["is_admin"]), "discord_id": reg.get(row["id"]),
            "created_at": row["created_at"], "last_used_at": row["last_used_at"],
            "expires_at": row["expires_at"],
            "daily_images": row["daily_images"], "daily_v5": row["daily_v5"],
            "today": {"images": counter["images"], "v5": counter["v5"], "anlas": round(float(counter["anlas"]), 2),
                      "text_tokens": counter["text_tokens"], "requests": counter["requests"]},
            "week": {"images": int(w.get("images", 0)), "v5": int(w.get("v5", 0)),
                     "anlas": round(float(w.get("anlas", 0)), 2), "requests": int(w.get("requests", 0))},
            "total_images": totals.get(row["id"], 0),
        })
    return {"members": out}


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
    prompts, thumbs, days = await ops.audit_flags(st.db, st.settings)
    return {
        "registration": reg,
        "features": {"global": await feature_defs.global_flags(st.db), "labels": feature_defs.FEATURES},
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
