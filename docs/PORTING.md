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

---

## 网关路由移植（gate-server）

旧版一个路由文件 → `crates/gate-server/src/routes/<组>.rs`（或 `routes/<组>/mod.rs`），在 `routes/mod.rs` 里 `.merge(<组>::router())`。
后台接口放 `routes/admin/<组>.rs`，在 `routes/admin/mod.rs` 的 `router()` 里 merge（这样自动经过操作日志中间件）。

### 能直接用的积木

| 旧版 | Rust | 说明 |
|---|---|---|
| `STATE` | `State(state): State<AppState>`（`crate::state`） | `state.env`（环境变量）、`db`、`clock`、`nai`（上游客户端）、`guard`、`alerter`（私信站长）、`announcer`、`bugs`、`params`、`pauses`、`cooldown`、`health`、`tasks`；`state.now()` `day()` `month()` `limits()` `record_upstream(ok)` `block_image_generation(secs)` `live_note(status)` |
| `raise err(s, msg, code)` / `GateError` | `crate::error::GateError::new` / `with_code` | 成员接口（NovelAI 格式 + 「猫头鹰公益站提醒：」前缀）。 |
| `HTTPException(s, detail)` | `crate::error::HttpError::new(s, detail)` | 后台 / 成员页 / 桥接：`{"detail": ...}`。`detail` 是 `Value`，需要对象时直接构造。 |
| 未捕获异常 | `crate::error::InternalError(msg)` | 500 + 请求编号；中间件自动记 Bug。 |
| 返回 dict | `crate::error::ok_json(&json!({...}))` / `json_response(status, &v)` | 与 Starlette 同样的序列化（紧凑、UTF-8、Python 浮点格式）。**键顺序照抄旧版。** |
| `authenticate(request, passive=)` | `crate::auth::authenticate(&state, &parts, passive)` | 返回 `Key`（`crate::key::Key`：`id() name() int() float() flag() str() get()` 等）。 |
| `require_feature` / `check_rpm` / `check_image_cooldown` / `limit_inflight` | `crate::auth::*` | |
| `request.client.host` | `crate::auth::client_ip(&parts, &state.env.forwarded_allow_ips)` | |
| `site_flags.get(db, SPEC, settings)` | `crate::auth::site_flag(&state, &SPEC)` / `site_flag_f64` | |
| `record(key, kind, model, status, ...)` | `state.record(&ctx, &key, Entry::new(kind, model, status).detail(..).reason(..))` | `crate::accounting::Entry`；`await main.record(...)` 对应 `state.settle_record(...).await`。 |
| `bug(source, exc, ...)` | `state.bug(source, Some((类型名, 消息)), &ctx)` 或 `state.bugs.capture(Report{..})` | |
| `notify_owner(kind, msg, cooldown)` | `state.alerter.notify(kind, msg, cooldown)` | 测试环境不配 Discord，不会真发。 |
| 请求上下文（rid / model / 是否已记账） | `crate::auth::ctx_of(&parts)` → `ReqCtx` | |
| `acquire_concurrency(key)` | `crate::admission::acquire_slots(&state, &key, image)` | |
| `read_json_body` / `read_json` | `crate::body::read_json_body`（HttpError）/ `read_json`（GateError） | 先 `let (parts, body) = req.into_parts();`。 |
| `require_admin(request)` | 处理函数参数里加 `_s: crate::session::AdminSession` | 写请求自动做来源校验。 |
| `member_state(request)` | `crate::session::member_state(&state, &headers)` 或提取器 `MaybeMember` | 返回 `(discord_id, Option<Key>)`。 |
| Cookie 读写 | `crate::session::{cookie, set_cookie_header, delete_cookie_header, sign_member, key_tag}` | |
| `log_action(db, actor, ...)` | `crate::action_log::log_action(&state.db, ...)`；`actor` 用 `routes::admin::actor(&state, &parts)` | 处理函数自己记了日志时，在响应扩展里放 `routes::admin::ActionLogged`，中间件就不再重复记。 |
| `db._db.execute(...)` 裸 SQL | 在 **gate-db** 里加方法（只有 gate-db 写 SQL） | 新文件放 `crates/gate-db/src/repo/<组>.rs`。 |

### 测试

- 集成测试：`crates/gate-server/tests/<组>.rs`，`mod common; use common::*;` → `setup(&[("ENV", "值")])` 起一个完整网关 + 假上游
  （`gate_testkit::MockUpstream`，可 `push(path, Reply::...)` 脚本化回复），`Call::new("GET", "/path").bearer(TOKEN).json(..).send(&e.state)`。
- 状态码、JSON 结构、文案、写进库的行都要断言；能从旧版跑出期望值的就跑旧版（`tools/dev/run-legacy.sh` 起旧网关 + 假上游，或直接 `uv run` 调函数）。
- 不打真实的 NovelAI / Discord：Discord REST 基地址要能用环境变量换成本地假服务。

### 红线（公开仓库）

- 不许出现真实成员数据（Discord ID / 用户名 / IP / Key / 提示词）。旧代码里写死的服务器 ID、角色 ID、站点地址**不要抄**，改成读环境变量。
- `tools/ci/check-fixtures.sh` 必须通过。
- 与旧版有意不同的地方，追加到 `docs/v3-deviations.md` 末尾（一条一段：旧行为、新行为、原因）。
