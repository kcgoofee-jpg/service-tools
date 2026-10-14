"""内核整轮回放 golden（milestone M6）：旧 Python 的 kernel tick / quota_algo.run / autopilot.run 在**真实 SQLite 库**上
跑一遍，记录每一步的返回值、每一次 site_settings 写入、每张表的行差异、发出的告警；Rust 侧
（crates/gate-server/tests/kernel_parity.rs）用同样的种子数据、同样冻结的时钟回放，必须逐项相同。

每个场景：
  1. 在临时目录里用真实的 `Database.connect()` + `status_page.ensure_schema()` 建一个全新库；
  2. 按场景执行 `seed_sql`（带列名的 INSERT，合成数据，Rust 侧原样执行同一批语句）；
  3. 冻结 `time.time()`，按 `steps` 依次执行（tick_all / quota_run / autopilot_run / snapshot / set_enabled / sql / probe）；
  4. 每一步记录：返回值（或异常类型）、本步的 `site_settings` 写入序列、本步发出的告警；
  5. 最后记录整库的表行差异（相对 seed 之后、第一步之前）。

不逐位比较的地方（Rust 侧只比异常类型）：Python 异常消息 / 堆栈（kernel_last.error、Bug 的 detail）。
risk 模块里「出站请求头」「TLS 指纹」两条在 3.0 换了文案（docs/v3-deviations.md），两边都用固定占位，其余逐项对拍。
所有数据都是合成的（假 Token / 假成员名），仓库是公开的。

用法（仓库根目录，Python 3.12）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/kernel_tick.py
"""
from __future__ import annotations

import asyncio
import json

from _kernel_replay import run_scenario, save
from kernel_tick_scenarios import build_scenarios


def main() -> None:
    scenarios = build_scenarios()
    names = [s.name for s in scenarios]
    assert len(set(names)) == len(names), "场景名重复"
    lines = []
    for sc in scenarios:
        out = asyncio.run(run_scenario(sc))
        lines.append(json.dumps(out, ensure_ascii=False, separators=(",", ":"), allow_nan=False))
    save("kernel_tick", lines)


if __name__ == "__main__":
    main()
