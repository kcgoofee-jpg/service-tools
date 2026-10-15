//! 后台「日志与记录」接口（旧版 `app/admin.py` 的用量日志 / 操作日志 / Bug / 生成记录 / 反馈 / 问卷 / 捐赠 / 全站重置部分）。
//! 导出（zip / CSV）的格式沿用旧版 `exporter.py` / `feedback.py`。

use super::req::{Ctx, R, Reject, bytes_response, internal, py_quote, sql_int};
use super::{ActionLogged, actor};
use crate::action_log::log_action;
use crate::error::{HttpError, ok_json};
use crate::feedback::{listing, questions_json, safe_cell};
use crate::state::AppState;
use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use gate_core::{features, pyc, pyext, pyval};
use gate_db::repo::admin_records::ActionFilter;
use gate_db::repo::usage::LogFilter;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/logs", get(logs))
        .route("/admin/api/actions", get(admin_actions))
        .route("/admin/api/errors", get(errors_list))
        .route("/admin/api/errors/resolve-all", post(errors_resolve_all))
        .route("/admin/api/errors/{sig}/resolve", post(errors_resolve))
        .route("/admin/api/audit", get(audit_list))
        .route("/admin/api/audit/export", get(audit_export))
        .route("/admin/api/audit/purge-images", post(audit_purge_images))
        .route("/admin/api/audit/{audit_id}/thumb", get(audit_thumb))
        .route("/admin/api/audit/{audit_id}/image", get(audit_image))
        .route("/admin/api/feedback", get(feedback_list))
        .route("/admin/api/feedback/export", get(feedback_export))
        .route("/admin/api/survey", get(survey_results))
        .route("/admin/api/donations", get(donations_list))
        .route("/admin/api/donations/count", get(donations_count))
        .route("/admin/api/donations/{did}/handle", post(donation_handle))
        .route("/admin/api/ops/reset-all-keys", post(ops_reset_all_keys))
}

fn http(status: u16, detail: impl Into<String>) -> Reject {
    HttpError::new(status, detail).into()
}

/// 页数：至少 1 页。
fn page_count(total: i64, per_page: i64) -> i64 {
    ((total + per_page - 1) / per_page).max(1)
}

/// `max(1, min(int(page), 1_000_000))`
fn clamp_page(page: i128) -> i64 {
    page.clamp(1, 1_000_000) as i64
}

fn rows_json(rows: Vec<Value>) -> Value {
    Value::Array(rows)
}

// ----------------------------------------------------------------------------- 用量日志

async fn logs(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let key_id = cx.query_opt_int("key_id");
    let page = cx.query_int("page", 1);
    let feature = cx.query("feature").map(str::to_string);
    let hide_test = cx.query_bool("hide_test", false);
    let rid = cx.query("rid").unwrap_or("").to_string();
    let per_page = cx.query_int("per_page", 20);
    cx.admin().await?;
    let per_page: i64 = if matches!(per_page, 20 | 50 | 100) {
        per_page as i64
    } else {
        20
    };
    let page = clamp_page(page);
    let st = cx.state.clone();
    let feature = feature.filter(|f| !f.is_empty());
    if feature
        .as_deref()
        .is_some_and(|f| !features::FEATURES.iter().any(|(n, _)| *n == f))
    {
        return Err(http(422, "未知功能"));
    }
    let kinds = feature.as_deref().map(|f| {
        features::kinds_for(f)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let rid: String = pyc::strip(&rid)
        .to_lowercase()
        .chars()
        .filter(|c| "0123456789abcdef".contains(*c))
        .take(16)
        .collect();
    let filter = LogFilter {
        key_id: key_id.map(sql_int).transpose()?,
        kinds,
        hide_test,
        rid,
    };
    let total = st.db.count_logs(&filter).await?;
    let pages = page_count(total, per_page);
    let page = page.min(pages);
    let rows = st
        .db
        .list_logs(per_page, (page - 1) * per_page, &filter)
        .await?;
    Ok(ok_json(&json!({
        "logs": rows_json(rows),
        "page": page,
        "per_page": per_page,
        "total": total,
        "pages": pages,
    })))
}

// ----------------------------------------------------------------------------- 操作日志

async fn admin_actions(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let page = cx.query_int("page", 1);
    let actor_q = cx.query("actor").unwrap_or("").to_string();
    let action_q = cx.query("action").unwrap_or("").to_string();
    cx.admin().await?;
    let st = cx.state.clone();
    let per_page = 30_i64;
    let filter = ActionFilter {
        actor_kind: pyc::strip(&actor_q).to_string(),
        // 后台按分组筛，一组可含多个旧名
        actions: action_q
            .split('|')
            .map(|a| pyc::strip(a).to_string())
            .filter(|a| !a.is_empty())
            .take(40)
            .collect(),
    };
    let total = st.db.count_actions_filtered(&filter).await?;
    let pages = page_count(total, per_page);
    let page = clamp_page(page).min(pages);
    let rows = st
        .db
        .list_actions_filtered(&filter, per_page, (page - 1) * per_page)
        .await?;
    let kinds = st.db.action_kinds().await?;
    Ok(ok_json(&json!({
        "actions": rows_json(rows),
        "page": page,
        "per_page": per_page,
        "total": total,
        "pages": pages,
        "action_kinds": kinds,
    })))
}

// ----------------------------------------------------------------------------- Bug 追踪

/// Bug 追踪：按特征归并的错误（未处理的在前）。detail 含堆栈，只在后台显示。
async fn errors_list(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let all = cx.query_bool("all", false);
    cx.admin().await?;
    let st = cx.state.clone();
    let mut items = st.db.list_error_events(all, 100).await?;
    let mut ids: Vec<i64> = items
        .iter()
        .filter_map(|r| r.get("last_key").and_then(Value::as_i64))
        .filter(|k| *k != 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let names: HashMap<i64, String> = st.db.key_names_for(&ids).await?.into_iter().collect();
    for r in &mut items {
        let key_name = match r
            .get("last_key")
            .and_then(Value::as_i64)
            .filter(|k| *k != 0)
        {
            // 旧版 `names.get(key_id) or f"Key #{key_id} (已删除)"`：名字是空串也当已删除
            Some(k) => names
                .get(&k)
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Key #{k} (已删除)")),
            None => String::new(),
        };
        if let Some(o) = r.as_object_mut() {
            o.insert("key_name".into(), Value::String(key_name));
        }
    }
    let open = st.db.open_error_count().await?;
    Ok(ok_json(&json!({"errors": items, "open": open})))
}

/// 标记已处理；之后再出现会作为「复发」重新提醒。
async fn errors_resolve(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let sig = pyc::take_chars(&cx.path("sig"), 12).to_string();
    if !cx
        .state
        .db
        .resolve_error_event(&sig, cx.state.now())
        .await?
    {
        return Err(http(404, "没有这条错误记录"));
    }
    Ok(ok_json(&json!({"ok": true})))
}

/// 一键标记所有未处理 Bug 为已处理。
async fn errors_resolve_all(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let count = cx.state.db.resolve_all_error_events(cx.state.now()).await?;
    Ok(ok_json(&json!({"ok": true, "count": count})))
}

// ----------------------------------------------------------------------------- 生成记录

async fn audit_list(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let key_id = cx.query("key_id").map(str::to_string);
    let page = cx.query_int("page", 1);
    let per_page = cx.query_int("per_page", 24);
    cx.admin().await?;
    let st = cx.state.clone();
    let per_page: i64 = if matches!(per_page, 24 | 48 | 96) {
        per_page as i64
    } else {
        24
    };
    let mut page = clamp_page(page);
    // 查询串里带逗号 = 同一成员的多把 Key 一起查；否则原文当作单个 Key 编号（旧版直接绑定文本）
    let key_ids: Option<Vec<i64>> = match key_id.as_deref().filter(|k| k.contains(',')) {
        Some(k) => {
            let mut v = Vec::new();
            for x in k
                .split(',')
                .filter(|x| !x.is_empty() && x.chars().all(pyext::is_digit))
            {
                let n = pyext::py_int_str(x).map_err(|e| internal(e.to_string()))?;
                v.push(sql_int(n)?);
            }
            Some(v)
        }
        None => None,
    };
    let fetch = |page: i64| {
        let st = st.clone();
        let key_ids = key_ids.clone();
        let key_id = key_id.clone();
        async move {
            let offset = (page - 1) * per_page;
            match key_ids.filter(|v| !v.is_empty()) {
                Some(ids) => st.db.list_audit_ids(per_page, offset, &ids).await,
                None => {
                    st.db
                        .list_audit_text(per_page, offset, key_id.as_deref())
                        .await
                }
            }
        }
    };
    let (mut rows, mut total) = fetch(page).await?;
    let pages = page_count(total, per_page);
    if page > pages {
        // 页码超出（跳页太大、或保留期清掉了记录）：回到最后一页，和用量日志 / 操作日志一致
        page = pages;
        (rows, total) = fetch(page).await?;
    }
    Ok(ok_json(&json!({
        "items": rows_json(rows),
        "page": page,
        "per_page": per_page,
        "total": total,
        "pages": pages,
    })))
}

/// 现场缩图约 70ms/张，翻页回来不再重算；约 400×55KB ≈ 22MB。
/// 旧版是模块级全局缓存，所以这里同样是进程级——按数据库路径区分实例（同进程里并存多个网关时互不串图）。
struct ThumbCache {
    map: HashMap<(PathBuf, i64), Vec<u8>>,
    order: VecDeque<(PathBuf, i64)>,
}

static THUMBS: LazyLock<Mutex<ThumbCache>> = LazyLock::new(|| {
    Mutex::new(ThumbCache {
        map: HashMap::new(),
        order: VecDeque::new(),
    })
});
const THUMB_CACHE_MAX: usize = 400;

impl ThumbCache {
    fn get(&mut self, k: &(PathBuf, i64)) -> Option<Vec<u8>> {
        let v = self.map.get(k)?.clone();
        self.order.retain(|x| x != k);
        self.order.push_back(k.clone());
        Some(v)
    }
    fn put(&mut self, k: (PathBuf, i64), v: Vec<u8>) {
        self.order.retain(|x| *x != k);
        self.order.push_back(k.clone());
        self.map.insert(k, v);
        while self.order.len() > THUMB_CACHE_MAX {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
    fn clear_db(&mut self, db: &std::path::Path) {
        self.map.retain(|(p, _), _| p != db);
        self.order.retain(|(p, _)| p != db);
    }
}

fn jpeg_cache_headers() -> [(&'static str, String); 1] {
    [("cache-control", "private, max-age=3600".to_string())]
}

/// 画廊用的小图：不再另存缩略图，从原图现场缩小（浏览器缓存 1 小时）；旧记录仍有存好的缩略图就直接用。
async fn audit_thumb(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let audit_id = cx.path_int("audit_id");
    cx.admin().await?;
    let audit_id = sql_int(audit_id)?;
    let st = cx.state.clone();
    let key = (st.db.path().to_path_buf(), audit_id);
    let cached = THUMBS.lock().expect("thumbs").get(&key);
    let mut data = match cached {
        Some(d) => Some(d),
        None => st.db.audit_thumb(audit_id).await?,
    };
    if data.is_none() {
        let (image, _) = st.db.audit_image(audit_id).await?;
        if let Some(image) = image {
            data = tokio::task::spawn_blocking(move || crate::audit::make_thumbnail(&image))
                .await
                .map_err(internal)?;
            if let Some(d) = &data {
                THUMBS.lock().expect("thumbs").put(key, d.clone());
            }
        }
    }
    let Some(data) = data else {
        return Err(http(404, "没有图片（原图已过期或未记录）"));
    };
    Ok(bytes_response(data, "image/jpeg", &jpeg_cache_headers()))
}

/// 原图（清晰版）。没有原图时回退到缩略图。
async fn audit_image(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let audit_id = cx.path_int("audit_id");
    cx.admin().await?;
    let audit_id = sql_int(audit_id)?;
    let st = cx.state.clone();
    let (mut data, mut ctype) = st.db.audit_image(audit_id).await?;
    if data.is_none() {
        data = st.db.audit_thumb(audit_id).await?;
        ctype = "image/jpeg".into();
    }
    let Some(data) = data else {
        return Err(http(404, "没有图片"));
    };
    let ctype = if ctype.is_empty() {
        "image/png".to_string()
    } else {
        ctype
    };
    Ok(bytes_response(data, &ctype, &jpeg_cache_headers()))
}

/// 旧版 `_safe_filename`：只留字母数字 `_.-`，其余字符换成 `_`，去掉两头的 `_.`，最长 40 个字符，空了叫 member。
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '_' || c == '.');
    let cut = pyc::take_chars(trimmed, 40);
    if cut.is_empty() {
        "member".into()
    } else {
        cut.to_string()
    }
}

/// 把某成员的全部原图打包成 zip 下载（附 prompts.txt）。
async fn audit_export(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let key_id = cx.query("key_id").map(str::to_string);
    cx.admin().await?;
    let Some(key_id) = key_id else {
        return Err(http(400, "缺少 key_id"));
    };
    let st = cx.state.clone();
    // 旧版把查询串原文当绑定参数：`key_id=3,5`（多 Key 成员）什么也查不到，回 404——前端按单把 Key 逐个下载
    let rows = st
        .db
        .audit_images_for_text(&key_id, 200 * 1024 * 1024, 0.0)
        .await?;
    if rows.is_empty() {
        return Err(http(404, "该成员没有可导出的原图"));
    }
    let who = st
        .db
        .key_name_text(&key_id)
        .await?
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| key_id.clone());
    let n = rows.len();
    let st2 = st.clone();
    let who2 = who.clone();
    let blob = tokio::task::spawn_blocking(move || {
        crate::exporter::build_zip(&rows, &who2, &st2.clock, st2.now())
    })
    .await
    .map_err(internal)?;
    let fname = format!("owl-{}-{n}.zip", safe_filename(&who));
    let enc = py_quote(&format!("owl-{who}-{n}.zip"));
    Ok(bytes_response(
        blob,
        "application/zip",
        &[
            (
                "content-disposition",
                format!("attachment; filename=\"{fname}\"; filename*=UTF-8''{enc}"),
            ),
            ("cache-control", "no-store".to_string()),
        ],
    ))
}

/// 站长手动清空服务器上的全部原图（只删原图，提示词和参数照留）。必须带 `{"confirm": "清空"}`。
async fn audit_purge_images(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let body = cx.json().await?;
    if body.get("confirm") != Some(&json!("清空")) {
        return Err(http(422, "需要 confirm=清空"));
    }
    let st = cx.state.clone();
    let n = st.db.purge_audit_images_before_id(1_i64 << 62).await?;
    THUMBS.lock().expect("thumbs").clear_db(st.db.path());
    log_action(
        &st.db,
        "站长",
        "清空原图",
        "",
        &format!("{n} 张"),
        true,
        st.now(),
    )
    .await;
    let mut resp = ok_json(&json!({"ok": true, "purged": n}));
    resp.extensions_mut().insert(ActionLogged);
    Ok(resp)
}

// ----------------------------------------------------------------------------- CSV

/// Python `csv.writer`（QUOTE_MINIMAL，行尾 `\r\n`）。
fn csv_line(cells: &[String]) -> String {
    let mut out = String::new();
    for (i, c) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if c.contains([',', '"', '\r', '\n']) {
            out.push('"');
            out.push_str(&c.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(c);
        }
    }
    out.push_str("\r\n");
    out
}

/// CSV 单元格：数字 / 空值按 Python 的 `str()` 写；文本做防注入处理。
fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => safe_cell(s),
        other => pyval::py_str(other),
    }
}

/// 成员 /反馈 提交的问卷，最新在前。
async fn feedback_list(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let items = listing(&cx.state.db, 500).await?;
    Ok(ok_json(
        &json!({"questions": questions_json(), "items": items}),
    ))
}

async fn feedback_export(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let st = cx.state.clone();
    let mut items = listing(&st.db, 100_000).await?;
    items.sort_by(|a, b| {
        pyval::number_like(&a["ts"])
            .partial_cmp(&pyval::number_like(&b["ts"]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let questions = questions_json();
    let qs = questions.as_array().cloned().unwrap_or_default();
    let mut header: Vec<String> = ["time", "discord_id", "username", "key_id", "key_name"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    header.extend(
        qs.iter()
            .map(|q| q["label"].as_str().unwrap_or("").to_string()),
    );
    let mut text = String::from('\u{feff}');
    text.push_str(&csv_line(&header));
    for it in &items {
        let mut row = vec![
            safe_cell(
                &st.clock
                    .format(it["ts"].as_f64().unwrap_or(0.0), "%Y-%m-%d %H:%M:%S"),
            ),
            safe_cell(it["discord_id"].as_str().unwrap_or("")),
            safe_cell(it["username"].as_str().unwrap_or("")),
            match it["key_id"].as_i64() {
                Some(k) if k != 0 => k.to_string(),
                _ => String::new(),
            },
            safe_cell(it["key_name"].as_str().unwrap_or("")),
        ];
        for q in &qs {
            let a = it["answers"]
                .get(q["id"].as_str().unwrap_or(""))
                .cloned()
                .unwrap_or(Value::String(String::new()));
            row.push(cell(&a));
        }
        text.push_str(&csv_line(&row));
    }
    log_action(
        &st.db,
        &actor(&st, &cx.parts),
        "导出成员反馈",
        "全部CSV",
        "",
        true,
        st.now(),
    )
    .await;
    let fn_ = format!(
        "owl-feedback-{}.csv",
        st.clock.format(st.now(), "%Y%m%d-%H%M")
    );
    // 注意 filename* 末尾多出来的那个引号是旧版的笔误，照抄（浏览器照样识别）
    Ok(bytes_response(
        text.into_bytes(),
        "text/csv; charset=utf-8",
        &[
            (
                "content-disposition",
                format!(
                    "attachment; filename=\"{fn_}\"; filename*=UTF-8''{}\"",
                    py_quote(&fn_)
                ),
            ),
            ("cache-control", "no-store".to_string()),
        ],
    ))
}

// ----------------------------------------------------------------------------- 问卷 / 捐赠

/// 成员详细问卷（/survey 网页页）：逐条明细 + 选项汇总；Discord /反馈 的结果在 /feedback。
async fn survey_results(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let rows = cx.state.db.list_survey().await?;
    let tally = |field: &str| -> Value {
        let mut counts: Vec<(String, i64)> = Vec::new();
        for r in &rows {
            let raw = match r.get(field) {
                Some(v) if pyval::truthy(v) => pyext::py_str(v),
                _ => String::new(),
            };
            for part in raw.split('、') {
                let p = pyc::strip(part);
                if p.is_empty() {
                    continue;
                }
                match counts.iter_mut().find(|(k, _)| k == p) {
                    Some(e) => e.1 += 1,
                    None => counts.push((p.to_string(), 1)),
                }
            }
        }
        counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n)); // 稳定排序：次数相同保持首次出现的顺序
        Value::Object(
            counts
                .into_iter()
                .map(|(k, n)| (k, json!(n)))
                .collect::<Map<_, _>>(),
        )
    };
    let summary = json!({
        "understand": tally("understand"),
        "improve": tally("improve"),
        "donate": tally("donate"),
        "client": tally("client"),
    });
    Ok(ok_json(
        &json!({"total": rows.len(), "rows": rows, "summary": summary}),
    ))
}

/// 首页「捐赠 Key」提交的号：未处理在前。Token 只在这里展示，站长验证后人工接入上游。
async fn donations_list(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let d = cx.state.db.list_donations().await?;
    Ok(ok_json(&json!(d)))
}

/// 导航角标轮询：未处理的捐赠数量。
async fn donations_count(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let n = cx.state.db.count_unhandled_donations().await?;
    Ok(ok_json(&json!({"open": n})))
}

/// 标记已处理（已验证接入 / 已联系 / 不可用），可留一句备注。
async fn donation_handle(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    let did = cx.path_int("did");
    cx.admin().await?;
    let did = sql_int(did)?;
    let body = cx.json().await?;
    let st = cx.state.clone();
    let note = match body.get("note") {
        Some(v) if pyval::truthy(v) => pyext::py_str(v),
        _ => String::new(),
    };
    if !st.db.handle_donation(did, &note, st.now()).await? {
        return Err(http(404, "不存在或已处理过"));
    }
    log_action(
        &st.db,
        &actor(&st, &cx.parts),
        "捐赠 Key 已处理",
        &format!("#{did}"),
        "",
        true,
        st.now(),
    )
    .await;
    Ok(ok_json(&json!({"ok": true})))
}

// ----------------------------------------------------------------------------- 全站重置

/// 【全站杀头重置】失效所有非管理员 Key 并解除 Discord 绑定映射，以便重新开始。
///
/// 带「邀请」标签的 Key 不受影响：保留启用状态、Discord 绑定和全部标签
/// （2026-10-13 站长：重置是为了腾名额给新成员，私邀的朋友要能继续用；
/// 名额按 discord_registrations 计数，所以绑定必须一并保留，否则邀请成员会丢名额也丢绑定）。
async fn ops_reset_all_keys(State(state): State<AppState>, req: Request) -> R {
    let mut cx = Ctx::new(state, req).await?;
    cx.admin().await?;
    let body = cx.json().await?;
    if body.get("confirm") != Some(&json!("确认重置")) {
        return Err(http(400, "安全确认未通过：必须传入 confirm='确认重置'"));
    }
    let st = cx.state.clone();
    let (disabled, kept) = st.db.reset_all_keys().await?;
    log_action(
        &st.db,
        "站长",
        "全站重置 Key",
        "全部非管理员",
        &format!(
            "已失效 {disabled} 把 Key 并清空 Discord 绑定；「邀请」Key 保留 {kept} 把不受影响"
        ),
        true,
        st.now(),
    )
    .await;
    let message = format!(
        "全站已成功重置！已失效 {disabled} 把 Key 并解除 Discord 绑定{}",
        if kept > 0 {
            format!("；{kept} 把「邀请」Key 原样保留。")
        } else {
            "。".to_string()
        }
    );
    let mut resp: Response = ok_json(&json!({
        "ok": true, "disabled_count": disabled, "kept_invites": kept, "message": message,
    }));
    resp.extensions_mut().insert(ActionLogged);
    Ok(resp)
}
