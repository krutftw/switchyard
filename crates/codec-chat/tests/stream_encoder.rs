//! Stream encoder: canonical events into the exact `chat.completion.chunk`
//! wire events.

mod common;

use common::{ctx_with_usage, encode_stream, payloads};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::error::ApiError;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, ToolCall, ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::{Protocol, Usage};

const ID: &str = "chatcmpl-abc";
const CREATED: i64 = 1741570002;
const MODEL: &str = "client-model";

fn ctx() -> ClientCtx {
    ClientCtx::new(MODEL)
}

fn wire(events: &[StreamEvent]) -> Vec<Value> {
    payloads(&encode_stream(events, &ctx()))
}

fn wire_with_usage(events: &[StreamEvent]) -> Vec<Value> {
    payloads(&encode_stream(events, &ctx_with_usage(MODEL)))
}

/// A chunk as sent to a client that did not ask for usage.
fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": ID,
        "object": "chat.completion.chunk",
        "created": CREATED,
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]
    })
}

fn delta(delta: Value) -> Value {
    chunk(delta, Value::Null)
}

fn role() -> Value {
    delta(json!({"role": "assistant", "content": ""}))
}

fn finish(reason: &str) -> Value {
    chunk(json!({}), json!(reason))
}

fn done() -> Value {
    json!("[DONE]")
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: ID.into(),
        model: "upstream-model".into(),
        created: CREATED,
    }
}

fn block(index: u32, block: BlockStart) -> StreamEvent {
    StreamEvent::BlockStart { index, block }
}

fn tool_block(index: u32, id: &str, name: &str) -> StreamEvent {
    block(
        index,
        BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolCallKind::Function,
            signature: None,
        },
    )
}

fn text(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        text: text.into(),
    }
}

fn stop(index: u32) -> StreamEvent {
    StreamEvent::BlockStop { index }
}

fn finished(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn sample_usage() -> Usage {
    Usage {
        input_tokens: 10,
        cache_read_tokens: 5,
        cache_write_tokens: 0,
        output_tokens: 7,
        reasoning_tokens: 2,
    }
}

// ---------------------------------------------------------------------------
// Whole responses replayed as streams
// ---------------------------------------------------------------------------

#[test]
fn text_response_without_usage_request() {
    let mut response = Response::new(ID, "upstream-model");
    response.created = CREATED;
    response.parts = vec![Part::text("Hello world")];
    response.usage = sample_usage();
    assert_eq!(
        wire(&response_to_events(&response)),
        vec![
            role(),
            delta(json!({"content": "Hello world"})),
            finish("stop"),
            // The client did not ask for usage: no usage chunk.
            done(),
        ]
    );
}

#[test]
fn usage_chunk_only_when_the_client_asked() {
    let mut response = Response::new(ID, "upstream-model");
    response.created = CREATED;
    response.parts = vec![Part::text("Hi")];
    response.usage = sample_usage();
    let with_null_usage = |mut chunk: Value| {
        chunk["usage"] = Value::Null;
        chunk
    };
    assert_eq!(
        wire_with_usage(&response_to_events(&response)),
        vec![
            // As OpenAI does, every ordinary chunk carries `usage: null` ...
            with_null_usage(role()),
            with_null_usage(delta(json!({"content": "Hi"}))),
            with_null_usage(finish("stop")),
            // ... and one extra chunk without choices carries the totals.
            json!({
                "id": ID,
                "object": "chat.completion.chunk",
                "created": CREATED,
                "model": MODEL,
                "choices": [],
                "usage": {
                    "prompt_tokens": 15,
                    "completion_tokens": 7,
                    "total_tokens": 22,
                    "prompt_tokens_details": {"cached_tokens": 5},
                    "completion_tokens_details": {"reasoning_tokens": 2}
                }
            }),
            done(),
        ]
    );
}

#[test]
fn include_usage_must_be_the_literal_true() {
    let events = [
        start(),
        StreamEvent::Usage(sample_usage()),
        finished(FinishReason::Stop),
    ];
    for options in [
        json!({"include_usage": false}),
        json!({"include_usage": "true"}),
        json!({}),
        Value::Null,
    ] {
        let ctx = ClientCtx::new(MODEL).with_request(std::sync::Arc::new(
            json!({"stream": true, "stream_options": options}),
        ));
        assert_eq!(
            payloads(&encode_stream(&events, &ctx)),
            vec![role(), finish("stop"), done()]
        );
    }
}

#[test]
fn response_with_reasoning_text_and_tool_calls() {
    let mut response = Response::new(ID, "upstream-model");
    response.created = CREATED;
    response.parts = vec![
        Part::reasoning("Need the weather."),
        Part::text("Checking."),
        Part::tool_call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
        Part::tool_call("call_2", "ping", ""),
    ];
    response.finish = FinishReason::ToolCalls;
    assert_eq!(
        wire(&response_to_events(&response)),
        vec![
            role(),
            // Reasoning text is mirrored in `reasoning_details`, whose
            // `index` numbers the reasoning blocks.
            delta(json!({
                "reasoning_content": "Need the weather.",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "Need the weather.", "index": 0}
                ]
            })),
            delta(json!({"content": "Checking."})),
            // Tool calls are numbered from zero whatever their block index.
            delta(json!({"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": ""}
            }]})),
            delta(json!({"tool_calls": [{
                "index": 0, "function": {"arguments": "{\"city\":\"Paris\"}"}
            }]})),
            delta(json!({"tool_calls": [{
                "index": 1, "id": "call_2", "type": "function",
                "function": {"name": "ping", "arguments": ""}
            }]})),
            // A call without arguments still has to accumulate to valid JSON
            // on the client.
            delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "{}"}}]})),
            finish("tool_calls"),
            done(),
        ]
    );
}

// ---------------------------------------------------------------------------
// Incremental sequences
// ---------------------------------------------------------------------------

#[test]
fn incremental_text_and_fragmented_tool_arguments() {
    let events = [
        start(),
        block(0, BlockStart::Text),
        text(0, "Let me "),
        text(0, "look."),
        // Empty deltas produce no chunk.
        text(0, ""),
        stop(0),
        tool_block(1, "call_1", "search"),
        StreamEvent::ToolArgsDelta {
            index: 1,
            fragment: "{\"q\":".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 1,
            fragment: String::new(),
        },
        StreamEvent::ToolArgsDelta {
            index: 1,
            fragment: "\"rust\"}".into(),
        },
        stop(1),
        tool_block(2, "call_2", "search"),
        StreamEvent::ToolArgsDelta {
            index: 2,
            fragment: "{}".into(),
        },
        stop(2),
        StreamEvent::Usage(sample_usage()),
        finished(FinishReason::ToolCalls),
    ];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({"content": "Let me "})),
            delta(json!({"content": "look."})),
            delta(json!({"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "search", "arguments": ""}
            }]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"q\":"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"rust\"}"}}]})),
            delta(json!({"tool_calls": [{
                "index": 1, "id": "call_2", "type": "function",
                "function": {"name": "search", "arguments": ""}
            }]})),
            delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "{}"}}]})),
            finish("tool_calls"),
            done(),
        ]
    );
}

#[test]
fn chunks_are_data_only_events_with_compact_json() {
    let events = [
        start(),
        block(0, BlockStart::Text),
        text(0, "hi"),
        stop(0),
        finished(FinishReason::Stop),
    ];
    let wire = encode_stream(&events, &ctx());
    assert!(wire.iter().all(|e| e.event.is_none()));
    assert_eq!(
        wire[1].data,
        r#"{"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1741570002,"model":"client-model","choices":[{"index":0,"delta":{"content":"hi"},"logprobs":null,"finish_reason":null}]}"#
    );
    assert_eq!(wire.last().map(|e| e.data.as_str()), Some("[DONE]"));
    assert_eq!(
        String::from_utf8_lossy(&wire.last().expect("a terminator").to_bytes()),
        "data: [DONE]\n\n"
    );
}

#[test]
fn reasoning_signatures_travel_in_reasoning_details() {
    let events = [
        start(),
        block(
            0,
            BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        ),
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "Hmm.".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "ErUB"),
        },
        stop(0),
        block(
            1,
            BlockStart::Reasoning {
                id: Some("rs_1".into()),
                redacted: true,
            },
        ),
        StreamEvent::ReasoningSignature {
            index: 1,
            signature: Signature::new(Protocol::OpenaiResponses, "gAAAA"),
        },
        stop(1),
        block(2, BlockStart::Text),
        text(2, "Done."),
        stop(2),
        finished(FinishReason::Stop),
    ];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({
                "reasoning_content": "Hmm.",
                "reasoning_details": [{"type": "reasoning.text", "text": "Hmm.", "index": 0}]
            })),
            // Another vendor's blob is wrapped for the client ...
            delta(json!({"reasoning_details": [
                {"type": "reasoning.text", "signature": "sy1.a.ErUB", "index": 0}
            ]})),
            // ... the OpenAI family's is native.
            delta(json!({"reasoning_details": [
                {"type": "reasoning.encrypted", "data": "gAAAA", "id": "rs_1", "index": 1}
            ]})),
            delta(json!({"content": "Done."})),
            finish("stop"),
            done(),
        ]
    );
}

#[test]
fn tool_call_kinds_ids_and_signatures() {
    let events = [
        start(),
        block(
            0,
            BlockStart::ToolCall {
                id: "toolu_01".into(),
                name: "f".into(),
                kind: ToolCallKind::Function,
                signature: Some(Signature::new(Protocol::Gemini, "CiQB")),
            },
        ),
        stop(0),
        block(
            1,
            BlockStart::ToolCall {
                id: String::new(),
                name: "run_sql".into(),
                kind: ToolCallKind::Custom,
                signature: None,
            },
        ),
        StreamEvent::ToolArgsDelta {
            index: 1,
            fragment: "SELECT 1".into(),
        },
        stop(1),
        finished(FinishReason::ToolCalls),
    ];
    let wire = wire(&events);
    assert_eq!(
        wire[1],
        delta(json!({"tool_calls": [{
            "index": 0, "id": "toolu_01", "type": "function",
            "function": {"name": "f", "arguments": ""},
            "extra_content": {"google": {"thought_signature": "sy1.g.CiQB"}}
        }]}))
    );
    assert_eq!(
        wire[2],
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}))
    );
    let custom = &common::delta(&wire[3])["tool_calls"][0];
    assert!(
        custom["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("call_"))
    );
    assert_eq!(custom["type"], json!("custom"));
    assert_eq!(custom["custom"], json!({"name": "run_sql", "input": ""}));
    assert_eq!(
        wire[4],
        delta(json!({"tool_calls": [{"index": 1, "custom": {"input": "SELECT 1"}}]}))
    );
    assert_eq!(wire[5], finish("tool_calls"));
}

#[test]
fn custom_tool_call_without_input_is_not_given_json() {
    let events = [
        start(),
        block(
            0,
            BlockStart::ToolCall {
                id: "c1".into(),
                name: "run_sql".into(),
                kind: ToolCallKind::Custom,
                signature: None,
            },
        ),
        stop(0),
        finished(FinishReason::ToolCalls),
    ];
    // Free-form input is text, and empty text is a valid input.
    assert_eq!(wire(&events).len(), 4);
}

#[test]
fn refusal_citations_and_whole_parts() {
    let citation = Citation {
        url: Some("https://example.com".into()),
        title: Some("Example".into()),
        cited_text: None,
        start: Some(0),
        end: Some(4),
    };
    let events = [
        start(),
        block(0, BlockStart::Text),
        text(0, "Intro. "),
        stop(0),
        block(
            1,
            BlockStart::Whole {
                part: Part::Image(MediaPart::base64("image/png", "AAAA")),
            },
        ),
        stop(1),
        block(2, BlockStart::Text),
        text(2, "Fact."),
        StreamEvent::Citation { index: 2, citation },
        stop(2),
        block(3, BlockStart::Refusal),
        text(3, "But not that."),
        stop(3),
        // Parts Chat cannot express are dropped without breaking the stream.
        block(
            4,
            BlockStart::Whole {
                part: Part::Audio(MediaPart::base64("audio/wav", "UklG")),
            },
        ),
        stop(4),
        block(
            5,
            BlockStart::Whole {
                part: Part::Opaque(OpaquePart {
                    origin: Protocol::Anthropic,
                    raw: json!({"type": "x"}),
                }),
            },
        ),
        stop(5),
        finished(FinishReason::Stop),
    ];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({"content": "Intro. "})),
            delta(json!({"images": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}, "index": 0}
            ]})),
            delta(json!({"content": "Fact."})),
            // Offsets count from the start of the whole message content.
            delta(
                json!({"annotations": [{"type": "url_citation", "url_citation": {
                    "url": "https://example.com", "title": "Example", "start_index": 7, "end_index": 11
                }}]})
            ),
            delta(json!({"refusal": "But not that."})),
            finish("stop"),
            done(),
        ]
    );
}

#[test]
fn whole_text_reasoning_and_tool_call_parts() {
    let events = [
        start(),
        block(
            0,
            BlockStart::Whole {
                part: Part::Reasoning(Reasoning {
                    id: None,
                    text: "thought".into(),
                    signature: Some(Signature::new(Protocol::OpenaiChat, "sig")),
                    redacted: false,
                }),
            },
        ),
        stop(0),
        block(
            1,
            BlockStart::Whole {
                part: Part::text("whole text"),
            },
        ),
        stop(1),
        block(
            2,
            BlockStart::Whole {
                part: Part::tool_call("c1", "f", "{\"a\":1}"),
            },
        ),
        stop(2),
        block(
            3,
            BlockStart::Whole {
                part: Part::Refusal(RefusalPart { text: "no".into() }),
            },
        ),
        stop(3),
        finished(FinishReason::ToolCalls),
    ];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({
                "reasoning_content": "thought",
                "reasoning_details": [{"type": "reasoning.text", "text": "thought", "index": 0}]
            })),
            delta(json!({"reasoning_details": [
                {"type": "reasoning.text", "signature": "sig", "index": 0}
            ]})),
            delta(json!({"content": "whole text"})),
            delta(json!({"tool_calls": [{
                "index": 0, "id": "c1", "type": "function",
                "function": {"name": "f", "arguments": "{\"a\":1}"}
            }]})),
            delta(json!({"refusal": "no"})),
            finish("tool_calls"),
            done(),
        ]
    );
}

#[test]
fn usage_events_are_merged_into_one_final_chunk() {
    let events = [
        start(),
        // Anthropic-style: input counts first, output counts at the end.
        StreamEvent::Usage(Usage {
            input_tokens: 100,
            cache_read_tokens: 20,
            ..Usage::default()
        }),
        block(0, BlockStart::Text),
        text(0, "x"),
        stop(0),
        StreamEvent::Usage(Usage {
            output_tokens: 9,
            reasoning_tokens: 4,
            ..Usage::default()
        }),
        finished(FinishReason::Stop),
    ];
    let wire = wire_with_usage(&events);
    assert_eq!(wire.len(), 5);
    assert_eq!(wire[3]["choices"], json!([]));
    assert_eq!(
        wire[3]["usage"],
        json!({
            "prompt_tokens": 120,
            "completion_tokens": 9,
            "total_tokens": 129,
            "prompt_tokens_details": {"cached_tokens": 20},
            "completion_tokens_details": {"reasoning_tokens": 4}
        })
    );
    assert_eq!(wire[4], done());
}

#[test]
fn usage_chunk_is_sent_even_when_the_upstream_reported_none() {
    let wire = wire_with_usage(&[start(), finished(FinishReason::Stop)]);
    assert_eq!(wire.len(), 4);
    assert_eq!(wire[2]["usage"]["total_tokens"], json!(0));
}

// ---------------------------------------------------------------------------
// Finish reasons, ids, terminators
// ---------------------------------------------------------------------------

#[test]
fn finish_reason_mapping() {
    let reason = |finish: FinishReason, with_call: bool| {
        let mut events = vec![start()];
        if with_call {
            events.extend([tool_block(0, "c", "f"), stop(0)]);
        }
        events.push(finished(finish));
        let wire = wire(&events);
        wire[wire.len() - 2]["choices"][0]["finish_reason"].clone()
    };
    assert_eq!(reason(FinishReason::Stop, false), json!("stop"));
    assert_eq!(reason(FinishReason::Length, false), json!("length"));
    assert_eq!(
        reason(FinishReason::ContentFilter, false),
        json!("content_filter")
    );
    assert_eq!(
        reason(FinishReason::Refusal, false),
        json!("content_filter")
    );
    assert_eq!(reason(FinishReason::PauseTurn, false), json!("stop"));
    assert_eq!(reason(FinishReason::ContextWindow, false), json!("length"));
    assert_eq!(reason(FinishReason::Error, false), json!("length"));
    assert_eq!(
        reason(FinishReason::Other("x".into()), false),
        json!("stop")
    );
    assert_eq!(reason(FinishReason::ToolCalls, false), json!("stop"));
    assert_eq!(reason(FinishReason::ToolCalls, true), json!("tool_calls"));
    assert_eq!(reason(FinishReason::Stop, true), json!("tool_calls"));
    assert_eq!(reason(FinishReason::Length, true), json!("length"));
}

#[test]
fn ids_are_shaped_like_chat_completion_ids_and_stay_constant() {
    let stream = |id: &str| {
        let events = [
            StreamEvent::Start {
                id: id.into(),
                model: "up".into(),
                created: 0,
            },
            block(0, BlockStart::Text),
            text(0, "x"),
            stop(0),
            finished(FinishReason::Stop),
        ];
        payloads(&encode_stream(&events, &ClientCtx::new("")))
    };
    let wire = stream("msg_01ABC");
    assert!(wire[..3].iter().all(|c| c["id"] == json!("chatcmpl-01ABC")));
    // No name from the client context: the upstream's model is reported.
    assert!(wire[..3].iter().all(|c| c["model"] == json!("up")));
    // Unknown creation time: the current time, the same on every chunk.
    let created = wire[0]["created"].as_i64().expect("created");
    assert!(created > 1_700_000_000);
    assert!(wire[..3].iter().all(|c| c["created"] == json!(created)));

    assert_eq!(stream("chatcmpl-xyz")[0]["id"], json!("chatcmpl-xyz"));
    let wire = stream("");
    let minted = wire[0]["id"].as_str().expect("id").to_string();
    assert!(minted.starts_with("chatcmpl-") && minted.len() == 33);
    assert!(wire[..3].iter().all(|c| c["id"] == json!(minted)));
}

#[test]
fn error_terminated_stream_has_an_error_frame_and_no_done() {
    let events = [
        start(),
        block(0, BlockStart::Text),
        text(0, "Partial"),
        StreamEvent::Error(
            ApiError::rate_limit("upstream rate limit").with_code("rate_limit_exceeded"),
        ),
    ];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({"content": "Partial"})),
            json!({"error": {
                "message": "upstream rate limit",
                "type": "rate_limit_error",
                "param": null,
                "code": "rate_limit_exceeded"
            }}),
        ]
    );
}

#[test]
fn error_before_anything_else() {
    let events = [start(), StreamEvent::Error(ApiError::upstream("boom"))];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            json!({"error": {
                "message": "boom", "type": "server_error", "param": null, "code": "upstream_error"
            }}),
        ]
    );
}

#[test]
fn nothing_is_encoded_after_a_terminal_event() {
    let mut encoder = ChatCodec.stream_encoder(&ctx());
    encoder.encode(&start());
    encoder.encode(&StreamEvent::Error(ApiError::upstream("boom")));
    assert!(encoder.encode(&text(0, "late")).is_empty());
    assert!(encoder.encode(&finished(FinishReason::Stop)).is_empty());
    assert!(encoder.finish().is_empty());

    let mut encoder = ChatCodec.stream_encoder(&ctx());
    encoder.encode(&start());
    assert_eq!(encoder.encode(&finished(FinishReason::Stop)).len(), 1);
    assert!(encoder.encode(&text(0, "late")).is_empty());
    assert!(encoder.encode(&finished(FinishReason::Stop)).is_empty());
    assert_eq!(payloads(&encoder.finish()), vec![done()]);
    assert!(encoder.finish().is_empty());
}

#[test]
fn truncated_sequence_is_closed_as_an_incomplete_answer() {
    let events = [start(), block(0, BlockStart::Text), text(0, "Par")];
    assert_eq!(
        wire(&events),
        vec![
            role(),
            delta(json!({"content": "Par"})),
            // No terminal event: the answer is cut short, not finished.
            finish("length"),
            done(),
        ]
    );

    let wire = wire_with_usage(&[start(), StreamEvent::Usage(sample_usage())]);
    assert_eq!(wire.len(), 4);
    assert_eq!(wire[1]["choices"][0]["finish_reason"], json!("length"));
    assert_eq!(wire[2]["usage"]["prompt_tokens"], json!(15));
    assert_eq!(wire[3], done());
}

#[test]
fn empty_sequence_still_yields_a_valid_stream() {
    let wire = payloads(&encode_stream(&[], &ctx()));
    assert_eq!(wire.len(), 3);
    assert_eq!(
        common::delta(&wire[0]),
        &json!({"role": "assistant", "content": ""})
    );
    assert_eq!(wire[1]["choices"][0]["finish_reason"], json!("length"));
    assert_eq!(wire[2], done());
}

#[test]
fn sequence_without_start_gets_a_role_chunk_first() {
    let events = [
        block(0, BlockStart::Text),
        text(0, "hi"),
        stop(0),
        finished(FinishReason::Stop),
    ];
    let wire = payloads(&encode_stream(&events, &ctx()));
    assert_eq!(wire.len(), 4);
    assert_eq!(
        common::delta(&wire[0]),
        &json!({"role": "assistant", "content": ""})
    );
    assert_eq!(common::delta(&wire[1]), &json!({"content": "hi"}));
    assert!(
        wire[0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("chatcmpl-"))
    );
}

#[test]
fn tool_call_part_in_a_whole_block_counts_as_a_tool_turn() {
    let call = Part::ToolCall(ToolCall {
        id: "c1".into(),
        name: "f".into(),
        arguments: String::new(),
        kind: ToolCallKind::Function,
        signature: None,
        cache_control: None,
    });
    let events = [
        start(),
        block(0, BlockStart::Whole { part: call }),
        stop(0),
        finished(FinishReason::Stop),
    ];
    let wire = wire(&events);
    assert_eq!(wire[2], finish("tool_calls"));
}
