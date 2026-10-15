//! `reasons` 与旧 Python（app/reasons.py）对拍：夹具由 tools/parity/golden/reasons.py 生成。
//!
//! 3.0 删掉的原因码 `over_base`（V4.5 保底用完后全站不空闲，docs/v3-deviations.md #61）：夹具冻结不改，
//! 在期望一侧去掉它（常量本身、`SITE_LEVEL` / `LABELS` / `BACKFILL` 里的那一项）；历史记录里的
//! `over_base` 没有短说明了，按 detail 的第一句显示。

use gate_core::reasons::{self, Reason};
use gate_golden::{assert_eq_py_ordered, load};
use serde_json::{Map, Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

/// 3.0 删掉的原因码（#61）。
const REMOVED: &str = "over_base";

/// 期望一侧去掉已删除的原因码：列表里的码、`[码, 模式]` 对、`{码: 说明}` 的键。
fn without_removed(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(
            a.iter()
                .filter(|x| {
                    x.as_str() != Some(REMOVED) && x.get(0).and_then(Value::as_str) != Some(REMOVED)
                })
                .cloned()
                .collect(),
        ),
        Value::Object(m) => {
            let mut m = m.clone();
            m.shift_remove(REMOVED);
            Value::Object(m)
        }
        other => other.clone(),
    }
}

#[test]
fn constants_golden() {
    let mut seen = 0;
    for c in load(M, "reasons/constants") {
        let name = c.raw["name"].as_str().unwrap();
        if name == "OVER_BASE" {
            continue;
        }
        let want = &without_removed(&c.raw["value"]);
        let got: Value = match name {
            "SITE_LEVEL" => json!(reasons::SITE_LEVEL),
            "LABELS" => {
                let mut m = Map::new();
                for (k, v) in reasons::LABELS {
                    m.insert((*k).into(), json!(v));
                }
                Value::Object(m)
            }
            "BACKFILL" => json!(
                reasons::BACKFILL
                    .iter()
                    .map(|(a, b)| [a, b])
                    .collect::<Vec<_>>()
            ),
            "KEY_PAUSED" => json!(reasons::KEY_PAUSED),
            "QUEUE_FULL" => json!(reasons::QUEUE_FULL),
            "KEY_BUSY" => json!(reasons::KEY_BUSY),
            "HOURLY_CAP" => json!(reasons::HOURLY_CAP),
            "QUIET_CAP" => json!(reasons::QUIET_CAP),
            "CAP_3H" => json!(reasons::CAP_3H),
            "BURST_CAP" => json!(reasons::BURST_CAP),
            "DAILY_CAP" => json!(reasons::DAILY_CAP),
            "PACED" => json!(reasons::PACED),
            "BREAKER" => json!(reasons::BREAKER),
            "COOLDOWN" => json!(reasons::COOLDOWN),
            "V5_EXHAUSTED" => json!(reasons::V5_EXHAUSTED),
            "QUEUE_TIMEOUT" => json!(reasons::QUEUE_TIMEOUT),
            "MEMBER_QUOTA" => json!(reasons::MEMBER_QUOTA),
            "SITE_QUOTA" => json!(reasons::SITE_QUOTA),
            "KEY_RATE" => json!(reasons::KEY_RATE),
            "BODY_ABORT" => json!(reasons::BODY_ABORT),
            "FEATURE_OFF" => json!(reasons::FEATURE_OFF),
            "PARAM" => json!(reasons::PARAM),
            other => panic!("夹具里有未覆盖的常量 {other}"),
        };
        assert_eq_py_ordered(&c, &got, want);
        seen += 1;
    }
    // 19 个原因码（夹具里的 20 个去掉 over_base）+ SITE_LEVEL + LABELS + BACKFILL
    assert_eq!(
        seen, 22,
        "夹具条数变了，检查 reasons.py 是否新增 / 删除了原因码"
    );
}

#[test]
fn code_of_golden() {
    for c in load(M, "reasons/code_of") {
        let kind = c.arg(0).as_str().unwrap();
        let text = c.arg(1).as_str().unwrap_or("");
        let reason = match kind {
            "reason" => Reason::new(text, c.arg(2).as_str().unwrap_or("")),
            // 普通 str / None：没有 .code 属性，返回 ""
            _ => Reason::plain(text),
        };
        assert_eq_py_ordered(&c, &json!(reasons::code_of(&reason)), c.out());
    }
}

#[test]
fn reason_label_golden() {
    for c in load(M, "reasons/reason_label") {
        let code = c.arg(0).as_str().unwrap_or("");
        let detail = c.args().get(1).and_then(Value::as_str).unwrap_or("");
        let got = reasons::reason_label(code, detail);
        if code == REMOVED {
            // 已删除的码没有短说明：与没有码时一样取 detail 的第一句
            assert_ne!(Some(got.as_str()), c.out().as_str(), "第 {} 行", c.line);
            assert_eq!(got, reasons::reason_label("", detail), "第 {} 行", c.line);
            continue;
        }
        assert_eq_py_ordered(&c, &json!(got), c.out());
    }
}
