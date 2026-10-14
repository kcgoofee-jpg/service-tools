"""app/registration.py 的文案 / app/exporter.py 的打包结果 golden 夹具（合成数据）。

输出 crates/gate-server/tests/fixtures/registration_text.json：
  welcome  —— welcome_dm(key, site, quota, expires_days, idle_days, notice, opened)：逐字节对拍
  labels   —— member_label(user)
  export   —— exporter.build_zip / build_epub（时钟固定成 NOW，TZ=Asia/Shanghai）：条目名 + 文本条目的内容；
              图片条目只记名字和字节数（压缩包本身的字节不要求一致）
路由层的期望值（桥接 / 网页登录 / 个人状态 / 捐赠 / 打包）直接写在 crates/gate-server/tests/registration.rs 里，
那些值是旧版 registration_routes.py 在同样场景下的真实输出。

用法（仓库根目录，Python 3.12，与线上一致）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/registration.py

注意：本文件里不要写 \\uXXXX 转义，非 ASCII 字符直接写（UTF-8 源码）。
"""
from __future__ import annotations

import io
import json
import time as _real_time
import zipfile

import _golden

from app import exporter
from app.registration import member_label, welcome_dm

OUT = _golden.ROOT / "crates" / "gate-server" / "tests" / "fixtures" / "registration_text.json"
NOW = 1_760_000_000.0


class _FixedTime:
    """exporter 里的 time.time() / time.strftime(fmt)（不带时间元组 = 现在）固定成 NOW。"""

    @staticmethod
    def time() -> float:
        return NOW

    @staticmethod
    def strftime(fmt, t=None):
        return _real_time.strftime(fmt, t if t is not None else _real_time.localtime(NOW))

    localtime = staticmethod(_real_time.localtime)


exporter.time = _FixedTime

WELCOME = [
    ("nai-abc", "https://gate.test/", "V4.5 及以下 30 张", 30, 3, "", "文生图"),
    ("nai-abc", "https://gate.test/", "V4.5 及以下 30 张", 0, 0, "", ""),
    ("nai-abc", "https://gate.test/", "V4.5 及以下保底 20 张、全站空闲时最多 30 张；V5 5 张", 7, 0, "📢 本站会记录提示词，保留 7 天。", "文生图、放大"),
    ("nai-abc", "https://gate.test/", "V4.5 及以下 30 张", 30, 3, "记" * 2500, "全部已开放功能"),
]
LABELS = [
    {"id": "900000000000000001", "username": "neo", "global_name": "Neo"},
    {"id": "900000000000000001", "username": "neo", "global_name": "neo"},
    {"id": "900000000000000001", "username": "neo", "global_name": None},
    {"id": "900000000000000001", "username": None, "global_name": "Neo"},
    {"id": "900000000000000001", "username": "", "global_name": ""},
    {"id": "900000000000000001"},
    {"id": "900000000000000001", "username": "u" * 90, "global_name": "d" * 90},
    {"id": "900000000000000001", "username": "u" * 90, "global_name": None},
    {"id": "900000000000000001", "username": "猫" * 70, "global_name": "头鹰" * 20},
]
ROWS = [
    (1, NOW - 3600, "image/png", "1girl, <forest> & 'mist'", 'lowres "bad"',
     '{"params":{"seed":42,"sampler":"k_euler","steps":28,"scale":5,"width":832,"height":1216}}', b"PNG-ONE"),
    (2, NOW - 3500, "image/jpeg", "1boy", "", '{"params":{"seed":0,"scale":5.5,"steps":0,"width":832}}', b"JPEG-TWO"),
    (3, NOW - 3400, "image/webp", "plain", "neg", "not json", b"WEBP-THREE"),
    (4, NOW - 3300, "image/gif", "unknown type", "", "", b"GIF-FOUR"),
    (5, NOW - 3200, "image/png", "null params", "", '{"params":null}', b"PNG-FIVE"),
]


def archive(blob: bytes) -> dict:
    with zipfile.ZipFile(io.BytesIO(blob)) as z:
        names = z.namelist()
        text = {}
        for n in names:
            if n.endswith((".png", ".jpg", ".webp")):
                text[n] = len(z.read(n))
            else:
                text[n] = z.read(n).decode("utf-8")
        return {"names": names, "files": text}


def main() -> None:
    doc = {
        "welcome": [{"args": list(a), "out": welcome_dm(*a)} for a in WELCOME],
        "labels": [{"user": u, "out": member_label(u)} for u in LABELS],
        "export": {
            "now": NOW,
            "who": "小明",
            "rows": [
                {"id": r[0], "ts": r[1], "type": r[2], "prompt": r[3], "negative": r[4], "extra": r[5], "image": r[6].decode()}
                for r in ROWS
            ],
            "zip": archive(exporter.build_zip(ROWS, "小明")),
            "epub": archive(exporter.build_epub(ROWS, "小明")),
        },
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(doc, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
