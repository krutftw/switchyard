//! The round-trip properties of DESIGN.md section 3:
//!
//! * `decode_request(encode_request(r))` preserves messages, tools, tool
//!   choice, sampling and reasoning depth;
//! * `decode_response(encode_response(x))` preserves parts, finish reason
//!   and usage;
//! * `stream_encoder` output fed into `stream_decoder` accumulates to the
//!   same response.

mod common;

use common::{
    accumulate, decode_events, decode_request, decode_response, encode_request, encode_response,
    encode_stream,
};
use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, FinishReason, FunctionTool, MediaPart, Message, OpaquePart,
    Part, Reasoning, Request, Response, ResponseFormat, Role, Signature, TextPart, Tool, ToolCall,
    ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events, validate_sequence};
use switchyard_core::{Protocol, Usage};

fn anthropic_signature(data: &str) -> Option<Signature> {
    Some(Signature::new(Protocol::Anthropic, data))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn rich_request() -> Request {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::Anthropic);
    request.stream = true;
    request.max_output_tokens = Some(16_000);
    request.system = vec![
        Part::Text(TextPart {
            text: "You are a weather assistant.".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
            citations: vec![],
            signature: None,
        }),
        Part::text("Be brief."),
    ];
    request.messages = vec![
        Message::new(
            Role::User,
            vec![
                Part::text("What is the weather in the city on this map?"),
                Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
                Part::Image(MediaPart::url("https://example.com/map.png")),
                Part::Document(MediaPart {
                    filename: Some("notes".into()),
                    ..MediaPart::base64("application/pdf", "JVBERi0xLjQ=")
                }),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: None,
                    text: "The map shows Paris.".into(),
                    signature: anthropic_signature("EqQBCgIYAhIM"),
                    redacted: false,
                }),
                Part::Reasoning(Reasoning {
                    id: None,
                    text: String::new(),
                    signature: anthropic_signature("EmwKAhgBEgy3"),
                    redacted: true,
                }),
                Part::text("Let me look that up."),
                Part::tool_call(
                    "toolu_01A",
                    "get_weather",
                    r#"{"city":"Paris","units":"metric"}"#,
                ),
                Part::tool_call("toolu_01B", "get_time", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("toolu_01A", "18°C, cloudy"),
                Part::ToolResult(ToolResult {
                    call_id: "toolu_01B".into(),
                    name: None,
                    content: vec![
                        Part::text("see screenshot"),
                        Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
                    ],
                    is_error: true,
                    cache_control: Some(json!({"type": "ephemeral"})),
                }),
                Part::text("And tomorrow?"),
            ],
        ),
        Message::assistant_text("Tomorrow looks sunny."),
        Message::user_text("Thanks!"),
    ];
    request.tools = vec![
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Get the current weather".into()),
            parameters: json!({"type": "object",
                               "properties": {"city": {"type": "string"},
                                              "units": {"type": "string", "enum": ["metric", "imperial"]}},
                               "required": ["city"]}),
            strict: Some(true),
            cache_control: Some(json!({"type": "ephemeral"})),
        }),
        Tool::Function(FunctionTool {
            name: "get_time".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        }),
    ];
    request.tool_choice = Some(ToolChoice::Auto);
    request.parallel_tool_calls = Some(false);
    request.temperature = Some(0.5);
    request.top_k = Some(40);
    request.stop = vec!["END".into()];
    request.user = Some("user-42".into());
    request.service_tier = Some("standard_only".into());
    request
}

#[test]
fn round_trip_request_preserves_everything_expressible() {
    let request = rich_request();
    let wire = encode_request(&request);
    let back = decode_request(&wire);
    assert_eq!(back, request);
}

#[test]
fn round_trip_request_is_stable_under_repeated_translation() {
    let request = rich_request();
    let once = encode_request(&request);
    let twice = encode_request(&decode_request(&once));
    assert_eq!(once, twice);
}

#[test]
fn round_trip_request_reasoning_depths() {
    for depth in [
        Depth::Off,
        Depth::Auto,
        Depth::Level(Effort::Low),
        Depth::Level(Effort::Medium),
        Depth::Level(Effort::High),
        Depth::Level(Effort::Xhigh),
        Depth::Level(Effort::Max),
        Depth::Budget(1024),
        Depth::Budget(10_000),
    ] {
        let mut request = Request::new("claude-opus-4-5", Protocol::Anthropic);
        request.max_output_tokens = Some(32_000);
        request.messages = vec![Message::user_text("hi")];
        request.reasoning = Some(ReasoningConfig::with_depth(depth));
        let back = decode_request(&encode_request(&request));
        assert_eq!(back.reasoning, request.reasoning, "{depth:?}");
        assert_eq!(back.max_output_tokens, Some(32_000));
    }
}

#[test]
fn round_trip_request_reasoning_summary_intent() {
    for summary in [Summary::Auto, Summary::Off] {
        let mut request = Request::new("claude-opus-4-8", Protocol::Anthropic);
        request.max_output_tokens = Some(8000);
        request.messages = vec![Message::user_text("hi")];
        request.reasoning = Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::High)),
            summary: Some(summary),
        });
        let back = decode_request(&encode_request(&request));
        assert_eq!(back.reasoning, request.reasoning);
    }
}

#[test]
fn round_trip_request_tool_choices() {
    for (choice, parallel) in [
        (ToolChoice::Auto, None),
        (ToolChoice::Required, None),
        (ToolChoice::None, None),
        (
            ToolChoice::Tool {
                name: "get_weather".into(),
            },
            None,
        ),
        (ToolChoice::Required, Some(false)),
        (
            ToolChoice::Tool {
                name: "get_weather".into(),
            },
            Some(false),
        ),
    ] {
        let mut request = rich_request();
        request.tool_choice = Some(choice.clone());
        request.parallel_tool_calls = parallel;
        let back = decode_request(&encode_request(&request));
        assert_eq!(back.tool_choice, Some(choice));
        assert_eq!(back.parallel_tool_calls, parallel);
        assert_eq!(back.tools, request.tools);
    }
}

#[test]
fn round_trip_request_structured_output_and_sampling() {
    let mut request = Request::new("claude-opus-4-5", Protocol::Anthropic);
    request.max_output_tokens = Some(1000);
    request.messages = vec![Message::user_text("hi")];
    request.top_p = Some(0.9);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema: json!({"type": "object", "properties": {"a": {"type": "integer"}},
                       "required": ["a"], "additionalProperties": false}),
        strict: None,
    });
    let back = decode_request(&encode_request(&request));
    assert_eq!(back, request);
}

#[test]
fn round_trip_native_request_body_survives_decode_and_encode() {
    // What a Messages client sent comes out equivalent when the gateway has
    // to re-encode it for an Anthropic upstream (wrapped-signature case).
    let body = json!({
        "model": "claude-opus-4-5",
        "max_tokens": 20000,
        "system": [{"type": "text", "text": "Be helpful.", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Search and compute."}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": "EqQBCgIYAhIM"},
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "x"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                    {"type": "web_search_result", "url": "https://x.example", "title": "X",
                     "encrypted_content": "enc", "page_age": null}]},
                {"type": "tool_use", "id": "toolu_1", "name": "calc", "input": {"expr": "1+1"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "2"}
            ]}
        ],
        "tools": [
            {"name": "calc", "description": "Calculator",
             "input_schema": {"type": "object", "properties": {"expr": {"type": "string"}}},
             "defer_loading": true, "input_examples": [{"expr": "2*3"}]},
            {"type": "web_search_20250305", "name": "web_search"}
        ],
        "tool_choice": {"type": "auto"},
        "thinking": {"type": "enabled", "budget_tokens": 8000, "display": "summarized"},
        "metadata": {"user_id": "u-1"},
        "stream": true,
        "speed": "fast"
    });
    let decoded = decode_request(&body);
    assert_eq!(encode_request(&decoded), body);
}

#[test]
fn round_trip_wrapped_foreign_signatures_never_reach_anthropic() {
    // A Messages client replays a turn it got from a Gemini upstream.
    let body = json!({
        "model": "claude-opus-4-5", "max_tokens": 1000,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "gemini thought", "signature": "sy1.g.CiQBgemini"},
                {"type": "redacted_thinking", "data": "sy1.r.gAAAAopenai"},
                {"type": "thinking", "thinking": "claude thought", "signature": "EqQBnative"},
                {"type": "text", "text": "hello"}
            ]},
            {"role": "user", "content": "more"}
        ]
    });
    let encoded = encode_request(&decode_request(&body));
    assert_eq!(
        encoded["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "claude thought", "signature": "EqQBnative"},
            {"type": "text", "text": "hello"}
        ])
    );
    assert!(!encoded.to_string().contains("sy1."));
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn rich_response() -> Response {
    Response {
        id: "msg_01XFDUDYJgAACzvnptvVoYEL".into(),
        model: "claude-opus-4-5".into(),
        created: 0,
        parts: vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Think first.".into(),
                signature: anthropic_signature("EqQBCgIYAhIM"),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: anthropic_signature("EmwKAhgBEgy3"),
                redacted: true,
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                            "input": {"query": "paris weather"}}),
            }),
            Part::Text(TextPart {
                text: "It is sunny in Paris.".into(),
                cache_control: None,
                citations: vec![Citation {
                    url: Some("https://weather.example".into()),
                    title: Some("Weather".into()),
                    cited_text: Some("sunny".into()),
                    start: None,
                    end: None,
                }],
                signature: None,
            }),
            Part::tool_call("toolu_01A", "get_weather", r#"{"city":"Paris"}"#),
            Part::tool_call("toolu_01B", "get_time", "{}"),
        ],
        finish: FinishReason::ToolCalls,
        stop_sequence: None,
        usage: Usage {
            input_tokens: 2095,
            cache_read_tokens: 1000,
            cache_write_tokens: 2051,
            output_tokens: 503,
            reasoning_tokens: 120,
        },
        service_tier: Some("standard".into()),
    }
}

#[test]
fn round_trip_response_preserves_parts_finish_and_usage() {
    let response = rich_response();
    let back = decode_response(&encode_response(&response, "claude-opus-4-5"));
    assert_eq!(back, response);
}

#[test]
fn round_trip_response_finish_reasons() {
    for (finish, stop_sequence) in [
        (FinishReason::Stop, None),
        (FinishReason::Stop, Some("END".to_string())),
        (FinishReason::Length, None),
        (FinishReason::PauseTurn, None),
        (FinishReason::Refusal, None),
        (FinishReason::ContextWindow, None),
    ] {
        let mut response = Response::new("msg_01", "m");
        response.parts = vec![Part::text("partial")];
        response.finish = finish.clone();
        response.stop_sequence = stop_sequence.clone();
        response.usage = Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..Usage::default()
        };
        let back = decode_response(&encode_response(&response, "m"));
        assert_eq!(back, response, "{finish:?}");
    }
}

#[test]
fn round_trip_response_unsigned_reasoning_stays_unsigned() {
    let mut response = Response::new("msg_01", "m");
    response.parts = vec![Part::reasoning("no signature"), Part::text("x")];
    let back = decode_response(&encode_response(&response, "m"));
    assert_eq!(back.parts, response.parts);
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// encode → decode → accumulate.
fn through_the_wire(events: &[StreamEvent], model: &str) -> Response {
    validate_sequence(events).expect("test input is a valid sequence");
    let wire = encode_stream(events, model);
    accumulate(&decode_events(&wire))
}

#[test]
fn round_trip_stream_of_a_replayed_response() {
    let response = rich_response();
    let back = through_the_wire(&response_to_events(&response), "claude-opus-4-5");
    // `service_tier` has no slot in the canonical stream.
    assert_eq!(
        back,
        Response {
            service_tier: None,
            ..response
        }
    );
}

#[test]
fn round_trip_stream_text_only_and_finish_reasons() {
    for (finish, stop_sequence) in [
        (FinishReason::Stop, None),
        (FinishReason::Stop, Some("END".to_string())),
        (FinishReason::Length, None),
        (FinishReason::PauseTurn, None),
        (FinishReason::Refusal, None),
        (FinishReason::ContextWindow, None),
    ] {
        let mut response = Response::new("msg_01", "m");
        response.parts = vec![Part::text("Hello\nworld — ünïcödé")];
        response.finish = finish.clone();
        response.stop_sequence = stop_sequence;
        response.usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        assert_eq!(
            through_the_wire(&response_to_events(&response), "m"),
            response,
            "{finish:?}"
        );
    }
}

#[test]
fn round_trip_stream_incremental_sequence() {
    let events = vec![
        StreamEvent::Start {
            id: "msg_inc".into(),
            model: "m".into(),
            created: 0,
        },
        StreamEvent::Usage(Usage {
            input_tokens: 40,
            cache_read_tokens: 100,
            output_tokens: 1,
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
            text: "step 1, ".into(),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "step 2".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "EqQBsig"),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "Calling ".into(),
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "tools.".into(),
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::ToolCall {
                id: "toolu_1".into(),
                name: "search".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 2,
            fragment: "{\"q\": \"ru".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 2,
            fragment: "st\", \"n\": 3}".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::ToolCall {
                id: "toolu_2".into(),
                name: "now".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::BlockStop { index: 3 },
        StreamEvent::Usage(Usage {
            output_tokens: 64,
            reasoning_tokens: 20,
            ..Usage::default()
        }),
        StreamEvent::Finish {
            reason: FinishReason::ToolCalls,
            stop_sequence: None,
        },
    ];
    let expected = accumulate(&events);
    assert_eq!(expected.usage.input_tokens, 40);
    assert_eq!(through_the_wire(&events, "m"), expected);
}

#[test]
fn round_trip_stream_error_terminated() {
    let mut response = Response::new("msg_err", "m");
    response.parts = vec![Part::text("Partial answer")];
    response.finish = FinishReason::Error;
    let back = through_the_wire(&response_to_events(&response), "m");
    assert_eq!(back, response);
}

#[test]
fn round_trip_stream_foreign_parts_are_dropped_and_the_rest_survives() {
    // A response from another vendor, streamed to a Messages client and read
    // back: what the protocol can express survives, in order.
    let mut response = Response::new("resp_abc", "gpt-5");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: Some("rs_1".into()),
            text: "summary".into(),
            signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAAA")),
            redacted: false,
        }),
        Part::Opaque(OpaquePart {
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "web_search_call", "id": "ws_1"}),
        }),
        Part::text("answer"),
        Part::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "f".into(),
            arguments: "{\"a\":1}".into(),
            kind: ToolCallKind::Function,
            signature: None,
            cache_control: None,
        }),
    ];
    response.finish = FinishReason::ToolCalls;
    let back = through_the_wire(&response_to_events(&response), "gpt-5");
    assert_eq!(back.id, "msg_abc");
    assert_eq!(
        back.parts,
        vec![
            // An upstream decoder tags what it reads as its own; the wrapped
            // form keeps the blob from being mistaken for a native one.
            Part::Reasoning(Reasoning {
                id: None,
                text: "summary".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "sy1.r.gAAAAA")),
                redacted: false,
            }),
            Part::text("answer"),
            Part::tool_call("call_1", "f", "{\"a\":1}"),
        ]
    );
    assert_eq!(back.finish, FinishReason::ToolCalls);
}
