//! 文案核对：Rust 源码里每一句面向成员的中文，都必须逐字出现在旧 Python 的字符串常量里。
//!
//! 「旧 Python 的字符串常量」是 `tools/parity/golden/discord_commands.py` 用 `ast` 从
//! `integration/discord_bot.py` / `discord_registration.py` / `warden_bot.py` 抽出来存进夹具的
//! （含 f-string 里的固定片段），旧代码删除后这个测试仍然有效。
//!
//! 做法：扫描 `handlers.rs`（全部回复文案）与 `commands.rs`（命令名 / 描述）里**非测试代码**的字符串字面量，
//! 按 `{}` 占位符切成片段，凡含中文 / 表情的片段都要在夹具里找到。内部诊断信息一律用英文，不受此限。

use std::collections::HashSet;

use gate_discord::handlers;

/// 3.0 新写的面向成员的文案，不在旧 Python 里：`/反馈` 改成一段文字（docs/v3-deviations.md #52）。
/// 冻结夹具不改，这几句在这里补进「允许出现」的集合；旧版 `/反馈` 的文案（选择题弹窗标题等）已不再出现在源码里。
const V3_COPY: &[&str] = &[
    // 命令描述
    "写几句话给站长（只有你能看到，不会私信你）",
    // 弹窗：标题、Label、占位符
    "猫头鹰公益站 · 反馈",
    "想对站长说的话",
    "遇到的问题、建议都可以写。只有站长能看到，回执只有你自己可见。",
    // /help 的命令清单（`/反馈` 的说明换了）
    "`/register` 领取 Key（结果仅你可见，含配置教程）· `/quota` 额度和服务状态 · `/resetkey` 换新 Key · `/反馈` 写几句话给站长",
];

fn python_strings() -> HashSet<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/discord_copy.json"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["strings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .chain(V3_COPY.iter().map(|s| (*s).to_owned()))
        .collect()
}

/// 极简 Rust 词法扫描：返回源码里所有字符串字面量的内容（已反转义），跳过注释、字符字面量。
fn string_literals(src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == 'r' && matches!(next, Some('"' | '#')) {
            // 原始字符串 r"..." / r#"..."#
            let mut j = i + 1;
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) != Some(&'"') {
                i += 1;
                continue;
            }
            j += 1;
            let start = j;
            loop {
                if chars[j] == '"' && (0..hashes).all(|k| chars.get(j + 1 + k) == Some(&'#')) {
                    break;
                }
                j += 1;
            }
            out.push(chars[start..j].iter().collect());
            i = j + 1 + hashes;
        } else if c == '"' {
            let mut j = i + 1;
            let mut s = String::new();
            while chars[j] != '"' {
                if chars[j] == '\\' {
                    j += 1;
                    match chars[j] {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        '0' => s.push('\0'),
                        '\\' => s.push('\\'),
                        '"' => s.push('"'),
                        '\'' => s.push('\''),
                        '\n' => {
                            // 行尾续行：吞掉换行与下一行的缩进
                            while chars.get(j + 1).is_some_and(|c| c.is_whitespace()) {
                                j += 1;
                            }
                        }
                        'u' => {
                            // \u{XXXX}
                            let end = chars[j..].iter().position(|&c| c == '}').unwrap() + j;
                            let hex: String = chars[j + 2..end].iter().collect();
                            s.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
                            j = end;
                        }
                        other => panic!("测试扫描器不认识的转义 \\{other}"),
                    }
                } else {
                    s.push(chars[j]);
                }
                j += 1;
            }
            out.push(s);
            i = j + 1;
        } else if c == '\'' {
            // 字符字面量 'x' / '\n' 与生命周期 'a：字符字面量的第三个字符（或转义后）是 '
            if next == Some('\\') {
                i += 2;
                while chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if chars.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 去掉 `#[cfg(test)]` 之后的测试模块。
fn production_part(src: &str) -> &str {
    src.split("#[cfg(test)]").next().unwrap()
}

/// `format!` 模板按 `{...}` 占位符切片（`{{` / `}}` 是转义的花括号）。
fn fragments(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1)) {
            ('{', Some('{')) => {
                cur.push('{');
                i += 2;
            }
            ('}', Some('}')) => {
                cur.push('}');
                i += 2;
            }
            ('{', _) => {
                out.push(std::mem::take(&mut cur));
                while chars[i] != '}' {
                    i += 1;
                }
                i += 1;
            }
            (c, _) => {
                cur.push(c);
                i += 1;
            }
        }
    }
    out.push(cur);
    out
}

fn has_cjk_or_emoji(s: &str) -> bool {
    s.chars().any(
        |c| !c.is_ascii() && c != '\u{2026}', /* … 是 discord.py 的占位符，见下 */
    )
}

/// discord.py 在没写 `describe` 的参数上自动填的占位描述，不在 Python 源码里。
const DISCORDPY_PLACEHOLDER: &str = "\u{2026}";

fn check_source(
    name: &str,
    src: &str,
    python: &HashSet<String>,
    missing: &mut Vec<String>,
) -> usize {
    let mut checked = 0;
    for lit in string_literals(production_part(src)) {
        for frag in fragments(&lit) {
            if frag == DISCORDPY_PLACEHOLDER || !has_cjk_or_emoji(&frag) {
                continue;
            }
            checked += 1;
            if !python.contains(&frag) {
                missing.push(format!("{name}: {frag:?}"));
            }
        }
    }
    checked
}

#[test]
fn user_visible_copy_exists_verbatim_in_python() {
    let python = python_strings();
    let mut missing = Vec::new();
    let mut checked = 0;
    checked += check_source(
        "handlers.rs",
        include_str!("../src/handlers.rs"),
        &python,
        &mut missing,
    );
    checked += check_source(
        "commands.rs",
        include_str!("../src/commands.rs"),
        &python,
        &mut missing,
    );
    assert!(
        checked > 60,
        "扫描到的中文片段太少（{checked}），扫描器可能坏了"
    );
    assert!(
        missing.is_empty(),
        "这些文案在 Python 源码里找不到：\n{}",
        missing.join("\n")
    );
}

#[test]
fn whole_sentence_constants_exist_in_python() {
    let python = python_strings();
    for s in handlers::ALL_COPY {
        assert!(python.contains(*s), "常量不在 Python 源码里：{s}");
    }
    // 燕双鹰的活动名；@奶妹回复的固定片段（f-string 切片）
    assert!(python.contains(gate_discord::warden::ACTIVITY_NAME));
    assert_eq!(
        handlers::mention_reply_text(900000000000000002),
        "我在床上陪主人呢 <@900000000000000002>"
    );
}

#[test]
fn scanner_handles_common_syntax() {
    let src = r##"
        // "注释里的字符串不算"
        let a = "你好\n{x}"; /* "块注释" */
        let c = 'x'; let d = '\n'; fn f<'a>(s: &'a str) {}
        let r = r#"原始 "引号" 字符串"#;
        let t = "续行\
                 接上";
    "##;
    let lits = string_literals(src);
    assert_eq!(lits, ["你好\n{x}", "原始 \"引号\" 字符串", "续行接上"]);
    assert_eq!(
        fragments("今日：{} / {}（{{x}}）"),
        ["今日：", " / ", "（{x}）"]
    );
}
