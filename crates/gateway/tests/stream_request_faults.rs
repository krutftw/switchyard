//! One client's unusable input must not rest a model for everybody.
//!
//! The Responses API answers a request whose image it cannot fetch or read
//! with `200`, then `response.failed` and a code such as
//! `failed_to_download_image`. Taken for an upstream failure, that would be
//! retried on the next credential — which fails the same way — and rest the
//! model on each of them: a client with a dead image URL could take a model
//! out of rotation for everyone, as often as it likes. It is a fault of the
//! request: no failover, nothing rests, and the client is told `400`.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{Harness, Output};
use switchyard_core::Protocol;
use switchyard_scheduler::CredentialStatus;

const MESSAGE: &str = "Failed to download image from https://client.example/dead.png.";

/// A Responses upstream that starts every stream and then fails it the way
/// OpenAI does for an image it cannot download. Counts the calls.
async fn upstream(calls: Arc<AtomicUsize>) -> String {
    let app = Router::new().fallback(move || {
        let calls = Arc::clone(&calls);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let created = json!({"type": "response.created", "sequence_number": 0, "response": {
                "id": "resp_1", "object": "response", "created_at": 1, "status": "in_progress",
                "model": "up-responses", "output": []
            }});
            let failed = json!({"type": "response.failed", "sequence_number": 1, "response": {
                "id": "resp_1", "object": "response", "created_at": 1, "status": "failed",
                "model": "up-responses", "output": [],
                "error": {"code": "failed_to_download_image", "message": MESSAGE}
            }});
            let body = format!(
                "event: response.created\ndata: {created}\n\nevent: response.failed\ndata: {failed}\n\n"
            );
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn config(base: &str) -> String {
    format!(
        r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "oai"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#
    )
}

#[tokio::test]
async fn an_image_the_upstream_cannot_fetch_is_the_requests_fault() {
    for client in [
        Protocol::OpenaiResponses,
        Protocol::OpenaiChat,
        Protocol::Anthropic,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = upstream(Arc::clone(&calls)).await;
        let harness = Harness::start(&config(&base)).await;

        let output = harness.ask(client, "m", true).await;
        let said = if output.streamed {
            output.wire_text()
        } else {
            String::from_utf8_lossy(&output.body).to_string()
        };
        // Whether the failure was seen before anything was sent (a 400) or
        // after (in-band), the client is told what is wrong with its image.
        assert!(
            said.contains("Failed to download image"),
            "{client}: {said}"
        );
        if !output.streamed {
            assert_eq!(output.status, 400, "{client}: {said}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{client}: no other credential is tried with the same image"
        );
        for credential in &harness.gateway.scheduler().snapshot()[0].credentials {
            assert_eq!(
                credential.status,
                CredentialStatus::Ready,
                "{client}: {credential:?}"
            );
            assert!(credential.model_cooldowns.is_empty(), "{client}");
            assert_eq!(credential.failures, 0, "{client}");
        }
        let record = harness.record(&output.request_id);
        assert_eq!(record.attempts.len(), 1, "{client}");

        // The model is still in rotation: the next request reaches the
        // upstream (with the first credential, which did nothing wrong).
        let again = Output::read(
            harness
                .gateway
                .generate(harness.request(client, "m", true))
                .await,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{client}");
        assert!(again.status == 400 || again.streamed, "{client}");
    }
}
