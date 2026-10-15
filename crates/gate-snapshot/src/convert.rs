//! 一次性转换工具：把旧 Python 对拍夹具转成快照时用的抽样和过滤（T1b / T2 用完即删，T8 删除本模块）。
//!
//! 只有工具函数，不转换任何现有夹具。一个套件的转换流程：
//! 1. 调用方先删掉对应功能已经删除的用例，把剩下的旧用例转成 [`Sample`]（输入一律是「去掉旧字段之后」的形状）；
//! 2. [`sample_per_fn`] 先丢掉会被红线规则命中的用例，再按分桶键在桶间轮转抽样、每桶先取输入最小的，每个函数最多 N 条；
//!    单文件超过 512 KB 时用 [`sample_within_budget`] 降低每函数上限；
//! 3. 轨迹类（guard、allowance、errors capture 等）用 [`greedy_cover`] 贪心挑选覆盖操作种类最多的几条；
//! 4. [`write_pending`] 写出 `{"in":...,"out":null}`，再跑一次 `UPDATE_SNAPSHOT=1` 填充输出，
//!    最后再跑一次默认模式确认确定、零 diff。

use crate::Case;
use crate::snapshot::{path, render_line, to_compact, write_atomic};
use fancy_regex::Regex;
use serde_json::{Number, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::LazyLock;

/// 每个函数默认最多保留的用例数。
pub const PER_FN_CAP: usize = 100;
/// 单个快照文件的体积上限（超过就降低每函数上限）。
pub const FILE_BUDGET_BYTES: usize = 512 * 1024;
/// 轨迹类：每种操作至少要出现在这么多条被选中的轨迹里。
pub const TRACE_MIN_EACH: usize = 3;
/// 轨迹类：最多挑这么多条。
pub const TRACE_MAX: usize = 20;
/// 分桶键里异常消息取多少个字符作为「前缀」。
pub const MSG_PREFIX_CHARS: usize = 12;

// ───────────────────────── 红线规则 ─────────────────────────
//
// 与 `tools/ci/check-fixtures.sh` 里的三条规则一致：17–20 位 Discord 雪花 ID、公网 IPv4、`pst-` 开头的上游 Token。
// 正则原文照抄（含环视），放行名单也逐条对应。

static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""([1-8][0-9]{16,19})""#).expect("ID 正则"));
static ZEROS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[1-9]0{6,}[0-9]*$").expect("合成 ID 正则"));
static IP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?<![0-9./])((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?!\.?[0-9])").expect("IP 正则")
});
static TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"pst-[A-Za-z0-9_-]{40,}").expect("Token 正则"));

/// 文本里有没有会被 `check-fixtures.sh` 拦下的内容；有就返回第一处的说明。
/// 正则引擎报错（回溯超限）时保守地当作命中。
pub fn redline_hit(text: &str) -> Option<String> {
    for caps in ID_RE.captures_iter(text) {
        let Ok(caps) = caps else {
            return Some("正则引擎出错，按命中处理".to_string());
        };
        let id = &caps[1];
        let synthetic = ZEROS_RE.is_match(id).unwrap_or(false)
            || "12345678901234567890".starts_with(id)
            || id == "18446744073709551615"
            || id == "18446744073709551616";
        if !synthetic {
            return Some(format!("疑似真实 Discord ID {id}"));
        }
    }
    for caps in IP_RE.captures_iter(text) {
        let Ok(caps) = caps else {
            return Some("正则引擎出错，按命中处理".to_string());
        };
        let ip = &caps[1];
        if !is_allowed_ip(ip) {
            return Some(format!("疑似公网 IP {ip}"));
        }
    }
    match TOKEN_RE.is_match(text) {
        Ok(false) => None,
        Ok(true) => Some("疑似真实上游 Token".to_string()),
        Err(_) => Some("正则引擎出错，按命中处理".to_string()),
    }
}

/// 脚本放行的 IP：某段超过 255、众所周知的占位地址、私网、RFC 5737 文档网段、回环和 0 开头。
fn is_allowed_ip(ip: &str) -> bool {
    const KNOWN: [&str; 6] = [
        "1.2.3.4",
        "8.8.8.8",
        "9.9.9.9",
        "1.1.1.1",
        "127.0.0.1",
        "0.0.0.0",
    ];
    let octets: Vec<u32> = ip.split('.').map(|o| o.parse().unwrap_or(999)).collect();
    if octets.iter().any(|&o| o > 255) || KNOWN.contains(&ip) {
        return true;
    }
    let (a, b, c) = (octets[0], octets[1], octets[2]);
    a == 10
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || matches!((a, b, c), (192, 0, 2) | (198, 51, 100) | (203, 0, 113))
        || a == 127
        || a == 0
}

// ───────────────────────── 抽样 ─────────────────────────

/// 一个待抽样的旧用例。
#[derive(Debug, Clone)]
pub struct Sample {
    /// 所属函数（分组用）；不分函数的套件传空串。
    pub func: String,
    /// 将写进快照 `"in"` 的输入（已去掉旧字段）。
    pub input: Value,
    /// 旧夹具里的输出，只用来分桶和估算体积。
    pub out: Value,
    /// Python 抛的异常：类型名和消息。
    pub err: Option<(String, String)>,
}

impl Sample {
    pub fn new(func: impl Into<String>, input: Value, out: Value) -> Sample {
        Sample {
            func: func.into(),
            input,
            out,
            err: None,
        }
    }

    pub fn with_err(mut self, ty: impl Into<String>, msg: impl Into<String>) -> Sample {
        self.err = Some((ty.into(), msg.into()));
        self
    }

    /// 按旧夹具的行格式转换：`fn` 做分组，输入是去掉 `out` / `err` / `after` / `note` 之后的整行
    /// （`fn` / `args` / `kwargs` 原样保留，所以新 `Case` 的取数方法照常能用），`out` 和 `err` 用来分桶。
    pub fn from_case(case: &Case) -> Sample {
        let func = case
            .get("fn")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut input = case.raw.as_object().cloned().unwrap_or_default();
        for k in ["out", "err", "after", "note"] {
            input.shift_remove(k);
        }
        let err = case.get("err").and_then(|e| {
            let ty = e.get("type")?.as_str()?;
            let msg = e.get("msg").and_then(Value::as_str).unwrap_or_default();
            Some((ty.to_string(), msg.to_string()))
        });
        Sample {
            func,
            input: Value::Object(input),
            out: case.get("out").cloned().unwrap_or(Value::Null),
            err,
        }
    }

    /// 这个用例会不会被红线规则拦下：扫描将写进快照的那一行，以及异常的类型和消息。
    pub fn redline_hit(&self) -> Option<String> {
        if let Some(hit) = redline_hit(&render_line(&self.input, &self.out)) {
            return Some(hit);
        }
        let (ty, msg) = self.err.as_ref()?;
        redline_hit(&to_compact(&Value::Array(vec![
            Value::String(ty.clone()),
            Value::String(msg.clone()),
        ])))
    }
}

/// 抽样结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    /// 保留的用例在输入切片里的下标：按函数首次出现的顺序分组，组内保持原来的顺序。
    pub indices: Vec<usize>,
    /// 因命中红线规则而丢弃的用例数。
    pub dropped_redline: usize,
}

/// 按函数抽样：每个函数最多 `cap` 条，先丢掉命中红线的用例。
/// 函数之内按 [`bucket_key`] 分桶，在桶之间轮转取样，每个桶里先取输入最小的（体积相同取靠前的）；
/// 桶的轮转顺序是「异常桶在前，其余按各桶最小输入的体积从小到大」，所以桶数超过 `cap` 时被舍弃的是最大最怪的桶。
pub fn sample_per_fn(samples: &[Sample], cap: usize) -> Picked {
    Pool::new(samples).pick(cap)
}

/// 单个文件有体积上限时：从 `cap` 开始，估算体积（`{"in":...,"out":旧输出}` 每行加换行）超标就按比例降低每函数上限，
/// 直到不超标或降到 1。返回抽样结果和最终采用的上限。
pub fn sample_within_budget(samples: &[Sample], cap: usize, max_bytes: usize) -> (Picked, usize) {
    let pool = Pool::new(samples);
    let mut cap = cap;
    loop {
        let picked = pool.pick(cap);
        let bytes: usize = picked.indices.iter().map(|&i| pool.line_bytes[i]).sum();
        if bytes <= max_bytes || cap <= 1 {
            return (picked, cap);
        }
        // 按比例缩小，并保证至少少 1，循环一定终止
        let scaled = (cap as u128 * max_bytes as u128 / bytes as u128) as usize;
        cap = scaled.clamp(1, cap - 1);
    }
}

/// 抽样用的预处理结果（红线扫描和体积计算只做一次，降低上限重抽时不重复）。
struct Pool<'a> {
    samples: &'a [Sample],
    /// 输入的紧凑 JSON 长度，用来比「输入大小」。
    input_bytes: Vec<usize>,
    /// 写成快照一行的字节数（含换行）。
    line_bytes: Vec<usize>,
    banned: Vec<bool>,
    dropped_redline: usize,
}

impl<'a> Pool<'a> {
    fn new(samples: &'a [Sample]) -> Pool<'a> {
        let banned: Vec<bool> = samples.iter().map(|s| s.redline_hit().is_some()).collect();
        Pool {
            samples,
            input_bytes: samples.iter().map(|s| to_compact(&s.input).len()).collect(),
            line_bytes: samples
                .iter()
                .map(|s| render_line(&s.input, &s.out).len() + 1)
                .collect(),
            dropped_redline: banned.iter().filter(|&&b| b).count(),
            banned,
        }
    }

    fn pick(&self, cap: usize) -> Picked {
        // 按函数分组，保留首次出现的顺序
        let mut order: Vec<&str> = Vec::new();
        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, s) in self.samples.iter().enumerate() {
            if self.banned[i] {
                continue;
            }
            groups
                .entry(s.func.as_str())
                .or_insert_with(|| {
                    order.push(s.func.as_str());
                    Vec::new()
                })
                .push(i);
        }
        let mut indices = Vec::new();
        for func in order {
            let mut chosen = self.pick_in_group(&groups[func], cap);
            chosen.sort_unstable();
            indices.extend(chosen);
        }
        Picked {
            indices,
            dropped_redline: self.dropped_redline,
        }
    }

    fn pick_in_group(&self, members: &[usize], cap: usize) -> Vec<usize> {
        if members.len() <= cap {
            return members.to_vec();
        }
        // 分桶；每个桶里按（输入大小，下标）从小到大
        let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for &i in members {
            buckets
                .entry(bucket_key(&self.samples[i]))
                .or_default()
                .push(i);
        }
        let mut ordered: Vec<(bool, usize, String, std::vec::IntoIter<usize>)> = buckets
            .into_iter()
            .map(|(key, mut idxs)| {
                idxs.sort_by_key(|&i| (self.input_bytes[i], i));
                let is_err = key.starts_with("err:");
                (is_err, self.input_bytes[idxs[0]], key, idxs.into_iter())
            })
            .collect();
        // 异常桶在前；其余按最小输入从小到大；再按键保证确定
        ordered.sort_by(|a, b| (!a.0, a.1, &a.2).cmp(&(!b.0, b.1, &b.2)));
        let mut chosen = Vec::with_capacity(cap);
        'rounds: loop {
            let mut progressed = false;
            for (_, _, _, rest) in &mut ordered {
                if let Some(i) = rest.next() {
                    chosen.push(i);
                    progressed = true;
                    if chosen.len() == cap {
                        break 'rounds;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        chosen
    }
}

/// 分桶键（函数之内）：异常用户的「异常类型 + 消息前缀」；正常返回的用输出的特征——
/// JSON 类型（int / float 分开）、布尔值、数的正负零、长度档（0 / 1 / 2–5 / 6+）、对象的键集合
/// （以及对象里布尔字段的取值，最多 6 个）。`$f` / `$b` / `$map` 编码按它们所代表的类型处理。
pub fn bucket_key(s: &Sample) -> String {
    match &s.err {
        Some((ty, msg)) => format!(
            "err:{ty}:{}",
            msg.chars().take(MSG_PREFIX_CHARS).collect::<String>()
        ),
        None => format!("ok:{}", shape(&s.out)),
    }
}

fn len_tier(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2..=5 => "2-5",
        _ => "6+",
    }
}

fn number_shape(n: &Number) -> String {
    if let Some(x) = n.as_f64().filter(|_| n.is_f64()) {
        let sign = if x == 0.0 {
            if x.is_sign_negative() { "-0" } else { "0" }
        } else if x < 0.0 {
            "neg"
        } else {
            "pos"
        };
        return format!("float:{sign}");
    }
    let sign = match n.as_i64() {
        Some(0) => "0",
        Some(i) if i < 0 => "neg",
        _ => "pos",
    };
    format!("int:{sign}")
}

fn shape(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) => number_shape(n),
        Value::String(s) => format!("str:{}", len_tier(s.chars().count())),
        Value::Array(a) => format!("arr:{}", len_tier(a.len())),
        Value::Object(m) => {
            if m.len() == 1 {
                if let Some(f) = m.get("$f") {
                    return format!("float:{}", f.as_str().unwrap_or("?"));
                }
                if let Some(b) = m.get("$b") {
                    return format!("bytes:{}", len_tier(b.as_str().map_or(0, str::len)));
                }
                if let Some(pairs) = m.get("$map") {
                    return format!("map:{}", len_tier(pairs.as_array().map_or(0, Vec::len)));
                }
            }
            let keys: BTreeSet<&str> = m.keys().map(String::as_str).collect();
            let bools: BTreeMap<&str, bool> = m
                .iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.as_str(), b)))
                .collect();
            let bools: Vec<String> = bools
                .into_iter()
                .take(6)
                .map(|(k, b)| format!("{k}={b}"))
                .collect();
            format!(
                "obj:{}:{{{}}}:{}",
                len_tier(m.len()),
                keys.into_iter().collect::<Vec<_>>().join(","),
                bools.join(",")
            )
        }
    }
}

// ───────────────────────── 轨迹类的贪心覆盖 ─────────────────────────

/// 轨迹类挑选：`kinds[i]` 是第 `i` 条轨迹里出现的操作种类（可以重复，按种类去重计数）。
/// 每一步挑出能让「还没凑够的种类」覆盖数增加最多的那条（同分取操作更少的、再取靠前的），
/// 直到每种操作至少出现在 `min_each` 条被选中的轨迹里（候选里本来就不够的种类按能凑到的算），或者已经选了 `max_cases` 条。
/// 返回被选中轨迹的下标（升序）。
pub fn greedy_cover<K: Ord + Clone>(
    kinds: &[Vec<K>],
    min_each: usize,
    max_cases: usize,
) -> Vec<usize> {
    let sets: Vec<BTreeSet<&K>> = kinds.iter().map(|k| k.iter().collect()).collect();
    let mut have: BTreeMap<&K, usize> = BTreeMap::new();
    let mut taken = vec![false; kinds.len()];
    let mut chosen = Vec::new();
    while chosen.len() < max_cases {
        let best = sets
            .iter()
            .enumerate()
            .filter(|(i, _)| !taken[*i])
            .map(|(i, set)| {
                let gain = set
                    .iter()
                    .filter(|k| have.get(**k).copied().unwrap_or(0) < min_each)
                    .count();
                (gain, i)
            })
            .filter(|&(gain, _)| gain > 0)
            .min_by_key(|&(gain, i)| (std::cmp::Reverse(gain), sets[i].len(), i));
        let Some((_, i)) = best else { break };
        taken[i] = true;
        chosen.push(i);
        for k in &sets[i] {
            *have.entry(*k).or_insert(0) += 1;
        }
    }
    chosen.sort_unstable();
    chosen
}

// ───────────────────────── 写出待填充的快照 ─────────────────────────

/// 写出 `tests/snapshots/<name>.jsonl`，每行 `{"in":<input>,"out":null}`（原子写，目录不存在会创建），
/// 接着用 `UPDATE_SNAPSHOT=1` 跑一次填充输出。
pub fn write_pending(manifest_dir: &str, name: &str, inputs: &[Value]) -> std::io::Result<PathBuf> {
    let mut text = String::new();
    for input in inputs {
        text.push_str(&render_line(input, &Value::Null));
        text.push('\n');
    }
    let target = path(manifest_dir, name);
    write_atomic(&target, &text)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Mode;
    use serde_json::json;

    // ── 红线规则 ──

    fn hit(text: &str) -> bool {
        redline_hit(text).is_some()
    }

    #[test]
    fn redline_discord_ids_follow_the_script() {
        // 命中：17–20 位、首位 1–8、加了引号
        assert!(hit(r#"{"id":"112233445566778899"}"#));
        assert!(hit(r#""18409589376783745023""#));
        // 放行：合成段、连着 6 个以上 0、顺序串、u64 边界
        assert!(!hit(r#""900000000000000042""#));
        assert!(!hit(r#""100000000000000001""#));
        assert!(!hit(r#""123456789012345678""#));
        assert!(!hit(r#""12345678901234567890""#));
        assert!(!hit(r#""18446744073709551615""#));
        assert!(!hit(r#""18446744073709551616""#));
        // 不是 17–20 位 / 没有引号 / 数字类型：不算
        assert!(!hit(r#""1122334455667788""#));
        assert!(!hit(r#""1122334455667788990011""#));
        assert!(!hit("112233445566778899"));
        assert!(!hit(r#"{"id":112233445566778899}"#));
    }

    #[test]
    fn redline_public_ips_follow_the_script() {
        assert!(hit(r#"{"ip":"8.8.4.4"}"#));
        assert!(hit("::100.55.1.2::"));
        assert!(hit("name123.55.1.2year"));
        assert!(hit("172.32.0.1"));
        // 众所周知的占位地址、私网、文档网段、回环
        for ok in [
            "1.2.3.4",
            "8.8.8.8",
            "9.9.9.9",
            "1.1.1.1",
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.0.1",
            "172.31.255.1",
            "192.0.2.7",
            "198.51.100.7",
            "203.0.113.9",
            "127.5.5.5",
            "0.1.2.3",
        ] {
            assert!(!hit(&format!(r#"{{"ip":"{ok}"}}"#)), "{ok}");
        }
        // 某段超过 255、版本号、更长的点分串里的片段
        assert!(!hit("256.1.1.1"));
        assert!(!hit("Chrome/129.0.0.0"));
        assert!(!hit("1.2.3.4.5"));
        assert!(!hit("1.2.3.4567"));
        assert!(!hit("version 1.2.3"));
        assert!(!hit("x.8.8.4.4"));
    }

    #[test]
    fn redline_upstream_tokens_follow_the_script() {
        let long = format!("pst-{}", "aB3_-".repeat(8));
        assert_eq!(long.len() - 4, 40);
        assert!(hit(&format!(r#"{{"k":"{long}"}}"#)));
        assert!(!hit(&format!("pst-{}", "a".repeat(39))));
        assert!(!hit("pst-short"));
    }

    #[test]
    fn redline_reports_what_it_found() {
        assert_eq!(
            redline_hit(r#""112233445566778899""#).unwrap(),
            "疑似真实 Discord ID 112233445566778899"
        );
        assert_eq!(redline_hit("8.8.4.4").unwrap(), "疑似公网 IP 8.8.4.4");
        assert_eq!(redline_hit("nothing here"), None);
    }

    #[test]
    fn samples_are_checked_on_input_output_and_error_text() {
        let clean = Sample::new("f", json!({"args": ["203.0.113.9"]}), json!(1));
        assert!(clean.redline_hit().is_none());
        let in_input = Sample::new("f", json!({"args": ["8.8.4.4"]}), json!(1));
        assert!(in_input.redline_hit().is_some());
        let in_output = Sample::new("f", json!({}), json!({"ip": "8.8.4.4"}));
        assert!(in_output.redline_hit().is_some());
        let in_err =
            Sample::new("f", json!({}), Value::Null).with_err("ValueError", "112233445566778899");
        assert!(in_err.redline_hit().is_some());
    }

    // ── 分桶键 ──

    fn ok(out: Value) -> Sample {
        Sample::new("f", json!({}), out)
    }

    #[test]
    fn bucket_key_separates_output_features() {
        let keys: Vec<String> = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(7),
            json!(-7),
            json!(0.0),
            json!(-0.0),
            json!(2.5),
            json!(-2.5),
            json!(""),
            json!("a"),
            json!("abc"),
            json!("abcdefg"),
            json!([]),
            json!([1]),
            json!([1, 2, 3]),
            json!([1, 2, 3, 4, 5, 6]),
            json!({"$f": "inf"}),
            json!({"$f": "nan"}),
            json!({"$b": ""}),
            json!({"$map": [[1, 2]]}),
            json!({"a": 1}),
            json!({"a": 1, "b": 2}),
            json!({"b": 2, "a": 1, "ok": true}),
            json!({"b": 2, "a": 1, "ok": false}),
        ]
        .into_iter()
        .map(|v| bucket_key(&ok(v)))
        .collect();
        let distinct: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(distinct.len(), keys.len(), "{keys:#?}");
    }

    #[test]
    fn bucket_key_ignores_what_it_should_not_see() {
        // 同一个键集合、同一档长度、同一个符号 → 同一个桶（键的写入顺序、具体数值、具体字符都不影响）
        assert_eq!(
            bucket_key(&ok(json!({"a": 1, "b": 2}))),
            bucket_key(&ok(json!({"b": 9, "a": 8})))
        );
        assert_eq!(bucket_key(&ok(json!(3))), bucket_key(&ok(json!(99))));
        assert_eq!(bucket_key(&ok(json!(3.5))), bucket_key(&ok(json!(0.25))));
        assert_eq!(
            bucket_key(&ok(json!("ab"))),
            bucket_key(&ok(json!("猫猫猫")))
        );
        assert_eq!(bucket_key(&ok(json!("a"))), bucket_key(&ok(json!("猫"))));
        assert_eq!(
            bucket_key(&ok(json!([1, 2]))),
            bucket_key(&ok(json!(["x", "y", "z"])))
        );
    }

    #[test]
    fn bucket_key_uses_error_type_and_message_prefix() {
        let e = |ty: &str, msg: &str| Sample::new("f", json!({}), Value::Null).with_err(ty, msg);
        assert_ne!(
            bucket_key(&e("ValueError", "x")),
            bucket_key(&e("KeyError", "x"))
        );
        assert_ne!(
            bucket_key(&e("ValueError", "不支持 NaN")),
            bucket_key(&e("ValueError", "不支持 Infinity"))
        );
        // 前缀之后的部分不影响（常常是变量）
        let long = "x".repeat(MSG_PREFIX_CHARS);
        assert_eq!(
            bucket_key(&e("ValueError", &format!("{long}1"))),
            bucket_key(&e("ValueError", &format!("{long}2")))
        );
        assert_ne!(
            bucket_key(&e("ValueError", "x")),
            bucket_key(&ok(Value::Null))
        );
    }

    // ── 抽样 ──

    /// `n` 条用例，输入大小不同，输出在 3 种形状之间轮换。
    fn many(func: &str, n: usize) -> Vec<Sample> {
        (0..n)
            .map(|i| {
                let out = match i % 3 {
                    0 => json!(i),
                    1 => json!("s".repeat(i % 7)),
                    _ => json!(i % 2 == 0),
                };
                Sample::new(func, json!({"args": ["x".repeat(n - i)]}), out)
            })
            .collect()
    }

    #[test]
    fn small_groups_are_kept_whole() {
        let samples = many("f", 10);
        let p = sample_per_fn(&samples, 100);
        assert_eq!(p.indices, (0..10).collect::<Vec<_>>());
        assert_eq!(p.dropped_redline, 0);
    }

    #[test]
    fn cap_applies_per_function_and_order_is_grouped_then_original() {
        let mut samples = many("b", 30);
        samples.extend(many("a", 30));
        samples.extend(many("b", 30)); // b 的后一半，混在 a 后面
        let p = sample_per_fn(&samples, 10);
        assert_eq!(p.indices.len(), 20);
        let funcs: Vec<&str> = p
            .indices
            .iter()
            .map(|&i| samples[i].func.as_str())
            .collect();
        // 先 b 后 a（首次出现的顺序），各自连成一段
        assert_eq!(&funcs[..10], &["b"; 10]);
        assert_eq!(&funcs[10..], &["a"; 10]);
        // 组内保持原来的顺序
        for chunk in p.indices.chunks(10) {
            assert!(chunk.windows(2).all(|w| w[0] < w[1]), "{chunk:?}");
        }
    }

    #[test]
    fn sampling_is_deterministic() {
        let samples = many("f", 200);
        assert_eq!(sample_per_fn(&samples, 20), sample_per_fn(&samples, 20));
    }

    #[test]
    fn buckets_are_visited_in_turn_and_each_takes_its_smallest_input_first() {
        // 3 个桶：整数 / 字符串 / 布尔；每个桶 4 条，输入越靠后越小
        let mut samples = Vec::new();
        for (kind, out) in [
            ("int", json!(1)),
            ("str", json!("abc")),
            ("bool", json!(true)),
        ] {
            for i in 0..4 {
                samples.push(Sample::new(
                    "f",
                    json!({"kind": kind, "pad": "p".repeat(10 - i)}),
                    out.clone(),
                ));
            }
        }
        let p = sample_per_fn(&samples, 6);
        assert_eq!(p.indices.len(), 6);
        // 每个桶恰好 2 条（轮转），且是桶里输入最小的 2 条（下标 2、3 / 6、7 / 10、11 → 最后两个）
        let mut per_kind: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for &i in &p.indices {
            per_kind
                .entry(samples[i].input["kind"].as_str().unwrap())
                .or_default()
                .push(i % 4);
        }
        for (kind, taken) in per_kind {
            assert_eq!(taken, vec![2, 3], "{kind}");
        }
    }

    #[test]
    fn every_error_kind_survives_a_tight_cap() {
        let mut samples = many("f", 60);
        for (ty, msg) in [
            ("ValueError", "bad a"),
            ("KeyError", "k"),
            ("TypeError", "t"),
        ] {
            // 异常用例的输入很大，按「输入最小优先」本来会被挤掉
            samples.push(
                Sample::new("f", json!({"args": ["z".repeat(500)]}), Value::Null).with_err(ty, msg),
            );
        }
        let p = sample_per_fn(&samples, 8);
        let kept_errs: BTreeSet<&str> = p
            .indices
            .iter()
            .filter_map(|&i| samples[i].err.as_ref().map(|(t, _)| t.as_str()))
            .collect();
        assert_eq!(
            kept_errs,
            BTreeSet::from(["ValueError", "KeyError", "TypeError"])
        );
        assert_eq!(p.indices.len(), 8);
    }

    #[test]
    fn redline_hits_are_dropped_and_counted() {
        let mut samples = many("f", 5);
        samples.push(Sample::new("f", json!({"args": ["8.8.4.4"]}), json!(1)));
        samples.push(Sample::new(
            "f",
            json!({"args": ["x"]}),
            json!("112233445566778899"),
        ));
        // 红线用例不算在「函数内用例数」里
        let p = sample_per_fn(&samples, 100);
        assert_eq!(p.indices, (0..5).collect::<Vec<_>>());
        assert_eq!(p.dropped_redline, 2);
    }

    #[test]
    fn from_case_strips_old_fields_and_keeps_the_input_shape() {
        let case = Case {
            line: 1,
            raw: json!({
                "fn": "clamp", "args": [1, 2], "kwargs": {"k": 3},
                "out": 5, "after": {"x": 1}, "note": "n"
            }),
        };
        let s = Sample::from_case(&case);
        assert_eq!(s.func, "clamp");
        assert_eq!(
            s.input,
            json!({"fn": "clamp", "args": [1, 2], "kwargs": {"k": 3}})
        );
        assert_eq!(s.out, json!(5));
        assert_eq!(s.err, None);

        let case = Case {
            line: 2,
            raw: json!({"fn": "p", "args": [], "out": null, "err": {"type": "ValueError", "msg": "boom"}}),
        };
        let s = Sample::from_case(&case);
        assert_eq!(s.err, Some(("ValueError".to_string(), "boom".to_string())));
        assert_eq!(s.input, json!({"fn": "p", "args": []}));
    }

    #[test]
    fn budget_lowers_the_per_function_cap() {
        let samples = many("f", 300);
        let (all, cap) = sample_within_budget(&samples, 100, usize::MAX);
        assert_eq!((all.indices.len(), cap), (100, 100));
        let total: usize = all
            .indices
            .iter()
            .map(|&i| render_line(&samples[i].input, &samples[i].out).len() + 1)
            .sum();
        let budget = total / 2;
        let (fit, cap) = sample_within_budget(&samples, 100, budget);
        assert!((1..100).contains(&cap));
        assert_eq!(fit.indices.len(), cap);
        let bytes: usize = fit
            .indices
            .iter()
            .map(|&i| render_line(&samples[i].input, &samples[i].out).len() + 1)
            .sum();
        assert!(bytes <= budget, "{bytes} > {budget}");
        // 预算小到一条都放不下：降到 1 为止，不死循环
        let (one, cap) = sample_within_budget(&samples, 100, 1);
        assert_eq!((one.indices.len(), cap), (1, 1));
    }

    // ── 贪心覆盖 ──

    #[test]
    fn greedy_cover_stops_when_every_kind_appears_three_times() {
        // 10 条轨迹：偶数条有 a、b；每 3 条有 c；第 9 条独有 d
        let kinds: Vec<Vec<&str>> = (0..10)
            .map(|i| {
                let mut v = Vec::new();
                if i % 2 == 0 {
                    v.extend(["a", "b", "a"]);
                }
                if i % 3 == 0 {
                    v.push("c");
                }
                if i == 9 {
                    v.push("d");
                }
                v
            })
            .collect();
        let chosen = greedy_cover(&kinds, 3, 20);
        let count = |k: &str| chosen.iter().filter(|&&i| kinds[i].contains(&k)).count();
        assert!(count("a") >= 3 && count("b") >= 3 && count("c") >= 3);
        // d 只在 1 条里，能凑到的就是 1 条，不影响收尾
        assert_eq!(count("d"), 1);
        // 选够就停：不会把 10 条全选
        assert!(chosen.len() < 10, "{chosen:?}");
        assert!(chosen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn greedy_cover_prefers_the_trace_with_most_new_kinds() {
        let kinds = vec![vec!["a"], vec!["a", "b", "c"], vec!["b"], vec!["a", "b"]];
        let chosen = greedy_cover(&kinds, 1, 20);
        // 第一步一定先选 1（覆盖 3 种），之后全部凑够，不再选别的
        assert_eq!(chosen, vec![1]);
        // 要求出现 2 次时：1 号加上覆盖多的 3 号
        assert_eq!(greedy_cover(&kinds, 2, 20), vec![1, 3]);
    }

    #[test]
    fn greedy_cover_respects_the_max_and_ties_go_to_shorter_then_earlier() {
        let kinds: Vec<Vec<u8>> = (0..30).map(|i| vec![(i % 5) as u8]).collect();
        assert_eq!(greedy_cover(&kinds, 100, 7).len(), 7);
        // 第一步选覆盖最多的 0 号；之后 1、2、3 号各只新增一种，取操作更少的 2 号（和 3 号同分，靠前）
        let kinds = vec![vec!["k", "m", "n"], vec!["a", "k"], vec!["a"], vec!["a"]];
        assert_eq!(greedy_cover(&kinds, 1, 2), vec![0, 2]);
        // 空输入、0 要求
        assert!(greedy_cover::<u8>(&[], 3, 20).is_empty());
        assert!(greedy_cover(&[vec![1u8]], 0, 20).is_empty());
        assert!(greedy_cover(&[vec![1u8]], 3, 0).is_empty());
    }

    // ── 写出待填充的快照 ──

    #[test]
    fn write_pending_writes_null_outs_that_the_harness_can_fill() {
        let root =
            std::env::temp_dir().join(format!("gate-snapshot-convert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let manifest = root.join("gate-demo");
        let manifest = manifest.to_string_lossy().into_owned();
        let inputs = vec![
            json!({"fn": "f", "args": [1]}),
            json!({"fn": "f", "args": ["猫"]}),
        ];
        let file = write_pending(&manifest, "demo/pending", &inputs).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"in\":{\"fn\":\"f\",\"args\":[1]},\"out\":null}\n{\"in\":{\"fn\":\"f\",\"args\":[\"猫\"]},\"out\":null}\n"
        );
        // 接着用更新模式填充，再用默认模式确认
        let f = |c: &Case| Some(json!({"echo": c.arg(0).clone()}));
        let filled = crate::snapshot::run(&manifest, "demo/pending", Mode::Update, f)
            .unwrap()
            .unwrap();
        assert_eq!((filled.changed, filled.deleted, filled.total), (2, 0, 2));
        crate::snapshot::run(&manifest, "demo/pending", Mode::Check, f).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
