//! The stream decoder against vendor-style transcripts. Every produced
//! sequence is checked with `validate_sequence` and folded back into a
//! `Response`.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::stream::{Accumulator, BlockStart, StreamEvent, validate_sequence};
use switchyard_core::{ApiError, Codec, ErrorKind, Protocol, SseEvent, SseParser, Usage};

const P: Protocol = Protocol::OpenaiResponses;

/// Feeds raw SSE text through the parser and the decoder, the way the gateway
/// does, and closes the stream.
fn decode_wire(wire: &str) -> Vec<StreamEvent> {
    let mut parser = SseParser::new();
    let mut decoder = ResponsesCodec.stream_decoder();
    let mut out = Vec::new();
    // Split at an awkward place to exercise incremental parsing too.
    let (head, tail) = wire.as_bytes().split_at(wire.len() / 3);
    for chunk in [head, tail] {
        for event in parser.push(chunk).expect("valid SSE") {
            out.extend(decoder.decode(&event).expect("decodable"));
        }
    }
    if let Some(event) = parser.finish() {
        out.extend(decoder.decode(&event).expect("decodable"));
    }
    out.extend(decoder.finish());
    assert!(decoder.finish().is_empty(), "finish() must be idempotent");
    if let Err(violation) = validate_sequence(&out) {
        panic!("sequence contract violated: {violation}\n{out:#?}");
    }
    out
}

/// Renders events the way the vendor frames them: `event:` + `data:` with a
/// monotonically increasing `sequence_number`.
fn wire(events: &[Value]) -> String {
    let mut out = String::new();
    for (n, event) in events.iter().enumerate() {
        let mut event = event.clone();
        event["sequence_number"] = json!(n);
        let name = event["type"].as_str().unwrap_or("message").to_string();
        out.push_str(&format!("event: {name}\ndata: {event}\n\n"));
    }
    out
}

fn decode_events(events: &[Value]) -> Vec<StreamEvent> {
    decode_wire(&wire(events))
}

fn accumulate(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    assert!(acc.started() && acc.finished());
    acc.into_response()
}

/// The streamed result must be the same response a non-streaming call would
/// have decoded from the terminal event's response object.
fn assert_matches_final(events: &[StreamEvent], final_response: &Value) {
    let mut expected = ResponsesCodec.decode_response(final_response).unwrap();
    expected.service_tier = None;
    assert_eq!(accumulate(events), expected);
}

fn response_shell(status: &str) -> Value {
    json!({
        "id": "resp_0f1e2d", "object": "response", "created_at": 1741290958, "status": status,
        "error": null, "incomplete_details": null, "model": "gpt-5-2025-08-07", "output": [],
        "usage": null
    })
}

fn created() -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": response_shell("in_progress")}),
        json!({"type": "response.in_progress", "response": response_shell("in_progress")}),
    ]
}

fn completed(output: Value, usage: Value) -> Value {
    let mut response = response_shell("completed");
    response["output"] = output;
    response["usage"] = usage;
    json!({"type": "response.completed", "response": response})
}

fn usage_json() -> Value {
    json!({
        "input_tokens": 100, "input_tokens_details": {"cached_tokens": 60},
        "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 20},
        "total_tokens": 150
    })
}

fn usage_ir() -> Usage {
    Usage {
        input_tokens: 40,
        cache_read_tokens: 60,
        cache_write_tokens: 0,
        output_tokens: 50,
        reasoning_tokens: 20,
    }
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "resp_0f1e2d".into(),
        model: "gpt-5-2025-08-07".into(),
        created: 1_741_290_958,
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn message_added(index: u64, id: &str) -> Value {
    json!({"type": "response.output_item.added", "output_index": index,
           "item": {"id": id, "type": "message", "status": "in_progress", "content": [], "role": "assistant"}})
}

fn text_part_added(index: u64, id: &str) -> Value {
    json!({"type": "response.content_part.added", "item_id": id, "output_index": index, "content_index": 0,
           "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""}})
}

fn text_delta(index: u64, id: &str, delta: &str) -> Value {
    json!({"type": "response.output_text.delta", "item_id": id, "output_index": index, "content_index": 0,
           "delta": delta, "logprobs": [], "obfuscation": "Zx9"})
}

fn message_item(id: &str, text: &str) -> Value {
    json!({"id": id, "type": "message", "status": "completed", "role": "assistant",
           "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": text}]})
}

/// The closing events of a streamed text message.
fn text_done(index: u64, id: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "response.output_text.done", "item_id": id, "output_index": index, "content_index": 0,
               "text": text, "logprobs": []}),
        json!({"type": "response.content_part.done", "item_id": id, "output_index": index, "content_index": 0,
               "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": text}}),
        json!({"type": "response.output_item.done", "output_index": index, "item": message_item(id, text)}),
    ]
}

/// A complete streamed text message.
fn text_message(index: u64, id: &str, deltas: &[&str]) -> Vec<Value> {
    let mut events = vec![message_added(index, id), text_part_added(index, id)];
    events.extend(deltas.iter().map(|d| text_delta(index, id, d)));
    events.extend(text_done(index, id, &deltas.concat()));
    events
}

fn function_item(id: &str, call_id: &str, name: &str, arguments: &str, status: &str) -> Value {
    json!({"id": id, "type": "function_call", "status": status, "arguments": arguments,
           "call_id": call_id, "name": name})
}

/// A complete streamed function call.
fn function_call(
    index: u64,
    id: &str,
    call_id: &str,
    name: &str,
    fragments: &[&str],
) -> Vec<Value> {
    let arguments = fragments.concat();
    let mut events = vec![
        json!({"type": "response.output_item.added", "output_index": index,
                                 "item": function_item(id, call_id, name, "", "in_progress")}),
    ];
    events.extend(fragments.iter().map(|f| {
        json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": index, "delta": f})
    }));
    events.push(json!({"type": "response.function_call_arguments.done", "item_id": id, "output_index": index,
                       "name": name, "arguments": arguments}));
    events.push(
        json!({"type": "response.output_item.done", "output_index": index,
                       "item": function_item(id, call_id, name, &arguments, "completed")}),
    );
    events
}

fn tool_start(index: u32, id: &str, name: &str) -> StreamEvent {
    StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolCallKind::Function,
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

fn text(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        text: text.into(),
    }
}

// ---------------------------------------------------------------------------
// Transcripts
// ---------------------------------------------------------------------------

/// A verbatim-style capture of a plain text answer.
const TEXT_ONLY: &str = r#"event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_0f1e2d","object":"response","created_at":1741290958,"status":"in_progress","background":false,"error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"gpt-5-2025-08-07","output":[],"parallel_tool_calls":true,"previous_response_id":null,"reasoning":{"effort":null,"summary":null},"service_tier":"auto","store":true,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":null,"user":null,"metadata":{}}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_0f1e2d","object":"response","created_at":1741290958,"status":"in_progress","background":false,"error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"gpt-5-2025-08-07","output":[],"parallel_tool_calls":true,"previous_response_id":null,"reasoning":{"effort":null,"summary":null},"service_tier":"auto","store":true,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":null,"user":null,"metadata":{}}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_0a1","type":"message","status":"in_progress","content":[],"role":"assistant"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_0a1","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_0a1","output_index":0,"content_index":0,"delta":"Hello","logprobs":[],"obfuscation":"Qf3aZ9"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_0a1","output_index":0,"content_index":0,"delta":" there","logprobs":[],"obfuscation":"b7"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":6,"item_id":"msg_0a1","output_index":0,"content_index":0,"delta":"!","logprobs":[],"obfuscation":"T0pQ2xY"}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":7,"item_id":"msg_0a1","output_index":0,"content_index":0,"text":"Hello there!","logprobs":[]}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":8,"item_id":"msg_0a1","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello there!"}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":9,"output_index":0,"item":{"id":"msg_0a1","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello there!"}],"role":"assistant"}}

event: response.completed
data: {"type":"response.completed","sequence_number":10,"response":{"id":"resp_0f1e2d","object":"response","created_at":1741290958,"status":"completed","background":false,"error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"gpt-5-2025-08-07","output":[{"id":"msg_0a1","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello there!"}],"role":"assistant"}],"parallel_tool_calls":true,"previous_response_id":null,"reasoning":{"effort":null,"summary":null},"service_tier":"default","store":true,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":50,"output_tokens_details":{"reasoning_tokens":20},"total_tokens":150},"user":null,"metadata":{}}}

"#;

#[test]
fn stream_text_only() {
    let events = decode_wire(TEXT_ONLY);
    assert_eq!(
        events,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text(0, "Hello"),
            text(0, " there"),
            text(0, "!"),
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Usage(usage_ir()),
            finish(FinishReason::Stop),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(response.text(), "Hello there!");
    assert_eq!(response.usage, usage_ir());
}

#[test]
fn stream_text_is_emitted_incrementally() {
    // Deltas must flow as they arrive, not be held until the end.
    let mut decoder = ResponsesCodec.stream_decoder();
    let mut feed = |event: Value| {
        decoder
            .decode(&SseEvent::json(event["type"].as_str(), &event))
            .unwrap()
    };
    let mut events = created().into_iter();
    assert_eq!(feed(events.next().unwrap()), vec![start()]);
    assert_eq!(feed(events.next().unwrap()), vec![]);
    assert_eq!(feed(message_added(0, "msg_1")), vec![]);
    assert_eq!(
        feed(text_part_added(0, "msg_1")),
        vec![StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text
        }]
    );
    assert_eq!(feed(text_delta(0, "msg_1", "Hel")), vec![text(0, "Hel")]);
    assert_eq!(feed(text_delta(0, "msg_1", "lo")), vec![text(0, "lo")]);
}

#[test]
fn stream_reasoning_then_text() {
    let reasoning_final = json!({
        "id": "rs_1", "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "First thought."}, {"type": "summary_text", "text": "Second."}],
        "encrypted_content": "gAAAAAB_final"
    });
    let summary = |n: u64, kind: &str, extra: Value| {
        let mut event =
            json!({"type": kind, "item_id": "rs_1", "output_index": 0, "summary_index": n});
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    let mut events = created();
    events.extend([
        // The blob in `added` may be incomplete; only the final one counts.
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "gAAAAAB_partial"}}),
        summary(0, "response.reasoning_summary_part.added", json!({"part": {"type": "summary_text", "text": ""}})),
        summary(0, "response.reasoning_summary_text.delta", json!({"delta": "First"})),
        summary(0, "response.reasoning_summary_text.delta", json!({"delta": " thought."})),
        summary(0, "response.reasoning_summary_text.done", json!({"text": "First thought."})),
        summary(0, "response.reasoning_summary_part.done", json!({"part": {"type": "summary_text", "text": "First thought."}})),
        summary(1, "response.reasoning_summary_part.added", json!({"part": {"type": "summary_text", "text": ""}})),
        summary(1, "response.reasoning_summary_text.delta", json!({"delta": "Second."})),
        summary(1, "response.reasoning_summary_text.done", json!({"text": "Second."})),
        summary(1, "response.reasoning_summary_part.done", json!({"part": {"type": "summary_text", "text": "Second."}})),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_final}),
    ]);
    events.extend(text_message(1, "msg_1", &["The answer", " is 42."]));
    let terminal = completed(
        json!([reasoning_final, message_item("msg_1", "The answer is 42.")]),
        usage_json(),
    );
    events.push(terminal.clone());

    let decoded = decode_events(&events);
    assert_eq!(
        decoded,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Reasoning {
                    id: Some("rs_1".into()),
                    redacted: false
                },
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "First".into()
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: " thought.".into()
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "\n\n".into()
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "Second.".into()
            },
            // The signature arrives with `output_item.done`, before the stop.
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: Signature::new(P, "gAAAAAB_final")
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::BlockStart {
                index: 1,
                block: BlockStart::Text
            },
            text(1, "The answer"),
            text(1, " is 42."),
            StreamEvent::BlockStop { index: 1 },
            StreamEvent::Usage(usage_ir()),
            finish(FinishReason::Stop),
        ]
    );
    assert_eq!(
        accumulate(&decoded).parts,
        vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "First thought.\n\nSecond.".into(),
                signature: Some(Signature::new(P, "gAAAAAB_final")),
                redacted: false,
            }),
            Part::text("The answer is 42."),
        ]
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_reasoning_whose_blob_only_arrives_in_item_done() {
    // No summary at all: the item is an encrypted blob and nothing else.
    let item = json!({"id": "rs_9", "type": "reasoning", "summary": [], "encrypted_content": "gAAAAAB_only"});
    let mut events = created();
    events.push(
        json!({"type": "response.output_item.added", "output_index": 0,
                       "item": {"id": "rs_9", "type": "reasoning", "summary": []}}),
    );
    events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": item}));
    events.extend(text_message(1, "msg_1", &["ok"]));
    let terminal = completed(json!([item, message_item("msg_1", "ok")]), usage_json());
    events.push(terminal.clone());

    let decoded = decode_events(&events);
    assert_eq!(
        decoded[1..4],
        [
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Reasoning {
                    id: Some("rs_9".into()),
                    redacted: false
                },
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: Signature::new(P, "gAAAAAB_only")
            },
            StreamEvent::BlockStop { index: 0 },
        ]
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_text_and_multiple_tool_calls_with_fragmented_arguments() {
    let mut events = created();
    events.extend(text_message(0, "msg_1", &["Let me ", "check."]));
    events.extend(function_call(
        1,
        "fc_1",
        "call_a",
        "get_weather",
        &["{\"ci", "ty\":\"Par", "is\"}"],
    ));
    events.extend(function_call(
        2,
        "fc_2",
        "call_b",
        "get_weather",
        &["{\"city\":", "\"Rome\"}"],
    ));
    let terminal = completed(
        json!([
            message_item("msg_1", "Let me check."),
            function_item(
                "fc_1",
                "call_a",
                "get_weather",
                "{\"city\":\"Paris\"}",
                "completed"
            ),
            function_item(
                "fc_2",
                "call_b",
                "get_weather",
                "{\"city\":\"Rome\"}",
                "completed"
            )
        ]),
        usage_json(),
    );
    events.push(terminal.clone());

    let decoded = decode_events(&events);
    assert_eq!(
        decoded,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text(0, "Let me "),
            text(0, "check."),
            StreamEvent::BlockStop { index: 0 },
            tool_start(1, "call_a", "get_weather"),
            args(1, "{\"ci"),
            args(1, "ty\":\"Par"),
            args(1, "is\"}"),
            StreamEvent::BlockStop { index: 1 },
            tool_start(2, "call_b", "get_weather"),
            args(2, "{\"city\":"),
            args(2, "\"Rome\"}"),
            StreamEvent::BlockStop { index: 2 },
            StreamEvent::Usage(usage_ir()),
            finish(FinishReason::ToolCalls),
        ]
    );
    assert_eq!(
        accumulate(&decoded).parts,
        vec![
            Part::text("Let me check."),
            Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
            Part::tool_call("call_b", "get_weather", "{\"city\":\"Rome\"}"),
        ]
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_interleaved_tool_calls_are_serialised() {
    // Deltas of two calls alternate; the canonical stream may not overlap.
    let delta = |id: &str, index: u64, fragment: &str| json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": index, "delta": fragment});
    let added = |id: &str, index: u64, call_id: &str, name: &str| {
        json!({"type": "response.output_item.added", "output_index": index,
               "item": function_item(id, call_id, name, "", "in_progress")})
    };
    let done = |id: &str, index: u64, call_id: &str, name: &str, arguments: &str| {
        json!({"type": "response.output_item.done", "output_index": index,
               "item": function_item(id, call_id, name, arguments, "completed")})
    };
    let mut events = created();
    events.extend([
        added("fc_1", 0, "call_a", "first"),
        added("fc_2", 1, "call_b", "second"),
        delta("fc_1", 0, "{\"a\":"),
        delta("fc_2", 1, "{\"b\":"),
        delta("fc_1", 0, "1}"),
        delta("fc_2", 1, "2}"),
        // The second call even completes first.
        done("fc_2", 1, "call_b", "second", "{\"b\":2}"),
        done("fc_1", 0, "call_a", "first", "{\"a\":1}"),
        completed(json!([]), usage_json()),
    ]);
    let decoded = decode_events(&events);
    assert_eq!(
        decoded,
        vec![
            start(),
            tool_start(0, "call_a", "first"),
            args(0, "{\"a\":"),
            args(0, "1}"),
            StreamEvent::BlockStop { index: 0 },
            tool_start(1, "call_b", "second"),
            args(1, "{\"b\":"),
            args(1, "2}"),
            StreamEvent::BlockStop { index: 1 },
            StreamEvent::Usage(usage_ir()),
            finish(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn stream_function_call_arguments_that_only_arrive_in_done_events() {
    let mut events = created();
    events.extend([
        // No deltas: the arguments come with `arguments.done`.
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": function_item("fc_1", "call_a", "alpha", "", "in_progress")}),
        json!({"type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 0,
               "arguments": "{\"x\":1}"}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": function_item("fc_1", "call_a", "alpha", "{\"x\":1}", "completed")}),
        // Not even that: only `output_item.done` has them.
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": function_item("fc_2", "call_b", "beta", "", "in_progress")}),
        json!({"type": "response.output_item.done", "output_index": 1,
               "item": function_item("fc_2", "call_b", "beta", "{\"y\":2}", "completed")}),
        // And an item that was never announced.
        json!({"type": "response.output_item.done", "output_index": 2,
               "item": function_item("fc_3", "call_c", "gamma", "{}", "completed")}),
        completed(json!([]), Value::Null),
    ]);
    let decoded = decode_events(&events);
    assert_eq!(
        accumulate(&decoded).parts,
        vec![
            Part::tool_call("call_a", "alpha", "{\"x\":1}"),
            Part::tool_call("call_b", "beta", "{\"y\":2}"),
            Part::tool_call("call_c", "gamma", "{}"),
        ]
    );
}

#[test]
fn stream_custom_tool_call() {
    let item = |status: &str, input: &str| {
        json!({"id": "ctc_1", "type": "custom_tool_call", "status": status, "call_id": "call_x",
               "name": "exec", "input": input})
    };
    let mut events = created();
    events.extend([
        json!({"type": "response.output_item.added", "output_index": 0, "item": item("in_progress", "")}),
        json!({"type": "response.custom_tool_call_input.delta", "item_id": "ctc_1", "output_index": 0, "delta": "ls "}),
        json!({"type": "response.custom_tool_call_input.delta", "item_id": "ctc_1", "output_index": 0, "delta": "-la"}),
        json!({"type": "response.custom_tool_call_input.done", "item_id": "ctc_1", "output_index": 0, "input": "ls -la"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item("completed", "ls -la")}),
    ]);
    let terminal = completed(json!([item("completed", "ls -la")]), usage_json());
    events.push(terminal.clone());
    let decoded = decode_events(&events);
    assert_eq!(
        accumulate(&decoded).parts,
        vec![Part::ToolCall(ToolCall {
            id: "call_x".into(),
            name: "exec".into(),
            arguments: "ls -la".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })]
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_refusal() {
    let part = |text: &str| json!({"type": "refusal", "refusal": text});
    let mut events = created();
    events.extend([
        message_added(0, "msg_1"),
        json!({"type": "response.content_part.added", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": part("")}),
        json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "I can't "}),
        json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "help with that."}),
        json!({"type": "response.refusal.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "refusal": "I can't help with that."}),
        json!({"type": "response.content_part.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": part("I can't help with that.")}),
    ]);
    let item = json!({"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                      "content": [part("I can't help with that.")]});
    events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": item}));
    let terminal = completed(json!([item]), usage_json());
    events.push(terminal.clone());

    let decoded = decode_events(&events);
    assert_eq!(
        decoded[1..5],
        [
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Refusal
            },
            text(0, "I can't "),
            text(0, "help with that."),
            StreamEvent::BlockStop { index: 0 },
        ]
    );
    let response = accumulate(&decoded);
    assert_eq!(
        response.parts,
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })]
    );
    assert_eq!(response.finish, FinishReason::Refusal);
    assert_matches_final(&decoded, &terminal["response"]);
}

// ---------------------------------------------------------------------------
// Usage placement
// ---------------------------------------------------------------------------

#[test]
fn stream_usage_placement_variants() {
    let usage_events = |terminal: Value| {
        let mut events = created();
        events.extend(text_message(0, "msg_1", &["hi"]));
        events.push(terminal);
        decode_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // The documented place: inside the terminal event's response object.
    assert_eq!(
        usage_events(completed(json!([]), usage_json())),
        vec![usage_ir()]
    );

    // Next to the response object (seen on compatible servers).
    let mut beside = completed(json!([]), Value::Null);
    beside["usage"] = usage_json();
    assert_eq!(usage_events(beside), vec![usage_ir()]);

    // Chat-style key names.
    let chat_style = completed(
        json!([]),
        json!({"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150,
               "prompt_tokens_details": {"cached_tokens": 60},
               "completion_tokens_details": {"reasoning_tokens": 20}}),
    );
    assert_eq!(usage_events(chat_style), vec![usage_ir()]);

    // On `response.incomplete` as well.
    let mut incomplete = completed(json!([]), usage_json());
    incomplete["type"] = json!("response.incomplete");
    incomplete["response"]["status"] = json!("incomplete");
    incomplete["response"]["incomplete_details"] = json!({"reason": "max_output_tokens"});
    assert_eq!(usage_events(incomplete), vec![usage_ir()]);

    // None at all: no Usage event is invented.
    assert_eq!(usage_events(completed(json!([]), Value::Null)), vec![]);
}

// ---------------------------------------------------------------------------
// Failures and truncation
// ---------------------------------------------------------------------------

#[test]
fn stream_error_event_mid_stream() {
    let mut events = created();
    events.extend([
        message_added(0, "msg_1"),
        text_part_added(0, "msg_1"),
        text_delta(0, "msg_1", "Hel"),
    ]);
    // The documented SSE shape is flat.
    events.push(json!({
        "type": "error", "code": "rate_limit_exceeded", "param": null,
        "message": "Rate limit reached for gpt-5 on tokens per min. Please try again in 1.5s."
    }));
    // Whatever follows a terminal event is ignored.
    events.push(text_delta(0, "msg_1", "lo"));
    let decoded = decode_events(&events);
    assert_eq!(
        decoded,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text(0, "Hel"),
            StreamEvent::Error(ApiError {
                status: 429,
                kind: ErrorKind::RateLimit,
                message:
                    "Rate limit reached for gpt-5 on tokens per min. Please try again in 1.5s."
                        .into(),
                code: Some("rate_limit_exceeded".into()),
                param: None,
                retry_after_secs: Some(2),
            }),
        ]
    );
    let response = accumulate(&decoded);
    assert_eq!(response.finish, FinishReason::Error);
    assert_eq!(response.text(), "Hel");
}

#[test]
fn stream_error_shapes_and_status_derivation() {
    let error_of = |event: Value| {
        let mut events = created();
        events.push(event);
        match decode_events(&events).pop() {
            Some(StreamEvent::Error(error)) => error,
            other => panic!("expected an error, got {other:?}"),
        }
    };

    // WebSocket-style nested error with an explicit status.
    let nested = error_of(json!({
        "type": "error", "status": 400,
        "error": {"type": "invalid_request_error", "code": "context_length_exceeded",
                  "message": "Your input exceeds the context window of this model.", "param": "input"}
    }));
    assert_eq!(
        (nested.status, nested.kind),
        (400, ErrorKind::InvalidRequest)
    );
    assert_eq!(nested.code.as_deref(), Some("context_length_exceeded"));
    assert_eq!(nested.param.as_deref(), Some("input"));

    // `response.failed` carries the error inside the response object.
    let mut failed_response = response_shell("failed");
    failed_response["error"] =
        json!({"code": "server_error", "message": "The model produced invalid content."});
    let failed = error_of(json!({"type": "response.failed", "response": failed_response}));
    assert_eq!((failed.status, failed.kind), (502, ErrorKind::Upstream));
    assert_eq!(failed.message, "The model produced invalid content.");
    assert_eq!(failed.code.as_deref(), Some("server_error"));

    // Status fields are honoured wherever the upstream put them.
    let status_code = error_of(json!({"type": "error", "status_code": 503,
                                      "error": {"message": "Overloaded", "type": "service_unavailable_error"}}));
    assert_eq!(
        (status_code.status, status_code.kind),
        (503, ErrorKind::Unavailable)
    );
    let inner = error_of(json!({"type": "error", "error": {"message": "nope", "status": 401}}));
    assert_eq!((inner.status, inner.kind), (401, ErrorKind::Authentication));

    // Without a status the vendor's code decides; unknown codes are a 502.
    let overloaded =
        error_of(json!({"type": "error", "code": "server_is_overloaded", "message": "busy"}));
    assert_eq!(overloaded.status, 503);
    let quota = error_of(
        json!({"type": "error", "error": {"type": "insufficient_quota", "message": "You exceeded your current quota"}}),
    );
    assert_eq!((quota.status, quota.kind), (429, ErrorKind::RateLimit));
    let unknown = error_of(json!({"type": "error", "message": "something broke"}));
    assert_eq!((unknown.status, unknown.kind), (502, ErrorKind::Upstream));

    // Any frame with an error object counts, whatever it calls itself.
    let disguised = error_of(
        json!({"type": "response.output_text.delta", "error": {"message": "stream cut", "code": "request_timeout"}}),
    );
    assert_eq!(disguised.status, 504);
}

#[test]
fn stream_error_as_the_very_first_event_still_yields_a_valid_sequence() {
    let decoded = decode_events(&[json!({
        "type": "error", "code": "invalid_prompt", "message": "Invalid prompt: flagged.", "param": null
    })]);
    assert_eq!(decoded.len(), 2);
    assert!(matches!(&decoded[0], StreamEvent::Start { id, .. } if id.starts_with("resp_")));
    match &decoded[1] {
        StreamEvent::Error(error) => {
            assert_eq!((error.status, error.kind), (400, ErrorKind::InvalidRequest));
            assert_eq!(error.code.as_deref(), Some("invalid_prompt"));
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn stream_truncated_without_terminal_event() {
    let mut events = created();
    events.extend([
        message_added(0, "msg_1"),
        text_part_added(0, "msg_1"),
        text_delta(0, "msg_1", "Half an ans"),
    ]);
    let decoded = decode_events(&events);
    assert_eq!(
        decoded,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text(0, "Half an ans"),
            // Synthesised by finish(): close the block, report the failure.
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Error),
        ]
    );
    assert_eq!(accumulate(&decoded).text(), "Half an ans");
}

#[test]
fn stream_truncated_mid_tool_call_and_empty_stream() {
    let mut events = created();
    events.push(
        json!({"type": "response.output_item.added", "output_index": 0,
                       "item": function_item("fc_1", "call_a", "f", "", "in_progress")}),
    );
    events.push(json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": "{\"a\""}));
    let decoded = decode_events(&events);
    let response = accumulate(&decoded);
    assert_eq!(
        response.parts,
        vec![Part::tool_call("call_a", "f", "{\"a\"")]
    );
    assert_eq!(response.finish, FinishReason::Error);

    // Nothing at all from the upstream.
    let empty = decode_wire("");
    assert_eq!(empty.len(), 2);
    assert!(matches!(empty[0], StreamEvent::Start { .. }));
    assert_eq!(empty[1], finish(FinishReason::Error));
}

// ---------------------------------------------------------------------------
// Unknown events and non-modelled items
// ---------------------------------------------------------------------------

#[test]
fn stream_unknown_events_interleaved() {
    let search = json!({"id": "ws_1", "type": "web_search_call", "status": "completed",
                        "action": {"type": "search", "query": "rust 2024 edition"}});
    let annotation = json!({"type": "url_citation", "url": "https://blog.rust-lang.org", "title": "Rust Blog",
                            "start_index": 0, "end_index": 9});
    let cited = json!({"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                       "content": [{"type": "output_text", "annotations": [annotation], "logprobs": [], "text": "Rust 2024 shipped."}]});
    let mut events = created();
    events.extend([
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "ws_1", "type": "web_search_call", "status": "in_progress"}}),
        json!({"type": "response.web_search_call.in_progress", "output_index": 0, "item_id": "ws_1"}),
        json!({"type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1"}),
        json!({"type": "response.some_future_event", "payload": {"anything": true}}),
        json!({"type": "response.web_search_call.completed", "output_index": 0, "item_id": "ws_1"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": search}),
        message_added(1, "msg_1"),
        text_part_added(1, "msg_1"),
        json!({"type": "keepalive"}),
        text_delta(1, "msg_1", "Rust 2024 shipped."),
        json!({"type": "response.output_text.annotation.added", "item_id": "msg_1", "output_index": 1,
               "content_index": 0, "annotation_index": 0, "annotation": annotation}),
        json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 1, "content_index": 0,
               "text": "Rust 2024 shipped.", "logprobs": []}),
        json!({"type": "response.content_part.done", "item_id": "msg_1", "output_index": 1, "content_index": 0,
               "part": cited["content"][0]}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": cited}),
    ]);
    let terminal = completed(json!([search, cited]), usage_json());
    events.push(terminal.clone());

    // Noise that is not even a Responses event.
    let mut raw = wire(&events[..4]);
    raw.push_str(": comment line\n\n");
    raw.push_str("data: not json at all\n\n");
    raw.push_str("data: [1, 2, 3]\n\n");
    raw.push_str("event: ping\ndata: {}\n\n");
    raw.push_str(&wire(&events[4..]));
    raw.push_str("data: [DONE]\n\n");

    let decoded = decode_wire(&raw);
    let citation = Citation {
        url: Some("https://blog.rust-lang.org".into()),
        title: Some("Rust Blog".into()),
        cited_text: None,
        start: Some(0),
        end: Some(9),
    };
    assert_eq!(
        decoded,
        vec![
            start(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Whole {
                    part: Part::Opaque(OpaquePart {
                        origin: P,
                        raw: search.clone()
                    }),
                },
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::BlockStart {
                index: 1,
                block: BlockStart::Text
            },
            text(1, "Rust 2024 shipped."),
            // Announced once by the annotation event, not again by the part.
            StreamEvent::Citation {
                index: 1,
                citation: citation.clone()
            },
            StreamEvent::BlockStop { index: 1 },
            StreamEvent::Usage(usage_ir()),
            finish(FinishReason::Stop),
        ]
    );
    assert_eq!(
        accumulate(&decoded).parts[1],
        Part::Text(TextPart {
            text: "Rust 2024 shipped.".into(),
            citations: vec![citation],
            ..TextPart::default()
        })
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_image_generation_item_becomes_a_whole_image_block() {
    let image = json!({"id": "ig_1", "type": "image_generation_call", "status": "completed",
                       "output_format": "png", "result": "iVBORw0KGgo="});
    let mut events = created();
    events.extend([
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "ig_1", "type": "image_generation_call", "status": "in_progress"}}),
        json!({"type": "response.image_generation_call.generating", "output_index": 0, "item_id": "ig_1"}),
        json!({"type": "response.image_generation_call.partial_image", "output_index": 0, "item_id": "ig_1",
               "partial_image_index": 0, "partial_image_b64": "AAAA"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": image}),
        completed(json!([image]), usage_json()),
    ]);
    assert_eq!(
        accumulate(&decode_events(&events)).parts,
        vec![Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo="))]
    );
}

// ---------------------------------------------------------------------------
// Terminal-event variants
// ---------------------------------------------------------------------------

#[test]
fn stream_empty_output_array_in_completed_is_rebuilt_from_item_done_events() {
    let mut events = created();
    events.extend(text_message(0, "msg_1", &["From the ", "items."]));
    events.extend(function_call(1, "fc_1", "call_a", "f", &["{}"]));
    // Some upstreams send `"output": []` here.
    events.push(completed(json!([]), usage_json()));
    let response = accumulate(&decode_events(&events));
    assert_eq!(
        response.parts,
        vec![
            Part::text("From the items."),
            Part::tool_call("call_a", "f", "{}")
        ]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
    assert_eq!(response.usage, usage_ir());
}

#[test]
fn stream_output_only_in_the_terminal_event() {
    // An upstream that does not stream at all: one terminal event.
    let reasoning = json!({"id": "rs_1", "type": "reasoning", "summary": [{"type": "summary_text", "text": "Hm."}],
                           "encrypted_content": "gAAAAAB_x"});
    let terminal = completed(
        json!([
            reasoning,
            message_item("msg_1", "All at once."),
            function_item("fc_1", "call_a", "f", "{\"k\":true}", "completed")
        ]),
        usage_json(),
    );
    let decoded = decode_events(std::slice::from_ref(&terminal));
    assert_eq!(decoded[0], start());
    assert_eq!(
        accumulate(&decoded).parts,
        vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "Hm.".into(),
                signature: Some(Signature::new(P, "gAAAAAB_x")),
                redacted: false,
            }),
            Part::text("All at once."),
            Part::tool_call("call_a", "f", "{\"k\":true}"),
        ]
    );
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_items_missing_their_done_event_are_completed_from_the_terminal_output() {
    let mut events = created();
    // The message streams but is never closed; the call is only announced.
    events.extend([
        message_added(0, "msg_1"),
        text_part_added(0, "msg_1"),
        text_delta(0, "msg_1", "Open ended"),
    ]);
    events.push(
        json!({"type": "response.output_item.added", "output_index": 1,
                       "item": function_item("fc_1", "call_a", "f", "", "in_progress")}),
    );
    let terminal = completed(
        json!([
            message_item("msg_1", "Open ended"),
            function_item("fc_1", "call_a", "f", "{\"late\":1}", "completed")
        ]),
        usage_json(),
    );
    events.push(terminal.clone());
    let decoded = decode_events(&events);
    // Nothing is duplicated and nothing is lost.
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_incomplete_maps_to_length_and_content_filter() {
    let finish_of = |reason: Value| {
        let mut events = created();
        events.extend([
            message_added(0, "msg_1"),
            text_part_added(0, "msg_1"),
            text_delta(0, "msg_1", "cut"),
        ]);
        let mut response = response_shell("incomplete");
        response["incomplete_details"] = reason;
        response["usage"] = usage_json();
        events.push(json!({"type": "response.incomplete", "response": response}));
        let decoded = decode_events(&events);
        let response = accumulate(&decoded);
        assert_eq!(response.text(), "cut");
        response.finish
    };
    assert_eq!(
        finish_of(json!({"reason": "max_output_tokens"})),
        FinishReason::Length
    );
    assert_eq!(
        finish_of(json!({"reason": "content_filter"})),
        FinishReason::ContentFilter
    );
    assert_eq!(finish_of(Value::Null), FinishReason::Length);
}

#[test]
fn stream_nothing_is_decoded_after_the_terminal_event() {
    let mut decoder = ResponsesCodec.stream_decoder();
    let feed = |decoder: &mut Box<dyn switchyard_core::StreamDecoder>, event: Value| {
        decoder
            .decode(&SseEvent::json(event["type"].as_str(), &event))
            .unwrap()
    };
    let mut all = Vec::new();
    for event in created() {
        all.extend(feed(&mut decoder, event));
    }
    all.extend(feed(
        &mut decoder,
        completed(json!([message_item("msg_1", "done")]), usage_json()),
    ));
    assert!(feed(&mut decoder, text_delta(0, "msg_1", "late")).is_empty());
    assert!(feed(&mut decoder, completed(json!([]), usage_json())).is_empty());
    assert!(decoder.finish().is_empty());
    validate_sequence(&all).unwrap();
    assert_eq!(accumulate(&all).text(), "done");
}

// ---------------------------------------------------------------------------
// Sloppy "compatible" upstreams
// ---------------------------------------------------------------------------

#[test]
fn stream_from_a_compatible_server_without_created_ids_or_part_events() {
    // No response.created, no item ids, no content_part events, data-only
    // framing, and a terminal event named only by its SSE event line.
    let text = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\" there\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"model\":\"llama-3\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
    );
    let decoded = decode_wire(text);
    match &decoded[0] {
        StreamEvent::Start { id, model, created } => {
            // No upstream id: one is minted in the vendor's shape.
            assert!(id.starts_with("resp_") && id.len() == 29, "{id}");
            assert_eq!((model.as_str(), *created), ("", 0));
        }
        other => panic!("expected Start, got {other:?}"),
    }
    let response = accumulate(&decoded);
    assert_eq!(response.text(), "Hi there");
    assert_eq!(response.finish, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        }
    );
}

#[test]
fn stream_raw_reasoning_text_channel() {
    // Open-weight models stream `reasoning_text` content instead of summaries.
    let item = json!({"id": "rs_1", "type": "reasoning", "summary": [],
                      "content": [{"type": "reasoning_text", "text": "Let me think."}]});
    let mut events = created();
    events.extend([
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "rs_1", "type": "reasoning", "summary": [], "content": []}}),
        json!({"type": "response.content_part.added", "item_id": "rs_1", "output_index": 0, "content_index": 0,
               "part": {"type": "reasoning_text", "text": ""}}),
        json!({"type": "response.reasoning_text.delta", "item_id": "rs_1", "output_index": 0, "content_index": 0, "delta": "Let me "}),
        json!({"type": "response.reasoning_text.delta", "item_id": "rs_1", "output_index": 0, "content_index": 0, "delta": "think."}),
        json!({"type": "response.reasoning_text.done", "item_id": "rs_1", "output_index": 0, "content_index": 0, "text": "Let me think."}),
        json!({"type": "response.content_part.done", "item_id": "rs_1", "output_index": 0, "content_index": 0,
               "part": {"type": "reasoning_text", "text": "Let me think."}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
    ]);
    events.extend(text_message(1, "msg_1", &["Done."]));
    let terminal = completed(json!([item, message_item("msg_1", "Done.")]), usage_json());
    events.push(terminal.clone());
    let decoded = decode_events(&events);
    assert_eq!(accumulate(&decoded).reasoning_text(), "Let me think.");
    assert_matches_final(&decoded, &terminal["response"]);
}

#[test]
fn stream_text_delivered_only_by_done_events() {
    // content_part.added, then straight to output_text.done.
    let mut events = created();
    events.extend([message_added(0, "msg_1"), text_part_added(0, "msg_1")]);
    events.extend(text_done(0, "msg_1", "No deltas here."));
    events.push(completed(json!([]), Value::Null));
    assert_eq!(
        accumulate(&decode_events(&events)).text(),
        "No deltas here."
    );

    // Only output_item.done knows the text.
    let mut events = created();
    events.push(message_added(0, "msg_1"));
    events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": message_item("msg_1", "Item only.")}));
    events.push(completed(json!([]), Value::Null));
    assert_eq!(accumulate(&decode_events(&events)).text(), "Item only.");
}

#[test]
fn stream_from_an_upstream_that_reuses_output_index_zero() {
    // Every item claims output_index 0; item ids (or their absence) are the
    // only thing telling them apart.
    let mut events = created();
    events.extend(text_message(0, "msg_1", &["First."]));
    events.extend(function_call(0, "fc_1", "call_a", "f", &["{\"a\":1}"]));
    events.extend([
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "message", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "Last."}),
    ]);
    events.push(completed(json!([]), usage_json()));
    assert_eq!(
        accumulate(&decode_events(&events)).parts,
        vec![
            Part::text("First."),
            Part::tool_call("call_a", "f", "{\"a\":1}"),
            Part::text("Last."),
        ]
    );
}

#[test]
fn stream_item_done_events_that_reuse_an_output_index_are_distinct_items() {
    // No `added` events and every `done` claims index 0: the ids decide.
    let mut events = created();
    events.extend([
        json!({"type": "response.output_item.done", "output_index": 0, "item": message_item("msg_1", "One.")}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": function_item("fc_1", "call_a", "f", "{}", "completed")}),
        // A repeated event for the same item is not a new item.
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": function_item("fc_1", "call_a", "f", "{}", "completed")}),
        completed(json!([]), Value::Null),
    ]);
    assert_eq!(
        accumulate(&decode_events(&events)).parts,
        vec![Part::text("One."), Part::tool_call("call_a", "f", "{}")]
    );
}
