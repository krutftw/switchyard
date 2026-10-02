//! Shared test support: a gateway on a temporary configuration with the
//! mock provider, the admin router served on `127.0.0.1:0`, an HTTP client
//! that signs in, and a WebSocket client for the live-event stream.
#![allow(dead_code)]

use axum::extract::{ConnectInfo, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use http::{HeaderMap, Method, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use switchyard_admin::{AdminHandle, AdminOptions};
use switchyard_core::Protocol;
use switchyard_gateway::{ClientRequest, Gateway, GatewayOptions, PresentedCredentials, Reply};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The admin secret of [`BASE`].
pub const SECRET: &str = "test-admin-secret-0123456789abcdef";

/// The client key of [`BASE`].
pub const CLIENT_KEY: &str = "sy-test-client-key-0123456789abcdef";

/// A configuration with an admin secret, one client key and the mock
/// provider. The comments are there to be preserved.
pub const BASE: &str = r#"# Switchyard test configuration.

[admin]
# The dashboard signs in with this.
secret = "test-admin-secret-0123456789abcdef"

[upstream]
proxy = "direct" # never the developer's proxy

[[auth.keys]]
key = "sy-test-client-key-0123456789abcdef"
name = "tester"

# The built-in fake models.
[[providers]]
name = "mock"
kind = "mock"
"#;

/// Lets a test choose the peer address a request appears to come from:
/// the `x-test-peer` header replaces the connection info the server
/// recorded. The header is test scaffolding; the admin router never reads
/// it.
async fn fake_peer(mut request: Request, next: Next) -> Response {
    let peer = request
        .headers()
        .get("x-test-peer")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<SocketAddr>().ok());
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    next.run(request).await
}

/// A running gateway with its admin interface.
pub struct App {
    pub gateway: Gateway,
    pub handle: AdminHandle,
    pub addr: SocketAddr,
    pub dir: TempDir,
    pub config_path: PathBuf,
    pub http: reqwest::Client,
    /// The secret [`App::send`] signs in with.
    pub secret: String,
    /// Stops the server gracefully when sent to (or dropped).
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
}

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl App {
    /// [`BASE`] with the default options.
    pub async fn start() -> App {
        App::start_with(BASE, AdminOptions::default()).await
    }

    pub async fn start_config(config: &str) -> App {
        App::start_with(config, AdminOptions::default()).await
    }

    pub async fn start_with(config: &str, mut options: AdminOptions) -> App {
        // reqwest is built without a TLS provider of its own.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = tempfile::tempdir().expect("a temporary directory");
        let config_path = dir.path().join("switchyard.toml");
        std::fs::write(&config_path, config).expect("the configuration file");
        let gateway = Gateway::start(GatewayOptions::new(&config_path).watch(false))
            .await
            .unwrap_or_else(|error| panic!("the gateway must start: {error}"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listening socket");
        let addr = listener.local_addr().expect("the bound address");
        options.listen = Some(addr);
        let secret = options
            .secret_override
            .clone()
            .unwrap_or_else(|| SECRET.to_string());
        let (router, handle) = switchyard_admin::router_with_handle(gateway.clone(), options);
        let app = router.layer(middleware::from_fn(fake_peer));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let service = app.into_make_service_with_connect_info::<SocketAddr>();
            let _ = axum::serve(listener, service)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await;
        });

        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client");
        App {
            gateway,
            handle,
            addr,
            dir,
            config_path,
            http,
            secret,
            stop: Some(stop),
        }
    }

    /// `http://<addr><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// `http://<addr>/admin/api<path>`.
    pub fn api(&self, path: &str) -> String {
        self.url(&format!("/admin/api{path}"))
    }

    /// A request to the admin API carrying the secret.
    pub fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.api(path))
            .bearer_auth(&self.secret)
    }

    /// Sends a signed-in request and reads the answer as JSON (`null` for
    /// an empty or non-JSON body).
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        read(request).await
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, path, Some(body)).await
    }

    pub async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::PUT, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.send(Method::DELETE, path, None).await
    }

    /// A 200 answer's body; panics with the body on any other status.
    pub async fn get_ok(&self, path: &str) -> Value {
        let (status, body) = self.get(path).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {body}");
        body
    }

    /// The configuration file as it is on disk.
    pub fn file(&self) -> String {
        std::fs::read_to_string(&self.config_path).expect("the configuration file")
    }

    /// A ticket for the live-event WebSocket.
    pub async fn ticket(&self) -> String {
        let (status, body) = self.post("/ws-ticket", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["ticket"].as_str().expect("a ticket").to_string()
    }

    pub fn ws_url(&self, ticket: &str) -> String {
        format!("ws://{}/admin/api/ws?ticket={ticket}", self.addr)
    }

    /// Opens the live-event WebSocket and returns it with its `hello`
    /// frame already read.
    pub async fn live(&self) -> (Socket, Value) {
        let ticket = self.ticket().await;
        let (mut socket, _) = tokio_tungstenite::connect_async(self.ws_url(&ticket))
            .await
            .expect("the WebSocket upgrade");
        let hello = next_frame(&mut socket).await;
        assert_eq!(hello["type"], "hello", "{hello}");
        (socket, hello)
    }

    /// The status a WebSocket upgrade is refused with (`101` when it is
    /// accepted).
    pub async fn upgrade_status(&self, url: &str, headers: &[(&'static str, &str)]) -> u16 {
        let mut request = url.into_client_request().expect("a WebSocket request");
        for (name, value) in headers {
            request
                .headers_mut()
                .insert(*name, value.parse().expect("a header value"));
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok(_) => 101,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                response.status().as_u16()
            }
            Err(other) => panic!("unexpected upgrade failure: {other}"),
        }
    }

    /// One Chat Completions request through the pipeline as `key`. Returns
    /// the status.
    pub async fn chat(&self, key: &str, model: &str) -> u16 {
        let identity = self
            .gateway
            .authenticate(&PresentedCredentials {
                x_api_key: Some(key.to_string()),
                ..PresentedCredentials::default()
            })
            .expect("the key must authenticate");
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello there"}],
        });
        let request = ClientRequest::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            serde_json::to_vec(&body).expect("a body"),
            identity,
        );
        match self.gateway.generate(request).await {
            Reply::Full(full) => full.status,
            Reply::Stream(_) => 200,
        }
    }
}

/// Sends a request and reads the answer as JSON.
pub async fn read(request: reqwest::RequestBuilder) -> (StatusCode, Value) {
    let (status, _, body) = read_with_headers(request).await;
    (status, body)
}

/// Sends a request and returns status, headers and the body as JSON.
pub async fn read_with_headers(request: reqwest::RequestBuilder) -> (StatusCode, HeaderMap, Value) {
    let response = request.send().await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await.expect("a body");
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, headers, body)
}

/// How long a test waits for a frame before failing.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The next JSON text frame. WebSocket pings are answered and skipped.
pub async fn next_frame(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(FRAME_TIMEOUT, socket.next())
            .await
            .expect("a frame in time")
            .expect("the socket is open")
            .expect("a readable frame");
        match message {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str()).expect("a JSON frame");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected WebSocket message: {other:?}"),
        }
    }
}

/// Reads frames until one of `kind` arrives and returns it together with
/// the types of the frames that came before it.
pub async fn frame_of(socket: &mut Socket, kind: &str) -> (Value, Vec<String>) {
    let mut skipped = Vec::new();
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == kind {
            return (frame, skipped);
        }
        skipped.push(frame["type"].as_str().unwrap_or_default().to_string());
    }
}

/// Reads frames until one satisfies `wanted`.
pub async fn frame_where(socket: &mut Socket, wanted: impl Fn(&Value) -> bool) -> Value {
    loop {
        let frame = next_frame(socket).await;
        if wanted(&frame) {
            return frame;
        }
    }
}

/// Sends a JSON message to the gateway.
pub async fn send_json(socket: &mut Socket, value: Value) {
    socket
        .send(Message::text(value.to_string()))
        .await
        .expect("the message is sent");
}

/// Narrows the socket's topics and waits for the acknowledgement.
pub async fn subscribe(socket: &mut Socket, topics: &[&str]) -> Vec<String> {
    send_json(socket, json!({"type": "subscribe", "topics": topics})).await;
    let (ack, skipped) = frame_of(socket, "subscribed").await;
    assert_eq!(ack["data"]["topics"], json!(topics), "{ack}");
    skipped
}

/// Every string in a JSON value, recursively: for looking for secrets.
pub fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                strings(item, out);
            }
        }
        _ => {}
    }
}

/// Polls `check` until it returns `Some`, for at most two seconds.
pub async fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the condition did not hold within two seconds"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
