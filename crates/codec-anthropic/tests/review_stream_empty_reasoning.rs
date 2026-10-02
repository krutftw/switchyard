//! Regression tests for review finding ANTH-5, written by the reviewer
//! against the defective code. The text below describes the behaviour
//! before the fix and is kept as the rationale for the tests.
//!
//! Review evidence: the stream encoder renders a reasoning block that has
//! neither text nor a signature as an empty `thinking` block, while
//! `encode_response` drops the very same part.
//!
//! Such blocks are produced for reasoning items that carry nothing the
//! client may see (an OpenAI Responses reasoning item without summary and
//! without encrypted content, a withheld Gemini thought). On the wire the
//! client receives
//!
//! ```text
//! content_block_start {"type":"thinking","thinking":"","signature":""}
//! content_block_stop
//! ```
//!
//! i.e. a thinking block with an empty signature that it stores in its
//! history. It carries no information, and once replayed to an Anthropic
//! upstream it is a guaranteed 400 (see
//! `review_passthrough_unsigned_thinking.rs`). The two client-side encoders
//! must agree, and DESIGN.md section 3 asks stream encoders to drop blocks
//! the protocol cannot express.

mod common;

use common::{encode_response, encode_stream, wire};
use serde_json::{Value, json};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Response};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};

fn started_blocks(events: &[(String, Value)]) -> Vec<Value> {
    events
        .iter()
        .filter(|(name, _)| name == "content_block_start")
        .map(|(_, data)| data["content_block"].clone())
        .collect()
}

#[test]
fn review_stream_encoder_drops_reasoning_without_text_and_signature() {
    let events = vec![
        StreamEvent::Start {
            id: "resp_1".into(),
            model: "gpt-5".into(),
            created: 0,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: Some("rs_1".into()),
                redacted: false,
            },
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "Hello".into(),
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: None,
        },
    ];
    let out = wire(&encode_stream(&events, "smart"));
    let blocks = started_blocks(&out);
    assert_eq!(
        blocks,
        vec![json!({"type": "text", "text": ""})],
        "an empty reasoning block must not reach the client as an unsigned thinking block"
    );
    // The surviving block is renumbered, as for every other dropped block.
    let first_start = out
        .iter()
        .find(|(name, _)| name == "content_block_start")
        .expect("a block");
    assert_eq!(first_start.1["index"], json!(0));
}

#[test]
fn review_stream_and_non_stream_agree_on_empty_reasoning() {
    let mut response = Response::new("resp_1", "gpt-5");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: Some("rs_1".into()),
            ..Reasoning::default()
        }),
        Part::text("Hello"),
    ];
    let whole = encode_response(&response, "smart");
    let streamed = wire(&encode_stream(&response_to_events(&response), "smart"));
    let streamed_types: Vec<Value> = started_blocks(&streamed)
        .into_iter()
        .map(|block| block["type"].clone())
        .collect();
    let whole_types: Vec<Value> = whole["content"]
        .as_array()
        .expect("content")
        .iter()
        .map(|block| block["type"].clone())
        .collect();
    assert_eq!(whole_types, vec![json!("text")]);
    assert_eq!(streamed_types, whole_types);
}
