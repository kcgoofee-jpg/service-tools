//! params / site_flags / features / bot_config 与旧 Python 的对拍（夹具：tools/parity/golden/flags.py）。

use gate_core::bot_config::{self, BotConfigError};
use gate_core::features::{self, FeaturesError, VibeAnlas};
use gate_core::params::Params;
use gate_core::site_flags::{self, Kind, Spec, SpecDefault};
use gate_core::{pyval, site_flags::ALL};
use gate_golden::{Case, assert_eq_py, assert_eq_py_ordered, load};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const M: &str = env!("CARGO_MANIFEST_DIR");

/// Rust 侧结果：成功的值，或 (Python 异常类型, 消息)。
type Got = Result<Value, (&'static str, String)>;

/// 与 Python 比较。`msg` 为真时连异常消息也比（面向用户的中文原因）。
#[track_caller]
fn check(c: &Case, got: Got, ordered: bool, msg: bool) {
    match (c.err(), got) {
        (None, Ok(v)) => {
            if ordered {
                assert_eq_py_ordered(c, &v, c.out());
            } else {
                assert_eq_py(c, &v, c.out());
            }
        }
        (Some(ty), Err((got_ty, got_msg))) => {
            assert_eq!(got_ty, ty, "第 {} 行：异常类型不同（{got_msg}）", c.line);
            if msg {
                let want = c.raw["err"]["msg"].as_str().unwrap();
                assert_eq!(got_msg, want, "第 {} 行：异常消息不同", c.line);
            }
        }
        (None, Err((t, m))) => panic!("第 {} 行：Python 成功，Rust 报 {t}: {m}\n{}", c.line, c.raw),
        (Some(ty), Ok(v)) => panic!(
            "第 {} 行：Python 抛 {ty}，Rust 却返回 {v}\n{}",
            c.line, c.raw
        ),
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// 夹具里的 Spec：`{"ref": key}` 取登记表，`{"inline": {...}}` 现场构造（覆盖登记表里没有的类型组合）。
fn spec_of(v: &Value) -> Spec {
    if let Some(k) = v.get("ref").and_then(Value::as_str) {
        return **ALL.iter().find(|s| s.key == k).expect("登记表里没有这个键");
    }
    let d = &v["inline"];
    let kind = match d["kind"].as_str().unwrap() {
        "flag" => Kind::Flag,
        "int" => Kind::Int,
        "float" => Kind::Float,
        "str" => Kind::Str,
        "choice" => Kind::Choice,
        other => panic!("未知类型 {other}"),
    };
    let default = match &d["default"] {
        Value::Null => SpecDefault::None,
        Value::Bool(b) => SpecDefault::Bool(*b),
        Value::Number(n) if n.is_f64() => SpecDefault::Float(n.as_f64().unwrap()),
        Value::Number(n) => SpecDefault::Int(n.as_i64().unwrap()),
        Value::String(s) => SpecDefault::Str(leak(s)),
        other => panic!("不支持的默认值 {other}"),
    };
    let choices: Vec<&'static str> = d["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| leak(c.as_str().unwrap()))
        .collect();
    Spec {
        key: leak(d["key"].as_str().unwrap()),
        kind,
        default,
        owner: leak(d["owner"].as_str().unwrap()),
        doc: leak(d["doc"].as_str().unwrap()),
        lo: d["lo"].as_i64(),
        hi: d["hi"].as_i64(),
        choices: Box::leak(choices.into_boxed_slice()),
        env: leak(d["env"].as_str().unwrap()),
    }
}

fn spec_json(s: &Spec) -> Value {
    json!({
        "key": s.key,
        "kind": s.kind.as_str(),
        "default": match s.default {
            SpecDefault::None => Value::Null,
            SpecDefault::Bool(b) => json!(b),
            SpecDefault::Int(i) => json!(i),
            SpecDefault::Float(x) => pyval::float(x),
            SpecDefault::Str(t) => json!(t),
        },
        "owner": s.owner,
        "doc": s.doc,
        "lo": s.lo,
        "hi": s.hi,
        "choices": s.choices,
        "env": s.env,
    })
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------- params

#[test]
fn params_p_golden() {
    let cases = load(M, "flags/params_p");
    assert!(cases.len() > 400);
    for c in cases {
        let p = Params::from_map(obj(&c.raw["data"]));
        let name = c.arg(0).as_str().unwrap();
        let default = c.arg(1);
        assert_eq_py(&c, &p.get(name, default), c.out());
        // 类型化的快捷读法必须与通用读法一致
        match default {
            Value::Bool(d) => assert_eq!(json!(p.bool(name, *d)), *c.out(), "行 {}", c.line),
            Value::Number(n) if !n.is_f64() => {
                assert_eq!(
                    json!(p.int(name, n.as_i64().unwrap())),
                    *c.out(),
                    "行 {}",
                    c.line
                )
            }
            Value::Number(n) => assert_eq_py(
                &c,
                &pyval::float(p.float(name, n.as_f64().unwrap())),
                c.out(),
            ),
            _ => {}
        }
    }
}

#[test]
fn params_snapshot_golden() {
    let mut n_snapshot = 0;
    for c in load(M, "flags/params_snapshot") {
        let p = Params::from_map(obj(&c.raw["data"]));
        match c.raw["fn"].as_str().unwrap() {
            "snapshot" => {
                let defaults: BTreeMap<String, Value> =
                    obj(&c.raw["defaults"]).into_iter().collect();
                assert_eq_py_ordered(&c, &p.snapshot(&defaults), c.out());
                n_snapshot += 1;
            }
            "overridden" => {
                let name = c.arg(0).as_str().unwrap();
                assert_eq_py(&c, &json!(p.overridden(name)), c.out());
            }
            other => panic!("未知 {other}"),
        }
    }
    assert!(n_snapshot > 100);
}

#[test]
fn params_update_golden() {
    for c in load(M, "flags/params_update") {
        let mut p = Params::from_map(obj(&c.raw["data"]));
        p.apply_update(&obj(c.arg(0)));
        // Python 写文件时 sort_keys，顺序不影响读取，这里只比内容
        assert_eq_py(&c, &Value::Object(p.as_map().clone()), c.out());
    }
}

#[test]
fn params_load_golden() {
    for c in load(M, "flags/params_load") {
        if c.raw["fn"] == "_load_missing" {
            assert_eq_py(
                &c,
                &Value::Object(Params::empty().as_map().clone()),
                c.out(),
            );
            continue;
        }
        let text = c.raw["text"].as_str().unwrap();
        let p = Params::from_json_text(text);
        assert_eq_py_ordered(&c, &Value::Object(p.as_map().clone()), c.out());
    }
}

// ---------------------------------------------------------------- site_flags

#[test]
fn site_registry_golden() {
    let mut n = 0;
    for c in load(M, "flags/site_registry") {
        match c.raw["fn"].as_str().unwrap() {
            "spec" => {
                let key = c.out()["key"].as_str().unwrap();
                let s = ALL.iter().find(|s| s.key == key).expect("缺少这个键");
                assert_eq_py(&c, &spec_json(s), c.out());
                n += 1;
            }
            "all_keys" => {
                let keys: Vec<Value> = ALL.iter().map(|s| json!(s.key)).collect();
                assert_eq_py(&c, &Value::Array(keys), c.out());
            }
            "facts_table" => assert_eq_py(&c, &json!(site_flags::facts_table()), c.out()),
            other => panic!("未知 {other}"),
        }
    }
    assert_eq!(n, ALL.len(), "登记表条数不一致");
}

#[test]
fn site_parse_golden() {
    let cases = load(M, "flags/site_parse");
    assert!(cases.len() > 10000);
    for c in cases {
        let spec = spec_of(&c.raw["spec"]);
        let got = site_flags::parse(&spec, c.arg(0), c.arg(1).clone());
        check(&c, Ok(got), false, false);
    }
}

#[test]
fn site_default_of_golden() {
    for c in load(M, "flags/site_default_of") {
        let spec = spec_of(&c.raw["spec"]);
        let env = c.raw.get("env").filter(|e| !e.is_null());
        check(&c, Ok(site_flags::default_of(&spec, env)), false, false);
    }
}

#[test]
fn site_get_golden() {
    for c in load(M, "flags/site_get") {
        let spec = spec_of(&c.raw["spec"]);
        let env = c.raw.get("env").filter(|e| !e.is_null());
        let raw = c.raw["present"]
            .as_bool()
            .unwrap()
            .then(|| c.raw["raw"].as_str().unwrap());
        check(
            &c,
            Ok(site_flags::get_from_raw(&spec, raw, env)),
            false,
            false,
        );
    }
}

#[test]
fn site_encode_golden() {
    for c in load(M, "flags/site_encode") {
        let spec = spec_of(&c.raw["spec"]);
        let got = site_flags::encode(&spec, c.arg(0))
            .map(Value::String)
            .map_err(|e| ("ValueError", e.0));
        check(&c, got, false, true);
    }
}

// ---------------------------------------------------------------- features

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

fn pairs(p: &[(&str, &str)]) -> Value {
    Value::Array(p.iter().map(|(a, b)| json!([a, b])).collect())
}

#[test]
fn features_pure_golden() {
    for c in load(M, "flags/features_pure") {
        let got: Got = match c.raw["fn"].as_str().unwrap() {
            "constants" => {
                assert_eq_py(
                    &c,
                    &json!({
                        "FEATURES": pairs(&features::FEATURES),
                        "GLOBAL_KEY": features::GLOBAL_KEY,
                        "ANLAS_FEATURES": pairs(&features::ANLAS_FEATURES),
                        "KIND_FEATURE": pairs(&features::KIND_FEATURE),
                    }),
                    c.out(),
                );
                continue;
            }
            "kinds_for" => Ok(json!(features::kinds_for(c.arg(0).as_str().unwrap()))),
            "parse_list" => Ok(features::parse_list(c.arg(0)).map_or(Value::Null, |l| json!(l))),
            "normalize" => Ok(json!(features::normalize(str_list(c.arg(0))))),
            "dump" => {
                let names = (!c.arg(0).is_null()).then(|| str_list(c.arg(0)));
                Ok(features::dump(names).map_or(Value::Null, Value::String))
            }
            "key_features" => {
                Ok(features::key_features(c.arg(0)).map_or(Value::Null, |l| json!(l)))
            }
            "global_flags" => {
                let raw = c.arg(0).as_str();
                features::parse_global_flags(raw)
                    .map(|f| {
                        // 与旧版同序：按 FEATURES 的顺序
                        let m: Map<String, Value> = features::FEATURES
                            .iter()
                            .map(|(n, _)| ((*n).to_string(), json!(f[n])))
                            .collect();
                        Value::Object(m)
                    })
                    .map_err(|e| match e {
                        FeaturesError::Attribute(m) => ("AttributeError", m),
                        other => panic!("意外的错误 {other}"),
                    })
            }
            other => panic!("未知 {other}"),
        };
        check(&c, got, true, false);
    }
}

#[test]
fn features_check_golden() {
    let cases = load(M, "flags/features_check");
    let (mut n_deny, mut n_allow, mut n_err, mut n_vibe) = (0, 0, 0, 0);
    for c in cases {
        let kv = c.arg(0);
        let key = c.arg(1);
        let name = c.arg(2).as_str().unwrap();
        let env = (!c.arg(3).is_null()).then(|| c.arg(3));
        let now = c.arg(4).as_f64().unwrap();
        let global = kv.get(features::GLOBAL_KEY).and_then(Value::as_str);

        let needs = features::needs_vibe_site_flags(global, key, name);
        let vibe = needs.then(|| {
            let until = site_flags::get_from_raw(
                &site_flags::GLOBAL_DAILY_ANLAS_UNTIL,
                kv.get("global_daily_anlas_until").and_then(Value::as_str),
                env,
            );
            let cap = site_flags::get_from_raw(
                &site_flags::GLOBAL_DAILY_ANLAS,
                kv.get("global_daily_anlas").and_then(Value::as_str),
                env,
            );
            VibeAnlas {
                until: pyval::number_like(&until).unwrap(),
                daily_cap: cap.as_i64().unwrap(),
            }
        });
        let got = features::check_decision(global, key, name, now, vibe);
        match c.err() {
            None => {
                let want = &c.out()["out"];
                let got =
                    got.unwrap_or_else(|e| panic!("第 {} 行：Rust 报 {e}\n{}", c.line, c.raw));
                assert_eq_py(&c, &got.map_or(Value::Null, Value::String), want);
                // 读库的惰性：旧版读了 vibe 设置 ⇔ 这里要求服务层去读
                let reads = c.out()["vibe_reads"].as_i64().unwrap();
                assert_eq!(
                    needs,
                    reads > 0,
                    "第 {} 行：惰性读取判断不一致\n{}",
                    c.line,
                    c.raw
                );
                if needs {
                    n_vibe += 1;
                }
                if want.is_null() {
                    n_allow += 1;
                } else {
                    n_deny += 1;
                }
            }
            Some(ty) => {
                n_err += 1;
                let got = got.expect_err("Python 抛异常，Rust 却返回了值");
                let got_ty = match got {
                    FeaturesError::Attribute(_) => "AttributeError",
                    FeaturesError::Key(_) => "KeyError",
                    FeaturesError::MissingVibe => "MissingVibe",
                };
                assert_eq!(got_ty, ty, "第 {} 行", c.line);
            }
        }
    }
    // 夹具要真的覆盖到各个分支
    assert!(
        n_deny > 300 && n_allow > 1000 && n_err > 50 && n_vibe > 100,
        "{n_deny} {n_allow} {n_err} {n_vibe}"
    );
}

// ---------------------------------------------------------------- bot_config

#[test]
fn bot_constants_golden() {
    for c in load(M, "flags/bot_constants") {
        let fields: Vec<Value> = bot_config::FIELDS
            .iter()
            .map(|f| {
                json!([
                    f.name,
                    f.default.to_value(),
                    match f.kind {
                        bot_config::FieldKind::Int => "int",
                        bot_config::FieldKind::Str => "str",
                    },
                    f.lo,
                    f.hi
                ])
            })
            .collect();
        assert_eq_py(
            &c,
            &json!({
                "FIELDS": fields,
                "STATUS_KEY": bot_config::STATUS_KEY,
                "EVENTS_KEY": bot_config::EVENTS_KEY,
                "EVENT_KINDS": pairs(&bot_config::EVENT_KINDS),
            }),
            c.out(),
        );
    }
}

fn bot_err(e: BotConfigError) -> (&'static str, String) {
    match e {
        BotConfigError::Invalid(m) => ("ValueError", m),
        BotConfigError::BadJson(m) => ("JSONDecodeError", m),
        BotConfigError::WrongType(m) => ("AttributeError", m),
        BotConfigError::Type(m) => ("TypeError", m),
    }
}

#[test]
fn bot_validate_golden() {
    let mut n_err = 0;
    for c in load(M, "flags/bot_validate") {
        let got = bot_config::validate(&obj(c.arg(0)))
            .map(Value::Object)
            .map_err(bot_err);
        n_err += usize::from(c.err().is_some());
        check(&c, got, true, true);
    }
    assert!(n_err > 500);
}

fn stored_of(v: &Value) -> BTreeMap<String, String> {
    obj(v)
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn bot_load_save_golden() {
    for c in load(M, "flags/bot_load") {
        let got = bot_config::load_from(&stored_of(c.arg(0)));
        check(&c, Ok(Value::Object(got)), true, false);
    }
    for c in load(M, "flags/bot_save") {
        let writes: Vec<Value> = bot_config::to_settings(&obj(c.arg(0)))
            .into_iter()
            .map(|(k, v)| json!([k, v]))
            .collect();
        assert_eq_py(&c, &Value::Array(writes), c.out());
    }
}

#[test]
fn bot_report_golden() {
    let (mut n_written, mut n_skipped) = (0, 0);
    for c in load(M, "flags/bot_report") {
        match c.raw["fn"].as_str().unwrap() {
            "report_status" => {
                let got = bot_config::status_to_store(&obj(c.arg(0)), c.arg(1).as_f64().unwrap());
                assert_eq_py(&c, &json!(got), c.out());
            }
            "report_event" => {
                let pre = c.arg(0).as_str();
                let got = bot_config::append_event(pre, &obj(c.arg(1)), c.arg(2).as_f64().unwrap())
                    .map(|o| o.map_or(Value::Null, Value::String))
                    .map_err(bot_err);
                if let Ok(v) = &got {
                    if v.is_null() {
                        n_skipped += 1;
                    } else {
                        n_written += 1;
                    }
                }
                check(&c, got, false, false);
            }
            other => panic!("未知 {other}"),
        }
    }
    assert!(n_written > 200 && n_skipped > 50, "{n_written} {n_skipped}");
}

#[test]
fn bot_snapshot_golden() {
    let mut n_ok = 0;
    for c in load(M, "flags/bot_snapshot") {
        let kv = stored_of(c.arg(0));
        let now = c.arg(1).as_f64().unwrap();
        let config = bot_config::load_from(&kv);
        let got = bot_config::snapshot(
            config,
            kv.get(bot_config::STATUS_KEY).map(String::as_str),
            kv.get(bot_config::EVENTS_KEY).map(String::as_str),
            now,
        )
        .map_err(bot_err);
        n_ok += usize::from(got.is_ok());
        check(&c, got, true, false);
    }
    assert!(n_ok > 200);
}
