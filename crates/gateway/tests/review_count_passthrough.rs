//! Regression test (review finding GW-6): token counting repairs a forwarded
//! Anthropic body the way generation does.
//!
//! When another vendor serves an Anthropic Messages client, the gateway
//! renders that vendor's reasoning as a `thinking` block with
//! `"signature": ""`; the client replays it; Anthropic answers such a block
//! with a 400. The Anthropic codec's `prepare_passthrough` therefore removes
//! those blocks from forwarded bodies, and both `generate` and
//! `count_tokens` run it — the latter without letting it add the
//! generation-only fields (`max_tokens`, `stream`) a counting endpoint
//! refuses.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use bytes::Bytes;
use serde_json::{Value, json};
use support::{FOUR_PROVIDERS, Harness, Kind, Output};
use switchyard_core::Protocol;
use switchyard_gateway::ClientRequest;

fn history() -> Value {
    json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            // Issued by the gateway itself while another vendor served
            // this conversation.
            {"type": "thinking", "thinking": "pondering the greeting", "signature": ""},
            {"type": "text", "text": "hello"}
        ]},
        {"role": "user", "content": "and again"}
    ])
}

#[tokio::test]
async fn counting_repairs_the_history_the_way_generation_does() {
    let harness = Harness::start(FOUR_PROVIDERS).await;

    // Generation: the unsigned thinking block is not sent to Anthropic.
    let body = json!({"model": "m-anthropic", "max_tokens": 256, "messages": history()});
    let output = harness
        .ask_with(Protocol::Anthropic, body, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200);
    let generated = harness.fake.last();
    assert!(
        !generated.body.to_string().contains("pondering"),
        "generation forwards the unsigned block: {}",
        generated.body
    );

    // Counting the same conversation.
    let request = ClientRequest::new(
        Protocol::Anthropic,
        "POST /v1/messages/count_tokens",
        Bytes::from(json!({"model": "m-anthropic", "messages": history()}).to_string()),
        harness.identity(),
    );
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 200);
    let counted = harness.fake.last();
    assert_eq!(counted.kind, Kind::Count);
    assert_eq!(counted.path, "/v1/messages/count_tokens");
    assert!(
        !counted.body.to_string().contains("pondering"),
        "counting forwards a thinking block without a signature, which Anthropic rejects with a 400: {}",
        counted.body
    );
    // A counting body must stay a counting body.
    assert_eq!(counted.body.get("max_tokens"), None);
    assert_eq!(counted.body.get("stream"), None);
}
