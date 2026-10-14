"""Golden 夹具生成的公共工具：在旧 Python 代码上调用函数，把输入 / 输出编码成 JSONL。

用法（仓库根目录，Python 3.12，与线上一致）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/<模块>.py

编码约定见 crates/gate-golden/src/lib.rs 文件头。所有随机输入都用固定种子，重复运行输出逐字节一致。
"""
from __future__ import annotations

import base64
import copy
import dataclasses
import json
import math
import os
import sys
from pathlib import Path
from typing import Any, Callable, Iterable

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))
OUT_DIR = ROOT / "crates" / "gate-core" / "tests" / "golden"

os.environ.setdefault("TZ", "Asia/Shanghai")
try:
    import time as _t
    _t.tzset()
except AttributeError:  # pragma: no cover
    pass


def enc(v: Any) -> Any:
    """Python 值 → 可无损还原的 JSON 值。"""
    if v is None or isinstance(v, (bool, str)):
        return v
    if isinstance(v, int):
        return v
    if isinstance(v, float):
        if math.isnan(v):
            return {"$f": "nan"}
        if math.isinf(v):
            return {"$f": "inf" if v > 0 else "-inf"}
        return v
    if isinstance(v, (bytes, bytearray, memoryview)):
        return {"$b": base64.b64encode(bytes(v)).decode()}
    if isinstance(v, (list, tuple)):
        return [enc(x) for x in v]
    if isinstance(v, (set, frozenset)):
        return [enc(x) for x in sorted(v, key=repr)]
    if isinstance(v, dict):
        if all(isinstance(k, str) for k in v):
            return {k: enc(x) for k, x in v.items()}
        return {"$map": [[enc(k), enc(x)] for k, x in v.items()]}
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
        return {f.name: enc(getattr(v, f.name)) for f in dataclasses.fields(v)}
    if hasattr(v, "__slots__"):
        return {k: enc(getattr(v, k)) for k in v.__slots__ if hasattr(v, k)}
    if hasattr(v, "__dict__"):
        return {k: enc(x) for k, x in vars(v).items() if not k.startswith("_")}
    raise TypeError(f"无法编码 {type(v).__name__}: {v!r}")


def _dump(obj: Any) -> str:
    return json.dumps(obj, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


class Writer:
    """收集一个夹具文件的用例；`with Writer("policy/fit_size") as w: w.call(fn, *args)`。"""

    def __init__(self, name: str):
        self.name = name
        self.lines: list[str] = []

    def __enter__(self) -> "Writer":
        return self

    def __exit__(self, *exc) -> None:
        if exc[0] is None:
            self.save()

    def call(self, fn: Callable, *args, mutates: Iterable[int] = (), note: str | None = None, **kwargs) -> Any:
        """调用 fn，记录入参、返回值或异常；`mutates` 列出会被原地修改的位置参数，额外记录调用后的值。"""
        rec_args = enc(copy.deepcopy(list(args)))
        rec_kwargs = enc(copy.deepcopy(kwargs))
        call_args = copy.deepcopy(list(args))
        call_kwargs = copy.deepcopy(kwargs)
        rec: dict[str, Any] = {"fn": getattr(fn, "__name__", str(fn)), "args": rec_args}
        if rec_kwargs:
            rec["kwargs"] = rec_kwargs
        try:
            out = fn(*call_args, **call_kwargs)
            rec["out"] = enc(out)
            rec["err"] = None
        except Exception as e:  # noqa: BLE001 — 异常本身就是要对拍的行为
            out = None
            rec["out"] = None
            rec["err"] = {"type": type(e).__name__, "msg": str(e)}
        if mutates:
            rec["after"] = {str(i): enc(call_args[i]) for i in mutates}
        if note:
            rec["note"] = note
        self.lines.append(_dump(rec))
        return out

    def record(self, **fields: Any) -> None:
        """直接写一条自定义记录（场景类夹具：轨迹、表数据等）。"""
        self.lines.append(_dump({k: enc(v) for k, v in fields.items()}))

    def save(self) -> None:
        """超过 256 KB 的夹具存成 .jsonl.gz（mtime 固定为 0，重复生成逐字节一致），控制公开仓库体积。"""
        import gzip
        path = OUT_DIR / f"{self.name}.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        data = ("\n".join(self.lines) + "\n").encode("utf-8")
        gz = path.with_name(path.name + ".gz")
        for stale in (path, gz):
            stale.unlink(missing_ok=True)
        if len(data) > 256 * 1024:
            gz.write_bytes(gzip.compress(data, compresslevel=9, mtime=0))
            path = gz
        else:
            path.write_bytes(data)
        print(f"{path.relative_to(ROOT)}: {len(self.lines)} 条")
