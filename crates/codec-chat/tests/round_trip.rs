//! The round-trip properties of DESIGN.md section 3: request, response and
//! stream.

mod common;

use common::{
    accumulate, ctx_with_usage, decode_events, decode_request, decode_stream, encode_request,
    encode_stream,
};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec, RequestPath};
use switchyard_core::error::ApiError;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, CustomTool, FinishReason, FunctionTool, MediaPart, Message,
    Part, Reasoning, RefusalPart, Request, Response, ResponseFormat, Role, Signature, TextPart,
    Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig};
use switchyard_core::stream::{Accumulator, BlockStart, StreamEvent, response_to_events};
use switchyard_core::{Protocol, Usage};

const MODEL: &str = "gpt-4o";

fn chat_signature(data: &str) -> Option<Signature> {
    Some(Signature::new(Protocol::OpenaiChat, data))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn rich_request() -> Request {
    let mut r = Request::new(MODEL, Protocol::OpenaiChat);
    r.stream = true;
    r.system = vec![
        Part::text("You are terse."),
        Part::text("Answer in French."),
    ];
    r.messages = vec![
        Message {
            role: Role::User,
            name: Some("alice".into()),
            parts: vec![
                Part::text("What is in these?"),
                Part::Image(MediaPart::base64("image/png", "iVBOR")),
                Part::Image(MediaPart {
                    detail: Some("low".into()),
                    ..MediaPart::url("https://example.com/cat.jpg")
                }),
                Part::Audio(MediaPart::base64("audio/wav", "UklGRg==")),
                Part::Document(MediaPart {
                    filename: Some("report.pdf".into()),
                    ..MediaPart::base64("application/pdf", "JVBERi0x")
                }),
            ],
        },
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: None,
                    text: "I should look both up.".into(),
                    signature: chat_signature("sig-1"),
                    redacted: false,
                }),
                Part::text("Let me check."),
                Part::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"q\": \"cat\"}".into(),
                    kind: ToolCallKind::Function,
                    signature: chat_signature("thought-sig"),
                    cache_control: None,
                }),
                Part::ToolCall(ToolCall {
                    id: "call_2".into(),
                    name: "run_sql".into(),
                    arguments: "SELECT 1".into(),
                    kind: ToolCallKind::Custom,
                    signature: None,
                    cache_control: None,
                }),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call_1", "a cat"),
                Part::tool_result_text("call_2", "1"),
            ],
        ),
        Message::new(Role::System, vec![Part::text("Stay on topic.")]),
        Message::new(
            Role::Assistant,
            vec![
                Part::reasoning("unsigned thought"),
                Part::text("A cat and a one."),
            ],
        ),
        Message::user_text("Thanks"),
        Message::user_text("One more thing"),
        Message::new(
            Role::Assistant,
            vec![Part::Refusal(RefusalPart {
                text: "I can't do that.".into(),
            })],
        ),
    ];
    r.tools = vec![
        Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: Some("Looks things up".into()),
            parameters: json!({
                "type": "object",
                "properties": {"q": {"type": "string"}},
                "required": ["q"]
            }),
            strict: Some(true),
            cache_control: None,
        }),
        Tool::Custom(CustomTool {
            name: "run_sql".into(),
            description: None,
            format: Some(json!({"type": "text"})),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::OpenaiChat,
            raw: json!({"web_search_options": {"search_context_size": "medium"}}),
        }),
    ];
    r.tool_choice = Some(ToolChoice::Tool {
        name: "lookup".into(),
    });
    r.parallel_tool_calls = Some(false);
    r.max_output_tokens = Some(512);
    r.temperature = Some(0.25);
    r.top_p = Some(0.9);
    r.top_k = Some(40);
    r.seed = Some(-7);
    r.presence_penalty = Some(0.5);
    r.frequency_penalty = Some(-0.25);
    r.stop = vec!["END".into(), "\n\n".into()];
    r.candidate_count = Some(2);
    r.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)));
    r.response_format = Some(ResponseFormat::JsonSchema {
        name: Some("answer".into()),
        description: Some("The answer".into()),
        schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
        strict: Some(true),
    });
    r.user = Some("user-1".into());
    r.metadata = json!({"trace": "abc"}).as_object().cloned();
    r.service_tier = Some("flex".into());
    r.prompt_cache_key = Some("conv-1".into());
    r.store = Some(false);
    r.extra = json!({"logprobs": true, "top_logprobs": 2, "verbosity": "low"})
        .as_object()
        .cloned()
        .expect("an object");
    r
}

#[test]
fn request_round_trip_preserves_everything_chat_can_express() {
    let request = rich_request();
    let wire = encode_request(&request);
    let back = decode_request(wire.clone());
    assert_eq!(back, request);
    // And the encoding is a fixed point.
    assert_eq!(encode_request(&back), wire);
}

#[test]
fn request_round_trip_for_each_reasoning_depth() {
    for (depth, expected) in [
        (Depth::Off, Some(Depth::Off)),
        (
            Depth::Level(Effort::Minimal),
            Some(Depth::Level(Effort::Minimal)),
        ),
        (
            Depth::Level(Effort::Xhigh),
            Some(Depth::Level(Effort::Xhigh)),
        ),
        // Chat has effort levels only: budgets come back as their bucket ...
        (Depth::Budget(8192), Some(Depth::Level(Effort::Medium))),
        // ... and "provider decides" is the absence of the field.
        (Depth::Auto, None),
    ] {
        let mut request = Request::new(MODEL, Protocol::OpenaiChat);
        request.messages.push(Message::user_text("hi"));
        request.reasoning = Some(ReasoningConfig::with_depth(depth));
        let back = decode_request(encode_request(&request));
        assert_eq!(back.reasoning.and_then(|r| r.depth), expected, "{depth:?}");
    }
}

#[test]
fn request_round_trip_for_each_tool_choice() {
    for choice in [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Tool { name: "f".into() },
    ] {
        let mut request = Request::new(MODEL, Protocol::OpenaiChat);
        request.tools = vec![Tool::Function(FunctionTool {
            name: "f".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        })];
        request.tool_choice = Some(choice.clone());
        assert_eq!(
            decode_request(encode_request(&request)).tool_choice,
            Some(choice)
        );
    }
}

#[test]
fn request_from_another_protocol_round_trips_its_conversation() {
    // An Anthropic-shaped turn: results and follow-up text in one user
    // message, a tool result carrying an image.
    let mut request = Request::new("m", Protocol::Anthropic);
    request.system = vec![Part::text("sys")];
    request.messages = vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("toolu_1", "shot", "{}")],
        ),
        Message::new(
            Role::User,
            vec![
                Part::ToolResult(ToolResult {
                    call_id: "toolu_1".into(),
                    name: None,
                    content: vec![
                        Part::text("captured"),
                        Part::Image(MediaPart::base64("image/png", "AAAA")),
                    ],
                    is_error: false,
                    cache_control: None,
                }),
                Part::text("what is it?"),
            ],
        ),
    ];
    let back = ChatCodec
        .decode_request(&encode_request(&request), &RequestPath::default())
        .expect("decodes");
    assert_eq!(back.system, request.system);
    assert_eq!(back.messages[..2], request.messages[..2]);
    // Chat splits the turn: the tool message, then a user message that
    // carries the image the tool returned. Nothing is lost.
    assert_eq!(
        back.messages[2..],
        [
            Message::new(
                Role::User,
                vec![Part::tool_result_text("toolu_1", "captured")]
            ),
            Message::new(
                Role::User,
                vec![
                    Part::text("Content returned by the preceding tool call(s):"),
                    Part::Image(MediaPart::base64("image/png", "AAAA")),
                    Part::text("what is it?"),
                ]
            ),
        ]
    );
}

#[test]
fn wire_request_survives_decode_then_encode() {
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": [
                {"type": "text", "text": "Describe"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png", "detail": "high"}}
            ]},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "f", "arguments": "{\"a\":1}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "thanks"}
        ],
        "max_completion_tokens": 100,
        "temperature": 0.5,
        "response_format": {"type": "json_object"},
        "tools": [{"type": "function", "function": {
            "name": "f", "description": "d",
            "parameters": {"type": "object", "properties": {"a": {"type": "integer"}}}
        }}],
        "tool_choice": "auto",
        "reasoning_effort": "low",
        "stream": false
    });
    assert_eq!(encode_request(&decode_request(body.clone())), body);
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn round_trip_response(response: &Response) -> Response {
    let wire = ChatCodec
        .encode_response(response, &ClientCtx::new(response.model.clone()))
        .expect("encodes");
    ChatCodec.decode_response(&wire).expect("decodes")
}

fn base_response(parts: Vec<Part>, finish: FinishReason) -> Response {
    let mut r = Response::new("chatcmpl-rt1", MODEL);
    r.created = 1_741_570_002;
    r.parts = parts;
    r.finish = finish;
    r.usage = Usage {
        input_tokens: 50,
        cache_read_tokens: 800,
        cache_write_tokens: 150,
        output_tokens: 300,
        reasoning_tokens: 128,
    };
    r.service_tier = Some("default".into());
    r
}

fn rich_response() -> Response {
    base_response(
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Thinking it through.".into(),
                signature: chat_signature("sig-1"),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: Some("rs_9".into()),
                text: String::new(),
                signature: chat_signature("encrypted"),
                redacted: true,
            }),
            Part::Text(TextPart {
                citations: vec![Citation {
                    url: Some("https://example.com".into()),
                    title: Some("Example".into()),
                    cited_text: None,
                    start: Some(0),
                    end: Some(7),
                }],
                ..TextPart::new("Calling two tools.")
            }),
            Part::tool_call("call_1", "lookup", "{\"q\": \"cat\"}"),
            Part::ToolCall(ToolCall {
                id: "call_2".into(),
                name: "lookup".into(),
                arguments: "{\"q\":\"dog\"}".into(),
                kind: ToolCallKind::Function,
                signature: chat_signature("thought-sig"),
                cache_control: None,
            }),
        ],
        FinishReason::ToolCalls,
    )
}

#[test]
fn response_round_trip_preserves_parts_finish_and_usage() {
    let response = rich_response();
    assert_eq!(round_trip_response(&response), response);
}

#[test]
fn response_round_trip_for_each_expressible_shape() {
    let shapes = [
        base_response(vec![Part::text("plain")], FinishReason::Stop),
        base_response(vec![Part::text("cut off")], FinishReason::Length),
        base_response(vec![Part::text("filtered")], FinishReason::ContentFilter),
        base_response(
            vec![Part::Refusal(RefusalPart {
                text: "I can't.".into(),
            })],
            FinishReason::Stop,
        ),
        base_response(
            vec![Part::reasoning("unsigned"), Part::text("answer")],
            FinishReason::Stop,
        ),
        base_response(
            vec![
                Part::text("Here you go."),
                Part::Image(MediaPart::base64("image/png", "AAAA")),
            ],
            FinishReason::Stop,
        ),
        base_response(
            vec![Part::ToolCall(ToolCall {
                id: "call_c".into(),
                name: "run_sql".into(),
                arguments: "SELECT 1".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            })],
            FinishReason::ToolCalls,
        ),
        base_response(vec![], FinishReason::Stop),
    ];
    for response in shapes {
        assert_eq!(round_trip_response(&response), response);
    }
}

#[test]
fn foreign_signatures_survive_the_trip_through_a_chat_client() {
    // Upstream was Anthropic / Gemini; the client speaks Chat.
    let anthropic = Signature::new(Protocol::Anthropic, "ErUBCkYIBxgC");
    let gemini = Signature::new(Protocol::Gemini, "CiQBsig");
    let response = base_response(
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Claude thinks.".into(),
                signature: Some(anthropic.clone()),
                redacted: false,
            }),
            Part::text("Calling."),
            Part::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "f".into(),
                arguments: "{}".into(),
                kind: ToolCallKind::Function,
                signature: Some(gemini.clone()),
                cache_control: None,
            }),
        ],
        FinishReason::ToolCalls,
    );
    let wire = ChatCodec
        .encode_response(&response, &ClientCtx::new(MODEL))
        .expect("encodes");
    // The client appends the assistant message to its history verbatim.
    let next_request = json!({
        "model": MODEL,
        "messages": [
            {"role": "user", "content": "go"},
            wire["choices"][0]["message"].clone(),
            {"role": "tool", "tool_call_id": "toolu_1", "content": "ok"}
        ]
    });
    assert!(switchyard_core::sig::contains_wrapped(
        next_request.to_string().as_bytes()
    ));
    let request = decode_request(next_request);
    assert_eq!(request.messages[1].parts, response.parts);

    // On the way to a Chat upstream the foreign blobs are gone.
    let upstream = encode_request(&request).to_string();
    assert!(!upstream.contains("ErUBCkYIBxgC"));
    assert!(!upstream.contains("CiQBsig"));
    assert!(!upstream.contains("sy1."));
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// encode -> wire -> decode -> accumulate.
fn round_trip_stream(events: &[StreamEvent]) -> Response {
    let wire = encode_stream(events, &ctx_with_usage(MODEL));
    accumulate(&decode_events(&wire))
}

fn direct(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    acc.into_response()
}

#[test]
fn stream_round_trip_of_whole_responses() {
    let mut no_service_tier = rich_response();
    // Chunks have no slot for the tier in the canonical stream.
    no_service_tier.service_tier = None;
    // Citations and redacted blocks included.
    let mut shapes = vec![no_service_tier];
    for parts in [
        vec![Part::text("plain text")],
        vec![Part::reasoning("unsigned"), Part::text("answer")],
        vec![Part::Refusal(RefusalPart {
            text: "I can't.".into(),
        })],
        vec![
            Part::text("before"),
            Part::tool_call("call_1", "a", "{\"x\":1}"),
            Part::tool_call("call_2", "b", "{}"),
            Part::tool_call("call_3", "c", "{\"z\":[1,2]}"),
        ],
        vec![
            Part::tool_call("call_1", "a", "{}"),
            Part::text("after the call"),
        ],
        vec![
            Part::text("Here."),
            Part::Image(MediaPart::base64("image/png", "AAAA")),
        ],
        vec![Part::ToolCall(ToolCall {
            id: "call_c".into(),
            name: "run_sql".into(),
            arguments: "SELECT 1".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })],
    ] {
        let finish = if parts.iter().any(|p| matches!(p, Part::ToolCall(_))) {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        let mut response = base_response(parts, finish);
        response.service_tier = None;
        shapes.push(response);
    }
    for response in shapes {
        let events = response_to_events(&response);
        assert_eq!(round_trip_stream(&events), response);
    }
}

#[test]
fn argument_less_tool_calls_come_back_as_the_empty_object() {
    // The IR spells "no arguments" as the empty string; Chat clients parse
    // the arguments as JSON, so both encoders send `{}`.
    let mut response = base_response(
        vec![
            Part::tool_call("call_1", "a", ""),
            Part::tool_call("call_2", "b", ""),
        ],
        FinishReason::ToolCalls,
    );
    response.service_tier = None;
    let expected = vec![
        Part::tool_call("call_1", "a", "{}"),
        Part::tool_call("call_2", "b", "{}"),
    ];
    assert_eq!(round_trip_response(&response).parts, expected);
    assert_eq!(
        round_trip_stream(&response_to_events(&response)).parts,
        expected
    );
}

#[test]
fn stream_round_trip_of_incremental_sequences() {
    let events = vec![
        StreamEvent::Start {
            id: "chatcmpl-inc".into(),
            model: MODEL.into(),
            created: 1_741_570_002,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "Let me ".into(),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "think.".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::OpenaiChat, "s1"),
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
            text: "Second thought.".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 1,
            signature: Signature::new(Protocol::OpenaiChat, "s2"),
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::Usage(Usage {
            input_tokens: 40,
            cache_read_tokens: 10,
            ..Usage::default()
        }),
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "Par".into(),
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "tial ".into(),
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "answer.".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "search".into(),
                kind: ToolCallKind::Function,
                signature: Some(Signature::new(Protocol::OpenaiChat, "thought")),
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 3,
            fragment: "{\"q\"".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 3,
            fragment: ":\"rust\"}".into(),
        },
        StreamEvent::BlockStop { index: 3 },
        StreamEvent::BlockStart {
            index: 4,
            block: BlockStart::ToolCall {
                id: "call_2".into(),
                name: "search".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 4,
            fragment: "{\"q\":\"go\"}".into(),
        },
        StreamEvent::BlockStop { index: 4 },
        StreamEvent::Usage(Usage {
            output_tokens: 25,
            reasoning_tokens: 9,
            ..Usage::default()
        }),
        StreamEvent::Finish {
            reason: FinishReason::ToolCalls,
            stop_sequence: None,
        },
    ];
    assert_eq!(round_trip_stream(&events), direct(&events));
}

#[test]
fn stream_round_trip_of_each_finish_reason() {
    for (reason, expected) in [
        (FinishReason::Stop, FinishReason::Stop),
        (FinishReason::Length, FinishReason::Length),
        (FinishReason::ContentFilter, FinishReason::ContentFilter),
        // Reasons Chat cannot spell come back as their nearest value.
        (FinishReason::Refusal, FinishReason::ContentFilter),
        (FinishReason::PauseTurn, FinishReason::Stop),
        (FinishReason::ContextWindow, FinishReason::Length),
        (FinishReason::Error, FinishReason::Length),
    ] {
        let mut response = base_response(vec![Part::text("x")], reason);
        response.service_tier = None;
        let back = round_trip_stream(&response_to_events(&response));
        assert_eq!(back.finish, expected);
        assert_eq!(back.parts, response.parts);
        assert_eq!(back.usage, response.usage);
    }
}

#[test]
fn stream_round_trip_of_an_error_terminated_sequence() {
    let events = vec![
        StreamEvent::Start {
            id: "chatcmpl-err".into(),
            model: MODEL.into(),
            created: 1,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Partial".into(),
        },
        StreamEvent::Error(ApiError::rate_limit("slow down").with_code("rate_limit_exceeded")),
    ];
    let wire = encode_stream(&events, &ctx_with_usage(MODEL));
    assert!(!wire.iter().any(|e| e.is_done_marker()));
    let decoded = decode_events(&wire);
    let mut acc = Accumulator::new();
    for event in &decoded {
        acc.push(event);
    }
    let error = acc.error().expect("the error survives").clone();
    assert_eq!(error.status, 429);
    assert_eq!(error.message, "slow down");
    assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
    let response = acc.into_response();
    assert_eq!(response.text(), "Partial");
    assert_eq!(response.finish, FinishReason::Error);
}

#[test]
fn stream_round_trip_of_a_truncated_sequence() {
    let events = vec![
        StreamEvent::Start {
            id: "chatcmpl-cut".into(),
            model: MODEL.into(),
            created: 1,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Cut sho".into(),
        },
    ];
    let response = round_trip_stream(&events);
    assert_eq!(response.text(), "Cut sho");
    // The client is told the answer is incomplete.
    assert_eq!(response.finish, FinishReason::Length);
}

#[test]
fn vendor_transcript_survives_decode_encode_decode() {
    let transcript = [
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(
            json!({"reasoning_content": "Plan: call twice."}),
            Value::Null,
        ),
        chunk(json!({"content": "On it."}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                                   "function": {"name": "alpha", "arguments": ""}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"x\":"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function",
                                   "function": {"name": "beta", "arguments": "{\"y\":2}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-V", "object": "chat.completion.chunk", "created": 1741570002,
                   "model": MODEL, "choices": [],
                   "usage": {"prompt_tokens": 30, "completion_tokens": 12, "total_tokens": 42,
                             "prompt_tokens_details": {"cached_tokens": 10},
                             "completion_tokens_details": {"reasoning_tokens": 4}}})
        ),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let first = decode_stream(&transcript);
    let once = accumulate(&first);
    assert_eq!(
        once.parts,
        vec![
            Part::reasoning("Plan: call twice."),
            Part::text("On it."),
            Part::tool_call("call_a", "alpha", "{\"x\":1}"),
            Part::tool_call("call_b", "beta", "{\"y\":2}"),
        ]
    );
    assert_eq!(round_trip_stream(&first), once);
}

fn chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-V",
        "object": "chat.completion.chunk",
        "created": 1741570002,
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]
    });
    format!("data: {chunk}\n\n")
}
