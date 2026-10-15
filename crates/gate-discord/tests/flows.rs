//! 与旧 Python 逐场景对拍：同样的交互 / 消息 / 网关脚本进来，
//! 发给网关桥接的请求、发给 Discord 的请求（路径 / 查询 / JSON 逐键逐序）必须和 discord.py 实际发出的一致。
//!
//! 夹具 `discord_flows.jsonl.gz` 由 `tools/parity/golden/discord_commands.py` 在旧代码上生成：
//! 真实的 discord.py `CommandTree` + 真实的 `Interaction` + 旧的 `backend()` / `handle_register()`，
//! 底层网络换成假的并逐条记录。这里把同样的输入喂给 Rust 版，底层网络换成 `common::MockServer`。

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{
    BOT_ID, GUILD, MockServer, NOW, OWNER, SECRET, SITE, deep_replace, modal_placeholders,
};
use gate_discord::bridge::{COMMAND_TIMEOUT, REGISTER_TIMEOUT};
use gate_discord::config::NaimeiConfig;
use gate_discord::events::{GuildInfo, Interaction, MessageCreate, User};
use gate_discord::naimei::Naimei;
use serde_json::Value;

fn naimei(server: &MockServer, secret: Option<&str>) -> Arc<Naimei> {
    let cfg = NaimeiConfig {
        token: "test-token".into(),
        guild_id: GUILD.parse().unwrap(),
        backend: server.backend_url(),
        site: SITE.into(),
        secret: secret.map(str::to_owned),
    };
    let app = Naimei::with_clock(cfg, Some(server.discord_proxy()), Box::new(|| NOW));
    app.set_bot_user(User {
        id: BOT_ID.into(),
        username: "naimei".into(),
        discriminator: Some("0".into()),
        ..User::default()
    });
    app.upsert_guild(GuildInfo {
        id: GUILD.into(),
        name: Some("测试服".into()),
        owner_id: Some(OWNER.into()),
        unavailable: false,
    });
    app
}

fn secret_for(cfg: &Value) -> Option<&'static str> {
    match cfg["secret"].as_str().unwrap_or("set") {
        "unset" => None,
        "empty" => Some(""),
        _ => Some(SECRET),
    }
}

/// Discord 请求：只留夹具里有的键，查询值按 discord.py 的写法（1 / 0）统一，消息随机 nonce 换占位符。
fn normalize_discord(calls: Vec<Value>, placeholders: &HashMap<String, String>) -> Vec<Value> {
    calls
        .into_iter()
        .map(|c| {
            let mut query = serde_json::Map::new();
            for (k, v) in c["query"].as_object().cloned().unwrap_or_default() {
                let v = match v.as_str() {
                    Some("true") => "1".to_owned(),
                    Some("false") => "0".to_owned(),
                    Some(other) => other.to_owned(),
                    None => String::new(),
                };
                query.insert(k, Value::String(v));
            }
            let mut body = c["json"].clone();
            if body.get("nonce").is_some() {
                body["nonce"] = Value::String("<nonce>".into());
            }
            deep_replace(&mut body, placeholders);
            serde_json::json!({"method": c["method"], "path": c["path"], "query": query, "json": body})
        })
        .collect()
}

/// 桥接请求：去掉 Rust 侧多出来的 content_type，并校验它是 JSON（有请求体时）。
/// 夹具里的 `timeout` 另行校验（见 `golden_timeouts_match_constants`）。
fn normalize_bridge(calls: Vec<Value>) -> Vec<Value> {
    calls
        .into_iter()
        .map(|c| {
            if !c["json"].is_null() {
                assert_eq!(c["content_type"], "application/json", "{c}");
            }
            serde_json::json!({
                "method": c["method"], "path": c["path"], "authorization": c["authorization"], "json": c["json"],
            })
        })
        .collect()
}

fn strip_timeout(calls: &Value) -> Vec<Value> {
    calls
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c.as_object_mut().unwrap().remove("timeout");
            c
        })
        .collect()
}

fn compare(label: &str, actual: &[Value], expected: &[Value], failures: &mut Vec<String>) {
    let a = Value::Array(actual.to_vec()).to_string();
    let e = Value::Array(expected.to_vec()).to_string();
    if a != e {
        failures.push(format!("{label}\n  实际: {a}\n  期望: {e}"));
    }
}

/// `/help` 里 `/反馈` 的说明：冻结夹具是旧版原文，3.0 改成「写几句话给站长」。
const LEGACY_HELP_FEEDBACK: &str = "`/反馈` 给站长提意见";
const V3_HELP_FEEDBACK: &str = "`/反馈` 写几句话给站长";

/// 把期望里的旧版 `/help` 文案换成 3.0 的（docs/v3-deviations.md #52）。
fn v3_help_copy(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.contains(LEGACY_HELP_FEEDBACK) {
                *s = s.replace(LEGACY_HELP_FEEDBACK, V3_HELP_FEEDBACK);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(v3_help_copy),
        Value::Object(o) => o.values_mut().for_each(v3_help_copy),
        _ => {}
    }
}

/// 心跳场景里，旧版每分钟先 `GET /self-register/bot/config` 拉「跑图分享设置」，再上报带 `ai_ready` / `ai_today` 的心跳。
/// 设置和 AI 评论 3.0 都删了（docs/v3-deviations.md #54）：期望里去掉那次 GET、心跳里的两个字段和 `config_after`。
const REMOVED_BOT_CONFIG_PATH: &str = "/self-register/bot/config";
const REMOVED_STATUS_FIELDS: [&str; 2] = ["ai_ready", "ai_today"];

/// 只测「拉到的配置怎么处理」的心跳场景（后台返回 500 / 不是对象）：配置没了，场景也就没有意义，滤掉。
/// `sync_config_connect_error` 同时让上报失败，上报失败不报错这一半仍然成立，保留。
const REMOVED_SYNC_SCENARIOS: [&str; 2] = ["sync_config_unavailable", "sync_config_not_object"];

fn v3_sync_expectation(sc: &mut Value) {
    let expect = sc["expect"].as_object_mut().unwrap();
    expect.remove("config_after");
    let requests = expect
        .get_mut("bridge_requests")
        .and_then(Value::as_array_mut)
        .unwrap();
    requests.retain(|r| r["path"] != REMOVED_BOT_CONFIG_PATH);
    for r in requests {
        if let Some(status) = r["json"]["status"].as_object_mut() {
            for field in REMOVED_STATUS_FIELDS {
                status.remove(field);
            }
        }
    }
}

/// 冻结的 Python 夹具，换成 3.0 的期望（夹具本身不改）：
/// - `feedback_*` 场景是旧版「先向网关取 5 道题、再弹出 5 个下拉框，提交 `answers`」的流程；`/反馈` 在 3.0 改成
///   一段文字（docs/v3-deviations.md #52），这些场景整组不再适用，在这里滤掉。新流程见 `tests/feedback_text.rs`；
/// - `/help` 的命令清单里 `/反馈` 的说明换了，期望里按 [`v3_help_copy`] 的规则换掉再比；
/// - 心跳场景按 [`v3_sync_expectation`] 去掉拉配置和 AI 字段（docs/v3-deviations.md #54）。
fn scenarios() -> Vec<Value> {
    gate_golden::load(env!("CARGO_MANIFEST_DIR"), "discord_flows")
        .into_iter()
        .map(|c| c.raw)
        .filter(|sc| {
            let name = sc["name"].as_str().unwrap_or_default();
            !name.starts_with("feedback_") && !REMOVED_SYNC_SCENARIOS.contains(&name)
        })
        .map(|mut sc| {
            v3_help_copy(&mut sc);
            if sc.get("ready_at").is_some() {
                v3_sync_expectation(&mut sc);
            }
            sc
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_and_modals_replay_like_discordpy() {
    let server = MockServer::start().await;
    let mut failures = Vec::new();
    let mut count = 0;
    for sc in scenarios().into_iter().filter(|s| s.get("steps").is_some()) {
        let name = sc["name"].as_str().unwrap().to_owned();
        let app = naimei(&server, secret_for(&sc["cfg"]));
        // 占位符 → 本次 Rust 生成的真实 custom_id（弹窗每次随机）
        let mut real_ids: HashMap<String, String> = HashMap::new();
        for (i, step) in sc["steps"].as_array().unwrap().iter().enumerate() {
            count += 1;
            server.reset(&step["bridge"], &step["discord"]);
            let mut interaction = step["interaction"].clone();
            deep_replace(&mut interaction, &real_ids);
            let it: Interaction = serde_json::from_value(interaction).unwrap();
            app.handle_interaction(it).await;

            let discord = server.discord_log();
            let fresh = modal_placeholders(&discord);
            for (real, ph) in &fresh {
                real_ids.insert(ph.clone(), real.clone());
            }
            let actual_discord = normalize_discord(discord, &fresh);
            let actual_bridge = normalize_bridge(server.bridge_log());
            let label = format!("[{name}] 第 {} 步", i + 1);
            compare(
                &format!("{label} 桥接请求"),
                &actual_bridge,
                &strip_timeout(&step["expect"]["bridge_requests"]),
                &mut failures,
            );
            let expected_discord: Vec<Value> =
                step["expect"]["discord_calls"].as_array().unwrap().clone();
            compare(
                &format!("{label} Discord 请求"),
                &actual_discord,
                &expected_discord,
                &mut failures,
            );
        }
    }
    // 夹具共 108 步，滤掉 `feedback_*` 的 32 步（见 `scenarios`）后剩 76
    assert!(count > 70, "场景数量异常：{count}");
    assert!(
        failures.is_empty(),
        "{} 处不一致：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mentions_replay_like_discordpy() {
    let server = MockServer::start().await;
    let mut failures = Vec::new();
    let mut count = 0;
    for sc in scenarios()
        .into_iter()
        .filter(|s| s.get("events").is_some())
    {
        let name = sc["name"].as_str().unwrap().to_owned();
        let app = naimei(&server, Some(SECRET)); // 每个场景一份全新的限流表
        for (i, ev) in sc["events"].as_array().unwrap().iter().enumerate() {
            count += 1;
            server.reset(&Value::Null, &ev["discord"]);
            let msg: MessageCreate = serde_json::from_value(ev["message"].clone()).unwrap();
            app.handle_message_at(msg, ev["mono"].as_f64().unwrap())
                .await;
            let actual = normalize_discord(server.discord_log(), &HashMap::new());
            let expected = ev["expect"]["discord_calls"].as_array().unwrap().clone();
            compare(
                &format!("[{name}] 第 {} 条消息", i + 1),
                &actual,
                &expected,
                &mut failures,
            );
        }
    }
    assert!(count >= 10, "场景数量异常：{count}");
    assert!(
        failures.is_empty(),
        "{} 处不一致：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_loop_iteration_replays_like_discordpy() {
    let server = MockServer::start().await;
    let mut failures = Vec::new();
    let mut count = 0;
    for sc in scenarios()
        .into_iter()
        .filter(|s| s.get("ready_at").is_some())
    {
        count += 1;
        let name = sc["name"].as_str().unwrap().to_owned();
        let app = naimei(&server, Some(SECRET));
        // 夹具里服务器可能不在缓存里（guild_name 为 null）
        if sc["guild_name"].is_null() {
            app.remove_guild(GUILD);
        }
        app.set_latency(Some(Duration::from_secs_f64(
            sc["latency_s"].as_f64().unwrap(),
        )));
        server.reset(&sc["bridge"], &Value::Null);
        app.sync_once(sc["ready_at"].as_f64().unwrap())
            .await
            .unwrap();

        compare(
            &format!("[{name}] 桥接请求"),
            &normalize_bridge(server.bridge_log()),
            &strip_timeout(&sc["expect"]["bridge_requests"]),
            &mut failures,
        );
        assert!(
            server.discord_log().is_empty(),
            "[{name}] 同步循环不应该调用 Discord"
        );
    }
    // 夹具共 5 个心跳场景，滤掉 `REMOVED_SYNC_SCENARIOS` 的 2 个后剩 3
    assert_eq!(count, 3, "场景数量异常：{count}");
    assert!(
        failures.is_empty(),
        "{} 处不一致：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 夹具记录了 Python 的 httpx 超时：命令类 8 秒、/register 12 秒、配置 / 上报 8 秒。
#[test]
fn golden_timeouts_match_constants() {
    let mut seen_issue = false;
    let mut seen_other = false;
    let check = |calls: &Value| {
        for c in calls.as_array().unwrap() {
            let secs = c["timeout"].as_f64().unwrap();
            if c["path"] == "/self-register/issue" {
                assert_eq!(secs, REGISTER_TIMEOUT.as_secs_f64());
            } else {
                assert_eq!(secs, COMMAND_TIMEOUT.as_secs_f64(), "{c}");
            }
        }
    };
    for sc in scenarios() {
        for step in sc["steps"].as_array().cloned().unwrap_or_default() {
            check(&step["expect"]["bridge_requests"]);
            for c in step["expect"]["bridge_requests"].as_array().unwrap() {
                if c["path"] == "/self-register/issue" {
                    seen_issue = true;
                } else {
                    seen_other = true;
                }
            }
        }
        if sc.get("ready_at").is_some() {
            check(&sc["expect"]["bridge_requests"]);
        }
    }
    assert!(seen_issue && seen_other);
}
