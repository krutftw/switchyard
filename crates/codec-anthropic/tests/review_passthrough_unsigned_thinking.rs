//! Regression tests for review finding ANTH-2, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence: thinking blocks the gateway itself hands to Messages
//! clients with `"signature": ""` are later forwarded verbatim to Anthropic.
//!
//! Chain of events, each step taken from this crate or from DESIGN.md:
//!
//! 1. A Messages client is served by a non-Anthropic upstream. Reasoning that
//!    has no signature (Chat `reasoning_content`, Gemini thought summaries)
//!    is rendered by `encode_response` / the stream encoder as
//!    `{"type":"thinking","thinking":"…","signature":""}`.
//! 2. The client replays its history, including that block (Claude Code and
//!    the Anthropic SDKs keep thinking blocks).
//! 3. A later request of the same conversation is routed to an Anthropic
//!    upstream (alias fail-over, cooldown on the first provider, `/model`).
//!    The body contains no wrapped signature (`sy1.`), so DESIGN.md section 2
//!    selects **passthrough**: the client's JSON is forwarded after
//!    `prepare_passthrough`.
//! 4. Anthropic verifies thinking signatures and has no bypass value: an
//!    empty / missing / foreign signature is a 400 (notes 09 section 7.1 and
//!    section 6 step 4: "thinking blocks whose signature is not recognised as
//!    an Anthropic signature (empty, missing, …) are dropped (whole block) —
//!    Anthropic … rejects invalid signatures with 400"). A 400 is a request
//!    fault, so there is no fail-over and the conversation is stuck.
//!
//! The reference removes such blocks on Claude -> Claude passthrough. Here
//! `prepare_passthrough` is the only hook that sees the body, and it leaves
//! them in (`raw_body.rs::prepare_passthrough_changes_nothing_in_a_complete_body`
//! even pins that).

use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{Part, Response};
use switchyard_core::{ClientCtx, Codec, UpstreamCtx, sig};

fn thinking_blocks(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "thinking")
        .collect()
}

#[test]
fn review_unsigned_thinking_emitted_by_the_gateway_is_not_forwarded_to_anthropic() {
    let codec = AnthropicCodec;

    // Step 1: a translated response with unsigned reasoning.
    let mut response = Response::new("chatcmpl-1", "deepseek-reasoner");
    response.parts = vec![Part::reasoning("The user greets me."), Part::text("Hello!")];
    let rendered = codec
        .encode_response(&response, &ClientCtx::new("smart"))
        .expect("encodes");
    assert_eq!(
        rendered["content"][0],
        json!({"type": "thinking", "thinking": "The user greets me.", "signature": ""})
    );

    // Step 2 + 3: the client replays it; the next attempt goes to Anthropic.
    let mut body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": rendered["content"].clone()},
            {"role": "user", "content": "and now?"}
        ]
    });
    // Nothing in the body makes the gateway leave the passthrough path.
    assert!(!sig::contains_wrapped(body.to_string().as_bytes()));

    codec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());

    // Step 4: the block Anthropic is guaranteed to reject must be gone, the
    // rest of the turn must stay.
    let unsigned: Vec<Value> = thinking_blocks(&body)
        .into_iter()
        .filter(|block| block["signature"].as_str().unwrap_or("").is_empty())
        .collect();
    assert!(
        unsigned.is_empty(),
        "thinking block without a signature forwarded to Anthropic: {unsigned:?}"
    );
    assert_eq!(
        body["messages"][1]["content"],
        json!([{"type": "text", "text": "Hello!"}])
    );
}

#[test]
fn review_signed_thinking_is_left_alone_by_passthrough() {
    // Guard for the fix: native blocks must stay byte-identical.
    let original = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBBgCKkD"},
                {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"},
                {"type": "text", "text": "Hello!"}
            ]},
            {"role": "user", "content": "and now?"}
        ]
    });
    let mut body = original.clone();
    AnthropicCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());
    assert_eq!(body, original);
}
