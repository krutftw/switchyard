//! Regression tests (review finding GW-7, GW-8): what request records and
//! captured bodies say about attempts that did not end in a successful
//! response.
//!
//! * The captured upstream response is the answer of the *last* attempt —
//!   its error body when it failed — never the answer of an earlier one.
//! * A request the client abandons while an upstream call is in progress
//!   still names the provider, credential and model that were being called.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use std::time::Duration;
use support::{Behaviour, FOUR_PROVIDERS, Harness, Output};
use switchyard_core::Protocol;
use tokio_util::sync::CancellationToken;

const LOG_ERRORS: &str = "[logging]\nrequest_log = \"errors\"\n";

/// `logging.request_log = "errors"` exists to see why a request failed, and
/// `CapturedBodies::upstream_response` is "what the upstream answered on the
/// last attempt" (`UpstreamError::body` is kept "for request logs"): for an
/// upstream HTTP error — the usual reason a request fails — the capture
/// holds the upstream's error body.
#[tokio::test]
async fn the_upstream_error_body_of_a_failed_request_is_captured() {
    let harness = Harness::start(&format!("{LOG_ERRORS}\n{FOUR_PROVIDERS}")).await;
    harness.fake.always(
        "key-chat-1",
        Behaviour::error(500, "the upstream exploded in a very specific way"),
    );
    // Translated, so the client's copy of the error is the gateway's
    // rendering and the upstream's own body exists nowhere else.
    let output = harness.ask(Protocol::Anthropic, "m-chat", false).await;
    assert_eq!(output.status, 502);
    assert!(harness.record(&output.request_id).has_bodies);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .expect("a failed request is captured in `errors` mode");
    assert!(bodies.upstream_request.is_some());
    let upstream = bodies
        .upstream_response
        .expect("the upstream's answer on the last attempt is captured");
    assert!(
        upstream.contains("the upstream exploded in a very specific way"),
        "{upstream}"
    );
}

/// With failover, the capture pairs the upstream *request* of the last
/// attempt with the upstream *response* of that same attempt, not of an
/// earlier one.
#[tokio::test]
async fn the_captured_upstream_response_belongs_to_the_last_attempt() {
    let config = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(&format!("{LOG_ERRORS}\n{config}")).await;
    // First attempt: a 200 whose stream fails at once (captured as the
    // upstream response). Second attempt: a plain HTTP 500.
    harness.fake.script(
        "key-a",
        [Behaviour::StreamError {
            status: 500,
            message: "first attempt: in-stream failure".to_string(),
        }],
    );
    harness.fake.script(
        "key-b",
        [Behaviour::error(500, "second attempt: http failure")],
    );
    let output = harness.ask(Protocol::Anthropic, "m", true).await;
    assert!(!output.streamed);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .expect("captured");
    let upstream = bodies.upstream_response.unwrap_or_default();
    assert!(
        upstream.contains("second attempt: http failure"),
        "the captured upstream response is not the last attempt's: {upstream}"
    );
    assert!(
        !upstream.contains("first attempt"),
        "the captured upstream response is an earlier attempt's: {upstream}"
    );
}

/// A request the client abandons while an upstream call is in progress was
/// still sent to a provider with a credential (and may well be billed).
/// The record says so: provider, credential, upstream model, and the
/// abandoned attempt.
#[tokio::test]
async fn a_cancelled_request_records_the_attempt_that_was_in_progress() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.script(
        "key-chat-1",
        [Behaviour::Slow {
            delay: Duration::from_secs(20),
            then: Box::new(Behaviour::text("never seen")),
        }],
    );
    let cancel = CancellationToken::new();
    let mut request = harness.request(Protocol::OpenaiChat, "m-chat", false);
    request.cancel = cancel.clone();
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.generate(request).await });
    support::eventually("the upstream call is in progress", || {
        harness.fake.count() == 1
    })
    .await;
    cancel.cancel();
    let output = Output::read(pending.await.unwrap()).await;
    assert_eq!(output.status, 499);

    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 499);
    assert_eq!(record.provider.as_deref(), Some("chat"), "{record:?}");
    assert_eq!(record.upstream_model.as_deref(), Some("up-chat"));
    assert!(record.credential_id.is_some());
    assert_eq!(record.attempts.len(), 1, "the upstream call was made");
    assert!(!record.attempts[0].ok);
}
