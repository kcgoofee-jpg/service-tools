"""公开页面、健康检查、静态文件、状态看板与用户订阅视图路由。

从原 main.py 剥离，实现公开展示层与核心生图网关的清晰解耦。
"""
from __future__ import annotations

import json
import time
from pathlib import Path
from urllib.parse import urlsplit

from fastapi import APIRouter, HTTPException, Request, Response
from fastapi.responses import FileResponse, HTMLResponse, JSONResponse

from .. import live, request_timing, site_flags
from ..audit import audit_disclosure
from ..client_views import subscription_payload

router = APIRouter()

# 公告是管理员存储的 HTML：沙箱化后脚本无法执行，也读不到与 /admin 同源的数据。
_ANNOUNCEMENT_HEADERS = {
    "Content-Security-Policy": "sandbox allow-popups allow-popups-to-escape-sandbox",
    "X-Content-Type-Options": "nosniff"
}

LANDING_HEADERS = {
    "Content-Security-Policy": (
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; "
        # 头像来自 Discord CDN（以前被挡，成员看到的是空白圆）
        "connect-src 'self'; img-src 'self' data: https://cdn.discordapp.com; frame-src 'self'; "
        "base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    ),
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "Cache-Control": "no-cache",
    # 首页 HTML 现在按登录态不同（<body class="authed haskey">）：共享缓存不能把成员的版本发给你
    "Vary": "Cookie",
}

_STATIC_DIR = Path(__file__).parent.parent / "static"
_FAVICON = (_STATIC_DIR / "favicon.svg").read_bytes()

# 首页首屏模板（只读一次）：成员版本把登录态与骨架屏直接写进 HTML
_LANDING_HTML: dict = {"text": None}
_ME_CARD = '<div class="me" id="meCard" hidden></div>'
_ME_CARD_SKELETON = ('<div class="me" id="meCard"><div class="me-tiles">'
                     '<div class="me-tile skel-tile"></div><div class="me-tile skel-tile"></div>'
                     '</div></div>')


def landing_html(body_class: str) -> str:
    """成员版首屏：先写 class，再放额度骨架屏，避免刷新时闪一下新用户教程。"""
    if _LANDING_HTML["text"] is None:
        _LANDING_HTML["text"] = (_STATIC_DIR / "landing.html").read_text(encoding="utf-8")
    page = _LANDING_HTML["text"]
    if not body_class:
        return page
    page = page.replace(_ME_CARD, _ME_CARD_SKELETON, 1)
    return page.replace("<body>", f'<body class="{body_class}">', 1)

_PUBLIC_STATUS_CACHE: dict = {"at": 0.0, "body": None}
_CLIENT_ERR: dict = {"window": 0.0, "total": 0, "ip": {}}

ADMIN_HEADERS = {
    "Content-Security-Policy": (
        "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; "
        "img-src 'self' data: https://cdn.discordapp.com; frame-src 'self'; object-src 'none'; "
        "base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    ),
    "X-Frame-Options": "DENY",
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "Cache-Control": "no-store",
}


def _state():
    from .. import main
    return getattr(main, "STATE", None)


def _settings():
    from .. import main
    return getattr(main, "SETTINGS", None)


@router.get("/healthz")
async def healthz():
    # 不返回版本号：匿名接口给出精确版本等于给攻击者指纹定位（部署信息只在后台看）
    state = _state()
    return {"ok": True, "upstream": state.nai.configured if state else False}


@router.get("/")
async def index(request: Request):
    """成员落地页：自包含的静态页面，数据来自 /public/status 与 /public/me。

    登录态在服务端就写进 `<body class="authed haskey">`：否则成员每次刷新都会先渲染
    「三步上手 / 使用须知」，等 /public/me 回来才被 JS 隐藏 —— 就是成员反馈的那一下闪烁。
    游客仍然直接送静态文件（不查库、不多做事）。"""
    from ..registration_routes import member_state
    who, key = await member_state(request)
    if who is None:
        return FileResponse(_STATIC_DIR / "landing.html", headers=LANDING_HEADERS)
    body_class = "authed haskey" if key is not None else "authed"
    return HTMLResponse(landing_html(body_class), headers=LANDING_HEADERS)


@router.get("/favicon.ico", include_in_schema=False)
@router.get("/favicon.svg", include_in_schema=False)
async def favicon():
    """🦉 站点图标；/favicon.ico 也返回 SVG，现代浏览器都能识别，避免日志里出现 404。"""
    return Response(_FAVICON, media_type="image/svg+xml",
                    headers={"Cache-Control": "public, max-age=86400", "X-Content-Type-Options": "nosniff"})


@router.get("/announcement")
async def announcement():
    """站长在后台编写的公告 HTML：沙箱化后嵌入落地页，其中脚本不会执行。"""
    settings = _settings()
    p = settings.announcement_path
    html = p.read_text(encoding="utf-8") if p.exists() else ""
    if not html.strip():
        raise HTTPException(404, "没有公告")
    return Response(html, media_type="text/html", headers=_ANNOUNCEMENT_HEADERS)


@router.get("/public/live")
async def public_live(request: Request):
    """首页实时架构图：每秒轮询，只含汇总数字（请求数、额度、排队数、账号保护、名额）。"""
    state = _state()
    body = await live.build(state, getattr(request.app.state, "registrar", None))
    # 登录的成员：附上「我的排队」，看板第一格直接显示排第几（不用再粘贴 Key）
    from ..registration_routes import _member_session
    discord_id = await _member_session(request)
    me = None
    if discord_id:
        reg = getattr(request.app.state, "registrar", None)
        key = await reg.key_row_for(discord_id) if reg is not None else None
        if key is not None and key["enabled"]:
            me = live.mine(state, key["id"])
    return JSONResponse({**body, "me": me}, headers={"Cache-Control": "no-store"})


def _injected_script_error(msg: str, stack: str, host: str = "") -> bool:
    """手机自带浏览器（荣耀 / 华为 / 小米等）、翻译和广告插件往页面注入的脚本出的错：我们修不了，不进 Bug 追踪。
    特征：堆栈里有调用帧，但没有一帧来自本站文件（全是 <anonymous> / 扩展协议）；或者是没有来源的「Script error.」。"""
    if msg.strip() == "Script error.":
        return True
    frames = [ln.strip() for ln in stack.splitlines() if ln.strip().startswith("at ") or "@" in ln]
    if not frames:
        return False
    ours = tuple(x for x in ("/static/", "/admin", host, "127.0.0.1", "localhost") if x)
    return not any(any(o in f for o in ours) for f in frames)


@router.post("/public/client-error")
async def client_error(request: Request):
    """首页 / 后台网页的脚本报错上报（只收本站页面的错误；每 IP 每 10 分钟 10 条，全站每 10 分钟 100 条）。"""
    _ref = request.headers.get("origin") or request.headers.get("referer") or ""
    if _ref:
        _oh = urlsplit(_ref).netloc.lower()
        _host = (request.headers.get("host") or "").lower()
        if _oh and _host and _oh != _host:
            return Response(status_code=204)
    now = time.time()
    if now - _CLIENT_ERR["window"] > 600:
        _CLIENT_ERR.update(window=now, total=0, ip={})
    ip = request.client.host if request.client else "?"
    if _CLIENT_ERR["total"] >= 100 or _CLIENT_ERR["ip"].get(ip, 0) >= 10:
        return Response(status_code=204)
    _CLIENT_ERR["total"] += 1
    _CLIENT_ERR["ip"][ip] = _CLIENT_ERR["ip"].get(ip, 0) + 1
    try:
        data = json.loads((await request.body())[:4096] or b"{}")
    except ValueError:
        return Response(status_code=204)
    if not isinstance(data, dict):
        return Response(status_code=204)
    page = str(data.get("page") or "")[:20]
    page = page if page in ("landing", "admin") else "other"
    msg = request_timing.client_name(str(data.get("msg") or ""))[:200] or "unknown"
    where = request_timing.client_name(f"{data.get('src') or ''}:{data.get('line') or ''}")
    stack = str(data.get("stack") or "")[:1500]
    if _injected_script_error(msg, stack, (request.headers.get("host") or "").lower()):
        return Response(status_code=204)
    ua = request_timing.client_name(request.headers.get("user-agent", ""))
    from .. import main
    main.bug(f"web:{page}", title=msg, detail=f"{where}\n{ua}\n{stack}", path=page, level="warn")
    return Response(status_code=204)


@router.get("/v1/live/me")
async def live_me(request: Request):
    """需要 Key：这把 Key 的图现在排第几 / 是否在生成。不写用量日志。"""
    from .. import main
    try:
        key = await main.authenticate(request, passive=True)
    except main.GateError as exc:
        return JSONResponse({"mine": None, "error": exc.message}, status_code=exc.status,
                            headers={"Cache-Control": "no-store"})
    state = _state()
    return JSONResponse(live.mine(state, key["id"]), headers={"Cache-Control": "no-store"})


@router.get("/public/status")
async def public_status(request: Request):
    """落地页使用的公开状态：不含任何密钥、成员信息或计数细节。缓存 8 秒，避免匿名请求放大数据库压力。"""
    now = time.monotonic()
    if _PUBLIC_STATUS_CACHE["body"] is not None and now - _PUBLIC_STATUS_CACHE["at"] < 8:
        return JSONResponse(_PUBLIC_STATUS_CACHE["body"], headers={"Cache-Control": "no-store"})
    body = await _public_status_body(request)
    _PUBLIC_STATUS_CACHE.update(at=now, body=body)
    return JSONResponse(body, headers={"Cache-Control": "no-store"})


def _public_limits() -> dict:
    """首页「使用须知」里的排队与保底规则；数值来自后台「账号保护与排队」。"""
    state = _state()
    guard = getattr(state, "guard", None)
    if guard is None:
        return {}
    v = guard.values
    return {"key_image_queue": v["key_image_queue"], "base_daily_images": v["base_daily_images"],
            "quiet_start": v["quiet_start"], "quiet_end": v["quiet_end"],
            "quiet": v["quiet_start"] != v["quiet_end"]}


async def _image_stability() -> dict:
    state = _state()
    getter = getattr(state.db, "image_stability", None) if state else None
    if getter is None:
        return {"ok": 0, "error": 0}
    try:
        return await getter(time.time() - 24 * 3600)
    except Exception:
        return {"ok": 0, "error": 0}


async def _public_status_body(request: Request) -> dict:
    from .. import features as feature_defs
    state = _state()
    settings = _settings()
    service = getattr(request.app.state, "registrar", None)
    flags = await feature_defs.global_flags(state.db)
    reg = {"open": False, "slots_left": None}
    defaults = ["image"] if flags.get("image") else []
    if service is not None:
        cfg = await service.settings()
        reg["open"] = cfg["open"]
        if cfg["max_users"]:
            reg["slots_left"] = max(0, cfg["max_users"] - await service.count_active())
        names = feature_defs.FEATURES if cfg["features"] is None else cfg["features"]
        defaults = [n for n in names if flags.get(n) and n != "voice"]
    p = settings.announcement_path
    return {
        "site": settings.site_url.rstrip("/"),
        "upstream": {k: v for k, v in state.upstream_health().items() if k in ("status", "image_cooldown_seconds")},
        "registration": reg,
        "default_features": [{"id": n, "label": feature_defs.FEATURES[n]} for n in defaults],
        "audit_notice": await audit_disclosure(state.db, settings),
        "web_login": not await site_flags.get(state.db, site_flags.WEB_LOGIN_PAUSED),
        "algo_notice": (await site_flags.get(state.db, site_flags.ALGO_NOTICE))[:300],
        "discord_invite": settings.discord_invite_url,
        "key_inactivity_delete_days": settings.key_inactivity_delete_days,
        "limits": _public_limits(),
        "image_jobs": len(getattr(state, "image_reservations", {}) or {}),
        "stability": await _image_stability(),
        "has_announcement": bool(p.exists() and p.read_text(encoding="utf-8").strip()),
    }


@router.get("/admin")
async def admin_page():
    return FileResponse(_STATIC_DIR / "index.html", headers=ADMIN_HEADERS)


@router.get("/user/subscription")
@router.get("/ai/user/subscription")
async def user_subscription(request: Request):
    """给原生 NAI 客户端的虚拟订阅视图，不暴露真实上游账户余额。"""
    from .. import main
    key = await main.authenticate(request)
    return await subscription_payload(_state(), key)


def _virtual_priority() -> dict:
    """官方 /user/priority 的形状（2026-10-13 实测 image.novelai.net 返回 200）：
    {maxPriorityActions, nextRefillAt, taskPriority}。本站没有「买优先队列」这回事，
    如实回 0 次优先操作、下个整点刷新，别编造余额。"""
    now = int(time.time())
    return {"maxPriorityActions": 0, "nextRefillAt": now + (3600 - now % 3600), "taskPriority": 10}


@router.get("/user/data")
@router.get("/ai/user/data")
async def user_data(request: Request):
    from .. import main
    key = await main.authenticate(request)
    sub = await subscription_payload(_state(), key)
    public_sub = {name: value for name, value in sub.items() if name != "naiGate"}
    return {"subscription": public_sub, "priority": _virtual_priority(),
            "trainingStepsLeft": sub["trainingStepsLeft"],
            "anlas": sub["trainingStepsLeft"]["fixedTrainingStepsLeft"]}


@router.get("/user/priority")
@router.get("/ai/user/priority")
async def user_priority(request: Request):
    """原生客户端（NAI-Workbench 等）会探这个端点；404 会被当成「渠道不支持」。
    官方是 200，我们也回同样的形状。"""
    from .. import main
    await main.authenticate(request)
    return _virtual_priority()


@router.get("/user/keystore")
@router.get("/ai/user/keystore")
async def user_keystore(request: Request):
    """官方这个端点只允许浏览器会话（2026-10-13 实测：
    api.novelai.net → 403「Persistent access token is not allowed to access this endpoint.」）。
    以前我们回 404，客户端会以为「网关缺端点」。照官方的说法回 403，客户端就知道这是
    「令牌无权访问」而不是「网关坏了」。"""
    from .. import main
    await main.authenticate(request)
    raise main.err(403, "该项目仅限浏览器会话访问（NovelAI 官方对持久令牌同样返回 403）")


@router.get("/user/information")
@router.get("/ai/user/information")
async def user_information(request: Request):
    from .. import main
    key = await main.authenticate(request)
    return {"tier": 3, "active": True, "username": key["name"],
            "expiresAt": int(key["expires_at"] or time.time() + 3650 * 86400)}


@router.get("/queue-status")
async def queue_status():
    state = _state()
    return state.queue_snapshot() if state else {}
