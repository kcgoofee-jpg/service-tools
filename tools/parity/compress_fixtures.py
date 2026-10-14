"""把已有的大夹具（> 256 KB 的 .jsonl）压成 .jsonl.gz（mtime=0，确定性）。合并子任务分支后跑一次。"""
import gzip
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2] / "crates"
for p in sorted(root.glob("*/tests/golden/**/*.jsonl")):
    data = p.read_bytes()
    if len(data) > 256 * 1024:
        p.with_name(p.name + ".gz").write_bytes(gzip.compress(data, compresslevel=9, mtime=0))
        p.unlink()
        print(f"{p.relative_to(root.parent)} → .gz", file=sys.stderr)
