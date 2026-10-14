"""app/reasons.py 的 golden 夹具：crates/gate-core/tests/golden/reasons/*.jsonl（供 gate_core::reasons 对拍）。"""
from __future__ import annotations

import random

from _golden import Writer

from app import reasons

rng = random.Random(20261014)

with Writer("reasons/constants") as w:
    # 原因码常量、SITE_LEVEL、LABELS（有序）、BACKFILL（有序）：逐项对拍，中文文案一个字都不能差
    codes = {name: getattr(reasons, name) for name in dir(reasons)
             if name.isupper() and isinstance(getattr(reasons, name), str)}
    for name in sorted(codes):
        w.record(name=name, value=codes[name])
    w.record(name="SITE_LEVEL", value=list(reasons.SITE_LEVEL))
    w.record(name="LABELS", value=reasons.LABELS)
    w.record(name="BACKFILL", value=[list(item) for item in reasons.BACKFILL])

with Writer("reasons/code_of") as w:
    # Reason 是 str 子类，enc() 会丢掉 .code，所以手工记录构造参数
    cases = [
        ("reason", "排队的人太多", reasons.QUEUE_FULL),
        ("reason", "x", ""),
        ("reason", "x", None),
        ("reason", "", reasons.PACED),
        ("plain", "普通字符串", None),
        ("plain", "", None),
        ("none", None, None),
    ]
    for code in sorted(c for c in dir(reasons) if c.isupper() and isinstance(getattr(reasons, c), str)):
        cases.append(("reason", f"说明 {code}", getattr(reasons, code)))
    for kind, text, code in cases:
        if kind == "reason":
            obj = reasons.Reason(text, code)
        else:
            obj = text
        w.record(fn="code_of", args=[kind, text, code], out=reasons.code_of(obj), err=None)

DETAILS = [
    "", None, "429 排队的人太多（请稍后）", "429排队的人太多", "403 暂无权使用该功能", "400 图片参数无效：width 必须是非负整数",
    "503", "50", "5", " 429 ", "４２９ 全角数字开头", "٤٢٩ 阿拉伯数字开头", "4290 多一位", "429　全角空格", "429\n换行",
    "429\x1c控制符", "429 (括号里的)", "429 （全角括号）", "429，逗号", "429,逗号", "429。句号", "429：冒号", "429:冒号",
    "429；分号", "429;分号", "只有一句话", "第一句，第二句", "(开头括号)内容", "：开头冒号", "一二三四五六七八九十一二三四五六七八",
    "一二三四五六七八九十一二三四五六七八九", "一二三四五六七八九十一二三四五六七八九十，后面", "abcdefghijklmnopqrstuvwxyz",
    "abcdefghijklmnopqr", "abcdefghijklmnopqrs", "   ", "\t\n", "，", "429 ，", "429 ", "429", "a,b", " ，x", "😀" * 20,
    "😀" * 18, "😀" * 19, "猫头鹰" * 10, "  前后有空白  ", "中间 不换行空格,尾巴", "（只有括号）", "x（", "429 429 429",
    "429  　  文字",
]


def rand_detail() -> str:
    atoms = ["429", " ", "排队", "的人", "太多", "（", "）", "(", ")", "，", ",", "。", "：", ":", "；", ";", "abc", "猫", "😀",
             "　", "\n", "٣", "５", "x" * 10]
    return "".join(rng.choice(atoms) for _ in range(rng.randint(0, 14)))


with Writer("reasons/reason_label") as w:
    all_codes = sorted(c for c in dir(reasons) if c.isupper() and isinstance(getattr(reasons, c), str)
                       and not c.startswith("_")) + ["", "unknown", "cap_3h", "CAP_3H", "key_paused ", None]
    for code in all_codes:
        w.call(reasons.reason_label, code or "")
        w.call(reasons.reason_label, code or "", "429 无关的说明")
    for code in all_codes[:6]:
        for d in DETAILS:
            w.call(reasons.reason_label, code or "", d)
    for d in DETAILS:
        w.call(reasons.reason_label, "", d)
        w.call(reasons.reason_label, "", d if d is not None else "")
    for _ in range(1500):
        w.call(reasons.reason_label, rng.choice(["", "", "unknown", reasons.QUEUE_FULL, reasons.PARAM, reasons.CAP_3H]),
               rand_detail())
