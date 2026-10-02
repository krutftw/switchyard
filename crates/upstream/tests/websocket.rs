//! Upstream WebSocket connections against a local WebSocket server,
//! directly and through HTTP CONNECT and SOCKS5 proxies.

mod common;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use common::{KEY, client, dead_addr, serve, target};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use switchyard_core::config::{ProviderKind, ProxySetting};
use switchyard_core::{FailureClass, Protocol};
use switchyard_upstream::{Target, UpstreamWebSocket, WsMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Accepts the upgrade when the upstream key is presented, greets with a
/// description of the handshake it saw, then echoes.
async fn ws_endpoint(ws: WebSocketUpgrade, headers: HeaderMap, uri: Uri) -> Response {
    if header_text(&headers, "authorization").as_deref() != Some(&format!("Bearer {KEY}")) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {
                "message": "Incorrect API key provided.",
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_api_key"
            }})),
        )
            .into_response();
    }
    let hello = json!({
        "type": "hello",
        "path": uri.path(),
        "query": uri.query(),
        "openai_beta": header_text(&headers, "openai-beta"),
        "user_agent": header_text(&headers, "user-agent"),
        "offered_protocols": header_text(&headers, "sec-websocket-protocol"),
        "cookie": header_text(&headers, "cookie"),
        "organization": header_text(&headers, "openai-organization"),
    });
    ws.protocols(["realtime"])
        .on_upgrade(move |socket| echo(socket, hello))
}

async fn echo(mut socket: WebSocket, hello: Value) {
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    while let Some(Ok(message)) = socket.recv().await {
        let reply = match message {
            Message::Text(text) => Message::Text(format!("echo: {}", text.as_str()).into()),
            Message::Binary(bytes) => Message::Binary(bytes),
            Message::Close(_) => break,
            _ => continue,
        };
        if socket.send(reply).await.is_err() {
            break;
        }
    }
}

async fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "11"), (header::CONTENT_TYPE, "application/json")],
        json!({"error": {"message": "Too many connections.", "type": "rate_limit_error", "code": "rate_limit_exceeded"}}).to_string(),
    )
        .into_response()
}

async fn not_a_websocket() -> Response {
    axum::Json(json!({"object": "list", "data": []})).into_response()
}

/// Rejects the handshake and quotes every credential it was shown, as naive
/// servers do.
async fn quotes_credentials(headers: HeaderMap) -> Response {
    let message = format!(
        "Invalid credentials: {} / {}",
        header_text(&headers, "authorization").unwrap_or_default(),
        header_text(&headers, "x-gateway-auth").unwrap_or_default(),
    );
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"error": {"message": message, "type": "invalid_request_error", "code": "invalid_api_key"}})),
    )
        .into_response()
}

async fn server() -> SocketAddr {
    serve(
        Router::new()
            .route("/v1/responses", get(ws_endpoint))
            .route("/v1/realtime", get(ws_endpoint))
            .route("/v1/busy", get(too_many))
            .route("/v1/quoting", get(quotes_credentials))
            .route("/v1/models", get(not_a_websocket)),
    )
    .await
}

fn ws_target(base: String) -> Target {
    target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        base,
        "gpt-test",
    )
}

async fn next_text(stream: &mut UpstreamWebSocket) -> String {
    loop {
        match stream
            .next()
            .await
            .expect("the socket closed")
            .expect("a frame")
        {
            WsMessage::Text(text) => return text.as_str().to_string(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn next_json(stream: &mut UpstreamWebSocket) -> Value {
    serde_json::from_str(&next_text(stream).await).unwrap()
}

#[tokio::test]
async fn connects_authenticates_and_echoes() {
    let addr = server().await;
    let mut target = ws_target(format!("http://{addr}/v1"));
    target.headers = vec![("OpenAI-Organization".into(), "org-config".into())];
    let mut extra = HeaderMap::new();
    extra.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    // Client credentials and cookies must not reach the upstream.
    extra.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer client-gateway-key"),
    );
    extra.insert(header::COOKIE, HeaderValue::from_static("session=1"));

    let mut stream = client()
        .connect_ws(&target, "responses", &extra)
        .await
        .unwrap();
    let hello = next_json(&mut stream).await;
    assert_eq!(hello["path"], "/v1/responses");
    assert_eq!(hello["query"], Value::Null);
    assert_eq!(hello["openai_beta"], "responses_websockets=2026-02-06");
    assert_eq!(hello["organization"], "org-config");
    assert_eq!(hello["cookie"], Value::Null);
    assert_eq!(hello["offered_protocols"], Value::Null);
    assert!(
        hello["user_agent"]
            .as_str()
            .unwrap()
            .starts_with("switchyard/")
    );

    let event = json!({"type": "response.create", "model": "gpt-test", "input": []}).to_string();
    stream
        .send(WsMessage::Text(event.clone().into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut stream).await, format!("echo: {event}"));

    stream
        .send(WsMessage::Binary(vec![0u8, 1, 2, 255].into()))
        .await
        .unwrap();
    match stream.next().await.unwrap().unwrap() {
        WsMessage::Binary(bytes) => assert_eq!(&bytes[..], &[0u8, 1, 2, 255]),
        other => panic!("unexpected frame {other:?}"),
    }

    // A large frame survives.
    let big = "x".repeat(300_000);
    stream
        .send(WsMessage::Text(big.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut stream).await.len(),
        big.len() + "echo: ".len()
    );

    stream.close(None).await.unwrap();
}

#[tokio::test]
async fn realtime_keeps_the_query_and_negotiates_the_subprotocol() {
    let addr = server().await;
    let target = ws_target(format!("http://{addr}/v1"));
    let mut extra = HeaderMap::new();
    extra.insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("realtime, openai-insecure-api-key.sk-client-secret"),
    );
    let mut connection = client()
        .connect_ws_with(
            &target,
            "realtime?model=gpt-realtime-2.1",
            &extra,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    // What the upstream selected is available for echoing to the client.
    assert_eq!(connection.headers["sec-websocket-protocol"], "realtime");

    let hello = next_json(&mut connection.stream).await;
    assert_eq!(hello["path"], "/v1/realtime");
    assert_eq!(hello["query"], "model=gpt-realtime-2.1");
    // The client's key-bearing subprotocol was not relayed.
    assert_eq!(hello["offered_protocols"], "realtime");
}

#[tokio::test]
async fn a_rejected_handshake_is_classified_like_an_http_response() {
    let addr = server().await;
    let client = client();

    let mut wrong_key = ws_target(format!("http://{addr}/v1"));
    wrong_key.auth = switchyard_upstream::Auth::ApiKey("sk-wrong".into());
    let error = client
        .connect_ws(&wrong_key, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.class, FailureClass::Auth);

    let target = ws_target(format!("http://{addr}/v1"));
    let error = client
        .connect_ws(&target, "busy", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.class, FailureClass::RateLimit);
    assert_eq!(error.retry_after_ms, Some(11_000));

    let error = client
        .connect_ws(&target, "nowhere", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, 404);

    // An ordinary HTTP endpoint answers 200 without upgrading.
    let error = client
        .connect_ws(&target, "models", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert!(
        error.info.message.contains("instead of upgrading"),
        "{}",
        error.info.message
    );
}

#[tokio::test]
async fn a_rejected_handshake_does_not_leak_the_credentials_it_presented() {
    let addr = server().await;
    let mut target = ws_target(format!("http://{addr}/v1"));
    target.headers = vec![(
        "X-Gateway-Auth".into(),
        "Bearer gw-secret-0123456789abcdef".into(),
    )];
    let error = client()
        .connect_ws(&target, "quoting", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.class, FailureClass::Auth);
    assert_eq!(
        error.info.message,
        "Invalid credentials: Bearer [redacted] / [redacted]"
    );
    let body = error.body.unwrap();
    assert!(!body.contains(KEY), "{body}");
    assert!(!body.contains("gw-secret-0123456789abcdef"), "{body}");
}

#[tokio::test]
async fn connection_refused_names_the_connect_phase() {
    let dead = dead_addr().await;
    let target = ws_target(format!("http://{dead}/v1"));
    let error = client()
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 0);
    assert!(
        error.info.message.starts_with("connect: "),
        "{}",
        error.info.message
    );
    assert!(
        error.info.message.contains(&dead.to_string()),
        "{}",
        error.info.message
    );
    assert!(!error.info.message.contains(KEY));
}

#[tokio::test]
async fn a_silent_server_hits_the_connect_timeout() {
    // Accepts TCP connections and never answers the handshake.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let target = ws_target(format!("http://{addr}/v1"));
    let started = Instant::now();
    let error = client()
        .connect_ws_with(
            &target,
            "responses",
            &HeaderMap::new(),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(error.class, FailureClass::Transport);
    assert_eq!(error.status, 408);
    assert!(
        error.info.message.starts_with("timeout: "),
        "{}",
        error.info.message
    );
}

#[tokio::test]
async fn tls_failures_name_the_tls_phase() {
    // A plain-HTTP server cannot complete a TLS handshake.
    let addr = server().await;
    let target = ws_target(format!("https://{addr}/v1"));
    let error = client()
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert!(
        error.info.message.starts_with("tls: "),
        "{}",
        error.info.message
    );
}

/// A minimal HTTP proxy that only implements `CONNECT`. Records the request
/// heads it receives.
async fn connect_proxy(
    required_auth: Option<&'static str>,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    match client.read_u8().await {
                        Ok(byte) => head.push(byte),
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                seen.lock().unwrap().push(head.clone());
                let authorized = required_auth.is_none_or(|expected| {
                    head.contains(&format!("Proxy-Authorization: {expected}\r\n"))
                });
                if !authorized {
                    let _ = client
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n")
                        .await;
                    return;
                }
                let destination = head
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("CONNECT "))
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("")
                    .to_string();
                let Ok(mut upstream) = TcpStream::connect(&destination).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                if client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (addr, log)
}

#[tokio::test]
async fn tunnels_through_an_http_proxy() {
    let addr = server().await;
    // base64("user:pass")
    let (proxy, log) = connect_proxy(Some("Basic dXNlcjpwYXNz")).await;
    let mut target = ws_target(format!("http://{addr}/v1"));
    target.proxy = ProxySetting::Url(format!("http://user:pass@{proxy}"));

    let mut stream = client()
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(next_json(&mut stream).await["path"], "/v1/responses");
    stream
        .send(WsMessage::Text("through the tunnel".into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut stream).await, "echo: through the tunnel");

    let heads = log.lock().unwrap().clone();
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].starts_with(&format!("CONNECT {addr} HTTP/1.1\r\n")),
        "{}",
        heads[0]
    );
    assert!(heads[0].contains(&format!("\r\nHost: {addr}\r\n")));
    // The upstream credential travels inside the tunnel, never to the proxy.
    assert!(!heads[0].contains(KEY));
}

#[tokio::test]
async fn proxy_refusals_are_explained() {
    let addr = server().await;
    let (proxy, _log) = connect_proxy(Some("Basic dXNlcjpwYXNz")).await;
    let client = client();

    // Wrong proxy password.
    let mut target = ws_target(format!("http://{addr}/v1"));
    target.proxy = ProxySetting::Url(format!("http://user:wrong-password@{proxy}"));
    let error = client
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    let message = &error.info.message;
    assert!(message.starts_with("connect: "), "{message}");
    assert!(message.contains("requires authentication"), "{message}");
    assert!(!message.contains("wrong-password"), "{message}");

    // The proxy cannot reach the destination.
    let dead = dead_addr().await;
    let mut target = ws_target(format!("http://{dead}/v1"));
    target.proxy = ProxySetting::Url(format!("http://user:pass@{proxy}"));
    let error = client
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert!(
        error.info.message.contains("refused the tunnel"),
        "{}",
        error.info.message
    );
    assert!(
        error.info.message.contains("HTTP 502"),
        "{}",
        error.info.message
    );

    // The proxy itself is down.
    let mut target = ws_target(format!("http://{addr}/v1"));
    target.proxy = ProxySetting::Url(format!("http://{dead}"));
    let error = client
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert!(
        error
            .info
            .message
            .starts_with("connect: could not connect to proxy"),
        "{}",
        error.info.message
    );
}

/// A minimal SOCKS5 proxy (no authentication). Records the destinations it
/// was asked for, as `ip:<addr>` or `domain:<name>`.
async fn socks_proxy() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut greeting = [0u8; 2];
                client.read_exact(&mut greeting).await.ok()?;
                let mut methods = vec![0u8; usize::from(greeting[1])];
                client.read_exact(&mut methods).await.ok()?;
                client.write_all(&[0x05, 0x00]).await.ok()?;

                let mut request = [0u8; 4];
                client.read_exact(&mut request).await.ok()?;
                let host = match request[3] {
                    0x01 => {
                        let mut ip = [0u8; 4];
                        client.read_exact(&mut ip).await.ok()?;
                        let ip = std::net::Ipv4Addr::from(ip).to_string();
                        seen.lock().unwrap().push(format!("ip:{ip}"));
                        ip
                    }
                    0x03 => {
                        let len = client.read_u8().await.ok()?;
                        let mut name = vec![0u8; usize::from(len)];
                        client.read_exact(&mut name).await.ok()?;
                        let name = String::from_utf8(name).ok()?;
                        seen.lock().unwrap().push(format!("domain:{name}"));
                        name
                    }
                    _ => return None,
                };
                let port = client.read_u16().await.ok()?;
                let mut upstream = TcpStream::connect((host.as_str(), port)).await.ok()?;
                client
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                    .await
                    .ok()?;
                tokio::io::copy_bidirectional(&mut client, &mut upstream)
                    .await
                    .ok()?;
                Some(())
            });
        }
    });
    (addr, log)
}

#[tokio::test]
async fn tunnels_through_a_socks5_proxy() {
    let addr = server().await;
    let (proxy, log) = socks_proxy().await;
    let client = client();

    // socks5h: the proxy resolves the name.
    let mut by_name = ws_target(format!("http://localhost:{}/v1", addr.port()));
    by_name.proxy = ProxySetting::Url(format!("socks5h://{proxy}"));
    let mut stream = client
        .connect_ws(&by_name, "responses", &HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(next_json(&mut stream).await["path"], "/v1/responses");
    stream
        .send(WsMessage::Text("via socks".into()))
        .await
        .unwrap();
    assert_eq!(next_text(&mut stream).await, "echo: via socks");

    // socks5 with an address literal: sent as an address.
    let mut by_ip = ws_target(format!("http://{addr}/v1"));
    by_ip.proxy = ProxySetting::Url(format!("socks5://{proxy}"));
    let mut stream = client
        .connect_ws(&by_ip, "responses", &HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(next_json(&mut stream).await["path"], "/v1/responses");

    assert_eq!(*log.lock().unwrap(), ["domain:localhost", "ip:127.0.0.1"]);
}

#[tokio::test]
async fn the_mock_kind_has_no_websocket() {
    let target = target(
        ProviderKind::Mock,
        Protocol::OpenaiResponses,
        "mock://local",
        "mock-echo",
    );
    let error = client()
        .connect_ws(&target, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.status, 0);
    assert!(
        error.info.message.contains("mock"),
        "{}",
        error.info.message
    );
}
