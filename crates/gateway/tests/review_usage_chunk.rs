//! Regression tests (review finding GW-4): Chat Completions passthrough does
//! not hand the client a usage-only chunk it never asked for.
//!
//! On a same-protocol Chat stream the gateway forces
//! `stream_options.include_usage = true` upstream (it needs the numbers for
//! accounting) and forwards the upstream's events — except the extra
//! `{"choices": [], "usage": {...}}` chunk, which the real API only sends to
//! clients that set `include_usage`: clients that index `choices[0]` on
//! every chunk break on it. The translation path honours the client's own
//! `stream_options` in the Chat stream encoder; passthrough does the same
//! (15-api-research, "Usage in streams": "if the client did not ask for it,
//! DROP the usage-only chunk before forwarding").

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use serde_json::{Value, json};
use support::{FOUR_PROVIDERS, Harness};
use switchyard_core::Protocol;

fn usage_only_chunks(events: &[switchyard_core::SseEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            serde_json::from_str::<Value>(&event.data).is_ok_and(|value| {
                value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            })
        })
        .map(|event| event.data.clone())
        .collect()
}

#[tokio::test]
async fn a_chat_client_that_did_not_ask_for_usage_gets_no_usage_chunk() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for stream_options in [None, Some(json!({"include_usage": false}))] {
        let mut body = json!({
            "model": "m-chat",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        if let Some(options) = &stream_options {
            body["stream_options"] = options.clone();
        }
        let output = harness
            .ask_with(Protocol::OpenaiChat, body, "m-chat", true)
            .await;
        assert!(output.streamed);

        // The gateway still asks the upstream for usage, and records it.
        assert_eq!(
            harness.fake.last().body["stream_options"]["include_usage"],
            true
        );
        let record = harness.record(&output.request_id);
        assert_eq!(record.usage.input_tokens, 11);
        assert_eq!(record.usage.output_tokens, 7);

        // But the client, which did not ask, is not sent the extra chunk.
        let extra = usage_only_chunks(&output.events);
        assert!(
            extra.is_empty(),
            "stream_options = {stream_options:?}: the client received a usage-only chunk it did not ask for: {extra:?}"
        );
        assert_eq!(
            output.response(Protocol::OpenaiChat).text(),
            "Hello from the fake upstream"
        );
    }
}

/// The control: a client that asks for usage gets the chunk.
#[tokio::test]
async fn a_chat_client_that_asked_for_usage_gets_the_usage_chunk() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let body = json!({
        "model": "m-chat",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, body, "m-chat", true)
        .await;
    assert_eq!(usage_only_chunks(&output.events).len(), 1);
}
