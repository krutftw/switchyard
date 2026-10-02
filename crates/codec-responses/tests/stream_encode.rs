//! The stream encoder: canonical sequences in, exact Responses wire events
//! out (names, JSON, ordering, indices, terminators).

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, Response, Signature,
    ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::{ApiError, ClientCtx, Codec, Protocol, SseEvent, Usage};

const P: Protocol = Protocol::OpenaiResponses;

/// Runs a sequence through a fresh encoder and returns the wire events as
/// JSON, after checking the invariants every event must satisfy.
fn run_with(events: &[StreamEvent], ctx: &ClientCtx) -> Vec<Value> {
    let mut encoder = ResponsesCodec.stream_encoder(ctx);
    let mut wire: Vec<SseEvent> = Vec::new();
    for event in events {
        wire.extend(encoder.encode(event));
    }
    wire.extend(encoder.finish());
    assert!(encoder.finish().is_empty(), "finish() must be idempotent");
    wire.iter()
        .enumerate()
        .map(|(n, sse)| {
            assert!(!sse.is_done_marker(), "Responses streams have no [DONE]");
            let value: Value = serde_json::from_str(&sse.data).expect("event data is JSON");
            // SSE event name and `type` agree; sequence numbers count from 0.
            assert_eq!(sse.event.as_deref(), value["type"].as_str());
            assert_eq!(value["sequence_number"], json!(n));
            value
        })
        .collect()
}

fn run(events: &[StreamEvent]) -> Vec<Value> {
    run_with(events, &ClientCtx::new("gpt-x"))
}

fn types(wire: &[Value]) -> Vec<&str> {
    wire.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "resp_1".into(),
        model: "upstream-model".into(),
        created: 1_700_000_000,
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        cache_read_tokens: 2,
        cache_write_tokens: 0,
        output_tokens: 5,
        reasoning_tokens: 1,
    }
}

fn usage_json() -> Value {
    json!({
        "input_tokens": 12, "input_tokens_details": {"cached_tokens": 2},
        "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 1},
        "total_tokens": 17
    })
}

/// The response object as rendered without a client request to echo.
fn shell(status: &str, output: Value, usage: Value) -> Value {
    json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 1700000000,
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "max_tool_calls": null,
        "model": "gpt-x",
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "prompt_cache_key": null,
        "reasoning": {"effort": null, "summary": null},
        "safety_identifier": null,
        "service_tier": "default",
        "store": true,
        "temperature": 1.0,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_logprobs": 0,
        "top_p": 1.0,
        "truncation": "disabled",
        "usage": usage,
        "user": null,
        "metadata": {}
    })
}

fn text_part(text: &str) -> Value {
    json!({"type": "output_text", "annotations": [], "logprobs": [], "text": text})
}

fn message(id: &str, status: &str, content: Value) -> Value {
    json!({"id": id, "type": "message", "status": status, "content": content, "role": "assistant"})
}

fn sample_response(parts: Vec<Part>, finish: FinishReason) -> Response {
    Response {
        id: "resp_1".into(),
        model: "upstream-model".into(),
        created: 1_700_000_000,
        parts,
        finish,
        stop_sequence: None,
        usage: usage(),
        service_tier: None,
    }
}

// ---------------------------------------------------------------------------
// Whole responses replayed as streams
// ---------------------------------------------------------------------------

#[test]
fn encode_text_only_stream_exactly() {
    let events = response_to_events(&sample_response(
        vec![Part::text("Hello")],
        FinishReason::Stop,
    ));
    let done_item = message("msg_1_0", "completed", json!([text_part("Hello")]));
    assert_eq!(
        run(&events),
        vec![
            json!({"type": "response.created", "sequence_number": 0,
                   "response": shell("in_progress", json!([]), Value::Null)}),
            json!({"type": "response.in_progress", "sequence_number": 1,
                   "response": shell("in_progress", json!([]), Value::Null)}),
            json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
                   "item": message("msg_1_0", "in_progress", json!([]))}),
            json!({"type": "response.content_part.added", "sequence_number": 3, "item_id": "msg_1_0",
                   "output_index": 0, "content_index": 0, "part": text_part("")}),
            json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_1_0",
                   "output_index": 0, "content_index": 0, "delta": "Hello", "logprobs": []}),
            json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_1_0",
                   "output_index": 0, "content_index": 0, "text": "Hello", "logprobs": []}),
            json!({"type": "response.content_part.done", "sequence_number": 6, "item_id": "msg_1_0",
                   "output_index": 0, "content_index": 0, "part": text_part("Hello")}),
            json!({"type": "response.output_item.done", "sequence_number": 7, "output_index": 0,
                   "item": done_item}),
            json!({"type": "response.completed", "sequence_number": 8,
                   "response": shell("completed", json!([done_item]), usage_json())}),
        ]
    );
}

#[test]
fn encode_reasoning_text_and_tool_call_stream_exactly() {
    let events = response_to_events(&sample_response(
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "think".into(),
                signature: Some(Signature::new(P, "gAAAAsig")),
                redacted: false,
            }),
            Part::text("Hello"),
            Part::tool_call("call_1", "read", "{\"p\":1}"),
        ],
        FinishReason::ToolCalls,
    ));
    let summary_position = json!({"item_id": "rs_1_0", "output_index": 0, "summary_index": 0});
    let with = |base: &Value, kind: &str, n: u64, extra: Value| {
        let mut event = json!({"type": kind, "sequence_number": n});
        for source in [base, &extra] {
            for (key, value) in source.as_object().unwrap() {
                event[key] = value.clone();
            }
        }
        event
    };
    let reasoning_done = json!({"id": "rs_1_0", "type": "reasoning",
                                "summary": [{"type": "summary_text", "text": "think"}],
                                "encrypted_content": "gAAAAsig"});
    let message_done = message("msg_1_1", "completed", json!([text_part("Hello")]));
    let call = |status: &str, arguments: &str| {
        json!({"id": "fc_call_1", "type": "function_call", "status": status, "arguments": arguments,
               "call_id": "call_1", "name": "read"})
    };
    let text_position = json!({"item_id": "msg_1_1", "output_index": 1, "content_index": 0});
    let none = json!({});

    assert_eq!(
        run(&events),
        vec![
            json!({"type": "response.created", "sequence_number": 0,
                   "response": shell("in_progress", json!([]), Value::Null)}),
            json!({"type": "response.in_progress", "sequence_number": 1,
                   "response": shell("in_progress", json!([]), Value::Null)}),
            // Reasoning item: the blob is only attached to the final copy.
            json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
                   "item": {"id": "rs_1_0", "type": "reasoning", "summary": []}}),
            with(
                &summary_position,
                "response.reasoning_summary_part.added",
                3,
                json!({"part": {"type": "summary_text", "text": ""}})
            ),
            with(
                &summary_position,
                "response.reasoning_summary_text.delta",
                4,
                json!({"delta": "think"})
            ),
            with(
                &summary_position,
                "response.reasoning_summary_text.done",
                5,
                json!({"text": "think"})
            ),
            with(
                &summary_position,
                "response.reasoning_summary_part.done",
                6,
                json!({"part": {"type": "summary_text", "text": "think"}})
            ),
            json!({"type": "response.output_item.done", "sequence_number": 7, "output_index": 0,
                   "item": reasoning_done}),
            // Message item, closed before the function call opens.
            json!({"type": "response.output_item.added", "sequence_number": 8, "output_index": 1,
                   "item": message("msg_1_1", "in_progress", json!([]))}),
            with(
                &text_position,
                "response.content_part.added",
                9,
                json!({"part": text_part("")})
            ),
            with(
                &text_position,
                "response.output_text.delta",
                10,
                json!({"delta": "Hello", "logprobs": []})
            ),
            with(
                &text_position,
                "response.output_text.done",
                11,
                json!({"text": "Hello", "logprobs": []})
            ),
            with(
                &text_position,
                "response.content_part.done",
                12,
                json!({"part": text_part("Hello")})
            ),
            json!({"type": "response.output_item.done", "sequence_number": 13, "output_index": 1,
                   "item": message_done}),
            // Function call item.
            json!({"type": "response.output_item.added", "sequence_number": 14, "output_index": 2,
                   "item": call("in_progress", "")}),
            with(
                &none,
                "response.function_call_arguments.delta",
                15,
                json!({"item_id": "fc_call_1", "output_index": 2, "delta": "{\"p\":1}"})
            ),
            with(
                &none,
                "response.function_call_arguments.done",
                16,
                json!({"item_id": "fc_call_1", "output_index": 2, "name": "read", "arguments": "{\"p\":1}"})
            ),
            json!({"type": "response.output_item.done", "sequence_number": 17, "output_index": 2,
                   "item": call("completed", "{\"p\":1}")}),
            json!({"type": "response.completed", "sequence_number": 18,
            "response": shell(
                "completed",
                json!([reasoning_done, message_done, call("completed", "{\"p\":1}")]),
                usage_json(),
            )}),
        ]
    );
}

#[test]
fn encode_stream_terminal_response_equals_the_non_streamed_encoding() {
    let response = sample_response(
        vec![
            Part::reasoning("plan"),
            Part::text("One. "),
            Part::text("Two."),
            Part::tool_call("call_a", "f", "{\"a\":1}"),
            Part::tool_call("call_b", "g", ""),
            Part::text("After."),
        ],
        FinishReason::ToolCalls,
    );
    let ctx = ClientCtx::new("gpt-x");
    let wire = run_with(&response_to_events(&response), &ctx);
    let terminal = wire.last().unwrap();
    assert_eq!(terminal["type"], json!("response.completed"));
    assert_eq!(
        terminal["response"],
        ResponsesCodec.encode_response(&response, &ctx).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Hand-written incremental sequences
// ---------------------------------------------------------------------------

#[test]
fn encode_incremental_text_with_adjacent_blocks_citations_and_refusal() {
    let citation = Citation {
        url: Some("https://example.com".into()),
        title: Some("Example".into()),
        cited_text: None,
        start: Some(0),
        end: Some(3),
    };
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Hel".into(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "lo".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        // An adjacent text block joins the same message as a second part.
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "Fact".into(),
        },
        StreamEvent::Citation { index: 1, citation },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Refusal,
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "But no.".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::Usage(usage()),
        finish(FinishReason::Stop),
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.annotation.added",
            "response.output_text.done",
            "response.content_part.done",
            "response.content_part.added",
            "response.refusal.delta",
            "response.refusal.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    // Every event of the message carries the same item and output index.
    for event in &wire[3..17] {
        assert_eq!(event["item_id"], json!("msg_1_0"), "{event}");
        assert_eq!(event["output_index"], json!(0), "{event}");
    }
    let indices: Vec<u64> = wire[3..17]
        .iter()
        .map(|e| e["content_index"].as_u64().expect("content_index"))
        .collect();
    assert_eq!(indices, vec![0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2, 2]);
    assert_eq!(wire[5]["delta"], json!("lo"));
    assert_eq!(wire[6]["text"], json!("Hello"));

    let annotation = json!({"type": "url_citation", "start_index": 0, "end_index": 3,
                            "url": "https://example.com", "title": "Example"});
    assert_eq!(
        wire[10],
        json!({"type": "response.output_text.annotation.added", "sequence_number": 10, "item_id": "msg_1_0",
               "output_index": 0, "content_index": 1, "annotation_index": 0, "annotation": annotation})
    );
    let cited =
        json!({"type": "output_text", "annotations": [annotation], "logprobs": [], "text": "Fact"});
    assert_eq!(wire[12]["part"], cited);

    assert_eq!(
        wire[13],
        json!({"type": "response.content_part.added", "sequence_number": 13, "item_id": "msg_1_0",
               "output_index": 0, "content_index": 2, "part": {"type": "refusal", "refusal": ""}})
    );
    assert_eq!(
        wire[14],
        json!({"type": "response.refusal.delta", "sequence_number": 14, "item_id": "msg_1_0",
               "output_index": 0, "content_index": 2, "delta": "But no."})
    );
    assert_eq!(
        wire[15],
        json!({"type": "response.refusal.done", "sequence_number": 15, "item_id": "msg_1_0",
               "output_index": 0, "content_index": 2, "refusal": "But no."})
    );
    let item = message(
        "msg_1_0",
        "completed",
        json!([text_part("Hello"), cited, {"type": "refusal", "refusal": "But no."}]),
    );
    assert_eq!(wire[17]["item"], item);
    assert_eq!(
        wire[18]["response"],
        shell("completed", json!([item]), usage_json())
    );
}

#[test]
fn encode_fragmented_tool_arguments_and_parallel_calls() {
    let tool = |index: u32, id: &str, name: &str| StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolCallKind::Function,
            signature: None,
        },
    };
    let fragment = |index: u32, text: &str| StreamEvent::ToolArgsDelta {
        index,
        fragment: text.into(),
    };
    let events = vec![
        start(),
        tool(0, "call_a", "get_weather"),
        fragment(0, "{\"city\":"),
        fragment(0, "\"Paris\"}"),
        StreamEvent::BlockStop { index: 0 },
        // A call without arguments.
        tool(1, "call_b", "now"),
        StreamEvent::BlockStop { index: 1 },
        finish(FinishReason::ToolCalls),
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        wire[3],
        json!({"type": "response.function_call_arguments.delta", "sequence_number": 3,
               "item_id": "fc_call_a", "output_index": 0, "delta": "{\"city\":"})
    );
    assert_eq!(
        wire[5],
        json!({"type": "response.function_call_arguments.done", "sequence_number": 5,
               "item_id": "fc_call_a", "output_index": 0, "name": "get_weather",
               "arguments": "{\"city\":\"Paris\"}"})
    );
    // No arguments at all is reported as the empty object.
    assert_eq!(wire[8]["arguments"], json!("{}"));
    assert_eq!(
        wire[9],
        json!({"type": "response.output_item.done", "sequence_number": 9, "output_index": 1,
               "item": {"id": "fc_call_b", "type": "function_call", "status": "completed",
                        "arguments": "{}", "call_id": "call_b", "name": "now"}})
    );
    assert_eq!(wire[10]["response"]["output"].as_array().unwrap().len(), 2);
    // No usage event was seen: the terminal usage is all zeros, not null.
    assert_eq!(wire[10]["response"]["usage"]["total_tokens"], json!(0));
}

/// A `function_call` item has no field for the signature another vendor put
/// on the call (Gemini's `thoughtSignature`), a `reasoning` item does. The
/// signature travels on a summary-less reasoning item directly ahead of the
/// call, marked as a call signature and tagged with its origin, so the
/// request decoder can put it back on the call.
#[test]
fn encode_call_signature_rides_on_a_reasoning_item_ahead_of_the_call() {
    let tool = |index: u32, id: &str, signature: Option<Signature>| StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: "get_weather".into(),
            kind: ToolCallKind::Function,
            signature,
        },
    };
    let events = vec![
        start(),
        tool(
            0,
            "call_a",
            Some(Signature::new(Protocol::Gemini, "CcallSig")),
        ),
        StreamEvent::ToolArgsDelta {
            index: 0,
            fragment: "{}".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        // An unsigned call next to it gets no carrier.
        tool(1, "call_b", None),
        StreamEvent::BlockStop { index: 1 },
        finish(FinishReason::ToolCalls),
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            // The carrier: opened and closed at once.
            "response.output_item.added",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let carrier = json!({"id": "rs_1_0", "type": "reasoning", "summary": [],
                         "encrypted_content": "sy1.g.call:CcallSig"});
    assert_eq!(
        wire[2],
        json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
               "item": {"id": "rs_1_0", "type": "reasoning", "summary": []}})
    );
    assert_eq!(
        wire[3],
        json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0,
               "item": carrier})
    );
    assert_eq!(wire[4]["output_index"], json!(1));
    assert_eq!(wire[4]["item"]["call_id"], json!("call_a"));
    assert_eq!(wire[8]["output_index"], json!(2));
    assert_eq!(wire[8]["item"]["call_id"], json!("call_b"));
    let output = wire[11]["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 3);
    assert_eq!(output[0], carrier);
    assert_eq!(output[1]["type"], json!("function_call"));
    assert_eq!(output[2]["type"], json!("function_call"));
}

#[test]
fn encode_custom_tool_calls_native_and_wrapped() {
    let request = json!({"model": "gpt-x", "input": "x", "tools": [
        {"type": "custom", "name": "exec"},
        {"type": "namespace", "name": "mcp__gh", "tools": [{"type": "function", "name": "get_me"}]}
    ]});
    let ctx = ClientCtx::new("gpt-x").with_request(Arc::new(request));
    let tool = |index: u32, id: &str, name: &str, kind: ToolCallKind| StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind,
            signature: None,
        },
    };
    let fragment = |index: u32, text: &str| StreamEvent::ToolArgsDelta {
        index,
        fragment: text.into(),
    };
    let events = vec![
        start(),
        // A Responses upstream: raw input streams as input deltas.
        tool(0, "call_1", "exec", ToolCallKind::Custom),
        fragment(0, "ls "),
        fragment(0, "-la"),
        StreamEvent::BlockStop { index: 0 },
        // Any other upstream: function-shaped JSON for the declared custom
        // tool. The raw input only exists once the wrapper is complete.
        tool(1, "toolu_2", "exec", ToolCallKind::Function),
        fragment(1, "{\"input\":"),
        fragment(1, "\"pwd\"}"),
        StreamEvent::BlockStop { index: 1 },
        // A namespaced function is reported the way the client declared it.
        tool(2, "call_3", "mcp__gh__get_me", ToolCallKind::Function),
        StreamEvent::BlockStop { index: 2 },
        finish(FinishReason::ToolCalls),
    ];
    let wire = run_with(&events, &ctx);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.custom_tool_call_input.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        wire[2]["item"],
        json!({"id": "ctc_call_1", "type": "custom_tool_call", "status": "in_progress", "input": "",
               "call_id": "call_1", "name": "exec"})
    );
    assert_eq!(
        wire[4],
        json!({"type": "response.custom_tool_call_input.delta", "sequence_number": 4,
               "item_id": "ctc_call_1", "output_index": 0, "delta": "-la"})
    );
    assert_eq!(
        wire[5],
        json!({"type": "response.custom_tool_call_input.done", "sequence_number": 5,
               "item_id": "ctc_call_1", "output_index": 0, "input": "ls -la"})
    );
    assert_eq!(
        wire[8],
        json!({"type": "response.custom_tool_call_input.done", "sequence_number": 8,
               "item_id": "ctc_toolu_2", "output_index": 1, "input": "pwd"})
    );
    assert_eq!(
        wire[9]["item"],
        json!({"id": "ctc_toolu_2", "type": "custom_tool_call", "status": "completed", "input": "pwd",
               "call_id": "toolu_2", "name": "exec"})
    );
    assert_eq!(
        wire[12]["item"],
        json!({"id": "fc_call_3", "type": "function_call", "status": "completed", "arguments": "{}",
               "call_id": "call_3", "name": "get_me", "namespace": "mcp__gh"})
    );
    // The request's tools are echoed in the response objects.
    assert_eq!(wire[0]["response"]["tools"], ctx.request["tools"]);
}

#[test]
fn encode_reasoning_blobs_of_other_vendors_are_wrapped() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: Some("rs_native".into()),
                redacted: false,
            },
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "Claude ".into(),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "thinks.".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "EqQBsig"),
        },
        StreamEvent::BlockStop { index: 0 },
        // Redacted thinking: no text, only the encrypted payload.
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Reasoning {
                id: None,
                redacted: true,
            },
        },
        StreamEvent::ReasoningSignature {
            index: 1,
            signature: Signature::new(Protocol::Anthropic, "EuYBdata"),
        },
        StreamEvent::BlockStop { index: 1 },
        finish(FinishReason::Stop),
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            // No summary events for an item without text.
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        wire[8]["item"],
        json!({"id": "rs_native", "type": "reasoning",
               "summary": [{"type": "summary_text", "text": "Claude thinks."}],
               "encrypted_content": "sy1.a.EqQBsig"})
    );
    assert_eq!(
        wire[10],
        json!({"type": "response.output_item.done", "sequence_number": 10, "output_index": 1,
               "item": {"id": "rs_1_1", "type": "reasoning", "summary": [],
                        "encrypted_content": "sy1.a.redacted:EuYBdata"}})
    );
}

#[test]
fn encode_whole_blocks() {
    let search = json!({"id": "ws_1", "type": "web_search_call", "status": "completed",
                        "action": {"type": "search", "query": "q"}});
    let whole = |index: u32, part: Part| {
        [
            StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole { part },
            },
            StreamEvent::BlockStop { index },
        ]
    };
    let mut events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Searching.".into(),
        },
        StreamEvent::BlockStop { index: 0 },
    ];
    // A block of another vendor cannot be expressed: it is dropped and does
    // not even close the open message.
    events.extend(whole(
        1,
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw: json!({"type": "server_tool_use", "id": "srvtoolu_1"}),
        }),
    ));
    events.extend(whole(
        2,
        Part::Opaque(OpaquePart {
            origin: P,
            raw: search.clone(),
        }),
    ));
    events.extend(whole(
        3,
        Part::Image(MediaPart::base64("image/png", "iVBOR")),
    ));
    events.extend(whole(4, Part::text("Whole text.")));
    events.push(finish(FinishReason::Stop));

    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            // web_search_call, verbatim
            "response.output_item.added",
            "response.output_item.done",
            // generated image
            "response.output_item.added",
            "response.output_item.done",
            // a whole text part streams like any other text
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        wire[8],
        json!({"type": "response.output_item.added", "sequence_number": 8, "output_index": 1, "item": search})
    );
    let image = json!({"id": "ig_1_2", "type": "image_generation_call", "status": "completed",
                       "output_format": "png", "result": "iVBOR"});
    assert_eq!(
        wire[11],
        json!({"type": "response.output_item.done", "sequence_number": 11, "output_index": 2, "item": image})
    );
    assert_eq!(
        wire[18]["response"]["output"],
        json!([
            message("msg_1_0", "completed", json!([text_part("Searching.")])),
            search,
            image,
            message("msg_1_3", "completed", json!([text_part("Whole text.")]))
        ])
    );
}

// ---------------------------------------------------------------------------
// Terminators
// ---------------------------------------------------------------------------

#[test]
fn encode_incomplete_and_failed_terminators() {
    let terminal = |reason: FinishReason| {
        let events = vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Cut of".into(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Usage(usage()),
            finish(reason),
        ];
        let wire = run(&events);
        // The trailing message was left unfinished.
        assert_eq!(
            wire[wire.len() - 2]["type"],
            json!("response.output_item.done")
        );
        assert_eq!(wire[wire.len() - 2]["item"]["status"], json!("incomplete"));
        wire.last().unwrap().clone()
    };
    let item = message("msg_1_0", "incomplete", json!([text_part("Cut of")]));

    let mut expected = shell("incomplete", json!([item]), usage_json());
    expected["incomplete_details"] = json!({"reason": "max_output_tokens"});
    assert_eq!(
        terminal(FinishReason::Length),
        json!({"type": "response.incomplete", "sequence_number": 8, "response": expected})
    );

    let filtered = terminal(FinishReason::ContentFilter);
    assert_eq!(filtered["type"], json!("response.incomplete"));
    assert_eq!(
        filtered["response"]["incomplete_details"],
        json!({"reason": "content_filter"})
    );

    let mut expected = shell("failed", json!([item]), usage_json());
    expected["error"] =
        json!({"code": "server_error", "message": "The model failed to generate a response."});
    assert_eq!(
        terminal(FinishReason::Error),
        json!({"type": "response.failed", "sequence_number": 8, "response": expected})
    );
}

#[test]
fn encode_error_terminated_sequence() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Hel".into(),
        },
        StreamEvent::Error(ApiError::rate_limit("Rate limit reached, slow down.")),
        // Contract violation; must be ignored after the terminal event.
        StreamEvent::TextDelta {
            index: 0,
            text: "lo".into(),
        },
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "error",
        ]
    );
    // The canonical terminal error event: flat fields as documented for SSE
    // plus the nested object WebSocket clients read.
    assert_eq!(
        wire[5],
        json!({
            "type": "error",
            "sequence_number": 5,
            "status": 429,
            "code": "rate_limit_exceeded",
            "message": "Rate limit reached, slow down.",
            "param": null,
            "error": {
                "message": "Rate limit reached, slow down.",
                "type": "rate_limit_error",
                "param": null,
                "code": "rate_limit_exceeded"
            }
        })
    );
}

#[test]
fn encode_error_before_anything_else() {
    let error = ApiError::invalid_request("`input` is too long")
        .with_code("context_length_exceeded")
        .with_param("input");
    let wire = run(&[StreamEvent::Error(error)]);
    assert_eq!(
        wire,
        vec![json!({
            "type": "error",
            "sequence_number": 0,
            "status": 400,
            "code": "context_length_exceeded",
            "message": "`input` is too long",
            "param": "input",
            "error": {
                "message": "`input` is too long",
                "type": "invalid_request_error",
                "param": "input",
                "code": "context_length_exceeded"
            }
        })]
    );
}

#[test]
fn encode_truncated_sequence_ends_with_a_terminal_error() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Half".into(),
        },
    ];
    let wire = run(&events);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "error",
        ]
    );
    assert_eq!(
        wire[5],
        json!({
            "type": "error",
            "sequence_number": 5,
            "status": 502,
            "code": "upstream_error",
            "message": "upstream stream closed before a terminal event",
            "param": null,
            "error": {
                "message": "upstream stream closed before a terminal event",
                "type": "server_error",
                "param": null,
                "code": "upstream_error"
            }
        })
    );

    // Nothing at all: the client still gets a terminal event.
    let empty = run(&[]);
    assert_eq!(types(&empty), vec!["error"]);
}

#[test]
fn encode_nothing_after_a_terminal_event() {
    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new("gpt-x"));
    let mut count = 0;
    for event in response_to_events(&sample_response(vec![Part::text("x")], FinishReason::Stop)) {
        count += encoder.encode(&event).len();
    }
    assert_eq!(count, 9);
    assert!(encoder.encode(&finish(FinishReason::Stop)).is_empty());
    assert!(
        encoder
            .encode(&StreamEvent::Error(ApiError::internal("late")))
            .is_empty()
    );
    // A completed stream needs no terminator of its own.
    assert!(encoder.finish().is_empty());
}

// ---------------------------------------------------------------------------
// Ids, model and echoed request fields
// ---------------------------------------------------------------------------

#[test]
fn encode_start_shapes_ids_and_fills_gaps() {
    let created = |id: &str, created: i64| {
        let events = vec![
            StreamEvent::Start {
                id: id.into(),
                model: "upstream".into(),
                created,
            },
            finish(FinishReason::Stop),
        ];
        let wire = run_with(&events, &ClientCtx::new(""));
        assert_eq!(
            types(&wire),
            vec![
                "response.created",
                "response.in_progress",
                "response.completed"
            ]
        );
        // All three response objects describe the same response.
        assert_eq!(wire[0]["response"]["id"], wire[2]["response"]["id"]);
        assert_eq!(
            wire[0]["response"]["created_at"],
            wire[2]["response"]["created_at"]
        );
        wire[0]["response"].clone()
    };
    let native = created("resp_abc", 1_700_000_000);
    assert_eq!(native["id"], json!("resp_abc"));
    assert_eq!(native["created_at"], json!(1_700_000_000));
    assert_eq!(native["status"], json!("in_progress"));
    assert_eq!(native["usage"], Value::Null);
    // Without a client-facing model name the upstream's is reported.
    assert_eq!(native["model"], json!("upstream"));

    assert_eq!(created("chatcmpl-9", 5)["id"], json!("resp_chatcmpl-9"));

    let minted = created("", 0);
    let id = minted["id"].as_str().unwrap();
    assert!(id.starts_with("resp_") && id.len() == 29, "{id}");
    assert!(minted["created_at"].as_i64().unwrap() > 1_700_000_000);
}

#[test]
fn encode_sequence_without_start_is_started_implicitly() {
    let events = vec![
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Stop),
    ];
    let wire = run(&events);
    assert_eq!(wire[0]["type"], json!("response.created"));
    assert_eq!(wire.last().unwrap()["type"], json!("response.completed"));
}

#[test]
fn encode_echoes_request_fields_in_every_response_object() {
    let request = json!({
        "model": "alias(high)",
        "input": "hi",
        "instructions": "Be brief.",
        "temperature": 0.2,
        "max_output_tokens": 64,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "metadata": {"k": "v"}
    });
    let ctx = ClientCtx::new("alias").with_request(Arc::new(request));
    let wire = run_with(&[start(), finish(FinishReason::Stop)], &ctx);
    for event in &wire {
        let response = &event["response"];
        assert_eq!(response["model"], json!("alias"));
        assert_eq!(response["instructions"], json!("Be brief."));
        assert_eq!(response["temperature"], json!(0.2));
        assert_eq!(response["max_output_tokens"], json!(64));
        assert_eq!(
            response["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(response["store"], json!(false));
        assert_eq!(response["metadata"], json!({"k": "v"}));
    }
}

#[test]
fn encode_refusal_finish_without_refusal_part_is_reported_as_filtered() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "I was about to".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Refusal),
    ];
    let wire = run(&events);
    let terminal = wire.last().unwrap();
    assert_eq!(terminal["type"], json!("response.incomplete"));
    assert_eq!(
        terminal["response"]["incomplete_details"],
        json!({"reason": "content_filter"})
    );

    // With a refusal part the response completes normally.
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Refusal,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "No.".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Refusal),
    ];
    let wire = run(&events);
    assert_eq!(wire.last().unwrap()["type"], json!("response.completed"));
}
