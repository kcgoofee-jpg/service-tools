//! `reasons` 与旧 Python（app/reasons.py）对拍：夹具由旧 Python 生成（脚本见标签 py-final）。

use gate_core::reasons::{self, Reason};
use gate_golden::{assert_eq_py_ordered, load};
use serde_json::{Map, Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn constants_golden() {
    let mut seen = 0;
    for c in load(M, "reasons/constants") {
        let name = c.raw["name"].as_str().unwrap();
        let want = &c.raw["value"];
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
            "OVER_BASE" => json!(reasons::OVER_BASE),
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
    // 20 个原因码 + SITE_LEVEL + LABELS + BACKFILL
    assert_eq!(
        seen, 23,
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
        assert_eq_py_ordered(&c, &json!(reasons::reason_label(code, detail)), c.out());
    }
}
