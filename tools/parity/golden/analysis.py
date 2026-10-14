"""上游表现分析 / 排队影子分析 / Bug 追踪 / 客户端识别的 golden 夹具。

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/analysis.py

覆盖 app/perf.py、app/shadow.py（不含 replay 及其派生字段）、app/errors.py（特征码与事件字段）、
app/request_timing.py（客户端识别 / 耗时换算）、app/client_views.py（订阅视图）。
全部是合成数据、固定种子，重复运行输出逐字节一致。

生成的夹具：
    perf/pure          family / _pct / _kind / _stats / _hourly / _flags
    perf/analyze       analyze 整体输出
    shadow/pure        _pct / _requests / contention / busy_windows / per_member
    shadow/analyze     analyze 输出（已去掉 fifo / drr / by_accounts 三个由 replay 派生的字段）
    errors/signature   特征码与数字折叠
    errors/capture     capture 的事件字段、日志行、堆栈打印节流
    errors/notify      入库后「新 bug / 复发」私信
    client/name        client_name / begin 的客户端识别 / refine_client_from_payload / 耗时换算
    client/subscription subscription_payload
"""
from __future__ import annotations

import asyncio
import builtins
import copy
import random
import traceback
from types import SimpleNamespace
from unittest import mock

from _golden import Writer, enc

from app import client_views, errors, perf, request_timing, shadow, site_flags

rng = random.Random(20261016)
NOW = 1_791_900_000.0
HOUR, DAY = 3600, 86400


# =============================================================== perf

MODELS = ["nai-diffusion-4-5-full", "nai-diffusion-4-5-curated", "nai-diffusion-5-full", "NAI-Diffusion-5-Medium",
          " nai-diffusion-5 ", "nai-diffusion-4-full", "nai-diffusion-3", "", None, "other", "NAI-DIFFUSION-4-5-FULL",
          "nai-diffusion-4-5", "nai-diffusion-5", "x-nai-diffusion-5", "K-model", "nai-diffusion-4-5İ"]


def family_cases(w: Writer) -> None:
    for m in MODELS + ["\t nai-diffusion-5　", "nai-diffusion-4-5 ", "Nai-Diffusion-5-Curated", "猫", "nai-diffusion-"]:
        w.call(perf.family, m)


def pct_cases(w: Writer) -> None:
    w.call(perf._pct, [], 0.5)
    for _ in range(300):
        vals = [rng.choice([rng.randint(1, 60000), rng.randint(1, 20)]) for _ in range(rng.randint(1, 40))]
        for q in (0.5, 0.9, 0.0, 1.0, 0.25, 0.99, 0.1, 0.333):
            w.call(perf._pct, vals, q)


def kind_cases(w: Writer) -> None:
    for status in ("ok", "error", "rejected", ""):
        for up in (0, 200, 400, 401, 402, 403, 404, 429, 500, 502, 599, 600):
            for detail in ("", None, "429", "上游 429 限流", "x429y", "error 4290", "other"):
                w.call(perf._kind, status, up, detail)


def rand_row(ts, model=None, force_ok=None, dur_scale=8000):
    ok = (rng.random() < .85) if force_ok is None else force_ok
    status = "ok" if ok else rng.choice(["error", "error", "rejected"])
    up = 200 if ok else rng.choice([0, 429, 429, 401, 402, 403, 500, 502, 503, 404, 400])
    detail = None if rng.random() < .1 else rng.choice(["", "上游 429", "timeout", "x", "429 too many"]) if not ok else rng.choice(["", None, "ok"])
    dur = 0 if rng.random() < .06 else int(dur_scale * rng.uniform(.5, 2.0))
    return (ts, model if model is not None else rng.choice(MODELS), status, rng.randint(1, 4) if ok else rng.choice([0, 0, 1]),
            int(rng.expovariate(1 / 2500)), dur, up, detail)


def rand_rows(n: int, now: float = NOW, models=None) -> list:
    rows = []
    scale = {m: rng.choice([3000, 6000, 12000, 25000]) for m in MODELS}
    for _ in range(n):
        r = rng.random()
        if r < .35:
            ts = now - rng.uniform(0, HOUR)
        elif r < .65:
            ts = now - rng.uniform(HOUR, DAY)
        elif r < .93:
            ts = now - rng.uniform(DAY, 7 * DAY)
        elif r < .97:
            ts = now - rng.uniform(7 * DAY, 8 * DAY)
        else:
            ts = now + rng.uniform(0, 600)
        m = rng.choice(models or MODELS)
        # 最近一段时间故意变慢 / 变坏，覆盖各条提示
        factor = 1.0
        if ts >= now - HOUR:
            factor = rng.choice([1.0, 1.0, 2.0, 3.5])
        row = rand_row(ts, m, dur_scale=scale[m] * factor)
        rows.append(row)
    rows.sort(key=lambda r: r[0])
    return rows


def boundary_rows() -> list:
    """恰好落在窗口边界上的行。"""
    rows = []
    for off in (0, HOUR, HOUR + .001, HOUR - .001, DAY, DAY + .001, DAY - .001, 7 * DAY, 7 * DAY - .001, 7 * DAY + .001):
        for model in ("nai-diffusion-4-5-full", "nai-diffusion-5-full"):
            rows.append((NOW - off, model, "ok", 1, 100, 5000, 200, ""))
            rows.append((NOW - off, model, "error", 0, 100, 0, 429, "x"))
    start = (int(NOW) // HOUR) * HOUR - 23 * HOUR
    for k in range(0, 25):
        for eps in (-1e-6, 0.0, 1e-6, 0.5, 3599.9999999):
            rows.append((start + k * HOUR + eps, "nai-diffusion-4-5-full", "ok", 1, 10, 4000, 200, ""))
    return sorted(rows, key=lambda r: r[0])


def stats_cases(w: Writer) -> None:
    for _ in range(300):
        rows = rand_rows(rng.randint(0, 30))
        slots = rng.choice([0, 1, 1, 2, 3, 8])
        site = rng.choice([15.0, 0.0, 1.0, 5.5, 30.0, 0.001, 100.0, -3.0, 3600.0])
        key = rng.choice([15.0, 0.0, 30.0, 2.5, 600.0])
        hours = rng.choice([1.0, 1, 24, 24.0, 144, 0.5, 0, 0.0, -1.0])
        cap = rng.choice([0, 0, 50, 100, 240, 1000, -5])
        w.call(perf._stats, rows, slots, site, key, hours, cap)
    for _ in range(40):  # 成功、没有耗时的各种组合
        rows = [rand_row(NOW - 10, force_ok=True) for _ in range(rng.randint(1, 12))]
        rows = [(r[0], r[1], r[2], r[3], r[4], rng.choice([0, 0, r[5]]), r[6], r[7]) for r in rows]
        w.call(perf._stats, rows, 2, 15.0, 15.0, 1.0, 0)


def hourly_cases(w: Writer) -> None:
    for _ in range(120):
        now = NOW + rng.choice([0, 1, 1799.5, 3600, 12345.678, -86400])
        rows = rand_rows(rng.randint(0, 40), now)
        w.call(perf._hourly, rows, now)
    w.call(perf._hourly, boundary_rows(), NOW)
    w.call(perf._hourly, boundary_rows(), float((int(NOW) // HOUR) * HOUR))
    w.call(perf._hourly, boundary_rows(), float((int(NOW) // HOUR) * HOUR) - 1e-6)


def rand_stats_for_flags() -> dict:
    requests = rng.choice([0, 1, 4, 5, 6, 9, 10, 11, 30, 100])
    ok = rng.randint(0, requests)
    samples = rng.choice([0, 3, 4, 5, 6, 20])
    # 真实统计里「有耗时样本」才有 p50（样本数 == 0 ⇔ p50 为 None），不造不可能的状态
    p50 = rng.choice([1000, 2000, 4000, 7500, 10000, 15000, 30000]) if samples else None
    return {"requests": requests, "rate_limited": rng.choice([0, 0, 1, 2, 3, 4, requests // 5, requests // 3]) if requests else 0,
            "auth": rng.choice([0, 0, 0, 1, 5]), "samples": samples, "p50_ms": p50,
            "success_rate": round(ok / requests, 3) if requests and rng.random() < .95 else None}


def flags_cases(w: Writer) -> None:
    for _ in range(1500):
        h1, d1, base = rand_stats_for_flags(), rand_stats_for_flags(), rand_stats_for_flags()
        if rng.random() < .4 and base["p50_ms"]:  # 制造「变慢」
            for s in (h1, d1):
                if s["samples"] >= 5:
                    s["p50_ms"] = int(base["p50_ms"] * rng.choice([1.0, 1.49, 1.5, 1.51, 2.0, 3.3]))
        if rng.random() < .3 and base["success_rate"] is not None:  # 成功率下滑
            d1["success_rate"] = round(max(0.0, base["success_rate"] - rng.choice([0.05, 0.1, 0.11, 0.3])), 3)
        w.call(perf._flags, rng.choice(["V4.5", "V5", "其他"]), h1, d1, base)


def perf_analyze_cases(w: Writer) -> None:
    w.call(perf.analyze, [], NOW, slots=1, site_interval=15.0, key_interval=15.0, hourly_cap=0)
    w.call(perf.analyze, boundary_rows(), NOW, slots=2, site_interval=15.0, key_interval=15.0, hourly_cap=100)
    w.call(perf.analyze, boundary_rows(), NOW, slots=1, site_interval=1.0, key_interval=30.0, hourly_cap=0)
    for _ in range(260):
        rows = rand_rows(rng.choice([0, 3, 10, 40, 80, 160]), models=rng.choice([None, MODELS[:2], MODELS[2:4], [MODELS[0]], MODELS[6:]]))
        w.call(perf.analyze, rows, NOW + rng.choice([0, 0, 17.25, 3600]),
               slots=rng.choice([1, 1, 2, 3, 6]), site_interval=rng.choice([15.0, 15.0, 5.0, 1.0, 30.0]),
               key_interval=rng.choice([15.0, 15.0, 5.0, 60.0]), hourly_cap=rng.choice([0, 0, 100, 240, 1000]))


# =============================================================== shadow

def shadow_row(ts, key, name=None, status="ok", images=1, wait=0.0, dur=7.0):
    return (ts, key, name if name is not None else f"k{key}", status, images, int(wait * 1000), int(dur * 1000))


def rand_shadow_rows(n: int) -> list:
    rows = []
    t0 = NOW - 6 * HOUR
    keys = list(range(1, rng.choice([2, 3, 5, 9, 14])))
    for _ in range(n):
        key = rng.choice(keys) if rng.random() < .97 else None
        # 突发：到达时间扎堆
        arrival = t0 + rng.choice([rng.uniform(0, 600), rng.uniform(0, 6 * HOUR), rng.uniform(1000, 1100)])
        wait = rng.choice([0.0, 0.0, rng.uniform(0, 30), rng.uniform(0, 400)])
        dur = rng.choice([0.0 if rng.random() < .05 else rng.uniform(1, 25), rng.uniform(5, 12)])
        status = "ok" if rng.random() < .88 else "error"
        name = rng.choice([None, f"成员{key}", f"k{key}", ""]) if key is not None else None
        rows.append((arrival + wait + dur, key, name, status, rng.randint(1, 4) if status == "ok" else 0, int(wait * 1000), int(dur * 1000)))
    return rows


def load_ramp_rows() -> list:
    """先高负载、再低负载的几个阶段（覆盖「繁忙 ↔ 空闲」滞回的两个方向）。"""
    rows, t = [], 10_000.0
    for _ in range(rng.randint(2, 5)):
        spacing = rng.choice([6.0, 10.0, 15.0, 20.0, 40.0, 90.0, 200.0])
        dur = rng.choice([5.0, 8.0, 12.0, 20.0])
        for _ in range(rng.randint(10, 80)):
            t += spacing * rng.uniform(.8, 1.2)
            key = rng.choice([1, 2, 3, 4])
            rows.append(shadow_row(t + dur, key, wait=rng.choice([0.0, 3.0, 60.0]), dur=dur))
    return rows


def shadow_pure_cases(w: Writer) -> None:
    w.call(shadow._pct, [], 0.5)
    for _ in range(200):
        vals = [rng.choice([rng.uniform(0, 600), float(rng.randint(0, 40)), rng.randint(0, 5) + 0.05]) for _ in range(rng.randint(1, 30))]
        for q in (0.5, 0.9, 1.0, 0.0, 0.25):
            w.call(shadow._pct, vals, q)
    for _ in range(150):
        w.call(shadow._requests, rand_shadow_rows(rng.randint(0, 25)))
    w.call(shadow._requests, [(100.0, 1, "old", "ok", 1, 0, 0), (200.0, None, "x", "ok", 1, 5, 7000), (300.0, 2, None, "error", 0, 1500, 7000)])
    for _ in range(150):
        reqs = shadow._requests(rand_shadow_rows(rng.randint(0, 40)))
        w.call(shadow.contention, reqs)
    for _ in range(150):
        reqs = shadow._requests(rand_shadow_rows(rng.randint(0, 40)))
        w.call(shadow.busy_windows, reqs, slots=rng.choice([1, 1, 2, 3, 0]), interval=rng.choice([15.0, 15.0, 5.0, 1.0, 30.0]))
    for _ in range(80):
        reqs = shadow._requests(load_ramp_rows())
        w.call(shadow.busy_windows, reqs, slots=rng.choice([1, 1, 2, 3]), interval=rng.choice([15.0, 5.0, 10.0]))
    # 满负荷：连续 30 分钟同一个人
    heavy = [shadow_row(5000 + 15 * i, 7) for i in range(120)] + [shadow_row(5000 + 15 * i + 1, 8) for i in range(3)]
    w.call(shadow.busy_windows, shadow._requests(heavy), slots=1, interval=15.0)
    w.call(shadow.busy_windows, shadow._requests(heavy), slots=3, interval=15.0)
    for _ in range(300):
        keys = rng.sample(range(1, 30), rng.randint(0, 10))
        day_images = {k: rng.choice([0, 1, 5, 10, 11, 99, 100, 101, 300, rng.randint(0, 400)]) for k in keys}
        names = {k: rng.choice([f"成员{k}", None, ""]) for k in keys if rng.random() < .8}
        w.call(shadow.per_member, day_images, names, accounts=rng.choice([0, 1, 2, 3]), per_person=rng.choice([0, 0, 1, 5, 10, 100, 300, -3]),
               account_cap=rng.choice([0, 1000, 1370, 5, 1]))


def shadow_analyze_cases(w: Writer) -> None:
    REPLAY_FIELDS = ("fifo", "drr", "by_accounts")

    def run(rows, **kw):
        out = shadow.analyze(rows, **kw)
        # replay 派生的字段不移植（DRR / HTB 调度器已从线上移除），夹具里去掉
        for k in REPLAY_FIELDS:
            out.pop(k, None)
        w.record(fn="analyze", args=[rows], kwargs=kw, out=out, err=None,
                 note="已去掉 fifo / drr / by_accounts（replay 派生）")

    run([], slots=1, interval=15.0, key_interval=15.0, accounts=1, per_person=0, account_cap=1000)
    run(rand_shadow_rows(30), slots=1, interval=15.0, key_interval=15.0, accounts=1, per_person=100, account_cap=1000)
    heavy = [shadow_row(5000 + 15 * i, 7) for i in range(120)] + [shadow_row(5000 + 15 * i + 1, 8) for i in range(3)]
    run(heavy, slots=1, interval=15.0, key_interval=15.0, accounts=1, per_person=100, account_cap=1000)
    run([shadow_row(1000 + 300 * i, 1 + i % 3) for i in range(12)], slots=1, interval=15.0, key_interval=15.0, accounts=1, per_person=0, account_cap=1000)
    run([(100.0, 1, "old", "ok", 1, 0, 0), shadow_row(200, 1)], slots=1, interval=15.0, key_interval=15.0, accounts=1, per_person=0, account_cap=1000)
    for _ in range(60):
        run(load_ramp_rows(), slots=rng.choice([1, 1, 2, 3]), interval=rng.choice([15.0, 5.0, 10.0]), key_interval=15.0,
            accounts=rng.choice([1, 2]), per_person=rng.choice([0, 10, 100]), account_cap=rng.choice([1000, 50]))
    for _ in range(220):
        run(rand_shadow_rows(rng.choice([0, 2, 6, 15, 30, 60])), slots=rng.choice([1, 1, 2, 4]), interval=rng.choice([15.0, 5.0, 1.0, 30.0]),
            key_interval=rng.choice([15.0, 60.0]), accounts=rng.choice([1, 1, 2, 3]), per_person=rng.choice([0, 5, 10, 100, 300]),
            account_cap=rng.choice([0, 1000, 1370, 50]))


# =============================================================== errors

ERR_APP_DIR = "/srv/app"


def make_exc(frames: list, exc_type="ValueError", message="boom"):
    """造一个回溯经过指定文件 / 函数名的异常（outermost → innermost）；回溯里不含本脚本的帧。"""
    cls = {"ValueError": ValueError, "KeyError": KeyError, "RuntimeError": RuntimeError}.get(exc_type)
    if cls is None:
        cls = type(exc_type, (Exception,), {})
    exc = cls(message)
    ns: dict = {"EXC": exc}
    for i, (filename, func) in reversed(list(enumerate(frames))):
        body = "raise EXC" if i == len(frames) - 1 else f"{frames[i + 1][1]}_{i + 1}()"
        src = f"def {func}_{i}():\n    {body}\n"
        exec(compile(src, filename, "exec"), ns)
    try:
        ns[f"{frames[0][1]}_0"]()
    except BaseException as e:  # noqa: BLE001
        e.__traceback__ = e.__traceback__.tb_next      # 去掉本脚本这一帧
        return e
    raise AssertionError("没有抛出")


FRAME_SETS = [
    [("/srv/app/nai.py", "request")],
    [("/srv/app/routers/image_routes.py", "generate"), ("/srv/app/nai.py", "post")],
    [("/srv/app/main.py", "handler"), ("/nonexistent/py312/json/decoder.py", "raw_decode")],
    [("/nonexistent/py312/asyncio/tasks.py", "run")],
    [("/srv/app/a.py", "outer"), ("/nonexistent/py312/x.py", "mid"), ("/srv/app/b.py", "inner"), ("/nonexistent/site-packages/httpx/_client.py", "send")],
    [("/srv/app2/z.py", "prefix_quirk")],
    [("/srv/application.py", "prefix_quirk2")],
    [("/srv/app/../app/norm.py", "normalized")],
    [("/srv/app/猫.py", "函数")],
    [("/srv/ap/x.py", "short")],
    [("/srv/app/database.py", "execute"), ("/srv/app/database.py", "execute_inner")],
]
EXC_TYPES = ["ValueError", "KeyError", "RuntimeError", "UpstreamError", "错误类型", "TimeoutError_"]
MESSAGES = ["boom", "", "a" * 250, "多字节" * 100, "line1\nline2", "'quoted'", "code 502 after 1.5s"]
TITLES = ["", "", "", "自定义标题 502", "维护循环失败 3 次，耗时 1.5s", "T" * 400, "数字 １２３ ٣٤ 与 1.2.3 以及 4.", "x²½Ⅷ", "带\n换行 12",
          "v1.2.3-45", "0", "...1...", "1.", ".5", "1.5.", "1..5", "a1.b2", "9" * 30, "٠١٢٣٤٥٦٧٨٩ १२३ ๑๒๓ １２３"]
SOURCES = ["background", "http_500", "upstream_5xx", "frontend", "中文来源", "s" * 80, "x|y", ""]


def analysis_exc_info(exc) -> dict:
    return {"type": type(exc).__name__, "message": str(exc), "app_frame": errors._app_frame(exc),
            "stack": "".join(traceback.format_exception(type(exc), exc, exc.__traceback__))}


def signature_cases(w: Writer) -> None:
    for t in TITLES + ["abc 12.5.7", "no digits", "9.9.9.9", "1.2.3.4.5", "٣.٤ ٥", "x1y2z3", "٣.", "1.٣", "１.２"]:
        w.record(fn="normalize_title", title=t, out=errors._NUM.sub("#", t)[:200])
    for _ in range(300):
        t = "".join(rng.choice(["a", "b", " ", "1", "22", "3.5", ".", "٣", "１", "x", "猫", "\n", "²", "-", "9.", "05"]) for _ in range(rng.randint(0, 120)))
        w.record(fn="normalize_title", title=t, out=errors._NUM.sub("#", t)[:200])
    w.record(fn="normalize_title", title="1" * 300, out=errors._NUM.sub("#", "1" * 300)[:200])
    w.record(fn="normalize_title", title="a" * 190 + "12345 tail", out=errors._NUM.sub("#", "a" * 190 + "12345 tail")[:200])
    for src in SOURCES:
        for t in TITLES:
            w.record(fn="signature", source=src, exc=None, title=t, out=errors.signature(src, None, t))
    with mock.patch.object(errors, "_APP_DIR", ERR_APP_DIR):
        for _ in range(400):
            frames = rng.choice(FRAME_SETS)
            exc = make_exc(frames, rng.choice(EXC_TYPES), rng.choice(MESSAGES))
            src, title = rng.choice(SOURCES), rng.choice(TITLES)
            info = analysis_exc_info(exc)
            w.record(fn="signature", source=src, exc={"type": info["type"], "app_frame": info["app_frame"]}, title=title,
                     out=errors.signature(src, exc, title))
    # 随机长度的输入，覆盖 SHA-1 的各种填充分支（含刚好 55 / 56 / 63 / 64 字节边界）
    for n in list(range(0, 140)):
        s = "".join(rng.choice("abc猫|1.2 ") for _ in range(n))
        w.record(fn="signature", source=s, exc=None, title="t", out=errors.signature(s, None, "t"))


def capture_cases(w: Writer) -> None:
    printed: list = []

    def fake_print(*args, **kw):
        printed.append(args[0] if args else "")

    for _ in range(260):
        tracker = errors.Tracker()
        steps = []
        now = NOW
        # 同一序列里大概率反复出现同一个 bug（同来源 / 同异常类型 / 同位置），才测得到堆栈打印的 10 分钟节流
        base = (rng.choice(FRAME_SETS), rng.choice(SOURCES), rng.choice(EXC_TYPES))
        for _ in range(rng.randint(1, 6)):
            now += rng.choice([0.0, 1.5, 120.0, 599.0, 600.0, 600.5, 601.0, 5000.0])
            frames, src, exc_type = base if rng.random() < .75 else (rng.choice(FRAME_SETS), rng.choice(SOURCES), rng.choice(EXC_TYPES))
            use_exc = rng.random() < .75
            title = rng.choice(TITLES)
            detail = rng.choice(["", "", "额外说明", "d" * 4100, "x\ny", "猫" * 4001])
            path = rng.choice(["", "/api/x", "p" * 250, "/猫" * 120])
            rid = rng.choice(["", "ab12cd34", "r" * 30, "猫" * 20])
            key_id = rng.choice([None, None, 7, 123456])
            level = rng.choice(["error", "warn", "error", "fatal", "", "ERROR"])
            exc = None
            exc_rec = None
            if use_exc:
                with mock.patch.object(errors, "_APP_DIR", ERR_APP_DIR):
                    exc = make_exc(frames, exc_type, rng.choice(MESSAGES))
                    exc_rec = analysis_exc_info(exc)
            printed.clear()
            with mock.patch.object(errors.time, "time", lambda n=now: n), mock.patch.object(builtins, "print", fake_print):
                with mock.patch.object(errors, "_APP_DIR", ERR_APP_DIR):
                    sig = tracker.capture(src, exc, title=title, detail=detail, path=path, key_id=key_id, rid=rid, level=level)
            event = dict(tracker.recent[-1])
            steps.append({"source": src, "exc": exc_rec, "title": title, "detail": detail, "path": path, "key_id": key_id, "rid": rid,
                          "level": level, "now": now, "out": {"sig": sig, "event": event, "log_line": printed[0],
                                                              "stack_printed": len(printed) > 1,
                                                              "stack_text": printed[1] if len(printed) > 1 else None}})
        w.record(fn="capture_seq", steps=steps)


class FakeCursor:
    def __init__(self, row):
        self.row = row

    async def fetchone(self):
        return self.row


class FakeDB:
    def __init__(self, row):
        self.row = row

    async def execute(self, sql, params=()):
        return FakeCursor(self.row if sql.lstrip().upper().startswith("SELECT") else None)

    async def commit(self):
        pass


def notify_cases(w: Writer) -> None:
    for _ in range(300):
        row = rng.choice([None, (3, None), (1, 123.5), (9, 0.0)])
        calls: list = []
        tracker = errors.Tracker(db=SimpleNamespace(_db=FakeDB(row)), notify=lambda k, t, c: calls.append([k, t, c]))
        event = {"sig": "".join(rng.choice("0123456789abcdef") for _ in range(12)), "source": rng.choice(SOURCES)[:60],
                 "level": rng.choice(["error", "error", "warn"]), "title": rng.choice(TITLES + ["t" * 300])[:300],
                 "detail": "d", "path": "/p", "key_id": rng.choice([None, 4]), "rid": rng.choice(["", "ab12cd34"]), "ts": NOW}
        asyncio.run(tracker._store_locked(event))
        w.record(fn="store_notify", existing=None if row is None else {"resolved_at": row[1]}, event=event, calls=calls)


# =============================================================== client / request_timing

UA_POOL = ["", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36", "Dart/3.2 (dart:io)", "Aaalice_NAI_Launcher/1.0",
           "dsh-tavern-image/1.0", "DSH-Tavern-Image (x)", "Luker/2.0", "luker", "TauriTavern/0.4 tauri", "Tauri/2", "NAI-Workbench/1",
           "workbench", "LittleWhiteBox/9", "xiaobaihe", "LITTLEWHITEBOX", "python-httpx/0.27", "curl/8.4", "node-fetch",
           "SillyTavern dart", "sh-dsh", "tauri-dsh", "luker-tauri", "\x00\x01Luker\x7f/1\r\n", "  spaced  UA  ", "猫头鹰/1.0",
           "UA with\ttab and\nnewline", "x" * 300, "dsh" + "y" * 300, "Luker" + "z" * 260, "Aaalice" + "q" * 250, "éßDSH"]


def client_name_cases(w: Writer) -> None:
    pool = ["a", "B", " ", "\t", "\n", "\r", "\x00", "\x1f", "\x7f", "猫", "é", "　", " ", "/", "1", ".", "😀", "\x1c", "\x85"]
    for ua in UA_POOL + [None]:
        w.call(request_timing.client_name, ua)
    for _ in range(300):
        ua = "".join(rng.choice(pool) for _ in range(rng.randint(0, 60)))
        w.call(request_timing.client_name, ua)
    for n in (255, 256, 257, 300):
        w.call(request_timing.client_name, "猫" * n)
        w.call(request_timing.client_name, " " + "a" * n + " ")


def classify(headers) -> str:
    token = request_timing.begin({"headers": headers})
    try:
        return request_timing._TIMING.get()["client"]
    finally:
        request_timing.end(token)


def classify_cases(w: Writer) -> None:
    other_headers = [(b"host", b"x"), (b"accept", b"*/*"), (b"x-correlation-id", b"abc"), (b"x-initiated-at", b"1"), (b"user-agent", b"")]
    for ua in UA_POOL:
        for extra in ([], [(b"x-correlation-id", b"1")], [(b"x-initiated-at", b"2")], [(b"x-other", b"1")]):
            hdrs = [(b"user-agent", ua.encode("utf-8"))] + extra
            w.record(fn="begin_client", headers=hdrs, out=classify(hdrs))
    for _ in range(400):
        hdrs = []
        for _ in range(rng.randint(0, 5)):
            name, val = rng.choice(other_headers)
            if name == b"user-agent":
                val = rng.choice(UA_POOL).encode("utf-8", "replace") if rng.random() < .8 else bytes(rng.randrange(256) for _ in range(rng.randint(0, 20)))
            hdrs.append((name, val))
        w.record(fn="begin_client", headers=hdrs, out=classify(hdrs))
    # 没有 headers / 非 ASCII 的 latin-1 字节
    w.record(fn="begin_client", headers=[], out=classify([]))
    w.record(fn="begin_client", headers=[(b"user-agent", b"\xc0\xd6\xd8 DSH \xde\xff")], out=classify([(b"user-agent", b"\xc0\xd6\xd8 DSH \xde\xff")]))


def refine(current, body):
    holder = {"client": current}
    token = request_timing._TIMING.set(holder)
    try:
        request_timing.refine_client_from_payload(body)
        return holder["client"]
    finally:
        request_timing._TIMING.reset(token)


def lwb_body() -> dict:
    return {"use_new_shared_trial": True, "input": "x", "action": "generate",
            "parameters": {"params_version": 4, "stream": "msgpack", "image_format": "png", "model": "nai-diffusion-4-5-full"}}


def probe_body() -> dict:
    return {"input": "test", "action": "generate", "parameters": {"model": "nai-diffusion-3", "width": 64, "height": 64, "steps": 1, "n_samples": 1}}


def dsh_body() -> dict:
    return {"action": "generate", "parameters": {"params_version": 3, "ucPreset": 3, "add_original_image": True, "controlnet_strength": 1,
                                                 "v4_prompt": {"use_order": True, "use_coords": False}}}


VARIANTS = [1, 1.0, True, 0, False, None, "1", 3, 3.0, 4, 4.0, 4.5, "4", 64, 64.0, 63, "msgpack", "png", "generate", "test", "nai-diffusion-3",
            "nai-diffusion-4-5-full", [], {}, [1], {"a": 1}, -1, 2 ** 40, "", "x", 1.0000000000000002]


def mutate_body(body: dict) -> dict:
    body = copy.deepcopy(body)
    for _ in range(rng.randint(1, 3)):
        params = body.get("parameters") if isinstance(body.get("parameters"), dict) else None
        v4 = params.get("v4_prompt") if params is not None and isinstance(params.get("v4_prompt"), dict) else None
        target = rng.choice([t for t in (body, params, v4) if t is not None])
        k = rng.choice(list(target.keys()) + ["ucPresetId", "qualityPresetId", "parameters", "v4_prompt", "model", "stream", "width"])
        if rng.random() < .35:
            target.pop(k, None)
        else:
            target[k] = rng.choice(VARIANTS)
    return body


def refine_cases(w: Writer) -> None:
    currents = ["", "Mozilla/5.0", "Aaalice_NAI_Launcher (x)", "Luker (a)", "TauriTavern (b)", "NAI-Workbench (c)", "DSH-Tavern-Image (d)",
                "LittleWhiteBox (e)", "Luk", "dsh-tavern", "X" * 250, "猫" * 260]
    bases = [lwb_body, probe_body, dsh_body, lambda: {"parameters": {"ucPresetId": 1}}, lambda: {"parameters": {"qualityPresetId": 0}},
             lambda: {}, lambda: {"parameters": {}}]
    for cur in currents:
        for make in bases:
            body = make()
            w.record(fn="refine", current=cur, body=body, out=refine(cur, body))
    for _ in range(1800):
        body = mutate_body(rng.choice(bases)())
        cur = rng.choice(currents)
        w.record(fn="refine", current=cur, body=body, out=refine(cur, body))
    for body in ([], None, "x", 1, [1], {"parameters": None}, {"parameters": []}, {"parameters": "x"}, {"parameters": {"v4_prompt": []}}):
        w.record(fn="refine", current="Mozilla", body=body, out=refine("Mozilla", body))


def timing_cases(w: Writer) -> None:
    for _ in range(300):
        t0 = rng.uniform(0, 1e6)
        sent = None if rng.random() < .3 else t0 + rng.choice([0.0, 0.0004, 0.5, 2.5, 100.123456, 3.0009995])
        now = (sent if sent is not None else t0) + rng.choice([0.0, 0.0005, 1.0, 7.77777, 300.0004999, 12.3456789])
        holder = {"t0": t0, "sent": sent, "client": "c", "status": 0, "rid": "r"}
        token = request_timing._TIMING.set(holder)
        try:
            with mock.patch.object(request_timing.time, "monotonic", lambda n=now: n):
                snap = request_timing.snapshot()
        finally:
            request_timing._TIMING.reset(token)
        w.record(fn="snapshot_ms", t0=t0, sent=sent, now=now, wait_ms=snap["wait_ms"], dur_ms=snap["dur_ms"])
    for m in ("", None, "nai-diffusion-4-5-full", "x" * 79, "x" * 80, "x" * 81, "猫" * 100):
        holder = {}
        token = request_timing._TIMING.set(holder)
        try:
            request_timing.set_model(m)
            w.record(fn="model_label", model=m, out=request_timing.model())
        finally:
            request_timing._TIMING.reset(token)


# ---------------------------------------------------------------- subscription_payload

class SubDB:
    def __init__(self, used, v5, glimit, gtotal):
        self.used, self.v5, self.glimit, self.gtotal = used, v5, glimit, gtotal

    async def month_anlas(self, key_id, month):
        return self.used

    async def get_counter(self, key_id, day):
        return {"v5": self.v5}

    async def get_setting(self, name, default):
        return str(self.glimit)

    async def day_v5_total(self, day):
        return self.gtotal


def subscription_cases(w: Writer) -> None:
    nums = [None, 0, 0.0, 1, 5, 10, 100, 100.9, 250.5, -3, True, False, 3600.0, 1e6]
    flags = [True, False, 0, 1, None]
    for _ in range(900):
        key = {"id": rng.randint(1, 50)}
        for name, pool in (("is_admin", flags + ["missing"]), ("allow_anlas", flags + ["missing"] * 1), ("monthly_anlas", nums + ["missing"]),
                           ("daily_v5", nums + ["missing"]), ("exclude_global_v5", flags + ["missing"] * 3),
                           ("expires_at", [None, 0, 1893456000, 1893456000.7, 1.5e9, "missing"]),
                           ("image_model_scope", ["all", "v45", "v5", "", None, "missing"])):
            v = rng.choice(pool)
            if v != "missing":
                key[name] = v
        if rng.random() < .85:        # 绝大多数用例字段齐全
            key.setdefault("allow_anlas", rng.choice(flags))
            key.setdefault("monthly_anlas", rng.choice(nums))
            key.setdefault("daily_v5", rng.choice(nums))
            key.setdefault("expires_at", rng.choice([None, 0, 1893456000]))
            key.setdefault("image_model_scope", rng.choice(["all", "v45"]))
        data = {"month_anlas_used": rng.choice([0, 0.0, 20, 80.5, 99.99, 100, 250, 1e7]), "daily_v5_used": rng.choice([0, 1, 3, 9, 10, 11, 100]),
                "global_v5_limit": rng.choice([0, 0, 1, 150, 147, 100000]), "global_v5_total": rng.choice([0, 1, 147, 150, 151, 99999])}
        now = rng.choice([NOW, NOW + .5, 1.0e9])
        st = SimpleNamespace(db=SubDB(data["month_anlas_used"], data["daily_v5_used"], data["global_v5_limit"], data["global_v5_total"]),
                             day=lambda *_: "2026-10-14", month=lambda *_: "2026-10", settings=SimpleNamespace(global_daily_v5=7))
        # 全站上限以 site_flags 解析后的值为准（范围外会回落到默认值）
        data["global_v5_limit"] = asyncio.run(site_flags.get(st.db, site_flags.GLOBAL_DAILY_V5, st.settings))
        try:
            out = asyncio.run(client_views.subscription_payload(st, key, now=now))
            w.record(fn="subscription_payload", key=key, now=now, data=data, out=out, err=None)
        except (KeyError, TypeError, ValueError) as e:
            w.record(fn="subscription_payload", key=key, now=now, data=data, out=None, err=type(e).__name__)


# =============================================================== main

if __name__ == "__main__":
    with Writer("perf/pure") as w:
        family_cases(w)
        pct_cases(w)
        kind_cases(w)
        stats_cases(w)
        hourly_cases(w)
        flags_cases(w)
    with Writer("perf/analyze") as w:
        perf_analyze_cases(w)
    with Writer("shadow/pure") as w:
        shadow_pure_cases(w)
    with Writer("shadow/analyze") as w:
        shadow_analyze_cases(w)
    with Writer("errors/signature") as w:
        signature_cases(w)
    with Writer("errors/capture") as w:
        capture_cases(w)
    with Writer("errors/notify") as w:
        notify_cases(w)
    with Writer("client/name") as w:
        client_name_cases(w)
        classify_cases(w)
        refine_cases(w)
        timing_cases(w)
    with Writer("client/subscription") as w:
        subscription_cases(w)
    with Writer("analysis/consts") as w:
        w.record(fn="consts",
                 perf={k: getattr(perf, k) for k in ("FAMILIES", "HOUR", "DAY", "BASELINE_DAYS", "MIN_SAMPLES", "SLOW_RATIO",
                                                     "THROTTLE_COUNT_1H", "THROTTLE_SHARE", "FAIL_DROP", "FAIL_FLOOR_1H")},
                 shadow={k: getattr(shadow, k) for k in ("BASE_QUOTA", "CEIL_QUOTA", "ACCOUNT_DAILY_CAP", "BUSY_UTIL", "IDLE_UTIL",
                                                         "UTIL_WINDOW", "LIGHT_USER_MAX", "MAX_ROWS")},
                 errors={"LEVELS": errors.LEVELS, "DETAIL_MAX": errors.DETAIL_MAX},
                 request_timing={"CLIENT_MAX": request_timing.CLIENT_MAX})
