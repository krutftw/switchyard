//! Regression tests for review findings ANTH-6 and ANTH-7, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence for two smaller request-side defects.

mod common;

use common::{decode_request, encode_request};
use serde_json::json;
use switchyard_core::Protocol;
use switchyard_core::ir::{Message, Part, Request, Role};
use switchyard_core::reasoning::{Depth, ReasoningConfig};

/// `ir.rs`: "system/developer instructions that precede the conversation
/// live in `Request::system`; ones that appear mid-conversation stay in
/// `Request::messages` with `Role::System`" (also DESIGN.md section 2:
/// "Leading system text lives in `Request::system`").
///
/// A `role: "system"` message at the head of `messages` (legal on current
/// models, and what clients written for OpenAI send) precedes the
/// conversation, yet the decoder leaves it in `messages`. Every encoder then
/// treats the operator's instructions as a mid-conversation remark; this
/// crate's own encoder turns them into `<system>` *user* text instead of the
/// top-level `system` field.
#[test]
fn review_leading_system_role_messages_are_request_system() {
    let request = decode_request(&json!({
        "model": "claude-sonnet-5", "max_tokens": 64,
        "system": "Top-level instructions.",
        "messages": [
            {"role": "system", "content": "You answer in French."},
            {"role": "developer", "content": [{"type": "text", "text": "Be brief."}]},
            {"role": "user", "content": "hi"},
            {"role": "system", "content": "From now on answer in German."},
            {"role": "user", "content": "again"}
        ]
    }));
    assert_eq!(
        request.system,
        vec![
            Part::text("Top-level instructions."),
            Part::text("You answer in French."),
            Part::text("Be brief."),
        ]
    );
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("hi"),
            Message::new(
                Role::System,
                vec![Part::text("From now on answer in German.")]
            ),
            Message::user_text("again"),
        ]
    );
}

/// DESIGN.md section 3: `encode_request` "must keep the body valid
/// (Anthropic: `budget_tokens < max_tokens` …)"; `Codec::write_reasoning`:
/// "Must also keep the body valid for the protocol".
///
/// When the model's output limit is unknown and the client's limit is at or
/// below the minimum budget, the encoder writes `budget_tokens >=
/// max_tokens`, which the API rejects with 400. Either thinking has to go or
/// `max_tokens` has to grow; emitting a body that cannot succeed is the one
/// thing the contract rules out.
#[test]
fn review_budget_is_below_max_tokens_even_without_model_metadata() {
    for max_tokens in [256_u64, 1000, 1024] {
        let mut request = Request::new("some-claude-compatible-model", Protocol::Gemini);
        request.messages = vec![Message::user_text("hi")];
        request.max_output_tokens = Some(max_tokens);
        request.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
        let body = encode_request(&request);
        if body["thinking"]["type"] == "enabled" {
            let budget = body["thinking"]["budget_tokens"].as_u64().expect("budget");
            let limit = body["max_tokens"].as_u64().expect("max_tokens");
            assert!(
                budget < limit,
                "client max_tokens {max_tokens}: budget_tokens {budget} >= max_tokens {limit}"
            );
            assert!(
                budget >= 1024,
                "budget_tokens {budget} is below the API minimum"
            );
        }
    }
}
