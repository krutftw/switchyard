//! Client side: canonical stream events → Messages SSE events, asserted as
//! exact wire events (names, JSON, order, indices, terminators).

mod common;

use common::{encode_stream, names, wire};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::{ApiError, ClientCtx, Codec, Protocol, Usage};

fn ev(name: &str, data: Value) -> (String, Value) {
    (name.to_string(), data)
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "msg_01ABC".into(),
        model: "claude-upstream-id".into(),
        created: 0,
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn message_start(usage: Value) -> (String, Value) {
    ev(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_01ABC", "type": "message", "role": "assistant", "model": "sonnet",
            "content": [], "stop_reason": null, "stop_sequence": null, "usage": usage
        }}),
    )
}

fn zero_usage() -> Value {
    json!({"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
           "output_tokens": 0})
}

fn block_start(index: u32, block: Value) -> (String, Value) {
    ev(
        "content_block_start",
        json!({"type": "content_block_start", "index": index, "content_block": block}),
    )
}

fn block_delta(index: u32, delta: Value) -> (String, Value) {
    ev(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": index, "delta": delta}),
    )
}

fn block_stop(index: u32) -> (String, Value) {
    ev(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

fn message_delta(stop_reason: Value, stop_sequence: Value, usage: Value) -> (String, Value) {
    ev(
        "message_delta",
        json!({"type": "message_delta",
               "delta": {"stop_reason": stop_reason, "stop_sequence": stop_sequence},
               "usage": usage}),
    )
}

fn message_stop() -> (String, Value) {
    ev("message_stop", json!({"type": "message_stop"}))
}

fn text_block(index: u32, text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::BlockStart {
            index,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index,
            text: text.into(),
        },
        StreamEvent::BlockStop { index },
    ]
}

fn tool_start(index: u32, id: &str, name: &str, kind: ToolCallKind) -> StreamEvent {
    StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind,
            signature: None,
        },
    }
}

fn args(index: u32, fragment: &str) -> StreamEvent {
    StreamEvent::ToolArgsDelta {
        index,
        fragment: fragment.into(),
    }
}

// ---------------------------------------------------------------------------
// Whole responses replayed as streams
// ---------------------------------------------------------------------------

#[test]
fn stream_encoder_text_response_replayed() {
    let mut response = Response::new("msg_01ABC", "claude-upstream-id");
    response.parts = vec![Part::text("Hello!")];
    response.usage = Usage {
        input_tokens: 12,
        output_tokens: 8,
        ..Usage::default()
    };
    let out = encode_stream(&response_to_events(&response), "sonnet");
    assert_eq!(
        wire(&out),
        vec![
            // Usage only arrives at the end of a replayed response, so the
            // initial counts are zeros; the client's model name is reported.
            message_start(zero_usage()),
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "Hello!"})),
            block_stop(0),
            message_delta(
                json!("end_turn"),
                Value::Null,
                json!({"input_tokens": 12, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0, "output_tokens": 8})
            ),
            message_stop(),
        ]
    );
}

#[test]
fn stream_encoder_reasoning_text_and_tool_calls_replayed() {
    let mut response = Response::new("msg_01ABC", "claude-upstream-id");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "Need the weather.".into(),
            signature: Some(Signature::new(Protocol::Anthropic, "EqQBCgIYAhIM")),
            redacted: false,
        }),
        Part::text("Checking."),
        Part::tool_call("toolu_01A", "get_weather", r#"{"city":"Paris"}"#),
        Part::tool_call("toolu_01B", "get_time", ""),
    ];
    response.finish = FinishReason::ToolCalls;
    response.usage = Usage {
        input_tokens: 100,
        cache_read_tokens: 2000,
        cache_write_tokens: 30,
        output_tokens: 77,
        reasoning_tokens: 40,
    };
    let out = encode_stream(&response_to_events(&response), "sonnet");
    assert_eq!(
        wire(&out),
        vec![
            message_start(zero_usage()),
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(
                0,
                json!({"type": "thinking_delta", "thinking": "Need the weather."})
            ),
            // The signature goes out right before the block closes.
            block_delta(
                0,
                json!({"type": "signature_delta", "signature": "EqQBCgIYAhIM"})
            ),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            block_delta(1, json!({"type": "text_delta", "text": "Checking."})),
            block_stop(1),
            block_start(
                2,
                json!({"type": "tool_use", "id": "toolu_01A", "name": "get_weather", "input": {}})
            ),
            block_delta(
                2,
                json!({"type": "input_json_delta", "partial_json": "{\"city\":\"Paris\"}"})
            ),
            block_stop(2),
            block_start(
                3,
                json!({"type": "tool_use", "id": "toolu_01B", "name": "get_time", "input": {}})
            ),
            // No arguments: still one (empty) delta.
            block_delta(3, json!({"type": "input_json_delta", "partial_json": ""})),
            block_stop(3),
            message_delta(
                json!("tool_use"),
                Value::Null,
                json!({"input_tokens": 100, "cache_creation_input_tokens": 30,
                       "cache_read_input_tokens": 2000, "output_tokens": 77,
                       "output_tokens_details": {"thinking_tokens": 40}})
            ),
            message_stop(),
        ]
    );
}

// ---------------------------------------------------------------------------
// Incremental sequences
// ---------------------------------------------------------------------------

#[test]
fn stream_encoder_usage_right_after_start_lands_in_message_start() {
    let mut events = vec![
        start(),
        StreamEvent::Usage(Usage {
            input_tokens: 25,
            cache_read_tokens: 1000,
            cache_write_tokens: 50,
            output_tokens: 1,
            reasoning_tokens: 0,
        }),
    ];
    events.extend(text_block(0, "Hi"));
    events.push(StreamEvent::Usage(Usage {
        output_tokens: 15,
        ..Usage::default()
    }));
    events.push(finish(FinishReason::Stop));
    let out = encode_stream(&events, "sonnet");
    assert_eq!(
        wire(&out),
        vec![
            message_start(
                json!({"input_tokens": 25, "cache_creation_input_tokens": 50,
                                 "cache_read_input_tokens": 1000, "output_tokens": 1})
            ),
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "Hi"})),
            block_stop(0),
            // The final usage is cumulative and repeats the input side.
            message_delta(
                json!("end_turn"),
                Value::Null,
                json!({"input_tokens": 25, "cache_creation_input_tokens": 50,
                       "cache_read_input_tokens": 1000, "output_tokens": 15})
            ),
            message_stop(),
        ]
    );
}

#[test]
fn stream_encoder_message_start_is_emitted_once_and_first() {
    let mut encoder = AnthropicCodec.stream_encoder(&ClientCtx::new("sonnet"));
    // Start alone produces nothing yet: usage may follow immediately.
    assert!(encoder.encode(&start()).is_empty());
    let out = encoder.encode(&StreamEvent::BlockStart {
        index: 0,
        block: BlockStart::Text,
    });
    // The block itself is announced with its first content.
    assert_eq!(names(&out), vec!["message_start"]);
    let out = encoder.encode(&StreamEvent::TextDelta {
        index: 0,
        text: "Hi".into(),
    });
    assert_eq!(
        names(&out),
        vec!["content_block_start", "content_block_delta"]
    );
    let out = encoder.encode(&StreamEvent::Usage(Usage {
        input_tokens: 5,
        ..Usage::default()
    }));
    assert!(out.is_empty(), "later usage waits for message_delta");
}

#[test]
fn stream_encoder_upstream_model_and_minted_id_when_context_is_empty() {
    let events = vec![
        StreamEvent::Start {
            id: String::new(),
            model: "gemini-2.5-pro".into(),
            created: 0,
        },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, ""));
    let message = &out[0].1["message"];
    assert_eq!(message["model"], json!("gemini-2.5-pro"));
    let id = message["id"].as_str().unwrap();
    assert!(id.starts_with("msg_") && id.len() == 28);
    // Foreign ids are re-prefixed, native ones kept.
    let events = vec![
        StreamEvent::Start {
            id: "chatcmpl-9xYz".into(),
            model: "gpt".into(),
            created: 0,
        },
        finish(FinishReason::Stop),
    ];
    assert_eq!(
        wire(&encode_stream(&events, "m"))[0].1["message"]["id"],
        json!("msg_9xYz")
    );
}

#[test]
fn stream_encoder_tool_arguments_stream_incrementally() {
    let events = vec![
        start(),
        tool_start(0, "call_abc", "get_weather", ToolCallKind::Function),
        args(0, ""),
        args(0, "  "),
        args(0, "{\"city\":"),
        args(0, " \"Paris\"}"),
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::ToolCalls),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..5],
        [
            // Ids of other vendors are kept so the result can be matched.
            block_start(
                0,
                json!({"type": "tool_use", "id": "call_abc", "name": "get_weather", "input": {}})
            ),
            // Leading whitespace is held until the object opens.
            block_delta(
                0,
                json!({"type": "input_json_delta", "partial_json": "  {\"city\":"})
            ),
            block_delta(
                0,
                json!({"type": "input_json_delta", "partial_json": " \"Paris\"}"})
            ),
            block_stop(0),
        ]
    );
    assert_eq!(out[5].1["delta"]["stop_reason"], json!("tool_use"));
    // What the client concatenates is exactly the upstream's argument text.
    let joined: String = out[2..4]
        .iter()
        .map(|(_, data)| data["delta"]["partial_json"].as_str().unwrap())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&joined).unwrap(),
        json!({"city": "Paris"})
    );
}

#[test]
fn stream_encoder_tool_without_fragments_gets_one_empty_delta() {
    let events = vec![
        start(),
        tool_start(0, "", "ping", ToolCallKind::Function),
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::ToolCalls),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    // A missing id is minted in the vendor's shape.
    let id = out[1].1["content_block"]["id"].as_str().unwrap();
    assert!(id.starts_with("toolu_") && id.len() > 6);
    assert_eq!(
        out[2..4],
        [
            block_delta(0, json!({"type": "input_json_delta", "partial_json": ""})),
            block_stop(0),
        ]
    );
}

#[test]
fn stream_encoder_arguments_that_are_not_an_object_are_wrapped_never_concatenated_raw() {
    let events = vec![
        start(),
        // A bare JSON array.
        tool_start(0, "a", "f", ToolCallKind::Function),
        args(0, "[1,"),
        args(0, "2]"),
        StreamEvent::BlockStop { index: 0 },
        // Not JSON at all.
        tool_start(1, "b", "f", ToolCallKind::Function),
        args(1, "hello"),
        StreamEvent::BlockStop { index: 1 },
        // A free-form (custom) tool call: raw text input.
        tool_start(2, "c", "apply_patch", ToolCallKind::Custom),
        args(2, "{\"looks\":"),
        args(2, "\"like json\"}"),
        StreamEvent::BlockStop { index: 2 },
        finish(FinishReason::ToolCalls),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    let deltas: Vec<(u64, Value)> = out
        .iter()
        .filter(|(name, _)| name == "content_block_delta")
        .map(|(_, data)| {
            let partial = data["delta"]["partial_json"].as_str().unwrap();
            (
                data["index"].as_u64().unwrap(),
                serde_json::from_str(partial).expect("each call's input is one valid object"),
            )
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            (0, json!({"input": [1, 2]})),
            (1, json!({"input": "hello"})),
            (2, json!({"input": "{\"looks\":\"like json\"}"})),
        ]
    );
    assert_eq!(out.last().unwrap().0, "message_stop");
    assert_eq!(
        out[out.len() - 2].1["delta"]["stop_reason"],
        json!("tool_use")
    );
}

#[test]
fn stream_encoder_half_written_tool_call_is_reported_as_truncated() {
    for reason in [FinishReason::ToolCalls, FinishReason::Stop] {
        let events = vec![
            start(),
            tool_start(0, "toolu_1", "write_file", ToolCallKind::Function),
            args(0, "{\"path\": \"/tmp/x\", \"content\": \"half"),
            StreamEvent::BlockStop { index: 0 },
            finish(reason),
        ];
        let out = wire(&encode_stream(&events, "sonnet"));
        // A client must not run a call whose input is cut off.
        assert_eq!(
            out[out.len() - 2].1["delta"]["stop_reason"],
            json!("max_tokens")
        );
    }
}

#[test]
fn stream_encoder_plain_stop_with_a_tool_call_is_tool_use() {
    let events = vec![
        start(),
        tool_start(0, "toolu_1", "f", ToolCallKind::Function),
        args(0, "{}"),
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[out.len() - 2].1["delta"]["stop_reason"],
        json!("tool_use")
    );
}

#[test]
fn stream_encoder_reasoning_signatures() {
    let events = vec![
        start(),
        // Foreign signature: wrapped. A later signature replaces an earlier one.
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: Some("rs_1".into()),
                redacted: false,
            },
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::OpenaiResponses, "early"),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "summary".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::OpenaiResponses, "gAAAAAfinal"),
        },
        StreamEvent::BlockStop { index: 0 },
        // No signature at all: no signature_delta.
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningDelta {
            index: 1,
            text: "unsigned".into(),
        },
        StreamEvent::BlockStop { index: 1 },
        // Redacted reasoning: a complete block carrying the payload.
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Reasoning {
                id: None,
                redacted: true,
            },
        },
        StreamEvent::ReasoningSignature {
            index: 2,
            signature: Signature::new(Protocol::Anthropic, "EmwKAhgB"),
        },
        StreamEvent::BlockStop { index: 2 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..out.len() - 2],
        [
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(0, json!({"type": "thinking_delta", "thinking": "summary"})),
            block_delta(
                0,
                json!({"type": "signature_delta", "signature": "sy1.r.gAAAAAfinal"})
            ),
            block_stop(0),
            block_start(
                1,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(1, json!({"type": "thinking_delta", "thinking": "unsigned"})),
            block_stop(1),
            block_start(2, json!({"type": "redacted_thinking", "data": "EmwKAhgB"})),
            block_stop(2),
        ]
    );
}

#[test]
fn stream_encoder_blocks_the_protocol_cannot_express_leave_no_index_gap() {
    let native = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                        "input": {"query": "rust"}});
    let whole = |index: u32, part: Part| {
        vec![
            StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole { part },
            },
            StreamEvent::BlockStop { index },
        ]
    };
    let mut events = vec![start()];
    events.extend(text_block(0, "a"));
    // Image output, another vendor's block, redacted reasoning with no
    // payload: none of these exist in a Messages stream.
    events.extend(whole(
        1,
        Part::Image(MediaPart::base64("image/png", "iVBOR")),
    ));
    events.extend(whole(
        2,
        Part::Opaque(OpaquePart {
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "web_search_call", "id": "ws_1"}),
        }),
    ));
    events.extend([
        StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::Reasoning {
                id: None,
                redacted: true,
            },
        },
        StreamEvent::BlockStop { index: 3 },
    ]);
    // A block of this vendor travels verbatim.
    events.extend(whole(
        4,
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw: native.clone(),
        }),
    ));
    events.extend(text_block(5, "b"));
    events.push(finish(FinishReason::Stop));
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..out.len() - 2],
        [
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "a"})),
            block_stop(0),
            block_start(1, native),
            block_stop(1),
            block_start(2, json!({"type": "text", "text": ""})),
            block_delta(2, json!({"type": "text_delta", "text": "b"})),
            block_stop(2),
        ]
    );
}

#[test]
fn stream_encoder_whole_parts_of_modelled_kinds_are_expanded() {
    let whole = |index: u32, part: Part| {
        [
            StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole { part },
            },
            StreamEvent::BlockStop { index },
        ]
    };
    let mut events = vec![start()];
    events.extend(whole(
        0,
        Part::Reasoning(Reasoning {
            id: None,
            text: "t".into(),
            signature: Some(Signature::new(Protocol::Gemini, "CiQB")),
            redacted: false,
        }),
    ));
    events.extend(whole(1, Part::text("whole text")));
    events.extend(whole(
        2,
        Part::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "f".into(),
            arguments: "{\"a\":1}".into(),
            kind: ToolCallKind::Function,
            signature: None,
            cache_control: None,
        }),
    ));
    events.push(finish(FinishReason::ToolCalls));
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..out.len() - 2],
        [
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(0, json!({"type": "thinking_delta", "thinking": "t"})),
            block_delta(
                0,
                json!({"type": "signature_delta", "signature": "sy1.g.CiQB"})
            ),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            block_delta(1, json!({"type": "text_delta", "text": "whole text"})),
            block_stop(1),
            block_start(
                2,
                json!({"type": "tool_use", "id": "call_1", "name": "f", "input": {}})
            ),
            block_delta(
                2,
                json!({"type": "input_json_delta", "partial_json": "{\"a\":1}"})
            ),
            block_stop(2),
        ]
    );
}

#[test]
fn stream_encoder_refusal_is_text_with_refusal_stop_reason() {
    let mut response = Response::new("msg_01ABC", "m");
    response.parts = vec![Part::Refusal(RefusalPart {
        text: "I can't help with that.".into(),
    })];
    let out = wire(&encode_stream(&response_to_events(&response), "sonnet"));
    assert_eq!(
        out[1..],
        [
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(
                0,
                json!({"type": "text_delta", "text": "I can't help with that."})
            ),
            block_stop(0),
            message_delta(json!("refusal"), Value::Null, zero_usage()),
            message_stop(),
        ]
    );
}

#[test]
fn stream_encoder_citations_become_citations_deltas() {
    let mut response = Response::new("msg_01ABC", "m");
    response.parts = vec![Part::Text(TextPart {
        text: "Rust is fast.".into(),
        cache_control: None,
        citations: vec![Citation {
            url: Some("https://rust-lang.org".into()),
            title: Some("Rust".into()),
            cited_text: Some("fast".into()),
            start: None,
            end: None,
        }],
        signature: None,
    })];
    let out = wire(&encode_stream(&response_to_events(&response), "sonnet"));
    assert_eq!(
        out[2..4],
        [
            block_delta(0, json!({"type": "text_delta", "text": "Rust is fast."})),
            block_delta(
                0,
                json!({"type": "citations_delta", "citation": {
                "type": "web_search_result_location", "url": "https://rust-lang.org",
                "title": "Rust", "encrypted_index": "", "cited_text": "fast"}})
            ),
        ]
    );
}

#[test]
fn stream_encoder_stop_reasons_and_stop_sequence() {
    let stop = |reason: FinishReason, stop_sequence: Option<&str>| {
        let mut events = vec![start()];
        events.extend(text_block(0, "x"));
        events.push(StreamEvent::Finish {
            reason,
            stop_sequence: stop_sequence.map(str::to_string),
        });
        let out = wire(&encode_stream(&events, "sonnet"));
        assert_eq!(out.last().unwrap(), &message_stop());
        out[out.len() - 2].1["delta"].clone()
    };
    assert_eq!(
        stop(FinishReason::Stop, None),
        json!({"stop_reason": "end_turn", "stop_sequence": null})
    );
    assert_eq!(
        stop(FinishReason::Stop, Some("END")),
        json!({"stop_reason": "stop_sequence", "stop_sequence": "END"})
    );
    assert_eq!(
        stop(FinishReason::Length, None),
        json!({"stop_reason": "max_tokens", "stop_sequence": null})
    );
    assert_eq!(
        stop(FinishReason::PauseTurn, None),
        json!({"stop_reason": "pause_turn", "stop_sequence": null})
    );
    assert_eq!(
        stop(FinishReason::ContentFilter, None),
        json!({"stop_reason": "refusal", "stop_sequence": null})
    );
    assert_eq!(
        stop(FinishReason::ContextWindow, None),
        json!({"stop_reason": "model_context_window_exceeded", "stop_sequence": null})
    );
    assert_eq!(
        stop(FinishReason::Other("weird".into()), None),
        json!({"stop_reason": "end_turn", "stop_sequence": null})
    );
}

// ---------------------------------------------------------------------------
// Errors and truncation
// ---------------------------------------------------------------------------

#[test]
fn stream_encoder_error_terminated_sequence() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Partial".into(),
        },
        StreamEvent::Error(ApiError::unavailable("Overloaded")),
        // Nothing after a terminal event reaches the wire.
        StreamEvent::TextDelta {
            index: 0,
            text: "late".into(),
        },
    ];
    let out = encode_stream(&events, "sonnet");
    assert_eq!(
        wire(&out),
        vec![
            message_start(zero_usage()),
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "Partial"})),
            ev(
                "error",
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
            ),
        ]
    );
}

#[test]
fn stream_encoder_error_before_start_is_only_the_error_event() {
    let out = encode_stream(
        &[StreamEvent::Error(ApiError::rate_limit("slow down"))],
        "sonnet",
    );
    assert_eq!(
        wire(&out),
        vec![ev(
            "error",
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        )]
    );
}

#[test]
fn stream_encoder_failed_generation_ends_with_an_error_event() {
    // What a decoder produces for an upstream that died mid-answer.
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Partial".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Error),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        names_of(&out),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "error"
        ]
    );
    assert_eq!(out[4].1["error"]["type"], json!("api_error"));
}

fn names_of(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn stream_encoder_truncated_sequence_is_closed_gracefully() {
    // No BlockStop, no Finish: finish() closes the block and the message.
    let events = vec![
        start(),
        StreamEvent::Usage(Usage {
            input_tokens: 9,
            ..Usage::default()
        }),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "thinking…".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "sig"),
        },
    ];
    let out = encode_stream(&events, "sonnet");
    assert_eq!(
        wire(&out),
        vec![
            message_start(json!({"input_tokens": 9, "cache_creation_input_tokens": 0,
                                 "cache_read_input_tokens": 0, "output_tokens": 0})),
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(
                0,
                json!({"type": "thinking_delta", "thinking": "thinking…"})
            ),
            block_delta(0, json!({"type": "signature_delta", "signature": "sig"})),
            block_stop(0),
            message_delta(
                Value::Null,
                Value::Null,
                json!({"input_tokens": 9, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0, "output_tokens": 0})
            ),
            message_stop(),
        ]
    );
}

#[test]
fn stream_encoder_start_only_and_empty_sequences() {
    // Only Start: a complete, empty message.
    let out = encode_stream(&[start()], "sonnet");
    assert_eq!(
        names(&out),
        vec!["message_start", "message_delta", "message_stop"]
    );
    // Nothing at all: nothing to close.
    assert!(encode_stream(&[], "sonnet").is_empty());
}

#[test]
fn stream_encoder_empty_deltas_are_not_forwarded() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: String::new(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Hi".into(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: String::new(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..4],
        [
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "Hi"})),
            block_stop(0),
        ]
    );
    assert_eq!(out.len(), 6);
}

/// A text block that never receives text would reach the client as
/// `{"type":"text","text":""}`, which the API refuses when the client
/// replays it; `encode_response` leaves the same part out.
#[test]
fn stream_encoder_text_and_refusal_blocks_without_content_are_not_announced() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: String::new(),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Refusal,
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "Hi".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..],
        [
            // The surviving block takes the first wire index.
            block_start(0, json!({"type": "text", "text": ""})),
            block_delta(0, json!({"type": "text_delta", "text": "Hi"})),
            block_stop(0),
            // The refusal still decides the stop reason.
            message_delta(
                json!("refusal"),
                Value::Null,
                json!({"input_tokens": 0, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0, "output_tokens": 0})
            ),
            message_stop(),
        ]
    );
}

/// The encoder for complete responses renders reasoning that has only a
/// signature as a thinking block with empty text; the stream does the same,
/// announcing the block when the signature arrives.
#[test]
fn stream_encoder_reasoning_with_only_a_signature_is_a_signed_empty_thinking_block() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "EqQBCkYIBBgCKkD"),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..4],
        [
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(
                0,
                json!({"type": "signature_delta", "signature": "EqQBCkYIBBgCKkD"})
            ),
            block_stop(0),
        ]
    );
}

/// A signature without data signs nothing. Wrapped for a client it would
/// read `sy1.g.`, a non-empty string that no upstream can verify.
#[test]
fn stream_encoder_ignores_a_signature_without_data() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Gemini, ""),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningDelta {
            index: 1,
            text: "hm".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 1,
            signature: Signature::new(Protocol::Gemini, ""),
        },
        StreamEvent::BlockStop { index: 1 },
        finish(FinishReason::Stop),
    ];
    let out = wire(&encode_stream(&events, "sonnet"));
    assert_eq!(
        out[1..4],
        [
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""})
            ),
            block_delta(0, json!({"type": "thinking_delta", "thinking": "hm"})),
            block_stop(0),
        ]
    );
    assert_eq!(out.len(), 6);
}

#[test]
fn stream_encoder_output_serialises_as_named_sse_events() {
    let mut response = Response::new("msg_01ABC", "m");
    response.parts = vec![Part::text("line one\nline two")];
    let out = encode_stream(&response_to_events(&response), "sonnet");
    for event in &out {
        let bytes = event.to_bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        // One `event:` line, one `data:` line (JSON never contains a raw
        // newline), one blank line; never an OpenAI-style [DONE].
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].starts_with("event: ") && lines[1].starts_with("data: {"));
        assert_eq!(lines[2..], ["", ""]);
        assert!(!text.contains("[DONE]"));
    }
}
