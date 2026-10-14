# Python → Rust 移植守则（3.0 重写期间有效，完成后删除）

目标：**行为 1:1**。旧 Python 代码（`app/`）在本分支最后才删，移植时它就是唯一的参照物。
评估周期间额度 / 处罚算法不许「顺手优化」——哪怕旧代码看起来有怪癖，也照抄，并在注释里写「与旧版一致」。

## 每个模块的四步

1. **生成 golden 夹具**：在 `tools/parity/golden/<模块>.py` 里用 `_golden.Writer` 调用旧函数，记录输入 / 输出 / 异常。
   ```bash
   uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/<模块>.py
   ```
   - 用固定种子的 `random.Random`，重复运行输出逐字节一致。
   - 覆盖：边界值（0、负数、空串、None、超大值、非法类型、bool 冒充 int）+ 每个函数约 1–3k 条随机输入。
   - 原地修改入参的函数用 `w.call(fn, body, mutates=[0])`，会额外记录调用后的入参（`after`）。
   - 依赖时间的函数把 `time.time` / `datetime` 打桩成固定值；依赖随机的函数把随机源作为参数传进去。
   - **夹具只用合成数据**：不许出现真实 Key、Discord ID、IP、提示词、成员名（仓库是公开的）。
2. **移植**：`crates/gate-core/src/<模块>.rs`，在 `lib.rs` 里 `pub mod`。
3. **对拍测试**：`crates/gate-core/tests/<模块>_golden.rs`，用 `gate_golden::{load, assert_eq_py}` 逐条比较。
4. **门槛**：`cargo test -p gate-core && cargo clippy -p gate-core --all-targets -- -D warnings && cargo fmt --all`，全部通过才算完成。

## 必须用的工具（不要自己再写一份）

| Python | Rust |
|---|---|
| `round(x)` / `round(x, n)` | `pyc::round0` / `pyc::round`（银行家舍入） |
| `sum(floats)` | `pyc::sum`（3.12 的 Neumaier 补偿求和） |
| `str(float)` / `f"{x}"` | `pyc::float_repr` |
| `int(s)` / `float(s)` / `int(float(s))` | `pyc::parse_int` / `parse_float` / `parse_int_via_float` |
| `s.strip()` / `s[:n]` / `s[-n:]` / `len(s)` | `pyc::strip` / `take_chars` / `last_chars` / `len_chars`（按码点） |
| `a // b` / `a % b` | `pyc::floordiv` / `pyc::modulo`（向负无穷） |
| `bool(x)` / `isinstance(x, int)` 等 | `pyval::truthy` / `int_like` / `strict_int` / `number_like` / `strict_number` |
| `d.pop(k)` / `del d[k]` | `pyval::pop`（保序；**禁止**用 `Map::remove`，它是 swap_remove） |
| `state.day()` / `day_start()` / `month()` | `clock::Clock` |
| `time.time()` | 调用方传 `now: f64`，gate-core 里不读时钟 |

格式化字符串里的浮点（`f"{x:.1f}"`）用 Rust `format!("{x:.1}")`（同样是正确舍入）；
`f"{x}"` 对 float 必须用 `pyc::float_repr`，对 int 直接 `to_string()`。

## 约定

- 请求体 / 任意 JSON 用 `serde_json::Value`（工作区已开 `preserve_order`）。键的插入顺序要和 Python dict 一致——它会原样转发给上游。
- Python 抛异常的函数返回 `Result<T, E>`，`E` 至少能区分出 golden 里记录的异常类型。
- gate-core 里没有全局状态、没有 I/O、没有 tokio、没有日志。
- 正则含前瞻 / 后顾时用 `fancy_regex`，否则用 `regex`。Python `re` 的 `\d` `\w` `\s` 默认匹配 Unicode，Rust `regex` 也是。
- 已删除功能的残留（3 小时上限、DRR 重排、防分享证据、`anlas_auto` 自动分配、`v5_pinned`）不移植，但要在 PR 描述里列出跳过了什么。
- 不改 `app/`、`tests/` 下的 Python 文件；只新增 golden 生成脚本和 Rust 代码。
- 注释用中文，说清楚「为什么」；与旧版一致的怪癖要注明。
