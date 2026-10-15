//! `Database` 全部方法的 golden 对拍：夹具由 `tools/parity/golden/db_methods.py` 在旧 Python 上生成。
//!
//! 每个用例：新建空库 → 执行 `seed_sql`（以及原图目录快照）→ 调用同名 Rust 方法 →
//! 比较返回值（含键顺序）、调用后所有非空表的整表内容、原图目录。浮点逐位相等。

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use gate_core::{clock::Clock, pyc, pyval};
use gate_db::repo::{
    audit::NewAudit,
    counters::CounterDelta,
    discord::BanInfo,
    feedback::Survey,
    keys::NewKey,
    usage::{LogFilter, NewLog, Success},
};
use gate_db::{Db, DbError};
use gate_golden::{Case, as_f64, assert_eq_py, assert_eq_py_ordered, f as fjson, load};
use rusqlite::types::ValueRef;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

// ------------------------------------------------------------------ 参数解码（Python 调用约定：位置 + 关键字）
fn arg<'a>(c: &'a Case, i: usize, name: &str) -> Option<&'a Value> {
    c.kwarg(name).or_else(|| c.args().get(i))
}

fn s(v: Option<&Value>, default: &str) -> String {
    v.map_or_else(|| default.to_string(), pyval::py_str)
}

fn int(v: Option<&Value>, default: i64) -> i64 {
    match v {
        None | Some(Value::Null) => default,
        // Python int(x)：浮点向零截断
        Some(Value::Number(n)) if n.is_f64() => n.as_f64().unwrap().trunc() as i64,
        Some(other) => pyval::int_like(other).unwrap_or_else(|| panic!("不是整数：{other}")),
    }
}

fn num(v: Option<&Value>, default: f64) -> f64 {
    match v {
        None | Some(Value::Null) => default,
        Some(other) => as_f64(other).unwrap_or_else(|| panic!("不是数字：{other}")),
    }
}

fn truthy(v: Option<&Value>, default: bool) -> bool {
    v.map_or(default, pyval::truthy)
}

fn opt_int(v: Option<&Value>) -> Option<i64> {
    match v {
        None | Some(Value::Null) => None,
        other => Some(int(other, 0)),
    }
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

fn unb(v: Option<&Value>) -> Option<Vec<u8>> {
    match v {
        Some(Value::Object(m)) => Some(B64.decode(m["$b"].as_str().unwrap()).unwrap()),
        _ => None,
    }
}

// ------------------------------------------------------------------ 返回值编码（与 _golden.enc 对应）
fn js<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

fn blob(b: Option<Vec<u8>>) -> Value {
    b.map_or(Value::Null, |b| json!({"$b": B64.encode(b)}))
}

/// dict[int, X]：非空时是 {"$map": [[k, v], ...]}，空时是 {}。
fn int_map<V: Serialize>(items: Vec<(i64, V)>) -> Value {
    if items.is_empty() {
        return json!({});
    }
    json!({"$map": items.iter().map(|(k, v)| json!([k, js(v)])).collect::<Vec<_>>()})
}

/// dict[str, X]：按插入顺序的 JSON 对象。
fn str_map<V: Serialize>(items: Vec<(String, V)>) -> Value {
    Value::Object(items.into_iter().map(|(k, v)| (k, js(&v))).collect())
}

fn float(x: f64) -> Value {
    fjson(x)
}

/// `app/reasons.py::reason_label`（member_milestones 的回调；测试里自己移植一份，不依赖其他 crate 的进度）。
fn reason_label(code: &str, detail: &str) -> String {
    const LABELS: [(&str, &str); 18] = [
        ("key_paused", "Key 被暂停"),
        ("queue_full", "排队满"),
        ("key_busy", "上一张没出完就重发"),
        ("hourly_cap", "本小时全站上限"),
        ("burst_cap", "突发窗口限速"),
        ("quiet_cap", "安静时段上限"),
        ("cap_3h", "3 小时全站上限"),
        ("daily_cap", "全站每日上限"),
        ("paced", "全天平滑放行"),
        ("over_base", "超出保底暂缓"),
        ("breaker", "全站熔断"),
        ("cooldown", "上游限流暂停"),
        ("v5_exhausted", "全站 V5 用完"),
        ("queue_timeout", "排队超时"),
        ("member_quota", "个人额度用完"),
        ("site_quota", "全站额度用完"),
        ("key_rate", "Key 请求过快"),
        ("body_abort", "请求体中断"),
    ];
    if !code.is_empty()
        && let Some((_, l)) = LABELS.iter().find(|(c, _)| *c == code)
    {
        return (*l).to_string();
    }
    // re.sub(r"^\d{3}\s*", "", detail).strip()
    let chars: Vec<char> = detail.chars().collect();
    let mut text: String = if chars.len() >= 3 && chars[..3].iter().all(char::is_ascii_digit) {
        chars[3..]
            .iter()
            .collect::<String>()
            .trim_start_matches(char::is_whitespace)
            .to_string()
    } else {
        detail.to_string()
    };
    text = pyc::strip(&text).to_string();
    // re.split(r"[（(，,。：:；;]", text, maxsplit=1)[0].strip()
    let cut = text
        .find(|c| "（(，,。：:；;".contains(c))
        .unwrap_or(text.len());
    let text = pyc::strip(&text[..cut]).to_string();
    if text.chars().count() > 18 {
        format!("{}…", text.chars().take(18).collect::<String>())
    } else if text.is_empty() {
        "其他".into()
    } else {
        text
    }
}

// ------------------------------------------------------------------ 库快照
fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(x) => fjson(x),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!({"$b": B64.encode(b)}),
    }
}

async fn dump_tables(db: &Db) -> Value {
    db.read(|c| {
        let names: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Map::new();
        for name in names {
            let mut stmt = c.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))?;
            let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let rows: Vec<Value> = stmt
                .query_map([], |r| {
                    let mut m = Map::new();
                    for (i, col) in cols.iter().enumerate() {
                        m.insert(col.clone(), sql_to_json(r.get_ref(i)?));
                    }
                    Ok(Value::Object(m))
                })?
                .collect::<rusqlite::Result<_>>()?;
            if !rows.is_empty() {
                out.insert(name, Value::Array(rows));
            }
        }
        Ok(Value::Object(out))
    })
    .await
    .unwrap()
}

/// 原图目录快照：({相对路径: base64}, [子目录相对路径])。`extra` 是目录之外的文件（相对路径含 ".."）。
fn snap_files(root: &Path, extra: &[String]) -> (Value, Value) {
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    if root.is_file() {
        files.insert(String::new(), B64.encode(std::fs::read(root).unwrap()));
    } else if root.is_dir() {
        fn walk(
            root: &Path,
            cur: &Path,
            files: &mut BTreeMap<String, String>,
            dirs: &mut Vec<String>,
        ) {
            for e in std::fs::read_dir(cur).unwrap().flatten() {
                let p = e.path();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                if p.is_dir() {
                    dirs.push(rel);
                    walk(root, &p, files, dirs);
                } else if p.is_file() {
                    files.insert(rel, B64.encode(std::fs::read(&p).unwrap()));
                }
            }
        }
        walk(root, root, &mut files, &mut dirs);
    }
    for rel in extra {
        let p = root.join(rel);
        if p.is_file() {
            files.insert(rel.clone(), B64.encode(std::fs::read(p).unwrap()));
        }
    }
    dirs.sort();
    (
        Value::Object(
            files
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        ),
        json!(dirs),
    )
}

fn set_mtime(p: &Path, secs: f64) {
    let t = UNIX_EPOCH + Duration::from_secs_f64(secs);
    std::fs::File::open(p).unwrap().set_modified(t).unwrap();
}

/// 3.0 新增、冻结的 2.x 夹具里没有的列（docs/v3-deviations.md #47）：比较前从 Rust 这一侧去掉。
const RUST_ONLY_COLUMNS: [&str; 1] = ["disabled_reason"];

/// 3.0 删除了客户端识别（docs/v3-deviations.md #50）：`usage_log.client` 列还在（2.x 能读），但不再写入、也不再从
/// `list_logs` 读出。冻结夹具里旧版写进这一列的客户端名，两边都不比：整表内容里把这一列按空串比较。
fn blank_usage_client(tables: &mut Value) {
    let rows = tables.get_mut("usage_log").and_then(Value::as_array_mut);
    for row in rows.into_iter().flatten() {
        if let Some(o) = row.as_object_mut()
            && o.contains_key("client")
        {
            o.insert("client".into(), json!(""));
        }
    }
}

/// 同上：`list_logs` 的旧版返回值每行带 `client`，新版没有这个键，比较前从旧版一侧去掉。
fn drop_expected_client(out: &Value) -> Value {
    let mut out = out.clone();
    for row in out.as_array_mut().into_iter().flatten() {
        if let Some(o) = row.as_object_mut() {
            o.shift_remove("client");
        }
    }
    out
}

fn strip_rust_only(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for c in RUST_ONLY_COLUMNS {
                m.remove(c);
            }
            m.values_mut().for_each(strip_rust_only);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_rust_only),
        _ => {}
    }
}

/// 夹具里 `api_keys` 的种子是 sqlite `.dump` 出来的不带列名整行插入；3.0 多了一列，补一个 NULL。
fn widen_seed(stmt: &str) -> String {
    match stmt.strip_suffix(");") {
        Some(head) if stmt.starts_with("INSERT INTO \"api_keys\" VALUES(") => {
            format!("{head},NULL);")
        }
        _ => stmt.to_string(),
    }
}

async fn seed(db: &Db, case: &Case) -> Vec<String> {
    for ddl in case.get("extra_ddl").unwrap().as_array().unwrap() {
        let ddl = ddl.as_str().unwrap().to_string();
        db.write(move |tx| tx.execute_batch(&ddl)).await.unwrap();
    }
    for stmt in case.get("seed_sql").unwrap().as_array().unwrap() {
        let stmt = widen_seed(stmt.as_str().unwrap());
        let sql = stmt.clone();
        db.write(move |tx| tx.execute_batch(&sql))
            .await
            .unwrap_or_else(|e| panic!("种子 SQL 执行失败：{e}\n{stmt:?}"));
    }
    let root = db.image_dir().to_path_buf();
    let mut extra = Vec::new();
    let seed_files = case.get("seed_files").unwrap().as_array().unwrap();
    let seed_dirs = case.get("seed_dirs").unwrap().as_array().unwrap();
    for d in seed_dirs {
        std::fs::create_dir_all(root.join(d["path"].as_str().unwrap())).unwrap();
    }
    for f in seed_files {
        let rel = f["path"].as_str().unwrap();
        let p = if rel.is_empty() {
            root.clone()
        } else {
            root.join(rel)
        };
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, B64.decode(f["b64"].as_str().unwrap()).unwrap()).unwrap();
        if rel.starts_with("..") {
            extra.push(rel.to_string());
        }
    }
    for f in seed_files {
        let rel = f["path"].as_str().unwrap();
        let p = if rel.is_empty() {
            root.clone()
        } else {
            root.join(rel)
        };
        set_mtime(&p, f["mtime"].as_f64().unwrap());
    }
    // 目录 mtime 必须在所有文件都建好之后再设置
    for d in seed_dirs {
        set_mtime(
            &root.join(d["path"].as_str().unwrap()),
            d["mtime"].as_f64().unwrap(),
        );
    }
    extra
}

// ------------------------------------------------------------------ 逐方法调用
type Out = Result<Value, DbError>;

async fn call(db: &Db, c: &Case) -> Out {
    let now = c.get("now").unwrap().as_f64().unwrap();
    let clock = Clock::new("Asia/Shanghai", 7);
    let a = |i: usize, name: &str| arg(c, i, name);
    let m = c.get("method").unwrap().as_str().unwrap();
    Ok(match m {
        // ---------------- reconciliation
        "reconciliation_totals" => db.reconciliation_totals().await?,
        "save_reconciliation" => {
            db.save_reconciliation(c.arg(0)).await?;
            Value::Null
        }
        "reconciliation_history" => json!(db.reconciliation_history(int(a(0, "limit"), 20)).await?),
        // ---------------- upstream
        "get_upstream_counter" => js(&db
            .get_upstream_counter(&s(a(0, "token_id"), ""), &s(a(1, "day"), ""))
            .await?),
        "migrate_upstream_token_ids" => {
            db.migrate_upstream_token_ids(&str_list(c.arg(0))).await?;
            Value::Null
        }
        "move_upstream_token" => {
            db.move_upstream_token(&s(a(0, "old_id"), ""), &s(a(1, "new_id"), ""))
                .await?;
            Value::Null
        }
        "get_upstream_token_limits" => str_map(db.get_upstream_token_limits().await?),
        "get_upstream_token_enabled" => str_map(db.get_upstream_token_enabled().await?),
        "get_upstream_token_image_concurrency" => {
            str_map(db.get_upstream_token_image_concurrency().await?)
        }
        "set_upstream_token_limit" => {
            db.set_upstream_token_limit(&s(a(0, "token_id"), ""), int(a(1, "limit"), 0))
                .await?;
            Value::Null
        }
        "set_upstream_token_enabled" => {
            db.set_upstream_token_enabled(&s(a(0, "token_id"), ""), truthy(a(1, "enabled"), false))
                .await?;
            Value::Null
        }
        "set_upstream_token_image_concurrency" => {
            db.set_upstream_token_image_concurrency(
                &s(a(0, "token_id"), ""),
                int(a(1, "limit"), 0),
            )
            .await?;
            Value::Null
        }
        "get_upstream_v5_counter" => json!(
            db.get_upstream_v5_counter(&s(a(0, "token_id"), ""), &s(a(1, "day"), ""))
                .await?
        ),
        "bump_upstream_image_counter" => {
            db.bump_upstream_image_counter(
                &s(a(0, "token_id"), ""),
                &s(a(1, "day"), ""),
                int(a(2, "images"), 0),
                num(a(3, "weight"), 1.0),
            )
            .await?;
            Value::Null
        }
        "bump_upstream_v5_counter" => {
            db.bump_upstream_v5_counter(&s(a(0, "token_id"), ""), &s(a(1, "day"), ""))
                .await?;
            Value::Null
        }
        // ---------------- settings
        "get_setting" => match db.get_setting(&s(a(0, "key"), "")).await? {
            Some(v) => json!(v),
            // 旧版返回调用方给的默认值（任意类型）
            None => a(1, "default").cloned().unwrap_or(Value::Null),
        },
        "set_setting" => {
            db.set_setting(&s(a(0, "key"), ""), &s(a(1, "value"), ""))
                .await?;
            Value::Null
        }
        "set_settings_bulk" => {
            let pairs: Vec<(String, String)> = c
                .arg(0)
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), pyval::py_str(v)))
                .collect();
            db.set_settings_bulk(&pairs).await?;
            Value::Null
        }
        // ---------------- coupons
        "add_coupon" => js(&db
            .add_coupon(
                int(a(0, "key_id"), 0),
                &s(a(1, "kind"), ""),
                num(a(2, "days"), 0.0),
                &s(a(3, "note"), ""),
                &s(a(4, "by"), ""),
                now,
            )
            .await?),
        "list_coupons" => json!(
            db.list_coupons(
                int(a(0, "key_id"), 0),
                truthy(a(1, "active_only"), true),
                now
            )
            .await?
        ),
        "use_coupon" => {
            json!(
                db.use_coupon(int(a(0, "coupon_id"), 0), int(a(1, "key_id"), 0), now)
                    .await?
            )
        }
        // ---------------- admin_actions
        "add_admin_action" => {
            db.add_admin_action(
                &s(a(0, "actor"), ""),
                &s(a(1, "action"), ""),
                &s(a(2, "target"), ""),
                &s(a(3, "detail"), ""),
                truthy(a(4, "ok"), false),
                now,
            )
            .await?;
            Value::Null
        }
        "list_admin_actions" => json!(
            db.list_admin_actions(int(a(0, "limit"), 30), int(a(1, "offset"), 0))
                .await?
        ),
        "count_admin_actions" => json!(db.count_admin_actions().await?),
        "purge_admin_actions" => json!(db.purge_admin_actions(num(a(0, "older_than"), 0.0)).await?),
        // ---------------- discord
        "forget_registration_for_key" => {
            json!(
                db.forget_registration_for_key(int(a(0, "key_id"), 0))
                    .await?
            )
        }
        "ban_discord" => {
            let info = BanInfo {
                reason: s(c.kwarg("reason"), ""),
                username: s(c.kwarg("username"), ""),
                display_name: s(c.kwarg("display_name"), ""),
                avatar: s(c.kwarg("avatar"), ""),
                banned_by: s(c.kwarg("banned_by"), "系统"),
            };
            db.ban_discord(&s(a(0, "discord_id"), ""), &info, now)
                .await?;
            Value::Null
        }
        "unban_discord" => json!(db.unban_discord(&s(a(0, "discord_id"), "")).await?),
        "list_bans" => {
            let q = match a(0, "q") {
                None | Some(Value::Null) => String::new(),
                Some(v) => pyval::py_str(v),
            };
            json!(db.list_bans(&q).await?)
        }
        // ---------------- feedback
        "save_survey" => {
            let sv = Survey {
                client: s(c.kwarg("client"), ""),
                understand: s(c.kwarg("understand"), ""),
                improve: s(c.kwarg("improve"), ""),
                improve_other: s(c.kwarg("improve_other"), ""),
                donate: s(c.kwarg("donate"), ""),
            };
            db.save_survey(int(a(0, "key_id"), 0), &sv, now).await?;
            Value::Null
        }
        "list_survey" => json!(db.list_survey().await?),
        "save_donation" => json!(
            db.save_donation(
                &s(a(0, "token"), ""),
                &s(a(1, "message"), ""),
                &s(a(2, "contact"), ""),
                now
            )
            .await?
        ),
        "list_donations" => js(&db.list_donations().await?),
        "count_unhandled_donations" => json!(db.count_unhandled_donations().await?),
        "handle_donation" => {
            json!(
                db.handle_donation(int(a(0, "did"), 0), &s(a(1, "note"), ""), now)
                    .await?
            )
        }
        // ---------------- misc
        "execute_fetchall_compat" => {
            let args = a(1, "args")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            json!(
                db.execute_fetchall_compat(&s(a(0, "sql"), ""), &args)
                    .await?
            )
        }
        // ---------------- keys
        "create_key" => {
            let nk: NewKey = serde_json::from_value(c.arg(0).clone())
                .map_err(|e| DbError::Invalid(format!("入参缺字段：{e}")))?;
            db.create_key(&nk, now).await?
        }
        "get_key" => json!(db.get_key(int(a(0, "key_id"), 0)).await?),
        "get_key_by_token" => json!(db.get_key_by_token(&s(a(0, "token"), "")).await?),
        "rotate_key_token" => {
            json!(
                db.rotate_key_token(int(a(0, "key_id"), 0), &s(a(1, "token"), ""))
                    .await?
            )
        }
        "update_key" => {
            db.update_key(int(a(0, "key_id"), 0), c.arg(1).as_object().unwrap())
                .await?;
            Value::Null
        }
        "delete_key" => {
            db.delete_key(int(a(0, "key_id"), 0)).await?;
            Value::Null
        }
        "inactive_key_ids" => json!(db.inactive_key_ids(num(a(0, "cutoff"), 0.0)).await?),
        "list_keys" => json!(db.list_keys().await?),
        "touch_key" => {
            db.touch_key(int(a(0, "key_id"), 0), now).await?;
            Value::Null
        }
        "get_key_role" => {
            let tags = match a(1, "tags") {
                None | Some(Value::Null) => None,
                Some(v) => Some(str_list(v)),
            };
            json!(db.get_key_role(c.arg(0), tags.as_deref()).await?)
        }
        "set_key_role" => {
            db.set_key_role(int(a(0, "key_id"), 0), &s(a(1, "role"), ""), now)
                .await?;
            Value::Null
        }
        // ---------------- counters
        "bump_counters" => {
            let d = CounterDelta {
                images: int(a(2, "images"), 0),
                anlas: num(a(3, "anlas"), 0.0),
                text_tokens: int(a(4, "text_tokens"), 0),
                requests: int(a(5, "requests"), 1),
                v5: int(a(6, "v5"), 0),
                legacy_free_images: int(a(7, "legacy_free_images"), 0),
            };
            db.bump_counters(int(a(0, "key_id"), 0), &s(a(1, "day"), ""), &d)
                .await?;
            Value::Null
        }
        "get_counter" => js(&db
            .get_counter(int(a(0, "key_id"), 0), &s(a(1, "day"), ""))
            .await?),
        "reset_daily_image_quota" => {
            db.reset_daily_image_quota(int(a(0, "key_id"), 0), &s(a(1, "day"), ""))
                .await?;
            Value::Null
        }
        "month_anlas" => float(
            db.month_anlas(int(a(0, "key_id"), 0), &s(a(1, "month"), ""))
                .await?,
        ),
        "day_v5_total" => json!(db.day_v5_total(&s(a(0, "day"), "")).await?),
        "day_anlas_total" => float(db.day_anlas_total(&s(a(0, "day"), "")).await?),
        "month_anlas_all" => float(db.month_anlas_all(&s(a(0, "month"), "")).await?),
        "member_usage" => int_map(db.member_usage(&s(a(0, "since_day"), "")).await?),
        "overview" => js(&db
            .overview(&s(a(0, "today"), ""), &str_list(c.arg(1)), now, &clock)
            .await?),
        // ---------------- usage
        "record_success" => {
            let sc = Success {
                key_id: int(a(0, "key_id"), 0),
                key_name: s(a(1, "key_name"), ""),
                kind: s(a(2, "kind"), ""),
                model: s(a(3, "model"), ""),
                day: s(a(4, "day"), ""),
                images: int(c.kwarg("images"), 0),
                anlas: num(c.kwarg("anlas"), 0.0),
                tokens: int(c.kwarg("tokens"), 0),
                v5: int(c.kwarg("v5"), 0),
                legacy_free_images: int(c.kwarg("legacy_free_images"), 0),
                detail: s(c.kwarg("detail"), ""),
                unconfirmed_anlas: num(c.kwarg("unconfirmed_anlas"), 0.0),
                wait_ms: int(c.kwarg("wait_ms"), 0),
                dur_ms: int(c.kwarg("dur_ms"), 0),
                up_status: int(c.kwarg("up_status"), 0),
                rid: s(c.kwarg("rid"), ""),
                src: s(c.kwarg("src"), ""),
            };
            db.record_success(&sc, now).await?;
            Value::Null
        }
        "add_log" => {
            let l = NewLog {
                key_id: opt_int(a(0, "key_id")),
                key_name: s(a(1, "key_name"), ""),
                kind: s(a(2, "kind"), ""),
                model: s(a(3, "model"), ""),
                status: s(a(4, "status"), ""),
                images: int(a(5, "images"), 0),
                anlas: num(a(6, "anlas"), 0.0),
                tokens: int(a(7, "tokens"), 0),
                detail: s(a(8, "detail"), ""),
                unconfirmed_anlas: num(a(9, "unconfirmed_anlas"), 0.0),
                wait_ms: int(a(10, "wait_ms"), 0),
                dur_ms: int(a(11, "dur_ms"), 0),
                up_status: int(a(13, "up_status"), 0),
                rid: s(a(14, "rid"), ""),
                src: s(a(15, "src"), ""),
                reason: s(a(16, "reason"), ""),
            };
            db.add_log(&l, now).await?;
            Value::Null
        }
        "list_logs" => {
            let fl = LogFilter {
                key_id: opt_int(a(2, "key_id")),
                kinds: a(3, "kinds").filter(|v| !v.is_null()).map(str_list),
                hide_test: truthy(a(4, "hide_test"), false),
                rid: s(a(5, "rid"), ""),
            };
            json!(
                db.list_logs(int(a(0, "limit"), 20), int(a(1, "offset"), 0), &fl)
                    .await?
            )
        }
        "count_logs" => {
            let fl = LogFilter {
                key_id: opt_int(a(0, "key_id")),
                kinds: a(1, "kinds").filter(|v| !v.is_null()).map(str_list),
                hide_test: truthy(a(2, "hide_test"), false),
                rid: s(a(3, "rid"), ""),
            };
            json!(db.count_logs(&fl).await?)
        }
        "purge_usage_log" => json!(db.purge_usage_log(num(a(0, "older_than"), 0.0)).await?),
        "member_milestones" => int_map(
            db.member_milestones(num(a(0, "since"), 0.0), &reason_label)
                .await?,
        ),
        "image_perf_rows" => json!(
            db.image_perf_rows(num(a(0, "since"), 0.0))
                .await?
                .into_iter()
                .map(|(ts, model, status, images, wait, dur, up, detail)| json!([
                    float(ts),
                    model,
                    status,
                    images,
                    wait,
                    dur,
                    up,
                    detail
                ]))
                .collect::<Vec<_>>()
        ),
        "shadow_rows" => json!(
            db.shadow_rows(num(a(0, "since"), 0.0))
                .await?
                .into_iter()
                .map(|(ts, key_id, key_name, status, images, wait, dur)| json!([
                    float(ts),
                    key_id,
                    key_name,
                    status,
                    images,
                    wait,
                    dur
                ]))
                .collect::<Vec<_>>()
        ),
        "image_stability" => js(&db.image_stability(num(a(0, "since"), 0.0)).await?),
        "usage_by_kind" => json!(db.usage_by_kind(num(a(0, "since"), 0.0)).await?),
        "generated_image_totals" => {
            int_map(db.generated_image_totals(opt_int(a(0, "key_id"))).await?)
        }
        "user_image_timing" => js(&db.user_image_timing(int(a(0, "key_id"), 0), now).await?),
        // ---------------- audit
        "add_audit" => {
            let thumb = unb(a(7, "thumb"));
            let image = unb(a(9, "image"));
            let (key_name, kind, model, status) = (
                s(a(1, "key_name"), ""),
                s(a(2, "kind"), ""),
                s(a(3, "model"), ""),
                s(a(4, "status"), ""),
            );
            let (prompt, negative, extra, image_type) = (
                s(a(5, "prompt"), ""),
                s(a(6, "negative"), ""),
                s(a(8, "extra"), ""),
                s(a(10, "image_type"), ""),
            );
            let na = NewAudit {
                key_id: opt_int(a(0, "key_id")),
                key_name: &key_name,
                kind: &kind,
                model: &model,
                status: &status,
                prompt: &prompt,
                negative: &negative,
                thumb: thumb.as_deref(),
                extra: &extra,
                image: image.as_deref(),
                image_type: &image_type,
            };
            let uuid = c
                .get("uuids")
                .and_then(Value::as_array)
                .and_then(|u| u.first())
                .and_then(Value::as_str)
                .unwrap_or("ffffffffffffffffffffffffffffffff")
                .to_string();
            db.add_audit_with_id(&na, now, &clock, &uuid).await?;
            Value::Null
        }
        "list_audit_ids" => {
            let ids: Vec<i64> = c
                .arg(2)
                .as_array()
                .unwrap()
                .iter()
                .map(|v| int(Some(v), 0))
                .collect();
            let (rows, total) = db
                .list_audit_ids(int(a(0, "limit"), 0), int(a(1, "offset"), 0), &ids)
                .await?;
            json!([rows, total])
        }
        "list_audit" => {
            let (rows, total) = db
                .list_audit(
                    int(a(0, "limit"), 0),
                    int(a(1, "offset"), 0),
                    opt_int(a(2, "key_id")),
                )
                .await?;
            json!([rows, total])
        }
        "audit_thumb" => blob(db.audit_thumb(int(a(0, "audit_id"), 0)).await?),
        "audit_image" => {
            let (data, ty) = db.audit_image(int(a(0, "audit_id"), 0)).await?;
            json!([blob(data), ty])
        }
        // 成员自助导出删了（docs/v3-deviations.md #49），整数绑定的 `audit_images_for` 没有了，只剩后台导出的文本绑定版；
        // 夹具里的 key_id 都是整数，换成文本绑定，SQLite 的列亲和性转回整数，结果相同
        "audit_images_for" => json!(
            db.audit_images_for_text(
                &int(a(0, "key_id"), 0).to_string(),
                int(a(1, "max_bytes"), 0),
                num(a(2, "since"), 0.0)
            )
            .await?
            .into_iter()
            .map(|i| json!([
                i.id,
                float(i.ts),
                i.image_type,
                i.prompt,
                i.negative,
                i.extra,
                blob(Some(i.image))
            ]))
            .collect::<Vec<_>>()
        ),
        "audit_image_bytes" => json!(db.audit_image_bytes().await?),
        "purge_audit_images_to_cap" => json!(
            db.purge_audit_images_to_cap(int(a(0, "cap_bytes"), 0), num(a(1, "low_water"), 0.9))
                .await?
        ),
        "purge_audit_images_before_id" => {
            json!(
                db.purge_audit_images_before_id(int(a(0, "before_id"), 0))
                    .await?
            )
        }
        "purge_audit_images" => json!(db.purge_audit_images(num(a(0, "older_than"), 0.0)).await?),
        "purge_audit" => json!(db.purge_audit(num(a(0, "older_than"), 0.0)).await?),
        "sweep_orphan_images" => json!(db.sweep_orphan_images(num(a(0, "older_than"), 0.0)).await?),
        other => panic!("夹具里有未实现的方法：{other}"),
    })
}

// ------------------------------------------------------------------ 驱动
/// 失败信息只带方法名 / 入参 / 备注，不带整份种子。
fn brief(case: &Case) -> Case {
    Case {
        line: case.line,
        raw: json!({
            "method": case.get("method"), "args": case.get("args"), "kwargs": case.get("kwargs"),
            "note": case.get("note"),
        }),
    }
}

/// `record_success` 的这一条：旧版会刷新 `last_image_at`、3.0 不刷新（只认 `image` / `image_stream`，
/// docs/v3-deviations.md #65）。返回那把 Key 的 id。
fn no_longer_refreshes_last_image(case: &Case) -> Option<i64> {
    if case.get("method").and_then(Value::as_str) != Some("record_success") {
        return None;
    }
    let kind = case.arg(2).as_str().unwrap_or("");
    let old =
        kind.starts_with("image") || matches!(kind, "upscale" | "augment-image" | "vibe_encode");
    let new = matches!(kind, "image" | "image_stream");
    (old && !new).then(|| case.arg(0).as_i64().unwrap_or(0))
}

/// 某把 Key 在表快照里的 `last_image_at`。
fn last_image_at(tables: &Value, key_id: i64) -> Option<Value> {
    tables["api_keys"]
        .as_array()?
        .iter()
        .find(|r| r["id"] == key_id)
        .map(|r| r["last_image_at"].clone())
}

async fn run_case(case: &Case) {
    // 「每把 Key 最近一次请求的客户端」整个方法已删除（docs/v3-deviations.md #50）
    if case.get("method").and_then(Value::as_str) == Some("latest_member_clients") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("gate.db"), 0.0, &[]).unwrap();
    db.set_rule_version(case.get("rule_version").unwrap().as_str().unwrap());
    let extra = seed(&db, case).await;
    let brief = brief(case);
    // 3.0 不再刷新的 last_image_at：期望一侧换回调用前的值（#65）
    let kept_last_image = match no_longer_refreshes_last_image(case) {
        Some(kid) => Some((kid, last_image_at(&dump_tables(&db).await, kid))),
        None => None,
    };

    let out = call(&db, case).await;
    match (case.err(), out) {
        (Some(_), Err(_)) => {}
        (Some(ty), Ok(v)) => panic!(
            "golden 第 {} 行：旧版抛 {ty}，Rust 却成功了：{v}\n{brief:?}",
            case.line
        ),
        (None, Err(e)) => panic!("golden 第 {} 行：Rust 报错 {e}\n{brief:?}", case.line),
        (None, Ok(mut v)) => {
            strip_rust_only(&mut v);
            if case.get("method").and_then(Value::as_str) == Some("list_logs") {
                assert_eq_py_ordered(&brief, &v, &drop_expected_client(case.out()))
            } else {
                assert_eq_py_ordered(&brief, &v, case.out())
            }
        }
    }

    let mut tables = dump_tables(&db).await;
    strip_rust_only(&mut tables);
    blank_usage_client(&mut tables);
    let mut want_tables = case.get("tables_after").unwrap().clone();
    blank_usage_client(&mut want_tables);
    if let Some((kid, before)) = kept_last_image
        && let Some(row) = want_tables["api_keys"]
            .as_array_mut()
            .and_then(|rows| rows.iter_mut().find(|r| r["id"] == kid))
    {
        row["last_image_at"] = before.unwrap_or(Value::Null);
    }
    assert_eq_py(&brief, &tables, &want_tables);
    let (files, dirs) = snap_files(db.image_dir(), &extra);
    assert_eq_py(&brief, &files, case.get("files_after").unwrap());
    assert_eq_py(&brief, &dirs, case.get("dirs_after").unwrap());
}

async fn run_area(area: &str) {
    let cases = load(env!("CARGO_MANIFEST_DIR"), area);
    assert!(!cases.is_empty());
    for case in &cases {
        // `audit_image_count` 只给成员自助导出的保留天数用，已随导出一起删除（docs/v3-deviations.md #49）
        if case.get("method").and_then(Value::as_str) == Some("audit_image_count") {
            continue;
        }
        run_case(case).await;
    }
}

macro_rules! area_tests {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            run_area(stringify!($name)).await;
        }
    )*};
}

area_tests!(
    reconciliation,
    upstream,
    settings,
    coupons,
    admin_actions,
    discord,
    feedback,
    misc,
    keys,
    counters,
    usage,
    audit,
);
