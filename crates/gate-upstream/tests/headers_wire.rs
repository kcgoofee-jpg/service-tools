//! 线上抓包对拍：wreq 发出的 HTTP/1.1 请求头顺序、大小写、取值必须与旧版 Python（httpx 0.28）一致。
//! 期望值来自 2026-10-14 对旧版 `NaiClient._client.request` 的原始套接字抓包。

use gate_upstream::headers::{ordered, profile_for, to_wreq};
use std::io::{Read, Write};
use std::net::TcpListener;

fn capture(f: impl FnOnce(u16) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = std::thread::spawn(move || f(port));
    let (mut s, _) = listener.accept().unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    s.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n{}",
    )
    .unwrap();
    client.join().unwrap();
    String::from_utf8(buf).unwrap()
}

fn send(port: u16, method: wreq::Method, path: &str, body: Option<&[u8]>, accept: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let p = profile_for(0, None);
        let emu = wreq_util::Emulation::builder()
            .profile(p.chrome)
            .platform(p.platform)
            .headers(false)
            .build();
        let client = wreq::Client::builder()
            .emulation(emu)
            .http1_only()
            .build()
            .unwrap();
        let (map, orig) = to_wreq(&ordered(&p, Some("tok-abc"), accept, body.is_some()));
        let mut req = client
            .request(method, format!("http://127.0.0.1:{port}{path}"))
            .default_headers(false)
            .headers(map)
            .orig_headers(orig);
        if let Some(b) = body {
            req = req.body(b.to_vec());
        }
        req.send().await.unwrap().bytes().await.unwrap();
    });
}

fn header_lines(raw: &str) -> Vec<String> {
    let head = raw.split("\r\n\r\n").next().unwrap_or("");
    head.split("\r\n")
        .filter(|l| !l.is_empty())
        .map(|l| l.replace("tok-abc", "<TOKEN>"))
        .collect()
}

#[test]
fn post_matches_python() {
    let raw = capture(|port| {
        send(
            port,
            wreq::Method::POST,
            "/ai/generate-image",
            Some(br#"{"b":1,"a":[1.0,"x"]}"#),
            "*/*",
        )
    });
    let lines = header_lines(&raw);
    let expected = [
        "POST /ai/generate-image HTTP/1.1",
        "Host: 127.0.0.1:PORT",
        "Accept-Encoding: gzip, deflate",
        "Connection: keep-alive",
        "Accept: */*",
        "User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
        "Origin: https://novelai.net",
        "Referer: https://novelai.net/",
        r#"Sec-Ch-Ua: "Google Chrome";v="129", "Not=A?Brand";v="8", "Chromium";v="129""#,
        "Sec-Ch-Ua-Mobile: ?0",
        r#"Sec-Ch-Ua-Platform: "Windows""#,
        "Sec-Fetch-Dest: empty",
        "Sec-Fetch-Mode: cors",
        "Sec-Fetch-Site: same-site",
        "Accept-Language: en-US,en;q=0.9",
        "Priority: u=1, i",
        "Authorization: Bearer <TOKEN>",
        "Content-Type: application/json",
        "Content-Length: 21",
    ];
    let got: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.starts_with("Host: ") {
                "Host: 127.0.0.1:PORT".to_string()
            } else {
                l.clone()
            }
        })
        .collect();
    assert_eq!(
        got.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "\n实际：\n{}",
        raw
    );
}

#[test]
fn get_has_no_content_type() {
    let raw = capture(|port| {
        send(
            port,
            wreq::Method::GET,
            "/ai/generate-image/suggest-tags?model=m&prompt=p",
            None,
            "application/json",
        )
    });
    let lines = header_lines(&raw);
    assert!(
        !lines
            .iter()
            .any(|l| l.to_ascii_lowercase().starts_with("content-type")),
        "{raw}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.to_ascii_lowercase().starts_with("content-length")),
        "{raw}"
    );
    assert_eq!(
        lines.last().unwrap(),
        "Authorization: Bearer <TOKEN>",
        "{raw}"
    );
}
