use std::collections::HashMap;

use gate_core::errors::{
    self, Capture, ErrorEvent, ExcInfo, ExcKey, build_event, normalize_title, should_print_stack,
};
use gate_golden::{assert_eq_py, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn signature_golden() {
    let (mut with_exc, mut without, mut norm) = (0, 0, 0);
    for c in load(M, "errors/signature") {
        match c.raw["fn"].as_str().unwrap() {
            "normalize_title" => {
                norm += 1;
                assert_eq_py(
                    &c,
                    &json!(normalize_title(c.raw["title"].as_str().unwrap())),
                    &c.raw["out"],
                );
            }
            "signature" => {
                let source = c.raw["source"].as_str().unwrap();
                let title = c.raw["title"].as_str().unwrap();
                let exc = &c.raw["exc"];
                let got = if exc.is_null() {
                    without += 1;
                    errors::signature(source, None, title)
                } else {
                    with_exc += 1;
                    let key = ExcKey {
                        type_name: exc["type"].as_str().unwrap(),
                        app_frame: exc["app_frame"].as_str().unwrap(),
                    };
                    errors::signature(source, Some(key), title)
                };
                assert_eq_py(&c, &json!(got), &c.raw["out"]);
            }
            other => panic!("未知函数 {other}"),
        }
    }
    assert!(
        with_exc > 300 && without > 150 && norm > 300,
        "{with_exc} / {without} / {norm}"
    );
}

fn event_json(e: &ErrorEvent) -> Value {
    json!({
        "sig": e.sig, "source": e.source, "level": e.level, "title": e.title, "detail": e.detail,
        "path": e.path, "key_id": e.key_id, "rid": e.rid, "ts": gate_golden::f(e.ts),
    })
}

#[test]
fn capture_golden() {
    let (mut printed_stack, mut skipped_stack, mut truncated) = (0, 0, 0);
    for c in load(M, "errors/capture") {
        let mut printed: HashMap<String, f64> = HashMap::new();
        for (i, step) in c.raw["steps"].as_array().unwrap().iter().enumerate() {
            let s = |k: &str| step[k].as_str().unwrap();
            let exc = &step["exc"];
            let info = (!exc.is_null()).then(|| ExcInfo {
                type_name: exc["type"].as_str().unwrap(),
                message: exc["message"].as_str().unwrap(),
                app_frame: exc["app_frame"].as_str().unwrap(),
                stack: exc["stack"].as_str().unwrap(),
            });
            let input = Capture {
                source: s("source"),
                exc: info,
                title: s("title"),
                detail: s("detail"),
                path: s("path"),
                key_id: step["key_id"].as_i64(),
                rid: s("rid"),
                level: s("level"),
            };
            let now = step["now"].as_f64().unwrap();
            let got = build_event(&input, now);
            let out = &step["out"];
            let ctx = format!("第 {} 行第 {i} 步", c.line);
            assert_eq_py(&c, &event_json(&got.event), &out["event"]);
            assert_eq!(got.event.sig, out["sig"].as_str().unwrap(), "{ctx}");
            assert_eq!(
                got.log_line,
                out["log_line"].as_str().unwrap(),
                "{ctx}：日志行"
            );
            // 带异常且距上次打印 > 600 秒才打印堆栈
            let print = input.exc.is_some()
                && should_print_stack(now, printed.get(&got.event.sig).copied());
            assert_eq!(
                print,
                out["stack_printed"].as_bool().unwrap(),
                "{ctx}：堆栈打印节流"
            );
            if print {
                printed.insert(got.event.sig.clone(), now);
                assert_eq!(
                    Some(got.stack_text.as_str()),
                    out["stack_text"].as_str(),
                    "{ctx}：堆栈文本"
                );
                printed_stack += 1;
            } else if input.exc.is_some() {
                skipped_stack += 1;
            }
            truncated += usize::from(got.event.detail.chars().count() == errors::DETAIL_MAX);
        }
    }
    assert!(
        printed_stack > 100 && skipped_stack > 20 && truncated > 10,
        "{printed_stack} / {skipped_stack} / {truncated}"
    );
}

#[test]
fn constants_golden() {
    let c = &load(M, "analysis/consts")[0];
    let e = &c.raw["errors"];
    let levels: Vec<&str> = e["LEVELS"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(errors::LEVELS.to_vec(), levels);
    assert_eq!(errors::DETAIL_MAX as u64, e["DETAIL_MAX"].as_u64().unwrap());
}
