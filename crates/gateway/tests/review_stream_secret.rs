//! Regression tests (review finding GW-3): an upstream error event that
//! arrives *after* the first event of a stream reaches the client without
//! the upstream credential.
//!
//! `switchyard_upstream` scrubs the credentials a call presented from HTTP
//! error bodies and tells the gateway to do the same for "text that reaches
//! the gateway by another route — an error event inside a stream" with
//! `Target::redact`. That holds before the first event (the bootstrap), for
//! the request record, and once the stream is committed: on the translation
//! path the error is re-encoded with its message scrubbed, on the
//! passthrough path the forwarded event is. A careless upstream (or a proxy
//! in front of it) that quotes the key it was called with does not hand the
//! gateway's upstream credential to the gateway's client.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use serde_json::json;
use support::Harness;
use switchyard_core::Protocol;

const UPSTREAM_KEY: &str = "sk-ant-api03-supersecretkeymaterial0123456789";

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// An Anthropic upstream whose stream starts normally and then fails with
/// an error that quotes the key the request presented.
async fn leaky_stream(headers: HeaderMap) -> Response {
    let key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let frame = |event: &str, data: serde_json::Value| format!("event: {event}\ndata: {data}\n\n");
    let frames = [
        frame(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-up",
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }}),
        ),
        frame(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        frame(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hel"}}),
        ),
        frame(
            "error",
            json!({"type": "error", "error": {"type": "api_error",
                   "message": format!("worker crashed while serving key {key}")}}),
        ),
    ];
    let stream = futures::stream::iter(
        frames
            .into_iter()
            .map(|frame| Ok::<_, std::io::Error>(Bytes::from(frame))),
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn harness() -> Harness {
    let base = serve(Router::new().fallback(leaky_stream)).await;
    Harness::start(&format!(
        r#"
[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "{base}"
api_keys = ["{UPSTREAM_KEY}"]
[[providers.models]]
id = "claude-up"
alias = "m"
"#
    ))
    .await
}

async fn assert_no_leak(client: Protocol) {
    let harness = harness().await;
    let output = harness.ask(client, "m", true).await;
    assert!(output.streamed, "the stream was committed before the error");
    let text = output.wire_text();
    // The client is told the stream failed …
    assert!(text.contains("worker crashed"), "{text}");
    // … the request record has the message without the key …
    let record = harness.record(&output.request_id);
    let recorded = record.error.as_ref().expect("the failure is recorded");
    assert!(!recorded.message.contains(UPSTREAM_KEY), "{recorded:?}");
    // … and the gateway's upstream credential is not in what the client
    // received.
    assert!(
        !text.contains(UPSTREAM_KEY),
        "{client} client: the upstream API key reached the client:\n{text}"
    );
}

#[tokio::test]
async fn a_translated_stream_error_does_not_carry_the_upstream_key() {
    assert_no_leak(Protocol::OpenaiChat).await;
}

#[tokio::test]
async fn a_passthrough_stream_error_does_not_carry_the_upstream_key() {
    assert_no_leak(Protocol::Anthropic).await;
}
