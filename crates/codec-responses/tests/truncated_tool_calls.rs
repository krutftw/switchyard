//! Tool calls that generation was cut off in.
//!
//! When the output limit (or a filter, or a failure) ends a response inside
//! a tool call, the vendor reports that call item with `status:
//! "incomplete"` and its arguments as far as they got. A client must be able
//! to tell such a call from a finished one: executing a call whose argument
//! document is half written is a bug, and replacing missing arguments with
//! `{}` would turn it into a valid-looking call. Both the non-streamed and
//! the streamed encoding have to agree on this.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{FinishReason, Part, Response, ToolCall, ToolCallKind};
use switchyard_core::stream::{Accumulator, BlockStart, StreamEvent, response_to_events};
use switchyard_core::{ApiError, ClientCtx, Codec, SseEvent};

fn response(parts: Vec<Part>, finish: FinishReason) -> Response {
    let mut response = Response::new("resp_1", "upstream-model");
    response.created = 1_700_000_000;
    response.parts = parts;
    response.finish = finish;
    response
}

fn encode(response: &Response, ctx: &ClientCtx) -> Value {
    ResponsesCodec
        .encode_response(response, ctx)
        .expect("encodes")
}

fn stream(events: &[StreamEvent], ctx: &ClientCtx) -> Vec<Value> {
    let mut encoder = ResponsesCodec.stream_encoder(ctx);
    let mut wire: Vec<SseEvent> = Vec::new();
    for event in events {
        wire.extend(encoder.encode(event));
    }
    wire.extend(encoder.finish());
    wire.iter()
        .enumerate()
        .map(|(n, sse)| {
            let value: Value = serde_json::from_str(&sse.data).expect("event data is JSON");
            assert_eq!(sse.event.as_deref(), value["type"].as_str());
            assert_eq!(value["sequence_number"], json!(n));
            value
        })
        .collect()
}

fn types(wire: &[Value]) -> Vec<&str> {
    wire.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

fn custom_call(id: &str, name: &str, input: &str) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: input.into(),
        kind: ToolCallKind::Custom,
        signature: None,
        cache_control: None,
    })
}

fn call_json(call_id: &str, status: &str, arguments: &str) -> Value {
    json!({"id": format!("fc_{call_id}"), "type": "function_call", "status": status,
           "arguments": arguments, "call_id": call_id, "name": "lookup"})
}

#[test]
fn non_streamed_trailing_call_is_incomplete_when_generation_was_cut_off() {
    let ctx = ClientCtx::new("gpt-x");
    let parts = || {
        vec![
            Part::text("Looking it up."),
            Part::tool_call("call_1", "lookup", "{\"q\":1}"),
            Part::tool_call("call_2", "lookup", "{\"q\":"),
        ]
    };
    for (finish, reason) in [
        (FinishReason::Length, json!({"reason": "max_output_tokens"})),
        (
            FinishReason::ContentFilter,
            json!({"reason": "content_filter"}),
        ),
    ] {
        let body = encode(&response(parts(), finish), &ctx);
        assert_eq!(body["status"], json!("incomplete"));
        assert_eq!(body["incomplete_details"], reason);
        assert_eq!(
            body["output"],
            json!([
                {"id": "msg_1_0", "type": "message", "status": "completed", "role": "assistant",
                 "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "Looking it up."}]},
                // Only the call generation stopped in is unfinished.
                call_json("call_1", "completed", "{\"q\":1}"),
                call_json("call_2", "incomplete", "{\"q\":"),
            ])
        );
    }

    // A normal finish: every call is complete.
    let body = encode(&response(parts(), FinishReason::ToolCalls), &ctx);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output"][2]["status"], json!("completed"));
}

#[test]
fn non_streamed_cut_off_call_without_arguments_is_not_given_an_empty_object() {
    let ctx = ClientCtx::new("gpt-x");
    let cut = encode(
        &response(
            vec![Part::tool_call("call_1", "lookup", "")],
            FinishReason::Length,
        ),
        &ctx,
    );
    assert_eq!(
        cut["output"],
        json!([call_json("call_1", "incomplete", "")])
    );
    // Whereas a finished call with no arguments means "no arguments".
    let whole = encode(
        &response(
            vec![Part::tool_call("call_1", "lookup", "")],
            FinishReason::ToolCalls,
        ),
        &ctx,
    );
    assert_eq!(
        whole["output"],
        json!([call_json("call_1", "completed", "{}")])
    );
}

#[test]
fn non_streamed_cut_off_message_after_a_call_leaves_the_call_complete() {
    let ctx = ClientCtx::new("gpt-x");
    let body = encode(
        &response(
            vec![
                Part::tool_call("call_1", "lookup", "{}"),
                Part::text("And th"),
            ],
            FinishReason::Length,
        ),
        &ctx,
    );
    assert_eq!(body["output"][0]["status"], json!("completed"));
    assert_eq!(body["output"][1]["status"], json!("incomplete"));
}

#[test]
fn streamed_cut_off_call_is_closed_as_incomplete_with_the_final_event() {
    let ctx = ClientCtx::new("gpt-x");
    let events = vec![
        StreamEvent::Start {
            id: "resp_1".into(),
            model: "upstream-model".into(),
            created: 1_700_000_000,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 0,
            fragment: "{\"q\":".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::Finish {
            reason: FinishReason::Length,
            stop_sequence: None,
        },
    ];
    let wire = stream(&events, &ctx);
    assert_eq!(
        types(&wire),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.incomplete",
        ]
    );
    assert_eq!(
        wire[4],
        json!({"type": "response.function_call_arguments.done", "sequence_number": 4,
               "item_id": "fc_call_1", "output_index": 0, "name": "lookup", "arguments": "{\"q\":"})
    );
    assert_eq!(
        wire[5],
        json!({"type": "response.output_item.done", "sequence_number": 5, "output_index": 0,
               "item": call_json("call_1", "incomplete", "{\"q\":")})
    );
    assert_eq!(wire[6]["response"]["status"], json!("incomplete"));
    assert_eq!(
        wire[6]["response"]["output"],
        json!([call_json("call_1", "incomplete", "{\"q\":")])
    );

    // The streamed terminal response is the non-streamed encoding.
    let whole = response(
        vec![Part::tool_call("call_1", "lookup", "{\"q\":")],
        FinishReason::Length,
    );
    assert_eq!(
        wire[6]["response"]["output"],
        encode(&whole, &ctx)["output"]
    );
}

#[test]
fn streamed_calls_followed_by_anything_else_are_complete() {
    let ctx = ClientCtx::new("gpt-x");
    let whole = response(
        vec![
            Part::tool_call("call_1", "lookup", ""),
            Part::tool_call("call_2", "lookup", "{\"q\":2}"),
            Part::text("cut he"),
        ],
        FinishReason::Length,
    );
    let wire = stream(&response_to_events(&whole), &ctx);
    let terminal = wire.last().unwrap();
    assert_eq!(terminal["type"], json!("response.incomplete"));
    assert_eq!(
        terminal["response"]["output"],
        json!([
            call_json("call_1", "completed", "{}"),
            call_json("call_2", "completed", "{\"q\":2}"),
            {"id": "msg_1_2", "type": "message", "status": "incomplete", "role": "assistant",
             "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "cut he"}]},
        ])
    );
    assert_eq!(
        terminal["response"]["output"],
        encode(&whole, &ctx)["output"]
    );

    // Every item is closed before the next one opens, in output order.
    let item_events: Vec<(&str, u64)> = wire
        .iter()
        .filter(|e| {
            matches!(
                e["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            )
        })
        .map(|e| {
            (
                e["type"].as_str().unwrap(),
                e["output_index"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        item_events,
        vec![
            ("response.output_item.added", 0),
            ("response.output_item.done", 0),
            ("response.output_item.added", 1),
            ("response.output_item.done", 1),
            ("response.output_item.added", 2),
            ("response.output_item.done", 2),
        ]
    );
}

#[test]
fn streamed_call_is_closed_no_later_than_the_next_event_that_follows_it() {
    // The closing events wait for the event after `BlockStop`; nothing else
    // may slip in between and nothing may be lost when the stream then fails
    // or simply ends.
    let ctx = ClientCtx::new("gpt-x");
    let head = vec![
        StreamEvent::Start {
            id: "resp_1".into(),
            model: "upstream-model".into(),
            created: 1_700_000_000,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::BlockStop { index: 0 },
    ];
    let closing = [
        "response.function_call_arguments.done",
        "response.output_item.done",
    ];

    // Usage does not close it; the finish does, as a completed call.
    let mut finished = head.clone();
    finished.push(StreamEvent::Usage(switchyard_core::Usage {
        input_tokens: 1,
        output_tokens: 1,
        ..Default::default()
    }));
    finished.push(StreamEvent::Finish {
        reason: FinishReason::ToolCalls,
        stop_sequence: None,
    });
    let wire = stream(&finished, &ctx);
    assert_eq!(
        types(&wire)[3..],
        [closing[0], closing[1], "response.completed"]
    );
    assert_eq!(wire[4]["item"], call_json("call_1", "completed", "{}"));

    // A stream error after the block had stopped: the call is whole.
    let mut failed = head.clone();
    failed.push(StreamEvent::Error(ApiError::upstream("boom")));
    let wire = stream(&failed, &ctx);
    assert_eq!(types(&wire)[3..], [closing[0], closing[1], "error"]);
    assert_eq!(wire[4]["item"]["status"], json!("completed"));

    // The sequence just stops.
    let wire = stream(&head, &ctx);
    assert_eq!(types(&wire)[3..], [closing[0], closing[1], "error"]);

    // A `Finish { Error }` is a cut-off like any other.
    let mut broken = head.clone();
    broken.push(StreamEvent::Finish {
        reason: FinishReason::Error,
        stop_sequence: None,
    });
    let wire = stream(&broken, &ctx);
    assert_eq!(
        types(&wire)[3..],
        [closing[0], closing[1], "response.failed"]
    );
    assert_eq!(wire[4]["item"], call_json("call_1", "incomplete", ""));
}

#[test]
fn cut_off_custom_tool_input_is_unwrapped_as_far_as_it_got() {
    // The upstream had no notion of custom tools and produced the function
    // wrapper `{"input": "..."}` for a tool the client declared as custom.
    let request =
        json!({"model": "gpt-x", "input": "go", "tools": [{"type": "custom", "name": "shell"}]});
    let ctx = ClientCtx::new("gpt-x").with_request(Arc::new(request));
    let expected = json!([{"id": "ctc_call_1", "type": "custom_tool_call", "status": "incomplete",
                           "input": "ls -la /va", "call_id": "call_1", "name": "shell"}]);

    let whole = response(
        vec![Part::tool_call(
            "call_1",
            "shell",
            "{\"input\":\"ls -la /va",
        )],
        FinishReason::Length,
    );
    assert_eq!(encode(&whole, &ctx)["output"], expected);

    let wire = stream(&response_to_events(&whole), &ctx);
    let done = wire
        .iter()
        .find(|e| e["type"] == json!("response.custom_tool_call_input.done"))
        .expect("input.done");
    assert_eq!(done["input"], json!("ls -la /va"));
    assert_eq!(wire.last().unwrap()["response"]["output"], expected);
    assert!(
        types(&wire)
            .iter()
            .all(|t| !t.starts_with("response.function_call_arguments")),
        "a custom tool never gets function-call events"
    );

    // A native custom call is raw text already.
    let native = response(
        vec![custom_call("call_1", "shell", "ls -la /va")],
        FinishReason::Length,
    );
    assert_eq!(encode(&native, &ctx)["output"], expected);
}

#[test]
fn incomplete_call_items_decode_and_round_trip() {
    // Upstream side: an `incomplete` call item is still a tool call part,
    // with its arguments untouched.
    let body = json!({
        "id": "resp_1", "object": "response", "status": "incomplete", "model": "m",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [call_json("call_1", "incomplete", "{\"q\":")]
    });
    let decoded = ResponsesCodec.decode_response(&body).expect("decodes");
    assert_eq!(decoded.finish, FinishReason::Length);
    assert_eq!(
        decoded.parts,
        vec![Part::tool_call("call_1", "lookup", "{\"q\":")]
    );

    // Encoded for a client (both ways) and decoded again: the same response.
    let ctx = ClientCtx::new("m");
    for arguments in ["{\"q\":", ""] {
        let original = response(
            vec![
                Part::text("x"),
                Part::tool_call("call_1", "lookup", arguments),
            ],
            FinishReason::Length,
        );
        let back = ResponsesCodec
            .decode_response(&encode(&original, &ctx))
            .unwrap();
        assert_eq!(back.parts, original.parts);
        assert_eq!(back.finish, FinishReason::Length);

        let mut encoder = ResponsesCodec.stream_encoder(&ctx);
        let mut decoder = ResponsesCodec.stream_decoder();
        let mut accumulator = Accumulator::new();
        let mut wire = Vec::new();
        for event in response_to_events(&original) {
            wire.extend(encoder.encode(&event));
        }
        wire.extend(encoder.finish());
        for sse in &wire {
            for event in decoder.decode(sse).unwrap() {
                accumulator.push(&event);
            }
        }
        for event in decoder.finish() {
            accumulator.push(&event);
        }
        let streamed = accumulator.into_response();
        assert_eq!(streamed.parts, original.parts);
        assert_eq!(streamed.finish, FinishReason::Length);
    }
}
