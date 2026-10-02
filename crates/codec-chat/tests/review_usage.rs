//! Review findings for usage decoding.
//!
//! Every test here asserts the correct behaviour. They failed against the
//! reviewed implementation and are kept as regression tests.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_chat::ChatCodec;
use switchyard_core::Usage;
use switchyard_core::codec::Codec;

// ---------------------------------------------------------------------------
// Finding: reasoning tokens reported OUTSIDE `completion_tokens` are thrown
// away.
//
// OpenAI counts reasoning inside `completion_tokens`. Several "compatible"
// servers that serve reasoning models do not: they report the visible
// completion in `completion_tokens`, the thinking in
// `completion_tokens_details.reasoning_tokens`, and a `total_tokens` that is
// the sum of all three. The payload itself proves which convention is in
// use: `total_tokens == prompt_tokens + completion_tokens + reasoning_tokens`
// cannot hold when reasoning is a subset of completion.
//
// `Usage::from_inclusive` clamps `reasoning` to `output_total`, so such a
// body decodes to output 5 / reasoning 5: 100 billed output tokens vanish
// from the gateway's accounting and cost estimate. The canonical buckets
// must be output = 105 (everything generated), reasoning = 100.
// ---------------------------------------------------------------------------

fn expected() -> Usage {
    Usage {
        input_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 105,
        reasoning_tokens: 100,
    }
}

#[test]
fn review_reasoning_tokens_outside_completion_tokens_are_counted_response() {
    let response = ChatCodec
        .decode_response(&json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "reasoner",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 115,
                "completion_tokens_details": {"reasoning_tokens": 100}
            }
        }))
        .unwrap();
    assert_eq!(response.usage, expected());
}

#[test]
fn review_reasoning_tokens_outside_completion_tokens_are_counted_stream() {
    let transcript = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "reasoner",
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "reasoner",
               "choices": [],
               "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 115,
                         "completion_tokens_details": {"reasoning_tokens": 100}}})
    );
    assert_eq!(accumulate(&decode_stream(&transcript)).usage, expected());
}

/// Guard: the OpenAI convention (reasoning inside completion,
/// total = prompt + completion) must keep decoding as it does today.
#[test]
fn review_openai_convention_is_unchanged() {
    let response = ChatCodec
        .decode_response(&json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": "o-x",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 105,
                "total_tokens": 115,
                "completion_tokens_details": {"reasoning_tokens": 100}
            }
        }))
        .unwrap();
    assert_eq!(response.usage, expected());
}
