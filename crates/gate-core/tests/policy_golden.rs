//! `policy` 与旧 Python（app/policy.py）对拍：夹具由旧 Python 生成（脚本见标签 py-final）。
//! 每个移植的函数都有一个测试；请求体类函数同时比较返回值和（原地修改后的）入参，键顺序也要一致。

use gate_core::policy::{self, ImageCost};
use gate_core::pyc;
use gate_core::pyext::{PyResult, is_digit, is_nd, json_int};
use gate_golden::{Case, assert_eq_py_ordered, f, load};
use serde_json::{Value, json};

const M: &str = env!("CARGO_MANIFEST_DIR");

/// 夹具里 `random.randint(0, 4294967295)` 被打桩成这个值。
const FIXED_RANDINT: u32 = 2_718_281_828;

fn opt_str(s: Option<String>) -> Value {
    s.map_or(Value::Null, Value::String)
}

fn strs(v: &[String]) -> Value {
    Value::Array(v.iter().cloned().map(Value::String).collect())
}

/// Python 抛异常 → Rust 必须返回同类型、同消息的 `Err`（OverflowError 的消息随场景变化，只比类型）。
#[track_caller]
fn check(c: &Case, got: PyResult<Value>) {
    match (c.err(), got) {
        (Some(ty), Err(e)) => {
            assert_eq!(
                e.type_name(),
                ty,
                "第 {} 行异常类型不同：{e}\n输入：{}",
                c.line,
                c.raw
            );
            if ty != "OverflowError" {
                let want = c.raw["err"]["msg"].as_str().unwrap_or_default();
                assert_eq!(
                    e.message(),
                    want,
                    "第 {} 行异常消息不同\n输入：{}",
                    c.line,
                    c.raw
                );
            }
        }
        (Some(ty), Ok(v)) => panic!(
            "第 {} 行：Python 抛了 {ty}，Rust 却返回 {v}\n输入：{}",
            c.line, c.raw
        ),
        (None, Err(e)) => panic!(
            "第 {} 行：Python 正常返回，Rust 却报 {}: {e}\n输入：{}",
            c.line,
            e.type_name(),
            c.raw
        ),
        (None, Ok(v)) => assert_eq_py_ordered(c, &v, c.out()),
    }
}

/// 比较「原地修改后的入参」。
#[track_caller]
fn check_after(c: &Case, body: &Value) {
    let after = c.after().expect("夹具缺少 after");
    assert_eq_py_ordered(c, body, &after["0"]);
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("期望字符串：{v}"))
}

fn int(v: &Value) -> i128 {
    pyext_int(v).unwrap_or_else(|| panic!("期望整数：{v}"))
}

fn pyext_int(v: &Value) -> Option<i128> {
    gate_core::pyext::strict_int(v)
}

fn kw_bool(c: &Case, name: &str, default: bool) -> bool {
    c.kwarg(name).and_then(Value::as_bool).unwrap_or(default)
}

fn kw_i64(c: &Case, name: &str) -> Option<i64> {
    c.kwarg(name).and_then(Value::as_i64)
}

fn cost_json(r: ImageCost) -> Value {
    json!({"anlas": r.anlas, "v5": r.v5})
}

fn pair(a: i128, b: i128) -> Value {
    json!([json_int(a), json_int(b)])
}

// ----------------------------------------------------------------------------- 常量

#[test]
fn constants_golden() {
    let strs_of = |xs: &[&str]| -> Value {
        let mut v: Vec<&str> = xs.to_vec();
        v.sort();
        json!(v)
    };
    for c in load(M, "policy/constants") {
        let name = c.raw["name"].as_str().unwrap();
        let want = &c.raw["value"];
        let got: Value = match name {
            "FREE_BURST" => f(policy::FREE_BURST),
            "FREE_REFILL_PER_HOUR" => f(policy::FREE_REFILL_PER_HOUR),
            "V5_COST_MULTIPLIER" => f(policy::V5_COST_MULTIPLIER),
            "LEGACY_IMAGE_MODEL_PREFIXES" => json!(policy::LEGACY_IMAGE_MODEL_PREFIXES),
            "LEGACY_IMAGE_MODEL_EXACT" => strs_of(&policy::LEGACY_IMAGE_MODEL_EXACT),
            "V5_NORMAL_PRESETS" => json!(policy::V5_NORMAL_PRESETS),
            "VIBE_RAW_MODELS" => strs_of(&policy::VIBE_RAW_MODELS),
            "VIBE_ENCODED_MODELS" => strs_of(&policy::VIBE_ENCODED_MODELS),
            "PRECISE_REFERENCE_MODELS" => strs_of(&policy::PRECISE_REFERENCE_MODELS),
            "REFERENCE_LIMIT" => json!(policy::REFERENCE_LIMIT),
            "VIBE_ENCODING_ANLAS" => json!(policy::VIBE_ENCODING_ANLAS),
            "REFERENCE_FIELDS" => json!(policy::REFERENCE_FIELDS),
            "INTEGER_IMAGE_PARAMS" => json!(policy::INTEGER_IMAGE_PARAMS),
            "MAX_PROMPT_BYTES" => json!(policy::MAX_PROMPT_BYTES),
            "DIMENSION_STEP" => json!(policy::DIMENSION_STEP),
            "V4_V5_REJECTED_SAMPLERS" => strs_of(&policy::V4_V5_REJECTED_SAMPLERS),
            "MAX_CHARACTERS" => json!(policy::MAX_CHARACTERS),
            "V5_MEDIUM_OF" => {
                let mut m = serde_json::Map::new();
                for (k, v) in policy::V5_MEDIUM_OF {
                    m.insert(k.into(), json!(v));
                }
                Value::Object(m)
            }
            "V5_MEDIUM_STEPS" => json!(policy::V5_MEDIUM_STEPS),
            "V5_MEDIUM_SAMPLER" => json!(policy::V5_MEDIUM_SAMPLER),
            "V5_MEDIUM_UC_PRESET" => json!(policy::V5_MEDIUM_UC_PRESET),
            "V5_MEDIUM_UC" => json!(policy::V5_MEDIUM_UC),
            "TEXT_CONTEXT_TOKENS" => json!(policy::TEXT_CONTEXT_TOKENS),
            "TEXT_MAX_GENERATION_TOKENS" => json!(policy::TEXT_MAX_GENERATION_TOKENS),
            "TEXT_INPUT_BUDGET_TOKENS" => json!(policy::TEXT_INPUT_BUDGET_TOKENS),
            other => panic!("夹具里有未覆盖的常量 {other}"),
        };
        // 集合在夹具里已排序；元组 / 列表保持原序
        assert_eq_py_ordered(&c, &got, want);
    }
}

#[test]
fn char_classes_match_python_re() {
    let ranges = |pred: &dyn Fn(char) -> bool| -> Value {
        let mut out: Vec<[u32; 2]> = Vec::new();
        let mut start: Option<u32> = None;
        for cp in 0..0x11_0000u32 {
            let hit = char::from_u32(cp).is_some_and(pred);
            match (hit, start) {
                (true, None) => start = Some(cp),
                (false, Some(a)) => {
                    out.push([a, cp - 1]);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(a) = start {
            out.push([a, 0x10_ffff]);
        }
        json!(out)
    };
    for c in load(M, "policy/charclass") {
        let name = c.raw["name"].as_str().unwrap();
        let got = match name {
            "re_s" => ranges(&pyc::is_space),
            "re_d" | "isdecimal" => ranges(&is_nd),
            "isdigit" => ranges(&is_digit),
            // Python 的 Unicode 表升级（3.13 起是 16.0）时，pyc / pyext 里的数字表要跟着重新核对
            "unicode_version" => {
                assert_eq!(c.raw["value"], "15.0.0", "Python 的 Unicode 版本变了");
                continue;
            }
            other => panic!("未覆盖的字符类 {other}"),
        };
        assert_eq_py_ordered(&c, &got, &c.raw["ranges"]);
    }
}

// ----------------------------------------------------------------------------- 小工具

#[test]
fn free_paced_cap_golden() {
    for c in load(M, "policy/free_paced_cap") {
        let got = f(policy::free_paced_cap(int(c.arg(0)) as i64));
        assert_eq_py_ordered(&c, &got, c.out());
    }
}

#[test]
fn estimate_tokens_golden() {
    for c in load(M, "policy/estimate_tokens") {
        assert_eq_py_ordered(&c, &json!(policy::estimate_tokens(s(c.arg(0)))), c.out());
    }
}

#[test]
fn estimate_text_tokens_golden() {
    for c in load(M, "policy/estimate_text_tokens") {
        assert_eq_py_ordered(
            &c,
            &json!(policy::estimate_text_tokens(s(c.arg(0)))),
            c.out(),
        );
    }
}

#[test]
fn truncate_middle_golden() {
    for c in load(M, "policy/_truncate_middle") {
        let ratio = c
            .kwarg("head_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(0.35);
        let (text, cut) = policy::truncate_middle(s(c.arg(0)), int(c.arg(1)) as i64, ratio);
        assert_eq_py_ordered(&c, &json!([text, cut]), c.out());
    }
}

#[test]
fn base64_data_golden() {
    for c in load(M, "policy/_base64_data") {
        let got = policy::base64_data(
            Some(c.arg(0)),
            kw_bool(&c, "source_image", false),
            kw_bool(&c, "png", false),
        );
        assert_eq_py_ordered(&c, &json!(got), c.out());
    }
}

#[test]
fn unit_value_golden() {
    for c in load(M, "policy/_unit_value") {
        assert_eq_py_ordered(&c, &json!(policy::unit_value(c.arg(0))), c.out());
    }
}

#[test]
fn reference_list_golden() {
    for c in load(M, "policy/_reference_list") {
        let p = c.arg(0).as_object().unwrap();
        let got = policy::reference_list(p, s(c.arg(1))).map(|l| Value::Array(l.to_vec()));
        check(&c, got);
    }
}

#[test]
fn validate_vibe_encoding_golden() {
    for c in load(M, "policy/validate_vibe_encoding") {
        assert_eq_py_ordered(
            &c,
            &opt_str(policy::validate_vibe_encoding(c.arg(0))),
            c.out(),
        );
    }
}

#[test]
fn validate_image_references_golden() {
    for c in load(M, "policy/validate_image_references") {
        assert_eq_py_ordered(
            &c,
            &opt_str(policy::validate_image_references(c.arg(0))),
            c.out(),
        );
    }
}

#[test]
fn reference_surcharge_golden() {
    for c in load(M, "policy/reference_surcharge") {
        check(&c, policy::reference_surcharge(c.arg(0)).map(|n| json!(n)));
    }
}

// ----------------------------------------------------------------------------- 规范化与提示词

#[test]
fn normalize_image_request_golden() {
    for c in load(M, "policy/normalize_image_request") {
        let mut body = c.arg(0).clone();
        let got =
            policy::normalize_image_request(&mut body, &mut || FIXED_RANDINT).map(|()| Value::Null);
        check(&c, got);
        check_after(&c, &body);
    }
}

#[test]
fn prompt_texts_golden() {
    for c in load(M, "policy/_prompt_texts") {
        let items: Vec<Value> = policy::prompt_texts(c.arg(0))
            .into_iter()
            .map(|(path, v)| json!([path, v.cloned().unwrap_or(Value::Null)]))
            .collect();
        assert_eq_py_ordered(&c, &Value::Array(items), c.out());
    }
}

#[test]
fn fix_name_digit_weights_golden() {
    for c in load(M, "policy/fix_name_digit_weights") {
        let mut body = c.arg(0).clone();
        let n = policy::fix_name_digit_weights(&mut body);
        assert_eq_py_ordered(&c, &json!(n), c.out());
        check_after(&c, &body);
    }
}

#[test]
fn clean_prompt_syntax_golden() {
    for c in load(M, "policy/clean_prompt_syntax") {
        assert_eq_py_ordered(
            &c,
            &json!(policy::clean_prompt_syntax(s(c.arg(0)))),
            c.out(),
        );
    }
}

#[test]
fn sanitize_and_translate_prompts_golden() {
    for c in load(M, "policy/sanitize_and_translate_prompts") {
        let mut body = c.arg(0).clone();
        let n = policy::sanitize_and_translate_prompts(&mut body);
        assert_eq_py_ordered(&c, &json!(n), c.out());
        check_after(&c, &body);
    }
}

#[test]
fn upstream_parameter_problem_golden() {
    for c in load(M, "policy/upstream_parameter_problem") {
        assert_eq_py_ordered(
            &c,
            &opt_str(policy::upstream_parameter_problem(c.arg(0))),
            c.out(),
        );
    }
}

// ----------------------------------------------------------------------------- 模型 / 计费

/// `(model or "")`：夹具里 None 表示空串。
fn model_arg(c: &Case) -> &str {
    c.arg(0).as_str().unwrap_or("")
}

#[test]
fn is_v5_model_golden() {
    for c in load(M, "policy/is_v5_model") {
        assert_eq_py_ordered(&c, &json!(policy::is_v5_model(model_arg(&c))), c.out());
    }
}

#[test]
fn image_model_tier_golden() {
    for c in load(M, "policy/image_model_tier") {
        let got = policy::image_model_tier(model_arg(&c)).map_or(Value::Null, |t| json!(t));
        assert_eq_py_ordered(&c, &got, c.out());
    }
}

#[test]
fn is_v5_medium_golden() {
    for c in load(M, "policy/is_v5_medium") {
        assert_eq_py_ordered(&c, &json!(policy::is_v5_medium(model_arg(&c))), c.out());
    }
}

#[test]
fn per_image_cost_golden() {
    for c in load(M, "policy/_per_image_cost") {
        let got = policy::per_image_cost(
            int(c.arg(0)),
            int(c.arg(1)),
            int(c.arg(2)),
            c.arg(3).as_bool().unwrap(),
            c.arg(4).as_bool().unwrap(),
        )
        .map(f);
        check(&c, got);
    }
}

#[test]
fn has_paid_extras_golden() {
    for c in load(M, "policy/_has_paid_extras") {
        check(&c, policy::has_paid_extras(c.arg(0)).map(|b| json!(b)));
    }
}

#[test]
fn opus_free_eligible_golden() {
    for c in load(M, "policy/opus_free_eligible") {
        check(&c, policy::opus_free_eligible(c.arg(0)).map(|b| json!(b)));
    }
}

#[test]
fn legacy_normal_free_eligible_golden() {
    for c in load(M, "policy/legacy_normal_free_eligible") {
        check(
            &c,
            policy::legacy_normal_free_eligible(c.arg(0)).map(|b| json!(b)),
        );
    }
}

#[test]
fn v5_allowance_eligible_golden() {
    for c in load(M, "policy/v5_allowance_eligible") {
        check(
            &c,
            policy::v5_allowance_eligible(c.arg(0)).map(|b| json!(b)),
        );
    }
}

#[test]
fn snap_v5_preset_golden() {
    for c in load(M, "policy/snap_v5_preset") {
        let (w, h) = policy::snap_v5_preset(int(c.arg(0)), int(c.arg(1)));
        assert_eq_py_ordered(&c, &pair(w, h), c.out());
    }
}

#[test]
fn estimate_image_cost_golden() {
    for c in load(M, "policy/estimate_image_cost") {
        let got = policy::estimate_image_cost(
            c.arg(0),
            kw_bool(&c, "is_opus", true),
            kw_bool(&c, "v5_allowance_available", true),
        )
        .map(cost_json);
        check(&c, got);
    }
}

// ----------------------------------------------------------------------------- 钳制

#[test]
fn fit_size_golden() {
    for c in load(M, "policy/fit_size") {
        let got =
            policy::fit_size(int(c.arg(0)), int(c.arg(1)), int(c.arg(2))).map(|(w, h)| pair(w, h));
        check(&c, got);
    }
}

#[test]
fn clamp_image_params_golden() {
    for c in load(M, "policy/clamp_image_params") {
        let got = policy::clamp_image_params(
            c.arg(0),
            kw_i64(&c, "max_pixels").unwrap(),
            kw_i64(&c, "max_steps").unwrap(),
            kw_bool(&c, "allow_img2img", false),
        )
        .map(|(out, notes, err)| json!([out, strs(&notes), opt_str(err)]));
        check(&c, got);
    }
}

#[test]
fn fill_v4_prompt_golden() {
    for c in load(M, "policy/fill_v4_prompt") {
        let (out, notes) = policy::fill_v4_prompt(c.arg(0));
        assert_eq_py_ordered(&c, &json!([out, strs(&notes)]), c.out());
    }
}

#[test]
fn medium_normalize_golden() {
    for c in load(M, "policy/medium_normalize") {
        let (out, notes) = policy::medium_normalize(c.arg(0));
        assert_eq_py_ordered(&c, &json!([out, strs(&notes)]), c.out());
    }
}

#[test]
fn clamp_text_params_golden() {
    for c in load(M, "policy/clamp_text_params") {
        let got = policy::clamp_text_params(
            c.arg(0),
            kw_i64(&c, "max_output_tokens").unwrap(),
            kw_i64(&c, "max_input_chars").unwrap(),
            kw_i64(&c, "max_input_tokens").unwrap_or(policy::TEXT_INPUT_BUDGET_TOKENS),
        )
        .map(|(out, notes, err)| json!([out, strs(&notes), opt_str(err)]));
        check(&c, got);
    }
}

#[test]
fn guard_image_action_golden() {
    for c in load(M, "policy/guard_image_action") {
        let (out, notes, err) = policy::guard_image_action(c.arg(0));
        assert_eq_py_ordered(&c, &json!([out, strs(&notes), opt_str(err)]), c.out());
    }
}

// ----------------------------------------------------------------------------- 杂项

#[test]
fn text_model_host_golden() {
    for c in load(M, "policy/text_model_host") {
        let got = policy::text_model_host(model_arg(&c), s(c.arg(1)), s(c.arg(2)));
        assert_eq_py_ordered(&c, &json!(got), c.out());
    }
}

#[test]
fn mask_token_golden() {
    for c in load(M, "policy/mask_token") {
        let keep = c.args().get(1).map_or(6, |k| int(k) as usize);
        assert_eq_py_ordered(&c, &json!(policy::mask_token(s(c.arg(0)), keep)), c.out());
    }
}

#[test]
fn gen_key_golden() {
    use base64::Engine;
    for c in load(M, "policy/gen_key") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(c.arg(1)["$b"].as_str().unwrap())
            .unwrap();
        let arr: [u8; 24] = bytes.try_into().expect("24 字节");
        assert_eq_py_ordered(&c, &json!(policy::gen_key(s(c.arg(0)), &arr)), c.out());
    }
}
