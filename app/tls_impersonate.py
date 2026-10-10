"""可选：上游请求改用 curl_cffi 发出，TLS（JA3/JA4）与 HTTP/2 握手模拟 Chrome。

默认关闭（UPSTREAM_TLS_IMPERSONATE 留空）：本模块不会被导入，上游仍是原来的 httpx 客户端。
开启后 NaiClient 用 CurlCffiTransport 作为 httpx.AsyncClient 的传输层，
build_request / send(stream=True) / aclose 等调用方代码一行不改。

要点：
- 只用我们自己的请求头（default_headers=False），不让 curl 补 Chrome 导航类请求头
  （Upgrade-Insecure-Requests、Sec-Fetch-User），否则和 Sec-Fetch-Mode: cors 自相矛盾；
- Accept-Encoding 由 curl 按 Chrome 发并自动解压，回给 httpx 前去掉 Content-Encoding / Content-Length，
  避免 httpx 二次解压；
- 重定向交给 httpx（curl 不跟随）；Cookie 交给 httpx 的 Cookie 罐（curl 侧丢弃），与原来一致；
- curl 的异常映射成对应的 httpx 异常，nai.py 里「是否可能已扣费」的判断照常生效。
"""
from __future__ import annotations

import asyncio
import logging
import re
from typing import Any, Optional

import httpx

log = logging.getLogger(__name__)

# Chrome 真实发出的 Accept-Encoding（curl-impersonate 编译时带了 brotli / zstd）
CHROME_ACCEPT_ENCODING = "gzip, deflate, br, zstd"
MIN_CURL_CFFI = (0, 16)
# 不转交给 curl 的请求头：连接层由 curl 自己管；Accept-Encoding 用上面 Chrome 的那一套
_DROP_REQUEST_HEADERS = {"host", "content-length", "connection", "transfer-encoding",
                         "keep-alive", "accept-encoding"}
# curl 已解压，长度和编码头不再对应 body
_DROP_RESPONSE_HEADERS = {"content-encoding", "content-length", "transfer-encoding"}
_HTTP_VERSIONS = {1: b"HTTP/1.0", 2: b"HTTP/1.1", 3: b"HTTP/2", 30: b"HTTP/3"}


def resolve_target(value: Optional[str]) -> tuple[Optional[str], Optional[str]]:
    """返回 (规范化后的 curl_cffi 目标, 失败原因)。留空 → (None, None)；导入失败 / 目标不支持 → (None, 原因)。"""
    value = (value or "").strip().lower()
    if not value:
        return None, None
    try:
        import curl_cffi
        from curl_cffi import Curl
        from curl_cffi.requests import impersonate as imp
    except Exception as exc:          # 未安装 / 动态库加载失败：不能让启动崩掉
        return None, f"curl_cffi 导入失败（{type(exc).__name__}: {exc}）"
    # 0.16 之前流式请求在「收到响应头前出错」（超时 / 连不上）时会把同一个 curl 句柄还回池子两次，
    # 之后的请求会互相取消；低于该版本不启用
    ver = tuple(int(x) for x in re.findall(r"\d+", str(getattr(curl_cffi, "__version__", "0")))[:2])
    if ver < MIN_CURL_CFFI:
        return None, f"curl_cffi 版本 {curl_cffi.__version__} 过旧，需要 ≥ {'.'.join(map(str, MIN_CURL_CFFI))}"
    resolve = getattr(imp, "resolve_latest_browser_type", None) or getattr(imp, "normalize_browser_type")
    try:
        target = str(resolve(value))
        curl = Curl()
        try:
            ret = curl.impersonate(target, default_headers=False)
        finally:
            curl.close()
    except Exception as exc:
        return None, f"curl_cffi 不支持目标 {value!r}（{type(exc).__name__}: {exc}）"
    if ret != 0:
        return None, f"curl_cffi 不支持目标 {value!r}"
    return target, None


def chrome_major(target: Optional[str]) -> Optional[int]:
    """桌面 Chrome 目标（如 chrome136）的大版本号；其它（edge / safari / chrome131_android 等）返回 None。"""
    m = re.fullmatch(r"chrome(\d+)", target or "")
    return int(m.group(1)) if m else None


def chrome_sec_ch_ua(major: int) -> str:
    """按 Chromium 源码 GetGreasedUserAgentBrandVersion 生成 sec-ch-ua（以大版本号为种子）。
    已对照真实值：128/129/131/133/136。"""
    chars = [" ", "(", ":", "-", ".", "/", ")", ";", "=", "?", "_"]
    greasy = f'"Not{chars[major % 11]}A{chars[(major + 1) % 11]}Brand";v="{["8", "99", "24"][major % 3]}"'
    order = [(0, 1, 2), (0, 2, 1), (1, 0, 2), (1, 2, 0), (2, 0, 1), (2, 1, 0)][major % 6]
    out = [""] * 3
    out[order[0]] = greasy
    out[order[1]] = f'"Chromium";v="{major}"'
    out[order[2]] = f'"Google Chrome";v="{major}"'
    return ", ".join(out)


def chrome_profiles(profiles: list[dict[str, str]], major: int) -> list[dict[str, str]]:
    """把浏览器 profile 的 UA / sec-ch-ua 改成和 TLS 目标同一个 Chrome 大版本（平台不变）。"""
    out = []
    for p in profiles:
        q = dict(p)
        q["user_agent"] = re.sub(r"Chrome/\d+\.0\.0\.0", f"Chrome/{major}.0.0.0", p["user_agent"])
        q["sec_ch_ua"] = chrome_sec_ch_ua(major)
        out.append(q)
    return out


# curl 错误码（CURLE_*）：连接阶段失败，请求肯定没发出去
_CONNECT_CODES = {5, 6, 7, 35, 60}   # 解析代理 / 解析域名 / 连不上 / TLS 握手失败 / 证书校验失败
_PROXY_CODES = {97}


def _map_error(exc: Exception, request: httpx.Request, *, streaming: bool) -> httpx.HTTPError:
    """curl_cffi 异常 → httpx 异常（按 curl 错误码；流式模式下 curl_cffi 只给基类 + 错误码）。
    发出后无法确定对方收没收到的，一律按读错误（可能已扣费）处理。"""
    code = int(getattr(exc, "code", 0) or 0)
    msg = f"curl_cffi: {exc}"
    if not streaming:
        if code in _PROXY_CODES:
            return httpx.ProxyError(msg, request=request)
        if code in _CONNECT_CODES:
            return httpx.ConnectError(msg, request=request)
        if code == 28 and ("Connection timed out" in str(exc) or "Resolving timed out" in str(exc)):
            return httpx.ConnectTimeout(msg, request=request)
    if code == 28:
        return httpx.ReadTimeout(msg, request=request)
    return httpx.ReadError(msg, request=request)


class _CurlStream(httpx.AsyncByteStream):
    """把 curl_cffi 的流式响应包成 httpx 的响应体；提前关闭时中止传输，不等上游发完。"""

    def __init__(self, session: Any, response: Any, request: httpx.Request):
        self._session = session
        self._response = response
        self._request = request
        self._closed = False

    async def __aiter__(self):
        try:
            async for chunk in self._response.aiter_content():
                yield chunk
        except httpx.HTTPError:
            raise
        except Exception as exc:
            from curl_cffi.requests.exceptions import RequestException
            if isinstance(exc, RequestException):
                raise _map_error(exc, self._request, streaming=True) from exc
            raise

    async def aclose(self) -> None:
        if self._closed:
            return
        self._closed = True
        rsp = self._response
        task = getattr(rsp, "astream_task", None)
        if task is None or task.done():
            return
        # 告诉 curl 不要再写数据，并把句柄从 multi 里摘掉，传输立刻结束（否则 aclose 会一直等到上游发完）
        quit_now = getattr(rsp, "quit_now", None)
        if quit_now is not None:
            quit_now.set()
        try:
            self._session.acurl.remove_handle(rsp.curl)
        except Exception:
            pass
        await asyncio.wait([task], timeout=5)


class CurlCffiTransport(httpx.AsyncBaseTransport):
    """httpx 传输层：用 curl_cffi.AsyncSession(impersonate=target) 实际发送请求。"""

    def __init__(self, target: str, *, proxy: Optional[str] = None, max_clients: int = 32):
        self.target = target
        self._proxy = proxy or None
        self._max_clients = max_clients
        self._session: Any = None

    def _get_session(self) -> Any:
        # 延迟到第一次请求再建：AsyncSession 绑定当前事件循环
        if self._session is None:
            from curl_cffi.requests import AsyncSession
            self._session = AsyncSession(
                impersonate=self.target, proxy=self._proxy, max_clients=self._max_clients,
                default_headers=False, allow_redirects=False, discard_cookies=True)
        return self._session

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        session = self._get_session()
        body = await request.aread()
        headers = [(k, v) for k, v in request.headers.multi_items()
                   if k.lower() not in _DROP_REQUEST_HEADERS]
        t = request.extensions.get("timeout") or {}
        connect = t.get("connect") or 15
        read = t.get("read")
        # curl 流式模式：connect 超时 + 「低于 1 字节/秒持续 connect+read 秒」视为读超时；None 表示不限
        timeout = (connect, read) if read is not None else None
        try:
            rsp = await session.request(
                request.method, str(request.url), headers=headers, data=body or None,
                timeout=timeout, stream=True, accept_encoding=CHROME_ACCEPT_ENCODING)
        except httpx.HTTPError:
            raise
        except Exception as exc:
            from curl_cffi.requests.exceptions import RequestException
            if isinstance(exc, RequestException):
                raise _map_error(exc, request, streaming=False) from exc
            raise
        resp_headers = [(k, v) for k, v in rsp.headers.multi_items()
                        if k.lower() not in _DROP_RESPONSE_HEADERS and v is not None]
        return httpx.Response(
            rsp.status_code, headers=resp_headers, stream=_CurlStream(session, rsp, request),
            extensions={"http_version": _HTTP_VERSIONS.get(getattr(rsp, "http_version", 0), b"HTTP/1.1"),
                        "reason_phrase": (getattr(rsp, "reason", "") or "").encode("ascii", "ignore")},
            request=request)

    async def aclose(self) -> None:
        if self._session is not None:
            session, self._session = self._session, None
            await session.close()
