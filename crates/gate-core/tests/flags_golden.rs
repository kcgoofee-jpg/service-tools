//! site_flags / features / bot_config 与旧 Python 的对拍（夹具生成脚本见标签 py-final）。
//!
//! 私密参数 `Params`（`flags/params_*.jsonl`）随私密参数文件一起取消（docs/v3-deviations.md #91），这几份夹具不再有对应的
//! 代码，保留在原处不动、不再读。

use gate_core::bot_config::{self, BotConfigError};
use gate_core::features::{self, FeaturesError, VibeAnlas};
use gate_core::site_flags::{self, Kind, Spec, SpecDefault};
use gate_core::{pyval, site_flags::ALL};
use gate_snapshot::{Case, assert_eq_py, assert_eq_py_ordered, load};
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

// ---------------------------------------------------------------- site_flags

/// 3.0 改过的设置说明（docs/v3-deviations.md #48）：冻结夹具里是旧说明，比较前换成新的。
///
/// `global_daily_anlas`：旧说明写「0 = 不限」，实际 0 是不开放 Vibe（准入和功能检查都这样判），3.0 把说明改对，
/// 后台设置页也用这句（docs/v3-deviations.md #92）。
const DOC_FIXES: [(&str, &str); 2] = [
    (
        "网页 Discord OAuth 登录暂停（Discord 回调修复 / 审核期间）。Key 登录照常可用，不受此开关影响",
        "网页 Discord 登录暂停（Discord 回调修复 / 审核期间）。网页只有 Discord 登录，暂停期间成员只能在 Discord 里用 /quota 看额度",
    ),
    (
        "全站每日 Anlas 额度（0 = 不限，Vibe PVP 抢额度）",
        "全站每日 Anlas 额度（0 = 不开放 Vibe；Vibe PVP 抢额度）",
    ),
];

/// 3.0 删掉的设置。冻结夹具里还有它们的用例，登记表里已经没有，逐个用例跳过；`all_keys`
/// 在期望值一侧去掉它们：
/// - `member_image_download`：成员自助导出原图（docs/v3-deviations.md #49）；
/// - `dm_enabled`：成员私信总闸门，机器人不再私信成员（docs/v3-deviations.md #76）；
/// - `guard_base_daily_images`：V4.5 保底（docs/v3-deviations.md #61）；
/// - `web_login_paused`（登录常开）、`home_join_open`（首页按钮跟随领 Key 开关）、`global_daily_anlas_until`
///   （后台从来没有写入口）、`audit_image_retention_days`（原图早已不按天数删）：docs/v3-deviations.md #92。
const REMOVED_KEYS: [&str; 7] = [
    "member_image_download",
    "dm_enabled",
    "guard_base_daily_images",
    "web_login_paused",
    "home_join_open",
    "global_daily_anlas_until",
    "audit_image_retention_days",
];

fn is_removed(key: &str) -> bool {
    REMOVED_KEYS.contains(&key)
}

/// 夹具用例的 Spec 引用的是不是已删除的设置。
fn removed_spec(c: &Case) -> bool {
    c.raw["spec"]["ref"].as_str().is_some_and(is_removed)
}

/// 默认值 3.0 不再取环境变量、固定写在登记表里的设置（键, 默认值, `facts_table` 里旧的「默认」列, 新的「默认」列）：
/// - `global_daily_v5` 固定 150（docs/v3-deviations.md #70）；
/// - `global_daily_anlas` 固定 200、`global_monthly_anlas` 固定 10000（#92，原来取 `GLOBAL_DAILY_ANLAS` / `GLOBAL_MONTHLY_ANLAS`）。
///
/// 登记表与 facts_table 在期望一侧改写；`default_of` / `get` 里结果取决于默认值的用例跳过，另由
/// [`fixed_defaults_ignore_the_environment`] 测。
const FIXED_DEFAULTS: [(&str, &str, &str, &str); 3] = [
    ("global_daily_v5", "150", "int", "150"),
    ("global_daily_anlas", "200", "int", "200"),
    ("global_monthly_anlas", "10000.0", "float", "10000.0"),
];

fn fixed_default(key: &str) -> Option<Value> {
    FIXED_DEFAULTS
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|(_, d, ..)| serde_json::from_str(d).unwrap())
}

fn fixed_default_spec(c: &Case) -> bool {
    c.raw["spec"]["ref"]
        .as_str()
        .is_some_and(|k| fixed_default(k).is_some())
}

#[test]
fn fixed_defaults_ignore_the_environment() {
    for (key, want, ..) in FIXED_DEFAULTS {
        let spec = **ALL.iter().find(|s| s.key == key).unwrap();
        let want: Value = serde_json::from_str(want).unwrap();
        assert_eq!(spec.env, "", "{key}");
        // 环境里有没有值都一样
        for env in [None, Some(json!({key: 7}))] {
            assert_eq!(site_flags::default_of(&spec, env.as_ref()), want, "{key}");
            assert_eq!(
                site_flags::get_from_raw(&spec, None, env.as_ref()),
                want,
                "{key}"
            );
            assert_eq!(
                site_flags::get_from_raw(&spec, Some("abc"), env.as_ref()),
                want,
                "{key}"
            );
            assert_eq!(
                site_flags::get_from_raw(&spec, Some("90"), env.as_ref()),
                if want.is_f64() {
                    json!(90.0)
                } else {
                    json!(90)
                },
                "{key}"
            );
        }
    }
}

fn fix_docs(v: &Value) -> Value {
    let mut text = v.to_string();
    for (old, new) in DOC_FIXES {
        text = text.replace(old, new);
    }
    serde_json::from_str(&text).unwrap()
}

#[test]
fn site_registry_golden() {
    let mut n = 0;
    for c in load(M, "flags/site_registry") {
        match c.raw["fn"].as_str().unwrap() {
            "spec" => {
                let key = c.out()["key"].as_str().unwrap();
                if is_removed(key) {
                    continue;
                }
                let s = ALL.iter().find(|s| s.key == key).expect("缺少这个键");
                let mut want = fix_docs(c.out());
                if let Some(d) = fixed_default(key) {
                    want["default"] = d;
                    want["env"] = json!("");
                }
                assert_eq_py(&c, &spec_json(s), &want);
                n += 1;
            }
            "all_keys" => {
                let keys: Vec<Value> = ALL.iter().map(|s| json!(s.key)).collect();
                let mut want = c.out().clone();
                want.as_array_mut()
                    .unwrap()
                    .retain(|k| !k.as_str().is_some_and(is_removed));
                assert_eq_py(&c, &Value::Array(keys), &want);
            }
            // SYSTEM-FACTS 的 Markdown 表格生成器没有生产调用方，已删除（docs/v3-deviations.md #101）
            "facts_table" => {}
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
        if removed_spec(&c) {
            continue;
        }
        let spec = spec_of(&c.raw["spec"]);
        let got = site_flags::parse(&spec, c.arg(0), c.arg(1).clone());
        check(&c, Ok(got), false, false);
    }
}

#[test]
fn site_default_of_golden() {
    for c in load(M, "flags/site_default_of") {
        if removed_spec(&c) || fixed_default_spec(&c) {
            continue;
        }
        let spec = spec_of(&c.raw["spec"]);
        let env = c.raw.get("env").filter(|e| !e.is_null());
        check(&c, Ok(site_flags::default_of(&spec, env)), false, false);
    }
}

#[test]
fn site_get_golden() {
    for c in load(M, "flags/site_get") {
        if removed_spec(&c) || fixed_default_spec(&c) {
            continue;
        }
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
        if removed_spec(&c) {
            continue;
        }
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

// 文本生成 / 语音随文本接口整套删除（docs/v3-deviations.md #81）：功能表里没有 `text` / `voice` 了。
// 冻结夹具照旧留着，比较时在期望值一侧把这两个功能去掉（列表里的名字、成对的 (名字, 说明) 条目、
// 开关字典的键、逗号串里的名字）；`kinds_for("text" / "voice")` 期望空列表；检查某个已删功能的用例不再跑。

const REMOVED_FEATURES: [&str; 2] = ["text", "voice"];

fn is_removed_feature(v: &Value) -> bool {
    match v {
        Value::String(s) => REMOVED_FEATURES.contains(&s.as_str()),
        // (名字, 说明) / (用量 kind, 功能) 成对的条目
        Value::Array(pair) => pair
            .iter()
            .take(2)
            .any(|x| x.as_str().is_some_and(|s| REMOVED_FEATURES.contains(&s))),
        _ => false,
    }
}

/// 期望值一侧去掉已删除的功能。
fn without_removed_features(v: &Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .filter(|x| !is_removed_feature(x))
                .map(without_removed_features)
                .collect(),
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !REMOVED_FEATURES.contains(&k.as_str()))
                .map(|(k, x)| (k.clone(), without_removed_features(x)))
                .collect(),
        ),
        other => other.clone(),
    }
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
                    &without_removed_features(c.out()),
                );
                continue;
            }
            "kinds_for" => {
                let name = c.arg(0).as_str().unwrap();
                let want = if REMOVED_FEATURES.contains(&name) {
                    json!([])
                } else {
                    c.out().clone()
                };
                assert_eq_py_ordered(&c, &json!(features::kinds_for(name)), &want);
                continue;
            }
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
        match (c.err(), got) {
            (None, Ok(v)) => {
                // dump 的结果是逗号串，其余是列表 / 字典 / null
                let want = match c.out() {
                    Value::String(joined) => Value::String(
                        joined
                            .split(',')
                            .filter(|n| !REMOVED_FEATURES.contains(n))
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    other => without_removed_features(other),
                };
                assert_eq_py_ordered(&c, &v, &want);
            }
            (_, got) => check(&c, got, true, false),
        }
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
        if REMOVED_FEATURES.contains(&name) {
            continue;
        }
        let env = (!c.arg(3).is_null()).then(|| c.arg(3));
        let now = c.arg(4).as_f64().unwrap();
        let global = kv.get(features::GLOBAL_KEY).and_then(Value::as_str);

        let needs = features::needs_vibe_site_flags(global, key, name);
        let vibe = needs.then(|| {
            // 旧版的两个设置：开放截止时间 `global_daily_anlas_until` 和全站每日 Anlas `global_daily_anlas`
            // （默认值取环境）。3.0 没有截止时间（docs/v3-deviations.md #92）：到了截止时间等于额度 0，
            // 这里折成传给 `check_decision` 的额度，等价于旧版的判定
            let old_spec = |key: &'static str, kind, hi| Spec {
                key,
                kind,
                default: SpecDefault::None,
                owner: "",
                doc: "",
                lo: Some(0),
                hi: Some(hi),
                choices: &[],
                env: key,
            };
            let until = site_flags::get_from_raw(
                &old_spec("global_daily_anlas_until", Kind::Float, 2_000_000_000),
                kv.get("global_daily_anlas_until").and_then(Value::as_str),
                env,
            );
            let cap = site_flags::get_from_raw(
                &old_spec("global_daily_anlas", Kind::Int, 100_000),
                kv.get("global_daily_anlas").and_then(Value::as_str),
                env,
            );
            let until = pyval::number_like(&until).unwrap();
            VibeAnlas {
                daily_cap: if until > 0.0 && now >= until {
                    0
                } else {
                    cap.as_i64().unwrap()
                },
            }
        });
        let got = features::check_decision(global, key, name, vibe);
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
//
// 3.0 删了「跑图分享设置」和「最近动作」，机器人只剩心跳（docs/v3-deviations.md #54）。冻结夹具照旧留着：
// `bot_validate` / `bot_load` / `bot_save`（设置的校验与读写）整组不再有对应的函数，不测；
// `bot_constants` / `bot_report` / `bot_snapshot` 里还和心跳有关的部分，在期望值一侧去掉设置、事件和 AI 字段再比。

/// 心跳白名单里 3.0 删掉的字段（AI 评论的计数）。
const REMOVED_STATUS_FIELDS: [&str; 2] = ["ai_ready", "ai_today"];

#[test]
fn bot_constants_golden() {
    let cases = load(M, "flags/bot_constants");
    assert_eq!(cases.len(), 1);
    for c in cases {
        // 夹具里还有 FIELDS / EVENTS_KEY / EVENT_KINDS，3.0 都删了；只剩心跳存放的键
        assert_eq!(json!(bot_config::STATUS_KEY), c.out()["STATUS_KEY"]);
    }
}

fn bot_err(e: BotConfigError) -> (&'static str, String) {
    match e {
        BotConfigError::BadJson(m) => ("JSONDecodeError", m),
        BotConfigError::WrongType(m) => ("AttributeError", m),
        BotConfigError::Type(m) => ("TypeError", m),
    }
}

#[test]
fn bot_report_golden() {
    let (mut n_exact, mut n_trimmed) = (0, 0);
    for c in load(M, "flags/bot_report") {
        match c.raw["fn"].as_str().unwrap() {
            "report_status" => {
                let status = obj(c.arg(0));
                let got = bot_config::status_to_store(&status, c.arg(1).as_f64().unwrap());
                if REMOVED_STATUS_FIELDS
                    .iter()
                    .any(|f| status.contains_key(*f))
                {
                    // 旧版会把 ai_ready / ai_today 一起存下；3.0 不存。期望值解析后去掉这两个键，
                    // 再连键的顺序一起比（解析比较，不依赖我们自己的 JSON 写法）
                    let mut want: Value = serde_json::from_str(c.out().as_str().unwrap()).unwrap();
                    for f in REMOVED_STATUS_FIELDS {
                        want.as_object_mut().unwrap().shift_remove(f);
                    }
                    let got: Value = serde_json::from_str(&got).unwrap();
                    assert_eq_py_ordered(&c, &got, &want);
                    n_trimmed += 1;
                } else {
                    assert_eq_py(&c, &json!(got), c.out());
                    n_exact += 1;
                }
            }
            // 事件（点赞 / 评论记录）3.0 没有了
            "report_event" => {}
            other => panic!("未知 {other}"),
        }
    }
    assert!(n_exact > 100 && n_trimmed > 100, "{n_exact} {n_trimmed}");
}

fn stored_of(v: &Value) -> BTreeMap<String, String> {
    obj(v)
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn bot_snapshot_golden() {
    let (mut n_ok, mut n_status_err, mut n_events_err) = (0, 0, 0);
    for c in load(M, "flags/bot_snapshot") {
        let kv = stored_of(c.arg(0));
        let now = c.arg(1).as_f64().unwrap();
        let got = bot_config::snapshot(kv.get(bot_config::STATUS_KEY).map(String::as_str), now);
        match (c.err(), got) {
            (None, Ok(v)) => {
                // 旧版快照是 {config, status, events, kinds}，3.0 只剩 status
                let want = json!({"status": c.out()["status"]});
                assert_eq_py_ordered(&c, &v, &want);
                n_ok += 1;
            }
            (Some(ty), Err(e)) => {
                assert_eq!(bot_err(e).0, ty, "第 {} 行：异常类型不同", c.line);
                n_status_err += 1;
            }
            // 旧版还会解析 bot_events（坏 JSON / 不能倒序的类型）而报错；3.0 不再读事件，状态本身没问题就成功
            (Some(ty), Ok(_)) => {
                assert!(
                    matches!(ty, "JSONDecodeError" | "TypeError") && kv.contains_key("bot_events"),
                    "第 {} 行：Python 抛 {ty}，Rust 却成功了\n{}",
                    c.line,
                    c.raw
                );
                n_events_err += 1;
            }
            (None, Err(e)) => {
                let (t, m) = bot_err(e);
                panic!("第 {} 行：Python 成功，Rust 报 {t}: {m}\n{}", c.line, c.raw)
            }
        }
    }
    assert!(
        n_ok > 100 && n_status_err > 50 && n_events_err > 10,
        "{n_ok} {n_status_err} {n_events_err}"
    );
}
