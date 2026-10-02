//! Regression tests added while fixing the review findings: the neighbouring
//! cases of each finding (the same rule applied where the reviewer did not
//! look), and guards for behaviour the fixes must not change.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::error::ApiError;
use switchyard_core::ir::{
    FinishReason, FunctionTool, MediaPart, MediaSource, Message, Part, Reasoning, Request,
    Response, ResponseFormat, Role, Signature, Tool, ToolCall, ToolCallKind, ToolResult,
};
use switchyard_core::protocol::Protocol;
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::{Usage, sig};

fn chunk(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    )
}

fn usage_only(usage: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [], "usage": usage
        })
    )
}

const DONE: &str = "data: [DONE]\n\n";

fn finish_of(events: &[StreamEvent]) -> FinishReason {
    match events.last() {
        Some(StreamEvent::Finish { reason, .. }) => reason.clone(),
        other => panic!("expected a Finish event, got {other:?}"),
    }
}

fn calls_of(response: &Response) -> Vec<(&str, &str)> {
    response
        .tool_calls()
        .map(|c| (c.name.as_str(), c.arguments.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// CHAT-1: schema normalisation
// ---------------------------------------------------------------------------

fn function_tool(parameters: Value) -> Tool {
    Tool::Function(FunctionTool {
        name: "f".into(),
        description: None,
        parameters,
        strict: Some(true),
        cache_control: None,
    })
}

fn encoded_parameters(source: Protocol, parameters: Value) -> Value {
    let mut request = Request::new("gpt-x", source);
    request.messages.push(Message::user_text("hi"));
    request.tools.push(function_tool(parameters));
    encode_request(&request)["tools"][0]["function"]["parameters"].clone()
}

#[test]
fn schemas_written_by_a_chat_client_are_forwarded_untouched() {
    // The client wrote this for a Chat API; re-encoding (which only happens
    // when its body carries a wrapped signature) must not edit it.
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string", "pattern": "^\\p{L}+$"}, "any": true},
        "additionalProperties": {"type": "object"}
    });
    assert_eq!(
        encoded_parameters(Protocol::OpenaiChat, schema.clone()),
        schema
    );
    // No parameters at all still becomes the canonical empty schema.
    assert_eq!(
        encoded_parameters(Protocol::OpenaiChat, Value::Null),
        json!({"type": "object", "properties": {}})
    );
}

#[test]
fn every_other_source_gets_its_schemas_normalised() {
    for source in [
        Protocol::Anthropic,
        Protocol::Gemini,
        Protocol::OpenaiResponses,
    ] {
        assert_eq!(
            encoded_parameters(
                source,
                json!({"type": "object", "$defs": {"node": {"type": "object"}}, "anyOf": [true]})
            ),
            json!({
                "type": "object",
                "$defs": {"node": {"type": "object", "properties": {}}},
                "anyOf": [{}],
                "properties": {}
            }),
            "{source}"
        );
        assert_eq!(
            encoded_parameters(source, Value::Null),
            json!({"type": "object", "properties": {}}),
            "{source}"
        );
        // A root that is not a schema object cannot be `parameters`.
        assert_eq!(
            encoded_parameters(source, json!(true)),
            json!({"type": "object", "properties": {}}),
            "{source}"
        );
    }
}

#[test]
fn response_format_schemas_follow_the_same_rule() {
    let schema =
        json!({"type": "object", "properties": {"items": {"type": "array", "items": true}}});
    let format = |source: Protocol| {
        let mut request = Request::new("gpt-x", source);
        request.messages.push(Message::user_text("hi"));
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: Some("out".into()),
            description: None,
            schema: schema.clone(),
            strict: None,
        });
        encode_request(&request)["response_format"]["json_schema"]["schema"].clone()
    };
    assert_eq!(format(Protocol::OpenaiChat), schema);
    assert_eq!(
        format(Protocol::Gemini),
        json!({"type": "object", "properties": {"items": {"type": "array", "items": {}}}})
    );
}

// ---------------------------------------------------------------------------
// CHAT-2: tool results that answer nothing
// ---------------------------------------------------------------------------

fn result(call_id: &str, content: Vec<Part>) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: None,
        content,
        is_error: false,
        cache_control: None,
    })
}

fn text_result(call_id: &str, text: &str) -> Part {
    result(call_id, vec![Part::text(text)])
}

#[test]
fn a_result_that_precedes_its_call_is_user_text_and_the_later_one_answers() {
    let mut request = Request::new("gpt-x", Protocol::Anthropic);
    request.messages = vec![
        Message::new(Role::User, vec![text_result("call_1", "too early")]),
        Message::new(Role::Assistant, vec![Part::tool_call("call_1", "f", "{}")]),
        Message::new(Role::User, vec![text_result("call_1", "on time")]),
    ];
    assert_eq!(
        encoded_messages(&request),
        json!([
            {"role": "user", "content": "too early"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "on time"}
        ])
    );
}

#[test]
fn an_id_issued_twice_may_be_answered_twice() {
    // Gemini-style histories reuse synthesised ids; each issue is one answer.
    let call = || Message::new(Role::Assistant, vec![Part::tool_call("call_1", "f", "{}")]);
    let answer = |text: &str| Message::new(Role::User, vec![text_result("call_1", text)]);
    let mut request = Request::new("gpt-x", Protocol::Gemini);
    request.messages = vec![
        call(),
        answer("one"),
        call(),
        answer("two"),
        answer("three"),
    ];
    let roles: Vec<String> = encoded_messages(&request)
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(roles, ["assistant", "tool", "assistant", "tool", "user"]);
}

#[test]
fn blank_orphan_results_are_dropped_and_orphan_media_is_still_relayed() {
    let mut request = Request::new("gpt-x", Protocol::OpenaiResponses);
    request.messages = vec![Message::new(
        Role::User,
        vec![
            text_result("call_gone", "   "),
            result(
                "call_gone_too",
                vec![Part::Image(MediaPart::base64("image/png", "AAAA"))],
            ),
        ],
    )];
    let messages = encoded_messages(&request);
    let messages = messages.as_array().unwrap();
    assert!(messages.iter().all(|m| m["role"] == "user"), "{messages:?}");
    // The image the orphan carried is not lost.
    let last = messages.last().unwrap();
    assert_eq!(
        last["content"].as_array().unwrap().last().unwrap(),
        &json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}})
    );
}

#[test]
fn an_orphan_between_two_answers_does_not_break_the_tool_group() {
    let mut request = Request::new("gpt-x", Protocol::Anthropic);
    request.messages = vec![
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("call_1", "f", "{}"),
                Part::tool_call("call_2", "g", "{}"),
                // Never answered: the batch is incomplete, so the message
                // realignment pass leaves this turn alone and the order has
                // to be right as emitted.
                Part::tool_call("call_3", "h", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                text_result("call_1", "one"),
                text_result("call_x", "stray"),
                text_result("call_2", "two"),
            ],
        ),
    ];
    let messages = encoded_messages(&request);
    let summary: Vec<(String, String)> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    // Both tool messages follow the assistant turn directly; the stray
    // output is shown afterwards.
    assert_eq!(
        summary,
        [
            ("assistant".to_string(), String::new()),
            ("tool".to_string(), "one".to_string()),
            ("tool".to_string(), "two".to_string()),
            ("user".to_string(), "stray".to_string()),
        ]
    );
}

// ---------------------------------------------------------------------------
// CHAT-3: cut-off tool arguments
// ---------------------------------------------------------------------------

fn call_chunk(index: u64, id: &str, name: &str, arguments: &str) -> String {
    chunk(
        json!({"tool_calls": [
            {"index": index, "id": id, "type": "function",
             "function": {"name": name, "arguments": arguments}}
        ]}),
        Value::Null,
    )
}

#[test]
fn explicit_length_and_content_filter_win_over_tool_calls() {
    for (reason, expected) in [
        ("length", FinishReason::Length),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        for arguments in ["{\"a\":1}", "{\"a\":"] {
            let transcript = [
                call_chunk(0, "call_1", "f", arguments),
                chunk(json!({}), json!(reason)),
                DONE.to_string(),
            ]
            .concat();
            assert_eq!(
                finish_of(&decode_stream(&transcript)),
                expected,
                "{reason} {arguments}"
            );
        }
    }
}

#[test]
fn whitespace_only_arguments_are_a_cut_off_call() {
    let transcript = [
        call_chunk(0, "call_1", "f", "  "),
        chunk(json!({}), json!("tool_calls")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(finish_of(&decode_stream(&transcript)), FinishReason::Length);
}

#[test]
fn arguments_cut_across_chunks_with_a_trailing_usage_chunk() {
    // No finish reason and no terminator: the trailing usage chunk ends the
    // stream, and the half-written call makes it an incomplete turn.
    let transcript = [
        call_chunk(0, "call_1", "f", "{\"path\":"),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"/tm"}}]}),
            Value::Null,
        ),
        usage_only(json!({"prompt_tokens": 7, "completion_tokens": 9, "total_tokens": 16})),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(finish_of(&events), FinishReason::Length);
    let response = accumulate(&events);
    assert_eq!(calls_of(&response), [("f", "{\"path\":\"/tm")]);
    assert_eq!(response.usage.output_tokens, 9);
}

#[test]
fn custom_tool_input_is_free_text_and_never_counts_as_cut_off() {
    let transcript = [
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "call_1", "type": "custom",
                 "custom": {"name": "shell", "input": "ls -la {"}}
            ]}),
            Value::Null,
        ),
        chunk(json!({}), json!("stop")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        finish_of(&decode_stream(&transcript)),
        FinishReason::ToolCalls
    );
}

#[test]
fn a_truncated_stream_stays_an_error_whatever_the_arguments_look_like() {
    // Connection lost: no finish reason, no usage, no terminator.
    for arguments in ["{\"a\":1}", "{\"a\":"] {
        let events = decode_stream(&call_chunk(0, "call_1", "f", arguments));
        assert_eq!(finish_of(&events), FinishReason::Error, "{arguments}");
    }
}

#[test]
fn non_streamed_responses_apply_the_same_rule() {
    let decode = |arguments: &str, finish: Value| {
        ChatCodec
            .decode_response(&json!({
                "id": "c1", "object": "chat.completion", "created": 1, "model": "m",
                "choices": [{"index": 0, "finish_reason": finish, "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function",
                                    "function": {"name": "f", "arguments": arguments}}]
                }}]
            }))
            .unwrap()
            .finish
    };
    for finish in [json!("stop"), json!("tool_calls"), Value::Null] {
        assert_eq!(
            decode("{\"a\":1}", finish.clone()),
            FinishReason::ToolCalls,
            "{finish}"
        );
        assert_eq!(
            decode("", finish.clone()),
            FinishReason::ToolCalls,
            "{finish}"
        );
        assert_eq!(
            decode("{\"a\":", finish.clone()),
            FinishReason::Length,
            "{finish}"
        );
    }
    assert_eq!(decode("{\"a\":1}", json!("length")), FinishReason::Length);
    assert_eq!(
        decode("{\"a\":", json!("content_filter")),
        FinishReason::ContentFilter
    );
}

// ---------------------------------------------------------------------------
// CHAT-5: usage placement
// ---------------------------------------------------------------------------

#[test]
fn usage_chunks_in_the_middle_do_not_complete_a_stream_that_then_breaks() {
    let transcript = [
        chunk(json!({"role": "assistant", "content": "The"}), Value::Null),
        usage_only(json!({"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6})),
        chunk(json!({"content": " answer is"}), Value::Null),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(finish_of(&events), FinishReason::Error);
    assert_eq!(accumulate(&events).text(), "The answer is");
}

#[test]
fn a_trailing_usage_chunk_still_completes_a_stream_without_finish_reason() {
    let usage = json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8});
    let transcript = [
        chunk(json!({"role": "assistant", "content": "The"}), Value::Null),
        usage_only(usage.clone()),
        chunk(json!({"content": " answer"}), Value::Null),
        usage_only(usage),
    ]
    .concat();
    let events = decode_stream(&transcript);
    assert_eq!(finish_of(&events), FinishReason::Stop);
    assert_eq!(accumulate(&events).usage.output_tokens, 3);

    // An empty delta after the usage chunk is not output and changes nothing.
    let transcript = [
        chunk(json!({"role": "assistant", "content": "Hi"}), Value::Null),
        usage_only(json!({"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6})),
        chunk(json!({"content": "", "tool_calls": []}), Value::Null),
    ]
    .concat();
    assert_eq!(finish_of(&decode_stream(&transcript)), FinishReason::Stop);
}

// ---------------------------------------------------------------------------
// CHAT-6: several calls on one wire index
// ---------------------------------------------------------------------------

fn idless(name: Option<&str>, arguments: &str) -> String {
    let mut function = json!({"arguments": arguments});
    if let Some(name) = name {
        function["name"] = json!(name);
    }
    chunk(
        json!({"tool_calls": [{"index": 0, "type": "function", "function": function}]}),
        Value::Null,
    )
}

fn tool_turn(chunks: &[String]) -> Response {
    let mut transcript = chunks.concat();
    transcript.push_str(&chunk(json!({}), json!("tool_calls")));
    transcript.push_str(DONE);
    accumulate(&decode_stream(&transcript))
}

#[test]
fn the_same_function_called_twice_on_one_index_without_ids() {
    let response = tool_turn(&[
        idless(Some("f"), "{\"x\":1}"),
        idless(Some("f"), "{\"x\":2}"),
    ]);
    assert_eq!(
        calls_of(&response),
        [("f", "{\"x\":1}"), ("f", "{\"x\":2}")]
    );
    let ids: Vec<&str> = response.tool_calls().map(|c| c.id.as_str()).collect();
    assert!(ids.iter().all(|id| id.starts_with("call_")));
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn a_repeated_header_without_ids_continues_the_call() {
    // The name is repeated on every fragment, and once more after the
    // arguments are complete (with nothing to add).
    let response = tool_turn(&[
        idless(Some("f"), "{\"x\""),
        idless(Some("f"), ":1}"),
        idless(Some("f"), ""),
        idless(None, ""),
    ]);
    assert_eq!(calls_of(&response), [("f", "{\"x\":1}")]);
}

#[test]
fn a_different_name_opens_a_call_even_before_the_first_is_complete() {
    let response = tool_turn(&[idless(Some("a"), "{\"x\""), idless(Some("b"), "{\"y\":2}")]);
    assert_eq!(calls_of(&response), [("a", "{\"x\""), ("b", "{\"y\":2}")]);
    // The first call was cut off, so this is not a tool turn.
    assert_eq!(response.finish, FinishReason::Length);
}

#[test]
fn ids_decide_when_both_are_known() {
    let with_id = |id: &str, name: &str, arguments: &str| call_chunk(0, id, name, arguments);
    // Same id: one call, even though the header is repeated after the
    // arguments were complete.
    let response = tool_turn(&[
        with_id("call_1", "f", "{\"x\":1}"),
        with_id("call_1", "f", ""),
    ]);
    assert_eq!(calls_of(&response), [("f", "{\"x\":1}")]);
    // Different id: two calls, even with the same name.
    let response = tool_turn(&[
        with_id("call_1", "f", "{\"x\":1}"),
        with_id("call_2", "f", "{\"x\":2}"),
    ]);
    assert_eq!(
        calls_of(&response),
        [("f", "{\"x\":1}"), ("f", "{\"x\":2}")]
    );
    let ids: Vec<&str> = response.tool_calls().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["call_1", "call_2"]);
}

// ---------------------------------------------------------------------------
// CHAT-7: reasoning blocks and their signatures
// ---------------------------------------------------------------------------

fn reasoning(id: Option<&str>, text: &str, blob: Option<&str>) -> Part {
    Part::Reasoning(Reasoning {
        id: id.map(str::to_string),
        text: text.into(),
        signature: blob.map(|b| Signature::new(Protocol::OpenaiChat, b)),
        redacted: false,
    })
}

fn stream_round_trip(parts: Vec<Part>) -> Vec<Part> {
    let mut response = Response::new("chatcmpl-1", "m");
    response.created = 1;
    response.parts = parts;
    let wire = encode_stream(&response_to_events(&response), &ClientCtx::new("m"));
    accumulate(&decode_events(&wire)).parts
}

#[test]
fn consecutive_reasoning_blocks_survive_the_stream_round_trip() {
    for parts in [
        // Unsigned then signed: the signature must stay on the second block.
        vec![
            reasoning(None, "first", None),
            reasoning(None, "second", Some("SIG_2")),
            Part::text("done"),
        ],
        // Signed then unsigned.
        vec![
            reasoning(None, "first", Some("SIG_1")),
            reasoning(None, "second", None),
            Part::text("done"),
        ],
        // Two unsigned blocks are still two blocks.
        vec![
            reasoning(None, "first", None),
            reasoning(None, "second", None),
        ],
        // Provider item ids travel with their block.
        vec![
            reasoning(Some("rs_1"), "first", Some("SIG_1")),
            reasoning(Some("rs_2"), "", Some("SIG_2")),
            reasoning(Some("rs_3"), "third", None),
            Part::tool_call("call_1", "f", "{}"),
        ],
    ] {
        assert_eq!(stream_round_trip(parts.clone()), parts);
    }
}

#[test]
fn mirrored_reasoning_text_is_not_counted_twice() {
    // OpenRouter repeats the text of `reasoning` in `reasoning_details`.
    let transcript = [
        chunk(
            json!({"role": "assistant", "reasoning": "Let me ",
                   "reasoning_details": [{"type": "reasoning.text", "text": "Let me ", "index": 0}]}),
            Value::Null,
        ),
        chunk(
            json!({"reasoning": "think.",
                   "reasoning_details": [{"type": "reasoning.text", "text": "think.", "index": 0}]}),
            Value::Null,
        ),
        chunk(
            json!({"reasoning_details": [{"type": "reasoning.text", "signature": "SIG", "index": 0}]}),
            Value::Null,
        ),
        chunk(json!({"content": "42"}), json!("stop")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![
            reasoning(None, "Let me think.", Some("SIG")),
            Part::text("42")
        ]
    );
}

#[test]
fn a_second_signature_never_replaces_the_first() {
    // No indices at all: the open block is signed, so the next signature is
    // a block of its own.
    let transcript = [
        chunk(json!({"reasoning_content": "plan"}), Value::Null),
        chunk(
            json!({"reasoning_details": [
                {"type": "reasoning.text", "signature": "SIG_A"},
                {"type": "reasoning.text", "signature": "SIG_B"}
            ]}),
            Value::Null,
        ),
        chunk(json!({}), json!("stop")),
        DONE.to_string(),
    ]
    .concat();
    assert_eq!(
        accumulate(&decode_stream(&transcript)).parts,
        vec![
            reasoning(None, "plan", Some("SIG_A")),
            reasoning(None, "", Some("SIG_B"))
        ]
    );
}

#[test]
fn signatures_buffered_behind_an_unfinished_call_keep_their_blocks() {
    let transcript = [
        call_chunk(0, "call_1", "f", "{\"a\":"),
        chunk(
            json!({"reasoning_content": "one",
                   "reasoning_details": [{"type": "reasoning.text", "text": "one", "index": 0}]}),
            Value::Null,
        ),
        chunk(
            json!({"reasoning_details": [
                {"type": "reasoning.text", "signature": "SIG_0", "index": 0},
                {"type": "reasoning.text", "signature": "SIG_1", "index": 1}
            ]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
            json!("tool_calls"),
        ),
        DONE.to_string(),
    ]
    .concat();
    let response = accumulate(&decode_stream(&transcript));
    assert_eq!(
        response.parts,
        vec![
            Part::tool_call("call_1", "f", "{\"a\":1}"),
            reasoning(None, "one", Some("SIG_0")),
            reasoning(None, "", Some("SIG_1")),
        ]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
}

#[test]
fn a_client_that_replays_streamed_reasoning_details_returns_text_and_signature() {
    // What an OpenRouter-style client does: concatenate the streamed
    // `reasoning_details` entries and send them back on the next turn.
    let signature = Signature::new(Protocol::Anthropic, "ErUBCkYIBxgC");
    let mut response = Response::new("msg_1", "claude");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "Check the forecast first.".into(),
            signature: Some(signature.clone()),
            redacted: false,
        }),
        Part::tool_call("call_1", "get_weather", "{}"),
    ];
    response.finish = FinishReason::ToolCalls;
    let wire = payloads(&encode_stream(
        &response_to_events(&response),
        &ClientCtx::new("alias"),
    ));
    let replayed: Vec<Value> = wire
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["reasoning_details"].as_array())
        .flatten()
        .cloned()
        .collect();
    assert_eq!(
        replayed,
        vec![
            json!({"type": "reasoning.text", "text": "Check the forecast first.", "index": 0}),
            json!({"type": "reasoning.text",
                   "signature": sig::encode_for_client(&signature, Protocol::OpenaiChat),
                   "index": 0}),
        ]
    );
    let request = decode_request(json!({
        "model": "alias",
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "reasoning_details": replayed,
             "tool_calls": [{"id": "call_1", "type": "function",
                             "function": {"name": "get_weather", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
        ]
    }));
    assert_eq!(
        request.messages[1].parts[0],
        Part::Reasoning(Reasoning {
            id: None,
            text: "Check the forecast first.".into(),
            // Unwrapped again: it goes back to the vendor that issued it.
            signature: Some(signature),
            redacted: false,
        })
    );
}

// ---------------------------------------------------------------------------
// CHAT-8: error frames
// ---------------------------------------------------------------------------

#[test]
fn an_error_after_the_stream_started_adds_no_chunk_of_its_own() {
    let events = [
        StreamEvent::Start {
            id: "chatcmpl-1".into(),
            model: "m".into(),
            created: 1,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "par".into(),
        },
        StreamEvent::Error(ApiError::upstream("connection reset")),
    ];
    let wire = payloads(&encode_stream(&events, &ClientCtx::new("m")));
    assert_eq!(wire.len(), 3, "{wire:#?}");
    assert_eq!(wire[2]["error"]["message"], json!("connection reset"));
    // Nothing after the error frame: no finish chunk, no `[DONE]`.
}

#[test]
fn nothing_follows_an_error_frame_on_a_fresh_encoder() {
    let mut encoder = ChatCodec.stream_encoder(&ctx_with_usage("m"));
    let first = encoder.encode(&StreamEvent::Error(ApiError::rate_limit("slow down")));
    assert_eq!(first.len(), 1);
    // Events after a terminal event are ignored, and so is `finish`.
    assert!(
        encoder
            .encode(&StreamEvent::Finish {
                reason: FinishReason::Stop,
                stop_sequence: None,
            })
            .is_empty()
    );
    assert!(encoder.finish().is_empty());
}

// ---------------------------------------------------------------------------
// CHAT-9: usage conventions
// ---------------------------------------------------------------------------

fn decoded_usage(usage: Value) -> Usage {
    ChatCodec
        .decode_response(&json!({
            "id": "c1", "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": usage
        }))
        .unwrap()
        .usage
}

#[test]
fn reasoning_outside_the_completion_count_with_cached_prompt_tokens() {
    assert_eq!(
        decoded_usage(json!({
            "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 60},
            "completion_tokens_details": {"reasoning_tokens": 30}
        })),
        Usage {
            input_tokens: 40,
            cache_read_tokens: 60,
            cache_write_tokens: 0,
            output_tokens: 50,
            reasoning_tokens: 30,
        }
    );
}

#[test]
fn reasoning_larger_than_the_completion_count_cannot_be_inside_it() {
    // No total at all, or a total with room for it: counted on top.
    for total in [Value::Null, json!(0), json!(140)] {
        assert_eq!(
            decoded_usage(json!({
                "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": total,
                "completion_tokens_details": {"reasoning_tokens": 100}
            })),
            Usage {
                input_tokens: 10,
                output_tokens: 105,
                reasoning_tokens: 100,
                ..Usage::default()
            },
            "{total}"
        );
    }
    // A total that says "prompt + completion" contradicts it: the body is
    // inconsistent and the reasoning figure is clamped as before.
    assert_eq!(
        decoded_usage(json!({
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "completion_tokens_details": {"reasoning_tokens": 100}
        })),
        Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 5,
            ..Usage::default()
        }
    );
}

#[test]
fn usage_this_codec_writes_decodes_back_to_itself() {
    for usage in [
        Usage {
            input_tokens: 40,
            cache_read_tokens: 60,
            cache_write_tokens: 5,
            output_tokens: 50,
            reasoning_tokens: 30,
        },
        // All reasoning, no visible output.
        Usage {
            input_tokens: 10,
            output_tokens: 100,
            reasoning_tokens: 100,
            ..Usage::default()
        },
        Usage {
            input_tokens: 10,
            output_tokens: 7,
            ..Usage::default()
        },
    ] {
        let mut response = Response::new("chatcmpl-1", "m");
        response.parts = vec![Part::text("hi")];
        response.usage = usage;
        let body = ChatCodec
            .encode_response(&response, &ClientCtx::new("m"))
            .unwrap();
        assert_eq!(ChatCodec.decode_response(&body).unwrap().usage, usage);
    }
}

// ---------------------------------------------------------------------------
// CHAT-10 / CHAT-11: media
// ---------------------------------------------------------------------------

fn user_content(request: &Request) -> Value {
    encoded_messages(request)[0]["content"].clone()
}

fn user_request(source: Protocol, parts: Vec<Part>) -> Request {
    let mut request = Request::new("m", source);
    request.messages.push(Message::new(Role::User, parts));
    request
}

#[test]
fn image_detail_values_chat_knows_pass_and_others_do_not() {
    let detail = |value: &str| {
        let mut media = MediaPart::url("https://example.com/cat.png");
        media.detail = Some(value.into());
        user_content(&user_request(
            Protocol::OpenaiChat,
            vec![Part::Image(media)],
        ))[0]["image_url"]
            .get("detail")
            .cloned()
    };
    assert_eq!(detail("auto"), Some(json!("auto")));
    assert_eq!(detail("low"), Some(json!("low")));
    assert_eq!(detail("HIGH"), Some(json!("high")));
    assert_eq!(detail("original"), Some(json!("high")));
    assert_eq!(detail("ultra"), None);
    assert_eq!(detail(""), None);
}

#[test]
fn video_parts_decode_from_every_spelling() {
    let decode = |part: Value| {
        decode_request(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [part]}]
        }))
        .messages
        .remove(0)
        .parts
    };
    let remote = |url: &str, media_type: &str| {
        vec![Part::Document(MediaPart {
            media_type: Some(media_type.into()),
            ..MediaPart::url(url)
        })]
    };
    // Remote URL: the type comes from the extension ...
    assert_eq!(
        decode(
            json!({"type": "video_url", "video_url": {"url": "https://example.com/a.webm?sig=1"}})
        ),
        remote("https://example.com/a.webm?sig=1", "video/webm")
    );
    // ... and defaults to mp4 when the URL does not say.
    assert_eq!(
        decode(json!({"type": "video_url", "video_url": "https://example.com/watch/42"})),
        remote("https://example.com/watch/42", "video/mp4")
    );
    // Responses-style part type sent to the Chat endpoint.
    assert_eq!(
        decode(json!({"type": "input_video", "video_url": "data:video/quicktime;base64,AAAA"})),
        vec![Part::Document(MediaPart::base64("video/quicktime", "AAAA"))]
    );
    // Nothing usable: no part, and no panic.
    for broken in [
        json!({"type": "video_url"}),
        json!({"type": "video_url", "video_url": {"url": ""}}),
        json!({"type": "video_url", "video_url": 7}),
        json!({"type": "video_url", "video_url": ""}),
    ] {
        assert_eq!(decode(broken), vec![]);
    }
}

#[test]
fn video_survives_the_request_round_trip_inline_and_remote() {
    for part in [
        json!({"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAAIGZ0eXA="}}),
        json!({"type": "video_url", "video_url": {"url": "https://example.com/clip.mp4"}}),
    ] {
        let request = decode_request(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                part.clone()
            ]}]
        }));
        assert_eq!(
            user_content(&request),
            json!([{"type": "text", "text": "describe"}, part])
        );
    }
}

#[test]
fn video_filed_under_any_media_variant_is_sent_as_video_url() {
    let clip = MediaPart::base64("video/webm", "GkXf");
    let expected =
        json!([{"type": "video_url", "video_url": {"url": "data:video/webm;base64,GkXf"}}]);
    for part in [
        Part::Document(clip.clone()),
        Part::Image(clip.clone()),
        Part::Audio(clip.clone()),
    ] {
        assert_eq!(
            user_content(&user_request(Protocol::Gemini, vec![part])),
            expected
        );
    }
    // A PDF is still a file.
    assert_eq!(
        user_content(&user_request(
            Protocol::Gemini,
            vec![Part::Document(MediaPart::base64(
                "application/pdf",
                "JVBERi0="
            ))]
        )),
        json!([{"type": "file", "file": {
            "filename": "document.pdf",
            "file_data": "data:application/pdf;base64,JVBERi0="
        }}])
    );
}

#[test]
fn a_video_returned_by_a_tool_is_relayed_as_video_url() {
    let mut request = Request::new("m", Protocol::Gemini);
    request.messages = vec![
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "record", "{}")],
        ),
        Message::new(
            Role::User,
            vec![result(
                "call_1",
                vec![Part::Document(MediaPart::base64("video/mp4", "AAAA"))],
            )],
        ),
    ];
    let messages = encoded_messages(&request);
    assert_eq!(messages[1]["role"], json!("tool"));
    assert_eq!(
        messages[2]["content"].as_array().unwrap().last().unwrap(),
        &json!({"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAA"}})
    );
}

#[test]
fn audio_url_parts_are_audio_in_both_directions() {
    let request = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "audio_url", "audio_url": {"url": "https://example.com/speech.mp3"}},
            {"type": "audio_url", "audio_url": {"url": "data:audio/wav;base64,UklGRg=="}}
        ]}]
    }));
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::Audio(MediaPart {
                media_type: Some("audio/mpeg".into()),
                ..MediaPart::url("https://example.com/speech.mp3")
            }),
            Part::Audio(MediaPart::base64("audio/wav", "UklGRg==")),
        ]
    );
    assert!(matches!(
        request.messages[0].parts[0],
        Part::Audio(MediaPart {
            source: MediaSource::Url { .. },
            ..
        })
    ));
    assert_eq!(
        user_content(&request),
        json!([
            // Remote audio cannot be an `input_audio` part (base64 only).
            {"type": "audio_url", "audio_url": {"url": "https://example.com/speech.mp3"}},
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}
        ])
    );
}

// ---------------------------------------------------------------------------
// Request fields a Chat upstream rejects
// ---------------------------------------------------------------------------

#[test]
fn tool_call_with_cut_off_arguments_in_history_is_replayed_verbatim() {
    // History is the client's business; only *responses* are judged.
    let mut request = Request::new("m", Protocol::OpenaiChat);
    request.messages = vec![Message::new(
        Role::Assistant,
        vec![Part::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "f".into(),
            arguments: "{\"a\":".into(),
            kind: ToolCallKind::Function,
            signature: None,
            cache_control: None,
        })],
    )];
    assert_eq!(
        encoded_messages(&request)[0]["tool_calls"][0]["function"]["arguments"],
        json!("{\"a\":")
    );
}
