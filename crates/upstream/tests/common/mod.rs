//! Helpers shared by the integration tests.
#![allow(dead_code)]

use axum::Router;
use std::net::SocketAddr;
use std::time::Duration;
use switchyard_core::Protocol;
use switchyard_core::config::{ProviderKind, ProxySetting};
use switchyard_upstream::{Auth, Target, Timeouts, UpstreamClient};

/// The API key every test target authenticates with.
pub const KEY: &str = "sk-upstream-test-key-0123456789";

/// Serves `app` on an ephemeral loopback port.
pub async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// A loopback address nothing listens on.
pub async fn dead_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// A target that talks to a local test server directly (never through a
/// proxy the developer's environment may define).
pub fn target(
    kind: ProviderKind,
    protocol: Protocol,
    base_url: impl Into<String>,
    model: &str,
) -> Target {
    Target {
        provider: "test".into(),
        kind,
        base_url: base_url.into(),
        protocol,
        model: model.into(),
        auth: Auth::ApiKey(KEY.into()),
        headers: Vec::new(),
        proxy: ProxySetting::Direct,
        project: String::new(),
        location: String::new(),
    }
}

pub fn openai(addr: SocketAddr) -> Target {
    target(
        ProviderKind::Openai,
        Protocol::OpenaiChat,
        format!("http://{addr}/v1"),
        "gpt-test",
    )
}

pub fn client() -> UpstreamClient {
    UpstreamClient::new().unwrap()
}

pub fn timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_secs(10),
    }
}
