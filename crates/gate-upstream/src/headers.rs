//! 发往 NovelAI 的请求头：集合、取值、顺序与旧版 Python（httpx 0.28）逐项一致。
//!
//! 旧版实测线上顺序（HTTP/1.1 抓包，见 tests/headers_wire.rs）：
//! `Host, Accept-Encoding, Connection, Accept, User-Agent, Origin, Referer, Sec-Ch-Ua, Sec-Ch-Ua-Mobile,
//!  Sec-Ch-Ua-Platform, Sec-Fetch-Dest, Sec-Fetch-Mode, Sec-Fetch-Site, Accept-Language, Priority,
//!  Authorization, [Content-Type, Content-Length]`
//! ——前三个是 httpx 的默认头（`Accept` 被我们的值覆盖后挪到了它们后面），其余是我们按 dict 顺序给的。
//! HTTP/2 下名字小写、`Connection` 由协议层去掉，其余顺序不变。
//!
//! TLS / HTTP2 指纹由 wreq 的 Chrome 模拟提供（站长 10-14 批准），这里只管头。

use wreq::header::{HeaderMap, HeaderName, HeaderValue, OrigHeaderMap};

/// 浏览器指纹池：按上游 Token 的序号轮流分配（与旧版 `BROWSER_PROFILES` 相同）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfile {
    pub user_agent: String,
    pub sec_ch_ua: &'static str,
    pub sec_ch_ua_mobile: &'static str,
    pub sec_ch_ua_platform: &'static str,
    /// 对应的 wreq Chrome 版本（TLS / HTTP2 指纹）。
    pub chrome: wreq_util::Profile,
    pub platform: wreq_util::Platform,
}

pub fn browser_profiles() -> [BrowserProfile; 3] {
    [
        BrowserProfile {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36".into(),
            sec_ch_ua: r#""Google Chrome";v="129", "Not=A?Brand";v="8", "Chromium";v="129""#,
            sec_ch_ua_mobile: "?0",
            sec_ch_ua_platform: r#""Windows""#,
            chrome: wreq_util::Profile::Chrome129,
            platform: wreq_util::Platform::Windows,
        },
        BrowserProfile {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36".into(),
            sec_ch_ua: r#""Chromium";v="128", "Not;A=Brand";v="24", "Google Chrome";v="128""#,
            sec_ch_ua_mobile: "?0",
            sec_ch_ua_platform: r#""Windows""#,
            chrome: wreq_util::Profile::Chrome128,
            platform: wreq_util::Platform::Windows,
        },
        BrowserProfile {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36".into(),
            sec_ch_ua: r#""Google Chrome";v="129", "Not=A?Brand";v="8", "Chromium";v="129""#,
            sec_ch_ua_mobile: "?0",
            sec_ch_ua_platform: r#""macOS""#,
            chrome: wreq_util::Profile::Chrome129,
            platform: wreq_util::Platform::MacOS,
        },
    ]
}

/// 第 `index` 把 Token 的指纹（`index % 3`），可选覆盖 UA（`UPSTREAM_USER_AGENT`）。
pub fn profile_for(index: usize, custom_user_agent: Option<&str>) -> BrowserProfile {
    let mut p = browser_profiles()[index % 3].clone();
    if let Some(ua) = custom_user_agent.filter(|s| !s.is_empty()) {
        p.user_agent = ua.to_string();
    }
    p
}

/// 一次请求的有序头列表（不含 Host / Content-Length，这两个由协议层按位置补上）。
/// `has_body`：POST 等带 JSON 体的请求加 `Content-Type: application/json`；GET / HEAD 不加——
/// 官方前端不会给 suggest-tags 这类查询带它，多余的头是现成的指纹差异。
pub fn ordered(
    profile: &BrowserProfile,
    token: Option<&str>,
    accept: &str,
    has_body: bool,
) -> Vec<(&'static str, String)> {
    let mut h: Vec<(&'static str, String)> = vec![
        ("Accept-Encoding", "gzip, deflate".into()),
        ("Connection", "keep-alive".into()),
        ("Accept", accept.into()),
        ("User-Agent", profile.user_agent.clone()),
        ("Origin", "https://novelai.net".into()),
        ("Referer", "https://novelai.net/".into()),
        ("Sec-Ch-Ua", profile.sec_ch_ua.into()),
        ("Sec-Ch-Ua-Mobile", profile.sec_ch_ua_mobile.into()),
        ("Sec-Ch-Ua-Platform", profile.sec_ch_ua_platform.into()),
        ("Sec-Fetch-Dest", "empty".into()),
        ("Sec-Fetch-Mode", "cors".into()),
        ("Sec-Fetch-Site", "same-site".into()),
        ("Accept-Language", "en-US,en;q=0.9".into()),
        ("Priority", "u=1, i".into()),
    ];
    if let Some(t) = token {
        h.push(("Authorization", format!("Bearer {t}")));
    }
    if has_body {
        h.push(("Content-Type", "application/json".into()));
    }
    h
}

/// 转成 wreq 需要的 (HeaderMap, OrigHeaderMap)：后者固定大小写与顺序，Host 排在最前。
pub fn to_wreq(list: &[(&'static str, String)]) -> (HeaderMap, OrigHeaderMap) {
    let mut map = HeaderMap::with_capacity(list.len());
    let mut orig = OrigHeaderMap::with_capacity(list.len() + 2);
    orig.insert("Host");
    for (name, value) in list {
        let n = HeaderName::from_static(lowercase_static(name));
        map.insert(
            n,
            HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        orig.insert(*name);
    }
    orig.insert("Content-Length");
    (map, orig)
}

/// `HeaderName::from_static` 只接受小写；这里把上面固定的名字映射到小写的 'static 字面量。
fn lowercase_static(name: &str) -> &'static str {
    match name {
        "Accept-Encoding" => "accept-encoding",
        "Connection" => "connection",
        "Accept" => "accept",
        "User-Agent" => "user-agent",
        "Origin" => "origin",
        "Referer" => "referer",
        "Sec-Ch-Ua" => "sec-ch-ua",
        "Sec-Ch-Ua-Mobile" => "sec-ch-ua-mobile",
        "Sec-Ch-Ua-Platform" => "sec-ch-ua-platform",
        "Sec-Fetch-Dest" => "sec-fetch-dest",
        "Sec-Fetch-Mode" => "sec-fetch-mode",
        "Sec-Fetch-Site" => "sec-fetch-site",
        "Accept-Language" => "accept-language",
        "Priority" => "priority",
        "Authorization" => "authorization",
        "Content-Type" => "content-type",
        other => panic!("未登记的请求头 {other}"),
    }
}

/// 上游 Token 的不可逆短标识：`token-` + sha256 前 16 位（永远不把原始 Token 写进库或日志）。
pub fn token_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    format!("token-{}", &hex::encode(digest)[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_id_matches_python() {
        // python: "token-" + hashlib.sha256(b"pst-abc").hexdigest()[:16]
        assert_eq!(token_id("pst-abc"), "token-249b0fb7a2455213");
    }
}
