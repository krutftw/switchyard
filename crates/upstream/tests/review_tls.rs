//! Review: the TLS success paths (https over HTTP/2 and HTTP/1.1, wss) that
//! the original test-suite never exercised.
//!
//! A throwaway CA (`fixtures/review_tls_ca.pem`) signs a leaf certificate for
//! `localhost` / `127.0.0.1`; the client under test trusts only that CA.
//! These tests document behaviour that was verified to work; they are
//! expected to PASS.

mod common;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{KEY, target, timeouts};
use futures::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::Protocol;
use switchyard_core::config::ProviderKind;
use switchyard_upstream::{Operation, TlsConfigs, UpstreamBody, UpstreamClient, WsMessage};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const CA_PEM: &str = include_str!("fixtures/review_tls_ca.pem");
const LEAF_PEM: &str = include_str!("fixtures/review_tls_leaf.pem");
const LEAF_KEY: &str = include_str!("fixtures/review_tls_leaf.key");

fn der(pem_text: &str) -> Vec<u8> {
    pem::parse(pem_text).unwrap().into_contents()
}

/// A client that trusts only the review CA.
fn client() -> UpstreamClient {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(der(CA_PEM))).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let base = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut http = base.clone();
    http.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    UpstreamClient::with_tls(TlsConfigs {
        http: Arc::new(http),
        ws: Arc::new(base),
    })
}

struct TlsListener {
    tcp: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((stream, addr)) = self.tcp.accept().await else {
                continue;
            };
            if let Ok(tls) = self.acceptor.accept(stream).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Serves `app` over TLS, offering `alpn` to clients.
async fn serve_tls(app: Router, alpn: &[&[u8]]) -> SocketAddr {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(der(LEAF_PEM))],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der(LEAF_KEY))),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = TlsListener {
        tcp,
        acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
    };
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn describe(request: Request) -> Response {
    let version = format!("{:?}", request.version());
    let auth = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    axum::Json(json!({"version": version, "authorization": auth})).into_response()
}

async fn gated_sse(axum::extract::State(release): axum::extract::State<Arc<Notify>>) -> Response {
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"n\":1}\n\n"));
        release.notified().await;
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn ws_echo(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let hello = json!({"authorization": auth}).to_string();
        if socket.send(Message::Text(hello.into())).await.is_err() {
            return;
        }
        while let Some(Ok(Message::Text(text))) = socket.recv().await {
            let reply = format!("echo: {}", text.as_str());
            if socket.send(Message::Text(reply.into())).await.is_err() {
                break;
            }
        }
    })
}

fn app(release: Arc<Notify>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(describe))
        .route("/v1/responses", post(gated_sse).get(ws_echo))
        .with_state(release)
}

async fn json_of(response: switchyard_upstream::UpstreamResponse) -> Value {
    serde_json::from_slice(&response.body.collect().await.unwrap()).unwrap()
}

#[tokio::test]
async fn https_negotiates_http2_and_authenticates() {
    let addr = serve_tls(app(Arc::new(Notify::new())), &[b"h2", b"http/1.1"]).await;
    for host in ["localhost", "127.0.0.1"] {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            format!("https://{host}:{}/v1", addr.port()),
            "gpt-test",
        );
        let response = client()
            .send(
                &t,
                &Operation::Generate { stream: false },
                Bytes::from_static(b"{}"),
                &HeaderMap::new(),
                timeouts(),
            )
            .await
            .unwrap_or_else(|e| panic!("https to {host} failed: {e:?}"));
        let seen = json_of(response).await;
        assert_eq!(seen["version"], "HTTP/2.0", "host {host}");
        assert_eq!(seen["authorization"], format!("Bearer {KEY}"));
    }
}

#[tokio::test]
async fn https_falls_back_to_http1_when_the_server_has_no_h2() {
    let addr = serve_tls(app(Arc::new(Notify::new())), &[b"http/1.1"]).await;
    let t = target(
        ProviderKind::Openai,
        Protocol::OpenaiChat,
        format!("https://localhost:{}/v1", addr.port()),
        "gpt-test",
    );
    let response = client()
        .send(
            &t,
            &Operation::Generate { stream: false },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    assert_eq!(json_of(response).await["version"], "HTTP/1.1");
}

#[tokio::test]
async fn sse_over_http2_is_not_buffered() {
    let release = Arc::new(Notify::new());
    let addr = serve_tls(app(release.clone()), &[b"h2", b"http/1.1"]).await;
    let t = target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        format!("https://localhost:{}/v1", addr.port()),
        "gpt-test",
    );
    let response = client()
        .send(
            &t,
            &Operation::Generate { stream: true },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap();
    let UpstreamBody::Stream(mut stream) = response.body else {
        panic!("expected a stream");
    };
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"data: {\"n\":1}\n\n");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "the second event arrived before it was sent"
    );
    release.notify_one();
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(&second[..], b"data: [DONE]\n\n");
}

#[tokio::test]
async fn wss_connects_even_when_the_server_offers_h2() {
    // The WebSocket TLS configuration advertises no ALPN, so a server that
    // would otherwise pick h2 serves the upgrade over HTTP/1.1.
    let addr = serve_tls(app(Arc::new(Notify::new())), &[b"h2", b"http/1.1"]).await;
    for host in ["localhost", "127.0.0.1"] {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            format!("https://{host}:{}/v1", addr.port()),
            "gpt-test",
        );
        let mut socket = client()
            .connect_ws(&t, "responses", &HeaderMap::new())
            .await
            .unwrap_or_else(|e| panic!("wss to {host} failed: {e:?}"));
        let hello = socket.next().await.unwrap().unwrap();
        let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
        assert_eq!(hello["authorization"], format!("Bearer {KEY}"));
        socket.send(WsMessage::Text("ping".into())).await.unwrap();
        let reply = socket.next().await.unwrap().unwrap();
        assert_eq!(reply.to_text().unwrap(), "echo: ping");
    }
}

/// Answers one `CONNECT` on `client` and pipes bytes to the destination.
async fn tunnel<S>(mut client: S, log: Arc<std::sync::Mutex<Vec<String>>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        match client.read_u8().await {
            Ok(byte) => head.push(byte),
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    log.lock().unwrap().push(head.clone());
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
}

/// A `CONNECT`-only proxy, reachable over plain TCP (`tls == false`) or
/// over TLS (`tls == true`, an "https://" proxy).
async fn connect_proxy(tls: bool) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = log.clone();
    let acceptor = tls.then(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(der(LEAF_PEM))],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der(LEAF_KEY))),
            )
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    });
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let seen = seen.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        if let Ok(stream) = acceptor.accept(client).await {
                            tunnel(stream, seen).await;
                        }
                    }
                    None => tunnel(client, seen).await,
                }
            });
        }
    });
    (addr, log)
}

#[tokio::test]
async fn https_and_wss_work_through_http_and_https_proxies() {
    use switchyard_core::config::ProxySetting;

    let addr = serve_tls(app(Arc::new(Notify::new())), &[b"h2", b"http/1.1"]).await;
    for tls_proxy in [false, true] {
        let (proxy, log) = connect_proxy(tls_proxy).await;
        let proxy_url = if tls_proxy {
            format!("https://puser:ppass@localhost:{}", proxy.port())
        } else {
            format!("http://puser:ppass@127.0.0.1:{}", proxy.port())
        };
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            format!("https://localhost:{}/v1", addr.port()),
            "gpt-test",
        );
        t.proxy = ProxySetting::Url(proxy_url.clone());
        let client = client();

        // HTTP call (reqwest's own proxy support).
        let mut chat = t.clone();
        chat.protocol = Protocol::OpenaiChat;
        let response = client
            .send(
                &chat,
                &Operation::Generate { stream: false },
                Bytes::from_static(b"{}"),
                &HeaderMap::new(),
                timeouts(),
            )
            .await
            .unwrap_or_else(|e| panic!("https via {proxy_url} failed: {e:?}"));
        let seen = json_of(response).await;
        assert_eq!(seen["authorization"], format!("Bearer {KEY}"));
        assert_eq!(seen["version"], "HTTP/2.0", "via {proxy_url}");

        // WebSocket (hand-dialled tunnel).
        let mut socket = client
            .connect_ws(&t, "responses", &HeaderMap::new())
            .await
            .unwrap_or_else(|e| panic!("wss via {proxy_url} failed: {e:?}"));
        let hello = socket.next().await.unwrap().unwrap();
        let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
        assert_eq!(hello["authorization"], format!("Bearer {KEY}"));

        let heads = log.lock().unwrap().clone();
        assert_eq!(heads.len(), 2, "one tunnel per transport: {heads:?}");
        for head in &heads {
            assert!(
                head.starts_with(&format!("CONNECT localhost:{} HTTP/1.1\r\n", addr.port())),
                "{head}"
            );
            // base64("puser:ppass")
            assert!(
                head.to_ascii_lowercase()
                    .contains("proxy-authorization: basic chvzzxi6chbhc3m=\r\n"),
                "{head}"
            );
            // The upstream key travels inside the tunnel only.
            assert!(!head.contains(KEY), "{head}");
        }
    }
}

#[tokio::test]
async fn an_untrusted_certificate_is_a_tls_failure_for_both_transports() {
    let addr = serve_tls(app(Arc::new(Notify::new())), &[b"h2", b"http/1.1"]).await;
    // The process-wide configuration does not trust the review CA.
    let untrusting = common::client();
    let t = target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        format!("https://localhost:{}/v1", addr.port()),
        "gpt-test",
    );
    let error = untrusting
        .send(
            &t,
            &Operation::Generate { stream: false },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert!(
        error.info.message.starts_with("tls: "),
        "{}",
        error.info.message
    );
    let error = untrusting
        .connect_ws(&t, "responses", &HeaderMap::new())
        .await
        .unwrap_err();
    assert!(
        error.info.message.starts_with("tls: "),
        "{}",
        error.info.message
    );
}
