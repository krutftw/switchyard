//! A WebSocket ticket is a credential: it never appears in a log line (at
//! any level, the HTTP internals included) or in a request record.
//!
//! A test binary of its own, with a single test: it installs the process's
//! log subscriber.

mod support;

use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use support::{KEY, Settings, TestServer, http, ws_connect};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

/// Every formatted log line, kept in memory.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tickets_never_reach_logs_or_request_records() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_filter(tracing_subscriber::filter::LevelFilter::TRACE),
    );
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let server = TestServer::with(Settings::default()).await;
    let minted: Value = http()
        .post(server.url("/v1/ws-ticket"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ticket = minted["ticket"].as_str().unwrap().to_string();

    // A turn over the socket, through an upstream that records what it is
    // sent, and a turn that fails.
    let mut client = ws_connect(
        &server.ws_url(&format!("/v1/responses?model=x&ticket={ticket}")),
        &[],
    )
    .await
    .expect("the ticket opens the socket");
    client
        .send_json(&json!({"type": "response.create", "model": "recorded", "input": []}))
        .await;
    client.read_until(&["response.completed", "error"]).await;
    client
        .send_json(&json!({"type": "response.create", "model": "mock-error-500", "input": []}))
        .await;
    client.read_until(&["error"]).await;
    // The relay, and the refusal of a used ticket.
    let used = ws_connect(
        &server.ws_url(&format!("/v1/realtime?model=rt&ticket={ticket}")),
        &[],
    )
    .await;
    assert!(used.is_err(), "used up");
    let fresh: Value = http()
        .post(server.url("/v1/ws-ticket"))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fresh = fresh["ticket"].as_str().unwrap().to_string();
    let mut relay = ws_connect(
        &server.ws_url(&format!("/v1/realtime?model=rt&ticket={fresh}")),
        &[],
    )
    .await
    .expect("a fresh ticket opens the relay");
    relay.receive().await;
    drop(relay);
    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let logs = captured.text();
    assert!(
        logs.contains("/v1/ws-ticket"),
        "the test saw the access log"
    );
    for secret in [&ticket, &fresh] {
        assert!(
            !logs.contains(secret.as_str()),
            "a ticket was logged:\n{logs}"
        );
    }
    let records = serde_json::to_string(&server.records()).unwrap();
    assert!(records.contains("WS /v1/responses"));
    for secret in [&ticket, &fresh] {
        assert!(!records.contains(secret.as_str()), "{records}");
    }
    for seen in server.fake.recorded() {
        for secret in [&ticket, &fresh] {
            assert!(!seen.query.contains(secret.as_str()), "{seen:?}");
        }
    }
}
