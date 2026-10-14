mod common;

use common::{b64, bytes_of, depth_of, tag};
use gate_core::image_events::{
    EncodeError, ImageEventTracker, WireFormat, b64decode_strict, encode_image_event,
    image_envelope,
};
use gate_core::pymsgpack;
use gate_golden::{assert_eq_py, load};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn encode_image_event_golden() {
    let (mut ok, mut err) = (0, 0);
    for c in load(M, "image_events/encode") {
        match c.get("fn").and_then(Value::as_str).unwrap() {
            "encode_image_event" => {
                let wire =
                    WireFormat::from_name_lenient(c.get("wire").and_then(Value::as_str).unwrap());
                let got = encode_image_event(&c.raw["payload"], wire);
                if c.get("ok").and_then(Value::as_bool).unwrap() {
                    ok += 1;
                    let bytes = got.unwrap_or_else(|e| {
                        panic!(
                            "golden 第 {} 行：Python 成功，Rust 报 {e:?}\n{}",
                            c.line, c.raw
                        )
                    });
                    assert_eq_py(&c, &json!({"$b": b64(&bytes)}), &c.raw["out"]);
                } else {
                    err += 1;
                    assert_eq!(
                        got,
                        Err(EncodeError::NoEventType),
                        "golden 第 {} 行：Python 报 {}",
                        c.line,
                        c.get("err").unwrap()
                    );
                }
            }
            "encode_image_event_big" => {
                let size = c.get("size").and_then(Value::as_u64).unwrap() as usize;
                let payload = match c.get("kind").and_then(Value::as_str).unwrap() {
                    "str" => json!({"event_type": "big", "v": "x".repeat(size)}),
                    _ => json!({"event_type": "big", "v": vec![Value::Null; size]}),
                };
                let wire =
                    WireFormat::from_name_lenient(c.get("wire").and_then(Value::as_str).unwrap());
                let bytes = encode_image_event(&payload, wire).unwrap();
                assert_eq!(
                    bytes.len() as u64,
                    c.get("out_len").and_then(Value::as_u64).unwrap()
                );
                assert_eq!(
                    hex::encode(Sha256::digest(&bytes)),
                    c.get("out_sha256").and_then(Value::as_str).unwrap(),
                    "golden 第 {} 行",
                    c.line
                );
            }
            other => panic!("未知函数 {other}"),
        }
    }
    assert!(ok > 400 && err > 5, "{ok} / {err}");
}

#[test]
fn envelope_golden() {
    let (mut t, mut f) = (0, 0);
    for c in load(M, "image_events/envelope") {
        let got = image_envelope(&bytes_of(c.arg(0)));
        assert_eq_py(&c, &json!(got), c.out());
        if got {
            t += 1;
        } else {
            f += 1;
        }
    }
    assert!(t > 300 && f > 300, "{t} / {f}");
}

#[test]
fn strict_base64_golden() {
    let (mut ok, mut bad) = (0, 0);
    for c in load(M, "image_events/b64") {
        let s = c.arg(0).as_str().unwrap();
        let got = b64decode_strict(s);
        if c.err().is_none() {
            ok += 1;
            let out = bytes_of(c.out());
            assert_eq!(
                got.as_deref(),
                Some(&out[..]),
                "golden 第 {} 行：输入 {s:?}",
                c.line
            );
        } else {
            bad += 1;
            assert_eq!(
                got, None,
                "golden 第 {} 行：Python 拒绝 {s:?}，Rust 解出了东西",
                c.line
            );
        }
    }
    assert!(ok > 300 && bad > 300, "{ok} / {bad}");
}

#[test]
fn unpackb_golden() {
    let (mut ok, mut bad) = (0, 0);
    for c in load(M, "image_events/unpackb") {
        let raw = bytes_of(&c.raw["raw"]);
        let got = pymsgpack::unpackb(&raw);
        if c.get("ok").and_then(Value::as_bool).unwrap() {
            ok += 1;
            let obj = got.unwrap_or_else(|e| {
                panic!(
                    "golden 第 {} 行：Python 解出了，Rust 报 {e:?}\n{}",
                    c.line, c.raw
                )
            });
            if let Some(d) = c.get("depth").and_then(Value::as_u64) {
                assert_eq!(depth_of(&obj) as u64, d, "第 {} 行深度", c.line);
            } else {
                assert_eq_py(&c, &tag(&obj), &c.raw["value"]);
            }
        } else {
            bad += 1;
            assert!(
                got.is_err(),
                "golden 第 {} 行：Python 拒绝（{}），Rust 解出 {:?}\n{}",
                c.line,
                c.get("err").unwrap(),
                got.as_ref().map(tag),
                c.raw
            );
        }
    }
    assert!(ok > 800 && bad > 300, "{ok} / {bad}");
}

#[test]
fn tracker_golden() {
    let (mut with_frames, mut with_err, mut with_failed, mut with_detail, mut done) =
        (0, 0, 0, 0, 0);
    for c in load(M, "image_events/tracker") {
        match c.get("fn").and_then(Value::as_str).unwrap() {
            "tracker_new" => {
                let expected = c.get("expected").and_then(Value::as_i64).unwrap();
                let ok = c.get("ok").and_then(Value::as_bool).unwrap();
                assert_eq!(
                    ImageEventTracker::new(expected, WireFormat::Sse).is_ok(),
                    ok
                );
                continue;
            }
            "tracker" => {}
            other => panic!("未知函数 {other}"),
        }
        let expected = c.get("expected").and_then(Value::as_i64).unwrap();
        let wire = WireFormat::from_name_lenient(c.get("wire").and_then(Value::as_str).unwrap());
        let max_event = c.get("max_event").and_then(Value::as_u64).unwrap();
        let max_stream = c.get("max_stream").and_then(Value::as_u64).unwrap();
        let mut t = ImageEventTracker::with_limits(expected, wire, max_event, max_stream).unwrap();
        let ops = c.get("ops").and_then(Value::as_array).unwrap();
        let steps = c.get("steps").and_then(Value::as_array).unwrap();
        assert_eq!(ops.len(), steps.len());
        for (i, (op, step)) in ops.iter().zip(steps).enumerate() {
            let (frames, err) = match op["op"].as_str().unwrap() {
                "frames" => {
                    let r = t.frames(&bytes_of(&op["chunk"]));
                    (r.frames, r.error.is_some())
                }
                "feed" => (vec![], t.feed(&bytes_of(&op["chunk"])).is_err()),
                "finish" => {
                    t.finish();
                    (vec![], false)
                }
                other => panic!("未知操作 {other}"),
            };
            let ctx = format!("第 {} 行第 {i} 步（{}）", c.line, op["op"]);
            let want_frames: Vec<Vec<u8>> = step
                .get("f")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(bytes_of).collect())
                .unwrap_or_default();
            let want_counts: Vec<u64> = step
                .get("n")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|v| v.as_u64().unwrap()).collect())
                .unwrap_or_default();
            let got_frames: Vec<Vec<u8>> = frames.iter().map(|f| f.bytes.clone()).collect();
            let got_counts: Vec<u64> = frames.iter().map(|f| f.completed_after as u64).collect();
            assert_eq!(got_frames, want_frames, "golden {ctx}：转发的帧不一致");
            assert_eq!(
                got_counts, want_counts,
                "golden {ctx}：每帧后的已完成张数不一致"
            );
            assert_eq!(err, step.get("e").is_some(), "golden {ctx}：是否报错");
            assert_eq!(t.failed(), step.get("x").is_some(), "golden {ctx}：failed");
            assert_eq!(
                t.completed_images() as u64,
                step["c"].as_u64().unwrap(),
                "golden {ctx}：completed"
            );
            assert_eq!(
                t.error_detail(),
                step.get("d").and_then(Value::as_str).unwrap_or(""),
                "golden {ctx}：error_detail"
            );
            let want_first = step.get("i").map(bytes_of);
            assert_eq!(
                t.first_image().map(<[u8]>::to_vec),
                want_first,
                "golden {ctx}：first_image"
            );
            with_frames += usize::from(!frames.is_empty());
            with_err += usize::from(err);
            with_failed += usize::from(t.failed());
            with_detail += usize::from(!t.error_detail().is_empty());
        }
        done += 1;
    }
    assert!(done > 1000);
    assert!(
        with_frames > 1000 && with_err > 300 && with_failed > 500 && with_detail > 100,
        "覆盖不足：帧 {with_frames} 错误 {with_err} 失败 {with_failed} 原话 {with_detail}"
    );
}

#[test]
fn constants_golden() {
    let c = &load(M, "streams/consts")[0];
    let u = |k: &str| c.get(k).and_then(Value::as_u64).unwrap();
    assert_eq!(
        gate_core::image_events::MAX_EVENT_BYTES,
        u("max_event_bytes")
    );
    assert_eq!(
        gate_core::image_events::MAX_STREAM_BYTES,
        u("max_stream_bytes")
    );
    assert_eq!(
        gate_core::image_events::SAFE_ERROR,
        c.get("safe_error").and_then(Value::as_str).unwrap()
    );
    assert_eq!(
        gate_core::image_events::ImageStreamProtocolError.to_string(),
        gate_core::image_events::SAFE_ERROR
    );
    assert_eq!(
        gate_core::json_body::MAX_JSON_DEPTH as u64,
        u("max_json_depth")
    );
    assert_eq!(
        gate_core::json_body::THREAD_PARSE_BYTES as u64,
        u("thread_parse_bytes")
    );
    let media = c.get("media_types").unwrap();
    assert_eq!(WireFormat::Sse.media_type(), media["sse"].as_str().unwrap());
    assert_eq!(
        WireFormat::Msgpack.media_type(),
        media["msgpack"].as_str().unwrap()
    );
}
