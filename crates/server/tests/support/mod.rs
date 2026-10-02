//! Shared test support: a gateway behind the real server on a loopback
//! port, a small fake upstream, and client helpers.
#![allow(dead_code)]

pub mod fake;

pub use fake::Fake;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use switchyard_gateway::{Gateway, GatewayOptions};
use switchyard_server::{ServeOptions, TlsFiles};
use switchyard_telemetry::RequestRecord;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The client key every test configuration defines.
pub const KEY: &str = "sy-test-key-0123456789";
/// A second client key, restricted to the mock models.
pub const SECOND_KEY: &str = "sy-second-key-abcdefghij";
/// The key the fake upstream expects from the gateway.
pub const UPSTREAM_KEY: &str = "upstream-key-1";

/// What a test may vary about the configuration.
#[derive(Clone, Debug)]
pub struct Settings {
    pub keepalive_secs: u64,
    pub cors: bool,
    pub auth_required: bool,
    pub body_limit_mb: u64,
    pub shutdown_grace: Duration,
    pub tls: Option<TlsFiles>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            keepalive_secs: 0,
            cors: true,
            auth_required: true,
            body_limit_mb: 1,
            shutdown_grace: Duration::from_secs(5),
            tls: None,
        }
    }
}

fn config(settings: &Settings, upstream: &str) -> String {
    format!(
        r#"
[server]
body_limit_mb = {body_limit_mb}
cors = {cors}

[auth]
required = {auth_required}

[[auth.keys]]
key = "{KEY}"
name = "tester"

[[auth.keys]]
key = "{SECOND_KEY}"
name = "second"
models = ["mock-*"]

[streaming]
keepalive_secs = {keepalive_secs}

[upstream]
proxy = "direct"

[usage]
persist = false

[[providers]]
name = "mock"
kind = "mock"

[[providers]]
name = "fake"
kind = "openai-compat"
base_url = "{upstream}/v1"
api_keys = ["{UPSTREAM_KEY}"]
discover = false
[[providers.models]]
id = "embed-up"
alias = "embed"
[[providers.models]]
id = "chat-up"
alias = "paused-chat"

[[providers]]
name = "fake-openai"
kind = "openai"
wire_api = "responses"
base_url = "{upstream}/v1"
api_keys = ["{UPSTREAM_KEY}"]
discover = false
[[providers.models]]
id = "resp-up"
alias = "recorded"
[[providers.models]]
id = "rt-up"
alias = "rt"
"#,
        body_limit_mb = settings.body_limit_mb,
        cors = settings.cors,
        auth_required = settings.auth_required,
        keepalive_secs = settings.keepalive_secs,
    )
}

/// A gateway served by `switchyard_server` on a loopback port.
pub struct TestServer {
    pub gateway: Gateway,
    pub addr: SocketAddr,
    pub fake: Fake,
    shutdown: Option<oneshot::Sender<()>>,
    serving: Option<JoinHandle<std::io::Result<()>>>,
    _dir: TempDir,
}

impl TestServer {
    pub async fn start() -> TestServer {
        TestServer::with(Settings::default()).await
    }

    pub async fn with(settings: Settings) -> TestServer {
        install_crypto();
        let fake = Fake::start().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        std::fs::write(&path, config(&settings, &fake.base())).unwrap();
        let gateway = Gateway::start(GatewayOptions::new(&path).watch(false))
            .await
            .unwrap_or_else(|error| panic!("the gateway must start: {error}"));

        let bound = switchyard_server::bind(ServeOptions {
            host: "127.0.0.1".to_string(),
            port: 0,
            tls: settings.tls.clone(),
            shutdown_grace: settings.shutdown_grace,
        })
        .await
        .expect("the server must bind");
        let addr = bound.local_addr();
        let app = switchyard_server::router(gateway.clone());
        let (shutdown, stop) = oneshot::channel::<()>();
        let serving = tokio::spawn(bound.serve(app, async move {
            let _ = stop.await;
        }));
        TestServer {
            gateway,
            addr,
            fake,
            shutdown: Some(shutdown),
            serving: Some(serving),
            _dir: dir,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// Asks the server to shut down without waiting for it.
    pub fn begin_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    /// Waits until `serve` has returned.
    pub async fn stopped(&mut self) {
        if let Some(serving) = self.serving.take() {
            tokio::time::timeout(Duration::from_secs(10), serving)
                .await
                .expect("the server must stop")
                .expect("the server task must not panic")
                .expect("serve must not fail");
        }
    }

    /// The records of finished requests, newest first.
    pub fn records(&self) -> Vec<Arc<RequestRecord>> {
        self.gateway
            .telemetry()
            .usage()
            .requests(&switchyard_telemetry::RequestQuery::default())
            .items
    }

    /// The record of a finished request, once it has been published.
    pub async fn record(&self, request_id: &str) -> Arc<RequestRecord> {
        for _ in 0..400 {
            if let Some(record) = self.gateway.telemetry().usage().get(request_id) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no record for request {request_id}");
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

/// reqwest is built without a crypto provider of its own; give the process
/// the one the gateway uses.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// An HTTP client that ignores the environment's proxy settings.
pub fn http() -> reqwest::Client {
    install_crypto();
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client")
}

/// Waits until `check` holds, polling briefly.
pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..600 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The events of an SSE body: `(event name, data)`.
pub fn sse_events(body: &str) -> Vec<(Option<String>, String)> {
    let mut events = Vec::new();
    for block in body.split("\n\n") {
        let mut name = None;
        let mut data: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event: ") {
                name = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data.push(value);
            }
        }
        if name.is_some() || !data.is_empty() {
            events.push((name, data.join("\n")));
        }
    }
    events
}

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A WebSocket client for the tests.
pub struct WsClient {
    pub socket: Socket,
    /// Headers of the `101` response.
    pub handshake: tokio_tungstenite::tungstenite::http::HeaderMap,
}

/// Opens a WebSocket with extra request headers.
pub async fn ws_connect(url: &str, headers: &[(&str, &str)]) -> Result<WsClient, WsError> {
    let mut request = url.into_client_request()?;
    for (name, value) in headers {
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    let (socket, response) = tokio_tungstenite::connect_async(request).await?;
    Ok(WsClient {
        socket,
        handshake: response.headers().clone(),
    })
}

/// What a client read from a socket.
#[derive(Debug, PartialEq)]
pub enum Received {
    Json(Value),
    Binary(Vec<u8>),
    Ping,
    Pong,
    Close(Option<(u16, String)>),
    /// The connection ended without a close frame.
    Gone,
}

impl WsClient {
    pub async fn send_json(&mut self, value: &Value) {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .expect("the message must be sent");
    }

    pub async fn send_text(&mut self, text: &str) {
        self.socket
            .send(Message::Text(text.to_string().into()))
            .await
            .expect("the message must be sent");
    }

    /// The next message of any kind.
    pub async fn receive(&mut self) -> Received {
        let next = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
            .await
            .expect("the server must say something");
        match next {
            Some(Ok(Message::Text(text))) => Received::Json(
                serde_json::from_str(text.as_str())
                    .unwrap_or_else(|_| Value::String(text.to_string())),
            ),
            Some(Ok(Message::Binary(bytes))) => Received::Binary(bytes.to_vec()),
            Some(Ok(Message::Ping(_))) => Received::Ping,
            Some(Ok(Message::Pong(_))) => Received::Pong,
            Some(Ok(Message::Close(frame))) => Received::Close(
                frame.map(|CloseFrame { code, reason }| (u16::from(code), reason.to_string())),
            ),
            Some(Ok(Message::Frame(_))) => unreachable!("raw frames are never read"),
            Some(Err(_)) | None => Received::Gone,
        }
    }

    /// The next data message as JSON, skipping pings and pongs.
    pub async fn next_json(&mut self) -> Value {
        loop {
            match self.receive().await {
                Received::Json(value) => return value,
                Received::Ping | Received::Pong => continue,
                other => panic!("expected a JSON frame, got {other:?}"),
            }
        }
    }

    /// Reads frames up to and including the first whose `type` is one of
    /// `until`.
    pub async fn read_until(&mut self, until: &[&str]) -> Vec<Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next_json().await;
            let kind = frame["type"].as_str().unwrap_or("").to_string();
            frames.push(frame);
            if until.contains(&kind.as_str()) {
                return frames;
            }
        }
    }

    /// Reads to the close frame (or the end of the connection), skipping
    /// everything else.
    pub async fn read_close(&mut self) -> Received {
        loop {
            match self.receive().await {
                Received::Gone => return Received::Gone,
                close @ Received::Close(_) => {
                    // Keep reading, as a real client's event loop would:
                    // that is what sends the acknowledging close frame.
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(Ok(_)) = self.socket.next().await {}
                    })
                    .await;
                    return close;
                }
                _ => continue,
            }
        }
    }
}

/// The `type` of each frame.
pub fn types(frames: &[Value]) -> Vec<&str> {
    frames
        .iter()
        .map(|frame| frame["type"].as_str().unwrap_or(""))
        .collect()
}
