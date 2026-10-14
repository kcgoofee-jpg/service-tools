"""app/upstream_policy.py 与 app/upstream_errors.py（仅 upstream_error_message）的 golden 夹具：
crates/gate-core/tests/golden/upstream/*.jsonl（供 gate_core::upstream_policy / upstream_errors 对拍）。

依赖时间的 parse_retry_after / classify_upstream：把 `upstream_policy.time` 换成固定时钟 NOW，
Rust 侧传入同一个 `now`。
"""
from __future__ import annotations

import random
import time as real_time
import types

from _golden import Writer

from app import upstream_errors, upstream_policy

NOW = 1_791_900_000.0   # 2026-10-13 14:00:00 UTC，纯合成
upstream_policy.time = types.SimpleNamespace(time=lambda: NOW)

rng = random.Random(20261014)

with Writer("upstream/meta") as w:
    w.record(name="now", value=NOW)
    w.record(name="MODEL_403_MARKERS", value=list(upstream_policy.MODEL_403_MARKERS))
    w.record(name="RETRY_AFTER_MIN", value=upstream_policy.RETRY_AFTER_MIN)
    w.record(name="RETRY_AFTER_MAX", value=upstream_policy.RETRY_AFTER_MAX)

with Writer("upstream/clamp_retry_after") as w:
    vals = ["junk", None, float("inf"), float("-inf"), float("nan"), 0, -3, 10 ** 9, 30, 5, 5.0, 4.999, 3600, 3600.5, 3599.999,
            "7", " 12 ", "1e3", "nan", "inf", "-inf", "Infinity", "+7", "1_0", "", "abc", True, False, [], [1], {}, {"a": 1},
            1e308, -1e308, 2 ** 63, 2 ** 53 + 1, 1e-9, -0.0, 0.0, 20, 20.0, 5.0000001, "５", "٣"]
    for v in vals:
        w.call(upstream_policy.clamp_retry_after, v)
    for _ in range(600):
        w.call(upstream_policy.clamp_retry_after, rng.choice([rng.uniform(-10, 4000), rng.randint(-10, 4000),
                                                              str(rng.randint(-10, 4000)), str(rng.uniform(0, 4000))]))

# ----------------------------------------------------------------------------- HTTP 日期

WDAY = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
MON = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]


def fmt(ts: float, style: str) -> str:
    t = real_time.gmtime(ts)
    wd, mo = WDAY[t.tm_wday], MON[t.tm_mon - 1]
    return {
        "imf": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "imf_nogmt": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d}",
        "no_wday": f"{t.tm_mday} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "rfc850": f"{['Monday','Tuesday','Wednesday','Thursday','Friday','Saturday','Sunday'][t.tm_wday]}, "
                  f"{t.tm_mday:02d}-{mo}-{t.tm_year % 100:02d} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "rfc850_4": f"{wd}, {t.tm_mday:02d}-{mo}-{t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "asctime": f"{wd} {mo} {t.tm_mday:2d} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} {t.tm_year}",
        "plus0800": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} +0800",
        "minus0500": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} -0500",
        "est": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} EST",
        "pdt": f"{t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} PDT",
        "utc": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} UTC",
        "z": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} Z",
        "minus0000": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} -0000",
        "lower": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} gmt".lower(),
        "upper": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT".upper(),
        "no_sec": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d} GMT",
        "dots": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}.{t.tm_min:02d}.{t.tm_sec:02d} GMT",
        "dots2": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}.{t.tm_min:02d} GMT",
        "spaces": f"  {wd},   {t.tm_mday:02d}   {mo}  {t.tm_year}   {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d}   GMT  ",
        "nospace_comma": f"{wd},{t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "month_first": f"{mo} {t.tm_mday:02d} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "fullmonth": f"{wd}, {t.tm_mday:02d} {['January','February','March','April','May','June','July','August','September','October','November','December'][t.tm_mon-1]} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "tm_first": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} {t.tm_year} GMT",
        "two_digit_year": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year % 100:02d} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "multi": f"garbage, {wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "multi2": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT, 30",
        "tail_comma": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT,",
        "nbsp": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT",
        "fullwidth_digits": f"{wd}, {t.tm_mday:02d} {mo} {t.tm_year} {t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d} GMT".translate(
            {ord(str(d)): ord("０") + d for d in range(10)}),
    }[style]


STYLES = ["imf", "imf_nogmt", "no_wday", "rfc850", "rfc850_4", "asctime", "plus0800", "minus0500", "est", "pdt", "utc", "z",
          "minus0000", "lower", "upper", "no_sec", "dots", "dots2", "spaces", "nospace_comma", "month_first", "fullmonth",
          "tm_first", "two_digit_year", "multi", "multi2", "tail_comma", "nbsp", "fullwidth_digits"]
DELTAS = [-86400 * 365, -86400, -3600, -61, -1, 0, 1, 4, 5, 6, 20, 120, 3599, 3600, 3601, 86400, 10 ** 7, 3 * 10 ** 9]

RAW = [
    "Wed", "garbage", "2026-10-21T07:28:00Z", "tomorrow", "Wed, 21 Oct", "21 Oct 2026", "21 Oct 2026 07:28", "21 Oct 2026 07:28 -0000",
    "21 Oct 26 07:28 EST", "31 Apr 2026 00:00:00 GMT", "30 Feb 2026 00:00:00 GMT", "29 Feb 2028 00:00:00 GMT",
    "29 Feb 2027 00:00:00 GMT", "21 Oct 2026 24:00:00 GMT", "21 Oct 2026 23:60:00 GMT", "21 Oct 2026 23:59:60 GMT",
    "21 Oct 2026 23:59:59 GMT", "0 Oct 2026 00:00:00 GMT", "32 Oct 2026 00:00:00 GMT", "21 Foo 2026 00:00:00 GMT",
    "21 Oct 0 00:00:00 GMT", "21 Oct 00 00:00:00 GMT", "21 Oct 68 00:00:00 GMT", "21 Oct 69 00:00:00 GMT", "21 Oct 99 00:00:00 GMT",
    "21 Oct 100 00:00:00 GMT", "21 Oct 1 00:00:00 GMT", "21 Oct 9999 23:59:59 GMT", "21 Oct 10000 00:00:00 GMT",
    "21 Oct -5 00:00:00 GMT", "21 Oct 99999999999 00:00:00 GMT", "21 Oct 2026 00:00:00 +9999", "21 Oct 2026 00:00:00 +2400",
    "21 Oct 2026 00:00:00 +2359", "21 Oct 2026 00:00:00 -2359", "21 Oct 2026 00:00:00 +0530", "21 Oct 2026 00:00:00 +05",
    "21 Oct 2026 00:00:00 5", "21 Oct 2026 00:00:00 -5", "21 Oct 2026 00:00:00 FOO", "21 Oct 2026 00:00:00 +99999999999999999",
    "21 Oct 2026 00:00:00 +99999999999999999999999", "21 Oct 2026 00:00:00 -99999999999999999999999", "21 Oct 2026 00:00:00 +",
    "21 Oct 2026 00:00:00 -", "21 Oct 2026 00:00:00+0800", "21 Oct 2026 00:00:00-0800", "21 Oct 2026 00:00:00 +08:00",
    "21 Oct 2026 00:00:99999999999 GMT", "21 Oct 2026 99999999999:00:00 GMT", "99999999999 Oct 2026 00:00:00 GMT",
    "21 Oct 2026 1_0:00:00 GMT", "21 1_0 2026 00:00:00 GMT", "1_0 Oct 2026 00:00:00 GMT", "21 Oct 2_026 00:00:00 GMT",
    "21 Oct ²026 00:00:00 GMT", "21 Oct ²026 00:00:00 2026", "Mon, 21 Oct 2026 00:00:00 GMT", "mon 21 oct 2026 00:00:00 gmt",
    "Funday, 21 Oct 2026 00:00:00 GMT", "Fun,21 Oct 2026 00:00:00 GMT", ",21 Oct 2026 00:00:00 GMT", "a,b,c,21 Oct 2026 00:00:00 GMT",
    "21-Oct-2026 00:00:00 GMT", "21-Oct-26 00:00:00", "21-Oct 00:00:00 GMT", "21-Oct-2026-x 00:00:00 GMT", "21,Oct 2026 00:00:00 GMT",
    "21 Oct, 2026 00:00:00 GMT", "21 Oct 2026, 00:00:00 GMT", "21 Oct 2026 00:00:00, GMT", "21 Oct 2026, GMT", "Oct 21, 2026 00:00:00 GMT",
    "Oct 21 00:00:00 2026", "Oct 21 2026 00:00:00", "21 Oct 00:00:00 2026 GMT", "21 Oct 00:00:00,2026 GMT", "21 Oct 2026 00:00:00 GMT GMT",
    "21 Oct 2026 00:00:00 GMT x", "21 Oct 2026 00:00", "21 Oct 2026 0:0:0 GMT", "21 Oct 2026 00:00:00:00 GMT", "21 Oct 2026 00 GMT",
    "21 Oct 2026 00.00 GMT", "21 Oct 2026 00.00.00.00 GMT", "21 Oct 2026 00:00.00 GMT", ":", "::", ",", ",,", "5,6", "5, 6", "5 6 7 8 9",
    "  ", "\t", "-5", "1e9", "inf", "nan", "+5", "5.5", "٣", "１２", "12a", "a12",
    "1 1 1 1 1", "a b c d e", "21 21 2026 00:00:00 GMT", "oct oct 2026 00:00:00 GMT", "21 Oct 2026 00:00:00 -0000 x",
    "21 Oct 2026 00:00:00 gmt+0100", "21 Oct 2026 00:00:00 UTC+1", "21 Oct 2026 00:00:00 GMT+0100", "21 Oct 2026 00:00:00 +0100+0200",
    "1 Jan 1970 00:00:00 GMT", "1 Jan 0001 00:00:00 GMT", "1 Jan 1 00:00:00 +0100", "31 Dec 9999 23:59:59 -0100",
    "31 Dec 9999 23:59:59 GMT", "29 Feb 1900 00:00:00 GMT", "29 Feb 2000 00:00:00 GMT",
]

DIGITS = ["0", "1", "4", "5", "6", "7", "30", "59", "60", "3599", "3600", "3601", "99999", "007", "0005", " 12 ", "\t9\n",
          "1" * 300, "9" * 308, "9" * 309, "1" * 4300, "1" * 4301, "0" * 4300, "0" * 4301, "1" + "0" * 308, "1" + "0" * 309]

with Writer("upstream/parse_retry_after") as w:
    for d in DIGITS:
        w.call(upstream_policy.parse_retry_after, d)
    for s in RAW:
        w.call(upstream_policy.parse_retry_after, s)
    w.call(upstream_policy.parse_retry_after, None)
    for delta in DELTAS:
        for style in STYLES:
            w.call(upstream_policy.parse_retry_after, fmt(NOW + delta, style))
    # 随机时间点 + 随机格式
    for _ in range(1500):
        delta = rng.choice([rng.randint(-100000, 100000), rng.randint(-10 ** 8, 10 ** 8), rng.randint(0, 7200)])
        w.call(upstream_policy.parse_retry_after, fmt(NOW + delta, rng.choice(STYLES)))
    # 随机拼接的碎片（覆盖解析器的各条分支）
    atoms = ["Wed", "Wed,", "21", "Oct", "oct", "2026", "26", "07:28:00", "07:28", "07.28.00", "GMT", "UTC", "EST", "+0800",
             "-0500", "-0000", "Z", ",", " ", "-", "+", "1", "00", "Jan", "February", "31", "29", "99", "100", ":", ".", "x", "٣",
             "²", "１", "_", "23:59:59", "24:00:00", "12:60:00", "ＧＭＴ"]
    for _ in range(2500):
        n = rng.randint(1, 9)
        sep = rng.choice([" ", " ", " ", "", ", ", "\t"])
        w.call(upstream_policy.parse_retry_after, sep.join(rng.choice(atoms) for _ in range(n)))
    for _ in range(700):
        d = rng.choice(["Wed, ", "", "21 ", "garbage, "]) + f"{rng.randint(0, 33)} {rng.choice(MON + ['Foo'])} " \
            f"{rng.choice([0, 1, 26, 68, 69, 99, 100, 2026, 2027, 9999, 10000])} " \
            f"{rng.randint(0, 25):02d}:{rng.randint(0, 61):02d}:{rng.randint(0, 61):02d} " \
            f"{rng.choice(['GMT', 'UTC', '+0000', '-0000', '+0530', '-0930', '+2500', '-2400', 'EST', 'PST', 'XYZ', ''])}"
        w.call(upstream_policy.parse_retry_after, d)

with Writer("upstream/is_model_level_403") as w:
    texts = ["", None, "model 'nai-glm-4-6' doesn't exist", "model 'x' does not exist", "model 'x' is only available via OpenAI-compatible API",
             "model must be a valid enum", "MODEL MUST BE A VALID ENUM", "Doesn't Exist", "DOES NOT EXIST", "does  not  exist",
             "does not\nexist", "doesn’t exist", "blocked", '{"statusCode":403,"message":"blocked"}', "account suspended",
             '{"statusCode":403,"message":"model \'nai-glm-4-6\' doesn\'t exist"}', "İs only available via", "IS ONLY AVAILABLE VIA",
             "is only available", "only available via", "model must be valid enum", "猫头鹰 doesn't exist", "x" * 1000 + "does not exist",
             "K" * 10, "ſ", "K"]
    for t in texts:
        w.call(upstream_policy.is_model_level_403, t)
    pieces = ["doesn't", "exist", "does", "not", "is only", "available", "via", "model", "must be", "a valid", "enum", " ", "X", "猫"]
    for _ in range(500):
        w.call(upstream_policy.is_model_level_403, " ".join(rng.choice(pieces) for _ in range(rng.randint(0, 6))))

with Writer("upstream/classify_upstream") as w:
    statuses = [100, 200, 201, 204, 301, 400, 401, 402, 403, 404, 409, 413, 422, 429, 500, 502, 503, 504, 599, 0, -1, 999, 2 ** 31, 2 ** 63 - 1]
    headers = [None, "", "0", "7", "30", "3600", "99999", " 15 ", "junk", "-5", "1e9", "inf", fmt(NOW + 120, "imf"), fmt(NOW - 3600, "imf"),
               fmt(NOW + 7200, "rfc850"), "tomorrow", "1" * 4301, "9" * 309, "21 Oct 99999999999 00:00:00 GMT"]
    peeks = ["", "model 'x' doesn't exist", "blocked", '{"statusCode":403,"message":"model must be a valid enum"}']
    for st in statuses:
        for h in headers:
            for pk in peeks[:2]:
                w.call(upstream_policy.classify_upstream, st, h, pk)
    for st in (403, 401, 429, 500):
        for pk in peeks:
            w.call(upstream_policy.classify_upstream, st, peek=pk)
            w.call(upstream_policy.classify_upstream, st)
            w.call(upstream_policy.classify_upstream, st, retry_after_header="30", peek=pk)
            w.call(upstream_policy.classify_upstream, st, None, pk)
    for _ in range(300):
        w.call(upstream_policy.classify_upstream, rng.choice(statuses), rng.choice(headers), rng.choice(peeks))

with Writer("upstream/upstream_error_message") as w:
    for st in list(range(100, 600)) + [-1, 0, 1, 99, 600, 999, 1000, 2 ** 31, 2 ** 63 - 1, -2 ** 63]:
        w.call(upstream_errors.upstream_error_message, st)
