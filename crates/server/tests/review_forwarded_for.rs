//! Review: `X-Forwarded-For` entries with a port let a client choose the
//! address its requests are recorded under.
//!
//! `app.rs::client_ip` takes, for loopback peers, "the last valid address,
//! the one the local proxy itself saw" — because everything further left is
//! only what a client claimed. But an entry counts as valid only when it
//! parses as a bare IP address. Reverse proxies that append `ip:port`
//! (IIS/ARR and Azure Application Gateway do by default; `[v6]:port` for
//! IPv6) produce a last entry that does not parse, so it is skipped and the
//! entry before it — the client's own claim — is recorded instead. With
//! nothing before it, the proxy's address is recorded and the real client
//! is lost.

mod support;

use serde_json::json;
use support::{KEY, TestServer, http};

async fn recorded_ip(server: &TestServer, forwarded: &str) -> Option<String> {
    let response = http()
        .post(server.url("/v1/chat/completions"))
        .bearer_auth(KEY)
        .header("x-forwarded-for", forwarded)
        .json(&json!({"model": "mock-echo",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    server.record(&id).await.client.ip.clone()
}

#[tokio::test]
async fn the_proxys_own_entry_counts_even_when_it_carries_a_port() {
    // The test client connects from loopback, like a reverse proxy on the
    // same machine.
    let server = TestServer::start().await;

    // What the proxy appended is the client; what precedes it is a claim.
    assert_eq!(
        recorded_ip(&server, "6.6.6.6, 203.0.113.9:4711")
            .await
            .as_deref(),
        Some("203.0.113.9"),
        "the client-supplied entry was recorded instead of the one the proxy appended"
    );
    assert_eq!(
        recorded_ip(&server, "6.6.6.6, [2001:db8::1]:443")
            .await
            .as_deref(),
        Some("2001:db8::1")
    );
    // Alone, it is still the client's address — not the proxy's.
    assert_eq!(
        recorded_ip(&server, "203.0.113.9:4711").await.as_deref(),
        Some("203.0.113.9")
    );
    // The plain form keeps working.
    assert_eq!(
        recorded_ip(&server, "6.6.6.6, 203.0.113.9")
            .await
            .as_deref(),
        Some("203.0.113.9")
    );
}
