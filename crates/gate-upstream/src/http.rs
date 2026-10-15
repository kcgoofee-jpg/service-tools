//! wreq 客户端：按指纹档位各建一个（同一档位共享连接池），按 [`crate::headers`] 的顺序组请求，读正文。

use crate::headers::{self, BrowserProfile};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::time::Duration;

/// 每个指纹档位一对（客户端, 请求头档案），下标就是 [`crate::pool::Token::profile_index`]。
pub(crate) struct Http {
    clients: Vec<(wreq::Client, BrowserProfile)>,
}

impl Http {
    pub(crate) fn new(
        proxy: Option<&str>,
        http2: bool,
        custom_user_agent: Option<&str>,
    ) -> Result<Self, wreq::Error> {
        let mut clients = Vec::with_capacity(3);
        for (i, p) in headers::browser_profiles().into_iter().enumerate() {
            let emu = wreq_util::Emulation::builder()
                .profile(p.chrome)
                .platform(p.platform)
                .http2(http2)
                .headers(false)
                .build();
            let mut b = wreq::Client::builder()
                .emulation(emu)
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(300))
                // 旧版 keepalive_expiry=120：出图间隔 15 秒，默认 5 秒会让每张图都重新 TLS 握手
                .pool_idle_timeout(Duration::from_secs(120))
                .pool_max_idle_per_host(8)
                .redirect(wreq::redirect::Policy::limited(20));
            if !http2 {
                b = b.http1_only();
            }
            if let Some(proxy) = proxy.filter(|p| !p.is_empty()) {
                b = b.proxy(wreq::Proxy::all(proxy)?);
            }
            clients.push((b.build()?, headers::profile_for(i, custom_user_agent)));
        }
        Ok(Self { clients })
    }

    /// 按 headers.rs 的顺序构造一个请求（指纹档位 `profile_index` 的客户端 + 同一档位的请求头）。
    pub(crate) fn build(
        &self,
        profile_index: usize,
        token: &str,
        method: wreq::Method,
        url: &str,
        body: Option<&Value>,
        accept: &str,
    ) -> wreq::RequestBuilder {
        let has_body = !matches!(method, wreq::Method::GET | wreq::Method::HEAD);
        let (client, profile) = &self.clients[profile_index % self.clients.len()];
        let list = headers::ordered(profile, Some(token), accept, has_body);
        let (map, orig) = headers::to_wreq(&list);
        let mut req = client
            .request(method, url)
            .default_headers(false)
            .headers(map)
            .orig_headers(orig);
        if let Some(b) = body {
            req = req.body(gate_core::pyjson::wire(b));
        }
        req
    }
}

/// 按块读完正文，超过 `cap` 字节返回 `Ok(None)`（已读的丢弃）。
pub(crate) async fn read_capped(
    resp: wreq::Response,
    cap: usize,
) -> Result<Option<Bytes>, wreq::Error> {
    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if data.len() + chunk.len() > cap {
            return Ok(None);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(Some(Bytes::from(data)))
}

/// 读上游错误响应体的一小段（≤ 300 字节、≤ 3 秒），**只用来判断 403 属于哪一类**，绝不回显给成员。
pub(crate) async fn peek_body(resp: wreq::Response) -> String {
    let read = async {
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            data.extend_from_slice(&chunk);
            if data.len() >= 300 {
                break;
            }
        }
        data.truncate(300);
        String::from_utf8_lossy(&data).into_owned()
    };
    tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .unwrap_or_default()
}
