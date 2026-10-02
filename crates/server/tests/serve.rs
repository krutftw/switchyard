//! Listening: TLS, HTTP versions, graceful shutdown.

mod support;

use futures::StreamExt;
use pretty_assertions::assert_eq;
use rustls::pki_types::CertificateDer;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{KEY, Received, Settings, TestServer, WsClient, http, install_crypto};
use switchyard_server::{ServeOptions, TlsFiles};
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn chat(model: &str, text: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "messages": [{"role": "user", "content": text}]})
}

/// Whether anything accepts connections on `addr`. (A refused connection
/// takes Windows about two seconds to report, hence the short wait: a
/// listener would have answered within it.)
async fn accepts(addr: std::net::SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect(addr)
        )
        .await,
        Ok(Ok(_))
    )
}

/// Writes a fresh self-signed certificate for `127.0.0.1` into `dir`.
fn self_signed(dir: &Path) -> (TlsFiles, CertificateDer<'static>) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("a certificate");
    let files = TlsFiles {
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    };
    std::fs::write(&files.cert, certified.cert.pem()).unwrap();
    std::fs::write(&files.key, certified.signing_key.serialize_pem()).unwrap();
    (files, certified.cert.der().clone())
}

/// A client configuration that trusts exactly `cert`.
fn trusting(cert: &CertificateDer<'static>, alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    config
}

#[tokio::test]
async fn https_with_http2_and_http1() {
    let dir = tempfile::tempdir().unwrap();
    let (files, cert) = self_signed(dir.path());
    let server = TestServer::with(Settings {
        tls: Some(files),
        ..Settings::default()
    })
    .await;
    let base = format!("https://127.0.0.1:{}", server.addr.port());

    // HTTP/2, negotiated by ALPN.
    let h2 = reqwest::Client::builder()
        .no_proxy()
        .tls_backend_preconfigured(trusting(&cert, &[b"h2", b"http/1.1"]))
        .build()
        .unwrap();
    let response = h2.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.headers()["server"], "switchyard");
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"status": "ok"})
    );

    // A stream over HTTP/2.
    let response = h2
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(KEY)
        .json(&chat("mock-echo", "over tls", true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    let body = response.text().await.unwrap();
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");

    // HTTP/1.1 for clients that offer nothing else.
    let h1 = reqwest::Client::builder()
        .no_proxy()
        .tls_backend_preconfigured(trusting(&cert, &[b"http/1.1"]))
        .build()
        .unwrap();
    let response = h1
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(KEY)
        .json(&chat("mock-echo", "over tls", false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), reqwest::Version::HTTP_11);
    // The peer address is seen through the TLS layer.
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        server.record(&id).await.client.ip.as_deref(),
        Some("127.0.0.1")
    );

    // A client that does not trust the certificate gets nowhere.
    let stranger = http();
    assert!(
        stranger
            .get(format!("{base}/healthz"))
            .send()
            .await
            .is_err()
    );
    // Nor does plain HTTP on the TLS port.
    let plain = http()
        .get(format!("http://127.0.0.1:{}/healthz", server.addr.port()))
        .send()
        .await;
    assert!(plain.is_err());
    // The server is none the worse for either.
    let response = h2.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn websockets_over_tls() {
    let dir = tempfile::tempdir().unwrap();
    let (files, cert) = self_signed(dir.path());
    let server = TestServer::with(Settings {
        tls: Some(files),
        ..Settings::default()
    })
    .await;
    let mut request = format!("wss://127.0.0.1:{}/v1/responses", server.addr.port())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-api-key", KEY.parse().unwrap());
    let connector = Connector::Rustls(Arc::new(trusting(&cert, &[])));
    let (socket, response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .expect("the upgrade must succeed over TLS");
    let mut client = WsClient {
        socket,
        handshake: response.headers().clone(),
    };
    client
        .send_json(&json!({"type": "response.create", "model": "mock-echo",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "secure"}]}]}))
        .await;
    let frames = client.read_until(&["response.completed", "error"]).await;
    assert_eq!(frames.last().unwrap()["type"], "response.completed");
}

#[tokio::test]
async fn unusable_tls_files_are_reported_by_name() {
    install_crypto();
    let dir = tempfile::tempdir().unwrap();
    let (files, _) = self_signed(dir.path());
    let options = |tls: TlsFiles| ServeOptions {
        host: "127.0.0.1".into(),
        port: 0,
        tls: Some(tls),
        shutdown_grace: Duration::from_secs(1),
    };
    let message = |tls: TlsFiles| async move {
        switchyard_server::bind(options(tls))
            .await
            .expect_err("bind must fail")
            .to_string()
    };

    let missing = dir.path().join("nope.pem");
    let error = message(TlsFiles {
        cert: missing.clone(),
        key: files.key.clone(),
    })
    .await;
    assert!(error.contains("TLS certificate file"), "{error}");
    assert!(error.contains("nope.pem"), "{error}");

    let error = message(TlsFiles {
        cert: files.cert.clone(),
        key: missing,
    })
    .await;
    assert!(error.contains("TLS private key file"), "{error}");
    assert!(error.contains("nope.pem"), "{error}");

    let garbage = dir.path().join("garbage.pem");
    std::fs::write(&garbage, "this is not PEM").unwrap();
    let error = message(TlsFiles {
        cert: garbage.clone(),
        key: files.key.clone(),
    })
    .await;
    assert!(error.contains("garbage.pem"), "{error}");
    assert!(error.contains("no PEM certificate"), "{error}");
    let error = message(TlsFiles {
        cert: files.cert.clone(),
        key: garbage,
    })
    .await;
    assert!(error.contains("TLS private key file"), "{error}");
    assert!(error.contains("garbage.pem"), "{error}");

    // A key that does not belong to the certificate.
    let other = tempfile::tempdir().unwrap();
    let (other_files, _) = self_signed(other.path());
    let error = message(TlsFiles {
        cert: files.cert.clone(),
        key: other_files.key,
    })
    .await;
    assert!(error.contains("cannot be used together"), "{error}");
    // No key material in any message.
    let key_pem = std::fs::read_to_string(&files.key).unwrap();
    let key_body = key_pem.lines().nth(1).unwrap();
    assert!(!error.contains(key_body));

    // The good pair binds.
    let bound = switchyard_server::bind(options(files)).await.unwrap();
    assert_ne!(bound.local_addr().port(), 0);
}

#[tokio::test]
async fn binding_reports_the_address_and_refuses_a_port_in_use() {
    let options = |port: u16| ServeOptions {
        host: "127.0.0.1".into(),
        port,
        tls: None,
        shutdown_grace: Duration::from_secs(1),
    };
    let first = switchyard_server::bind(options(0)).await.unwrap();
    let port = first.local_addr().port();
    assert_ne!(port, 0);
    assert!(first.local_addr().ip().is_loopback());

    let error = switchyard_server::bind(options(port))
        .await
        .expect_err("the port is taken");
    assert!(
        error
            .to_string()
            .contains(&format!("cannot listen on 127.0.0.1:{port}")),
        "{error}"
    );

    // IPv6 literals may be written the way URLs write them.
    if let Ok(v6) = switchyard_server::bind(ServeOptions {
        host: "[::1]".into(),
        ..options(0)
    })
    .await
    {
        assert!(v6.local_addr().is_ipv6());
    }
    let error = switchyard_server::bind(ServeOptions {
        host: "not a host name".into(),
        ..options(0)
    })
    .await
    .expect_err("an unusable host");
    assert!(error.to_string().contains("cannot listen on"), "{error}");
}

#[test]
fn serve_options_come_from_the_server_section() {
    let config = switchyard_core::Config::from_toml(
        "[server]\nhost = \"0.0.0.0\"\nport = 9000\n[server.tls]\ncert = \"tls/cert.pem\"\nkey = \"/etc/abs/key.pem\"\n",
    )
    .unwrap();
    let options = ServeOptions::from_config(&config, Path::new("/etc/switchyard"));
    assert_eq!(options.host, "0.0.0.0");
    assert_eq!(options.port, 9000);
    let tls = options.tls.expect("tls is configured");
    assert_eq!(tls.cert, Path::new("/etc/switchyard").join("tls/cert.pem"));
    // An absolute path stays what it is.
    assert_eq!(tls.key, Path::new("/etc/abs/key.pem"));
    assert!(options.shutdown_grace >= Duration::from_secs(1));

    let plain = switchyard_core::Config::default();
    assert!(
        ServeOptions::from_config(&plain, Path::new("."))
            .tls
            .is_none()
    );
}

#[tokio::test]
async fn shutdown_lets_a_stream_in_flight_finish() {
    let mut server = TestServer::start().await;
    let client = http();
    let said = "one two three four five six seven eight nine";
    let response = client
        .post(server.url("/v1/chat/completions"))
        .bearer_auth(KEY)
        .json(&chat("mock-slow", said, true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    let mut body = stream
        .next()
        .await
        .expect("a first chunk")
        .unwrap()
        .to_vec();

    // Shutdown is requested with most of the stream still to come. New
    // connections are turned away at once …
    server.begin_shutdown();
    let mut listening = true;
    for _ in 0..20 {
        listening = accepts(server.addr).await;
        if !listening {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!listening, "the listener must close when shutdown begins");
    // … while the stream runs to its end.
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.expect("the stream must not be cut"));
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    let text: String = support::sse_events(&body)
        .iter()
        .filter(|(_, data)| data != "[DONE]")
        .filter_map(|(_, data)| {
            serde_json::from_str::<Value>(data).unwrap()["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert!(text.contains(said), "{text}");

    // Then the server stops.
    server.stopped().await;
    assert!(!accepts(server.addr).await);
}

#[tokio::test]
async fn shutdown_stops_accepting_at_once_and_closes_idle_connections() {
    let mut server = TestServer::start().await;
    let client = http();
    // Leaves an idle keep-alive connection in the client's pool.
    let response = client.get(server.url("/healthz")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    drop(response);

    let started = Instant::now();
    server.begin_shutdown();
    server.stopped().await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an idle connection must not hold the server up: {:?}",
        started.elapsed()
    );
    assert!(!accepts(server.addr).await);
}

#[tokio::test]
async fn the_grace_period_bounds_the_wait() {
    let mut server = TestServer::with(Settings {
        shutdown_grace: Duration::from_millis(300),
        ..Settings::default()
    })
    .await;
    let gauges = server.gateway.telemetry().gauges().clone();

    // A stream that would run for a long time …
    let long = "word ".repeat(300);
    let response = http()
        .post(server.url("/v1/chat/completions"))
        .bearer_auth(KEY)
        .json(&chat("mock-slow", &long, true))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    stream.next().await.expect("a first chunk").unwrap();

    // … and a WebSocket turn that would, too.
    let auth = format!("Bearer {KEY}");
    let mut socket = support::ws_connect(
        &server.ws_url("/v1/responses"),
        &[("authorization", auth.as_str())],
    )
    .await
    .unwrap();
    socket
        .send_json(&json!({"type": "response.create", "model": "mock-slow",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": long}]}]}))
        .await;
    assert_eq!(socket.next_json().await["type"], "response.created");
    // The client keeps reading, as clients do.
    let closed = tokio::spawn(async move { socket.read_close().await });

    let started = Instant::now();
    server.begin_shutdown();
    server.stopped().await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(300) && waited < Duration::from_secs(2),
        "{waited:?}"
    );

    // The stream was cut …
    let mut rest = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        rest.extend_from_slice(&chunk);
    }
    assert!(!String::from_utf8_lossy(&rest).contains("[DONE]"));
    // … the socket was told why …
    assert!(matches!(
        closed.await.unwrap(),
        Received::Close(Some((1001, _)))
    ));
    // … and the gateway was not left with requests it thinks are running.
    support::eventually("the requests to be released", || {
        gauges.active_streams() == 0 && gauges.in_flight() == 0 && gauges.ws_connections() == 0
    })
    .await;
}
