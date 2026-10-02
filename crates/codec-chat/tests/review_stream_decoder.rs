//! Review findings for the stream decoder (`chat.completion.chunk` -> IR).
//!
//! Every test here asserts the correct behaviour. They failed against the
//! reviewed implementation and are kept as regression tests.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Response, Signature};
use switchyard_core::protocol::Protocol;
use switchyard_core::stream::{StreamEvent, response_to_events, validate_sequence};

fn chunk(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    )
}

const DONE: &str = "data: [DONE]\n\n";

// ---------------------------------------------------------------------------
// Finding: two calls that share an index and carry no ids are merged.
//
// The decoder already treats "a header with a different id on a used index"
// as a new call (servers that send every call with index 0). When the server
// sends no ids at all, the second header (it has a *name*, and the first
// call's arguments are already a complete JSON document) is appended to the
// first call: one call `a` with arguments `{"x":1}{"y":2}` and call `b` lost.
// ---------------------------------------------------------------------------

#[test]
fn review_headers_without_ids_on_the_same_index_are_separate_calls() {
    let transcript = [
        chunk(
            json!({"role": "assistant", "tool_calls": [
                {"index": 0, "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}}
            ]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [
                {"index": 0, "type": "function", "function": {"name": "b", "arguments": "{\"y\":2}"}}
            ]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    let calls: Vec<(&str, &str)> = response
        .tool_calls()
        .map(|c| (c.name.as_str(), c.arguments.as_str()))
        .collect();
    assert_eq!(calls, vec![("a", "{\"x\":1}"), ("b", "{\"y\":2}")]);
}

// ---------------------------------------------------------------------------
// Finding: a stream that consists of `[DONE]` alone is reported as a
// successful, empty answer (`Start`, `Finish { Stop }`).
//
// Nothing was generated: there is no id, no model, no delta and no finish
// reason. The reference treats this as `empty_stream` (notes 99 U10: an
// error that is eligible for bootstrap retries; notes 08 section 5.3:
// "`[DONE]` before any chunk -> nothing"), and `translate::Transcoder`
// documents `[DONE]` as an event decoders skip, so that
// `saw_first_event()` stays false and the gateway can fail over (DESIGN
// section 8, step 5). The decoder's own rule for a stream that sent nothing
// at all is `Finish { Error }`; a bare terminator is the same situation.
// ---------------------------------------------------------------------------

#[test]
fn review_done_alone_is_not_a_successful_answer() {
    let mut decoder = ChatCodec.stream_decoder();
    let mut events = decoder.decode(&sse(DONE)[0]).expect("decoder never fails");
    events.extend(decoder.finish());
    validate_sequence(&events).expect("still a valid sequence");
    let terminal = events.last().expect("a terminal event");
    assert!(
        matches!(
            terminal,
            StreamEvent::Error(_)
                | StreamEvent::Finish {
                    reason: FinishReason::Error,
                    ..
                }
        ),
        "an upstream that produced nothing must not look like a completed turn: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding: a usage-only chunk that arrives BEFORE any output marks the
// stream as complete, so a stream that is then cut off (no finish reason, no
// `[DONE]`) ends with `Finish { Stop }` instead of `Finish { Error }`.
//
// Notes 06 section 7.2 step 6: a usage-only chunk is "trailing" (and may end
// the message without a finish reason) only when `choices[0]` is absent AND
// something was already seen (`finish_reason != "" OR saw_tool_call OR a
// text/thinking block is open OR seen_text OR buffered non-empty`). DESIGN
// section 3: a truncated stream must end with `Finish { reason: Error }`.
// ---------------------------------------------------------------------------

#[test]
fn review_usage_only_chunk_before_any_output_does_not_complete_the_stream() {
    let transcript = [
        format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                   "choices": [],
                   "usage": {"prompt_tokens": 12, "completion_tokens": 0, "total_tokens": 12}})
        ),
        chunk(json!({"role": "assistant", "content": "The answer is"}), Value::Null),
        // connection drops here: no finish_reason, no [DONE]
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(
        events.last(),
        Some(&StreamEvent::Finish {
            reason: FinishReason::Error,
            stop_sequence: None
        }),
        "a cut-off answer must not be reported as complete"
    );
}

// ---------------------------------------------------------------------------
// Finding: a function call whose streamed arguments are not a complete JSON
// document is still reported as a finished tool turn (`Finish { ToolCalls }`).
//
// Notes 06 section 7.2 step 4 / 7.4 and the tests pinned in section 10
// ("truncated args with no finish reason -> max_tokens; ... truncated args +
// stop -> max_tokens; ... one of two parallel calls truncated -> max_tokens"):
// when a tool call was announced, the turn is `tool_calls` only if every
// accumulated argument string is valid (empty, or a JSON object); otherwise
// it is reported as cut off (`length`), "so the client does not execute a
// half-written call". Section 12.3 asks the rewrite to keep this invariant.
// ---------------------------------------------------------------------------

fn finish_of(transcript: &str) -> FinishReason {
    match decode_stream(transcript).last() {
        Some(StreamEvent::Finish { reason, .. }) => reason.clone(),
        other => panic!("expected a Finish event, got {other:?}"),
    }
}

fn half_written_call() -> String {
    chunk(
        json!({"role": "assistant", "tool_calls": [
            {"index": 0, "id": "call_1", "type": "function",
             "function": {"name": "Bash", "arguments": "{\"command\": \"ls -la /va"}}
        ]}),
        Value::Null,
    )
}

#[test]
fn review_truncated_tool_arguments_with_stop_are_not_a_tool_turn() {
    // A server that ran out of tokens in the middle of the arguments but
    // labels the end of the stream `stop`.
    let transcript = [
        half_written_call(),
        chunk(json!({}), json!("stop")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(finish_of(&transcript), FinishReason::Length);
}

#[test]
fn review_truncated_tool_arguments_without_finish_reason_are_not_a_tool_turn() {
    // No finish reason at all, only the terminator (which relays append
    // even to streams they cut short).
    let transcript = [half_written_call(), DONE.to_string()].concat();
    assert_eq!(finish_of(&transcript), FinishReason::Length);
}

#[test]
fn review_one_truncated_call_of_two_makes_the_turn_incomplete() {
    let transcript = [
        chunk(
            json!({"role": "assistant", "tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "Read", "arguments": "{\"path\":\"a.txt\"}"}}
            ]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [
                {"index": 1, "id": "call_2", "type": "function",
                 "function": {"name": "Read", "arguments": "{\"path\":\"b"}}
            ]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(finish_of(&transcript), FinishReason::Length);
}

/// Guard: complete arguments (and calls without arguments)
/// keep producing a tool turn.
#[test]
fn review_complete_or_empty_tool_arguments_stay_a_tool_turn() {
    for arguments in ["{\"command\":\"pwd\"}", "{}", ""] {
        let transcript = [
            chunk(
                json!({"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function",
                     "function": {"name": "Bash", "arguments": arguments}}
                ]}),
                Value::Null,
            ),
            chunk(json!({}), json!("stop")),
            DONE.to_string(),
        ]
        .concat();
        assert_eq!(
            finish_of(&transcript),
            FinishReason::ToolCalls,
            "{arguments:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Finding: a signature-only `reasoning_details` entry is attached to the
// reasoning block that is open even when it belongs to another block
// (different `index`, or the open block is already signed). The earlier
// signature is overwritten (`ReasoningSignature` "replaces any earlier
// signature for the same block") and one reasoning block disappears.
//
// This is exactly what the crate's own stream encoder emits for two
// consecutive reasoning blocks when the second one has no visible text
// (a Responses reasoning item with an empty summary, Anthropic thinking with
// `display: "omitted"`), so the stream round-trip property of DESIGN
// section 3 does not hold: the non-streaming path keeps both blocks.
// ---------------------------------------------------------------------------

fn signed(text: &str, blob: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature: Some(Signature::new(Protocol::OpenaiChat, blob)),
        redacted: false,
    })
}

#[test]
fn review_signature_only_detail_with_another_index_is_its_own_block() {
    let transcript = [
        chunk(json!({"role": "assistant", "reasoning_content": "plan"}), Value::Null),
        chunk(
            json!({"reasoning_details": [{"type": "reasoning.text", "signature": "SIG_A", "index": 0}]}),
            Value::Null,
        ),
        chunk(
            json!({"reasoning_details": [{"type": "reasoning.text", "signature": "SIG_B", "index": 1}]}),
            Value::Null,
        ),
        chunk(json!({"content": "done"}), json!("stop")),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(
        response.parts,
        vec![
            signed("plan", "SIG_A"),
            signed("", "SIG_B"),
            Part::text("done")
        ]
    );
}

#[test]
fn review_stream_round_trip_keeps_both_signed_reasoning_blocks() {
    let mut response = Response::new("chatcmpl-1", "m");
    response.created = 1;
    response.parts = vec![
        signed("plan", "SIG_A"),
        signed("", "SIG_B"),
        Part::text("done"),
    ];
    response.finish = FinishReason::Stop;

    let wire = encode_stream(&response_to_events(&response), &ClientCtx::new("m"));
    let back = accumulate(&decode_events(&wire));
    assert_eq!(back.parts, response.parts);

    // The non-streaming path already gets this right.
    let body = ChatCodec
        .encode_response(&response, &ClientCtx::new("m"))
        .unwrap();
    assert_eq!(
        ChatCodec.decode_response(&body).unwrap().parts,
        response.parts
    );
}
