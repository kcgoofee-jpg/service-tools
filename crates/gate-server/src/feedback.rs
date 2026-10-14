//! 成员反馈（旧版 `app/feedback.py`）：`/反馈` 命令弹出 Discord 表单（5 道必填单选题），答案存库，后台查看与导出。
//!
//! 不私信、不群发：成员自己发起，回执只有本人可见（680009 申诉期规则）。

use gate_core::pyc::{strip, take_chars};
use gate_core::pyext::py_str;
use gate_core::pyval::truthy;
use gate_db::{Db, DbError};
use serde_json::{Map, Value, json};

/// 一道单选题。
pub struct Question {
    pub id: &'static str,
    pub label: &'static str,
    pub options: &'static [&'static str],
}

/// 全部是必填单选题（站长 10/11：不要填空题）。Discord 弹窗最多 5 题，题目 ≤45 字，每题最多 25 个选项、选项 ≤100 字。
/// 改这里，下次有人打开 /反馈 就生效（机器人每次现取题目）。
pub const QUESTIONS: [Question; 5] = [
    Question {
        id: "score",
        label: "整体用下来感觉怎么样？",
        options: &["非常满意", "满意", "一般", "不太满意", "很不满意"],
    },
    Question {
        id: "pain",
        label: "目前最困扰你的是？",
        options: &[
            "排队太久 / 经常提示人多",
            "每天张数不够用",
            "V5 额度太少",
            "出图失败或报错",
            "不会配置 / 教程看不懂",
            "没什么困扰",
        ],
    },
    Question {
        id: "wish",
        label: "最希望优先改进哪一项？",
        options: &[
            "多给一些 V5",
            "多给一些 V4.5 张数",
            "出图更快、少排队",
            "开放更多功能（放大、导演工具等）",
            "更清楚的教程",
            "保持现状就好",
        ],
    },
    Question {
        id: "client",
        label: "你主要用什么来出图？",
        options: &[
            "网页前端（类似官网）",
            "SillyTavern 等聊天前端",
            "ComfyUI / SD WebUI 类工具",
            "手机 App",
            "其他",
        ],
    },
    Question {
        id: "freq",
        label: "你多久用一次公益站？",
        options: &["每天都用", "每周几次", "偶尔用", "刚来还没怎么用"],
    },
];

/// 同一个人 10 分钟内只收一份，防刷。
pub const COOLDOWN: f64 = 600.0;

/// `QUESTIONS` 的 JSON 形态：`[{"id","label","options":[...]}]`（`/feedback/questions` 的返回）。
pub fn questions_json() -> Value {
    Value::Array(
        QUESTIONS
            .iter()
            .map(|q| json!({"id": q.id, "label": q.label, "options": q.options}))
            .collect(),
    )
}

/// 提交失败：旧版抛 `ValueError`（中文原因，机器人原样转述给成员）。
#[derive(Debug)]
pub enum SubmitError {
    Rejected(String),
    Db(DbError),
}

impl From<DbError> for SubmitError {
    fn from(e: DbError) -> Self {
        SubmitError::Db(e)
    }
}

/// 只收题目里有的选项；不认识的题目和选项丢掉。
pub fn clean(answers: &Map<String, Value>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for q in &QUESTIONS {
        // str(answers.get(id, "") or "").strip()
        let v = match answers.get(q.id) {
            Some(v) if truthy(v) => strip(&py_str(v)).to_string(),
            _ => String::new(),
        };
        if q.options.contains(&v.as_str()) {
            out.push((q.id, v));
        }
    }
    out
}

/// 存一份反馈。返回给本人看的回执文字；不合格返回 [`SubmitError::Rejected`]。
pub async fn submit(
    db: &Db,
    discord_id: &str,
    username: &str,
    key_id: Option<i64>,
    answers: &Map<String, Value>,
    now: f64,
) -> Result<String, SubmitError> {
    let a = clean(answers);
    if a.len() < QUESTIONS.len() {
        return Err(SubmitError::Rejected(
            "有题目没选，请重新打开 /反馈 选完再提交。".into(),
        ));
    }
    if let Some(last) = db.last_feedback_ts(discord_id).await?
        && last != 0.0
        && now - last < COOLDOWN
    {
        return Err(SubmitError::Rejected(
            "刚刚已经收到你的反馈啦，过 10 分钟再提交新的吧。".into(),
        ));
    }
    let stored: Map<String, Value> = a
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v)))
        .collect();
    db.insert_feedback(
        now,
        discord_id,
        take_chars(username, 80),
        key_id,
        &gate_core::jsondump::dumps(&Value::Object(stored)),
    )
    .await?;
    Ok("收到啦，谢谢你的反馈～奶妹会转给站长 ✨".into())
}

/// 后台查看用：`answers` 已解析成对象（旧版 `listing`）。
pub async fn listing(db: &Db, limit: i64) -> Result<Vec<Value>, DbError> {
    let rows = db.list_feedback(limit).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let answers = r
                .get("answers")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .and_then(gate_core::jsondump::loads)
                .unwrap_or_else(|| json!({}));
            json!({
                "id": r["id"], "ts": r["ts"], "discord_id": r["discord_id"], "username": r["username"],
                "key_id": r["key_id"], "answers": answers,
                "key_name": r.get("name").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect())
}

/// 防 CSV 公式注入：以 = + - @ 制表符 回车开头的文本前加 '（`client_export.safe_cell`）。
pub fn safe_cell(v: &str) -> String {
    if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{v}")
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_fit_discord_modal_limits() {
        assert!((1..=5).contains(&QUESTIONS.len()));
        for q in &QUESTIONS {
            assert!(q.label.chars().count() <= 45);
            assert!((2..=25).contains(&q.options.len()));
            assert!(q.options.iter().all(|o| o.chars().count() <= 100));
            let mut seen = q.options.to_vec();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), q.options.len());
        }
    }

    #[test]
    fn clean_drops_unknown() {
        let mut m = Map::new();
        m.insert("score".into(), json!(" 满意 "));
        m.insert("pain".into(), json!("我自己编的"));
        m.insert("bogus".into(), json!("x"));
        let c = clean(&m);
        assert_eq!(c, vec![("score", "满意".to_string())]);
    }

    #[test]
    fn csv_formula_guard() {
        assert_eq!(safe_cell("=1+1"), "'=1+1");
        assert_eq!(safe_cell("正常"), "正常");
    }
}
