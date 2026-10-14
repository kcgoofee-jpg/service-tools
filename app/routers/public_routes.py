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


@router.get("/survey")
async def survey_page():
    """成员详细问卷页：自包含静态页，提交用虚拟 Key 识别身份（不落明文）。Discord 里的 /反馈 是另一个快速通道。"""
    return FileResponse(_STATIC_DIR / "survey.html", headers=LANDING_HEADERS)


# 服务条款 / 隐私政策：自包含静态文档页（Discord 开发者门户「APP 验证」要求配置这两个链接）
_DOC_HEADERS = {
    "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'; sandbox; frame-ancestors 'none'",
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "Cache-Control": "public, max-age=3600",
}


def _doc_page(title: str, updated: str, sections: list[tuple[str, list[str]]]) -> str:
    rows = "".join(
        f"<h2>{_esc(h)}</h2>" + "".join(f"<p>{_esc(p)}</p>" for p in ps) for h, ps in sections)
    return (
        '<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width,initial-scale=1">'
        f'<title>{_esc(title)} · 猫头鹰公益站</title><style>'
        'body{margin:0;background:#f6f5f1;color:#22211d;font:15px/1.75 -apple-system,"PingFang SC",'
        '"Microsoft YaHei",sans-serif;padding:2.5rem 1rem}'
        'main{max-width:46rem;margin:0 auto;background:#fff;border:1px solid #dedcd1;border-radius:12px;'
        'padding:2rem 2.2rem}'
        'h1{font-size:1.4rem;margin:.2rem 0}h2{font-size:1.05rem;margin:1.6rem 0 .4rem}'
        '.upd{color:#87867f;font-size:.85rem;margin:0 0 1rem}p{margin:.4rem 0}'
        '</style></head><body><main><h1>' + _esc(title) + '</h1><p class="upd">猫头鹰公益站（gate.davidzhao.top）'
        ' · 最近更新：' + _esc(updated) + '</p>' + rows + '</main></body></html>'
    )


def _esc(s: str) -> str:
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
             .replace('"', "&quot;"))


@router.get("/terms")
async def terms_page():
    """服务条款（Discord APP 验证要求）。内容与首页「使用须知」同一口径。"""
    html = _doc_page("服务条款", "2026-10-14", [
        ("服务性质", [
            "「猫头鹰公益站」（gate.davidzhao.top，下称「本站」）是由个人运营的非商业公益服务，"
            "为 Discord 社区「猫头鹰公益站」的成员提供 NovelAI 生图请求的中转（代理）。",
            "本站不保证服务可用性或出图质量，可能因上游（NovelAI）状况、账号额度、维护或风控随时暂停、"
            "限流、调整规则或终止服务，且无需事先通知。"]),
        ("资格与 Key", [
            "服务面向本站 Discord 社区成员。成员通过社区内 /register 命令领取专属 API Key，一人一把。",
            "Key 仅限本人使用。严禁转售、转借、多人共用或用于商业用途。多人共用或转售会被系统记录并回收 Key。"]),
        ("使用规范", [
            "严禁生成违反法律法规或极端有害的内容（包括但不限于违法、危害未成年人、仇恨与骚扰内容），"
            "违规账号将被永久封禁且不再恢复。",
            "请遵守本站公布的额度与并发规则（如一次一张、勿狂重试）。退出 Discord 服务器或 Key 长期闲置后，"
            "Key 会被自动回收。"]),
        ("额度与公平", [
            "本站为免费公益服务，出图额度有限。站方可根据上游状况随时调整每人额度、全站上限、"
            "限流与排队策略，并在状态页与社区公告中说明。"]),
        ("责任限制", [
            "在法律允许的最大范围内，本站按「现状」提供服务，不对任何直接或间接损失承担责任。",
            "使用本站即表示你已阅读并同意本条款。条款更新后在本页公布即生效，重大变更会在社区公告。"]),
        ("联系", ["如有疑问，请在 Discord 社区「猫头鹰公益站」内联系站长。"]),
    ])
    return HTMLResponse(html, headers=_DOC_HEADERS)


@router.get("/privacy")
async def privacy_page():
    """隐私政策（Discord APP 验证要求）。与实际数据流一致：提示词/用量/图片仅站长可见。"""
    html = _doc_page("隐私政策", "2026-10-14", [
        ("我们收集什么", [
            "Discord 账号信息：成员领取或登录时收集 Discord 用户 ID、用户名、显示名与头像，用于识别身份与展示。",
            "使用记录：Key 的请求时间、模型、尺寸、等待与拒绝原因等用量日志；你输入的生图提示词；生成的图片。",
            "如你使用「捐赠 Key」功能：你提交的 NovelAI Token、留言与联系方式。"]),
        ("用途", [
            "发放与管理 Key、展示个人额度；防滥用与防多人共用（行为统计）；公平调度与容量规划；社区公告与私信通知。"]),
        ("保存期限", [
            "用量日志与提示词长期保存（用于审计与防滥用）；生成的图片自动轮删（服务器空间满时删除最旧的，"
            "默认保留数天）；Discord 资料与领取记录在 Key 存续期间保存，Key 回收后随之清理或匿名化。"]),
        ("谁能看到", [
            "仅站长本人可查看上述数据。本站不会出售、出租或与第三方共享你的个人数据。"]),
        ("Cookie 与第三方", [
            "本站仅在浏览器写入一个登录态会话 Cookie（HttpOnly，约 30 天），不用于跨站跟踪。",
            "网页「用 Discord 登录」通过 Discord OAuth 完成，只申请 identify（身份识别）权限，"
            "不读取你的消息、好友或服务器列表；生图请求仅转发给 NovelAI 官方接口。"]),
        ("你的选择", [
            "退出 Discord 服务器即可使 Key 被回收；如需删除登记资料或用量数据，请在社区内联系站长。"]),
        ("联系", ["隐私相关问题请在 Discord 社区「猫头鹰公益站」内联系站长。"]),
    ])
    return HTMLResponse(html, headers=_DOC_HEADERS)


@router.post("/member/survey")
async def member_survey(request: Request):
    """问卷提交：Bearer 虚拟 Key 识别身份（passive 鉴权，不算活跃、不进防分享统计）；一人一份，重复提交覆盖。"""
    from .. import main
    try:
        key = await main.authenticate(request, passive=True)
    except main.GateError as exc:
        raise HTTPException(exc.status, exc.message) from None
    try:
        data = json.loads((await request.body())[:4096] or b"{}")
    except ValueError:
        raise HTTPException(400, "请求体不是合法 JSON") from None
    if not isinstance(data, dict):
        raise HTTPException(400, "请求体格式无效")
    if not str(data.get("understand") or "").strip() or not str(data.get("donate") or "").strip():
        raise HTTPException(422, "还有必选题没填：限额理解、捐赠意愿")
    improve = data.get("improve") or []
    if not isinstance(improve, list):
        raise HTTPException(422, "多选项格式无效")
    await _state().db.save_survey(
        key["id"],
        client=str(data.get("client") or "").strip(),
        understand=str(data.get("understand") or "").strip(),
        improve="、".join(str(i)[:40] for i in improve if str(i).strip()),
        improve_other=str(data.get("improve_other") or "").strip(),
        donate=str(data.get("donate") or "").strip())
    return {"ok": True}


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


_LIVE_META: dict = {"at": 0.0, "body": None}   # at 用 time.monotonic()


async def _live_meta(state, registrar) -> dict:
    """/public/live 的 meta 字段（10-14 重构）：变化很慢（站点地址、公告、注册开关、额度说明），
    单独 30s 子缓存，不跟着 1 秒轮询每次重算。原 30s 的 /public/status 轮询由它接替。"""
    now = time.monotonic()
    if _LIVE_META["body"] is not None and now - _LIVE_META["at"] < 30:
        return _LIVE_META["body"]
    settings = state.settings
    p = settings.announcement_path
    reg: dict = {"configured": False, "open": False}   # 没配注册服务 = 关闭（fail-closed，与 configured_service 一致）
    if registrar is not None:
        cfg = await registrar.settings()
        reg = {"configured": True, "open": cfg["open"]}
        if cfg["max_users"]:
            reg["slots_left"] = max(0, cfg["max_users"] - await registrar.count_active())
    body = {"site": settings.site_url.rstrip("/"),
            "discord_invite": settings.discord_invite_url,
            "has_announcement": bool(p.exists() and p.read_text(encoding="utf-8").strip()),
            "algo_notice": (await site_flags.get(state.db, site_flags.ALGO_NOTICE))[:300],
            "key_inactivity_delete_days": settings.key_inactivity_delete_days,
            "web_login": not await site_flags.get(state.db, site_flags.WEB_LOGIN_PAUSED),
            "home_join_open": await site_flags.get(state.db, site_flags.HOME_JOIN_OPEN),
            "audit_notice": await audit_disclosure(state.db, settings),
            "registration": reg}
    _LIVE_META.update(at=now, body=body)
    return body


@router.get("/public/live")
async def public_live(request: Request):
    """首页实时架构图：每秒轮询，只含汇总数字（请求数、额度、排队数、账号保护、名额）+ meta（30s 子缓存）。"""
    state = _state()
    registrar = getattr(request.app.state, "registrar", None)
    body = await live.build(state, registrar)
    # 浅拷贝拼响应：live.build 返回的是共享缓存对象，绝不能原地写（test_review_regressions 的红线）
    body = {**body, **await _live_meta(state, registrar)}
    # 登录的成员：附上「我的排队」，看板第一格直接显示排第几（不用再粘贴 Key）
    from ..registration_routes import _member_session
    discord_id = await _member_session(request)
    me = None
    if discord_id:
        key = await registrar.key_row_for(discord_id) if registrar is not None else None
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
