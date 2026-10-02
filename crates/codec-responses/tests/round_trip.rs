//! The round-trip properties of DESIGN.md §3: request, response and stream.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, CustomTool, FinishReason, FunctionTool, MediaPart, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, Response, ResponseFormat, Role, Signature,
    TextPart, Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult, normalize_turns,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::stream::{
    Accumulator, BlockStart, StreamEvent, response_to_events, validate_sequence,
};
use switchyard_core::{ApiError, ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx, Usage};

const P: Protocol = Protocol::OpenaiResponses;

fn request_round_trip(request: &Request) -> Request {
    let body = ResponsesCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("encodes");
    ResponsesCodec
        .decode_request(&body, &RequestPath::default())
        .expect("decodes")
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

fn signed_reasoning(id: &str, text: &str, blob: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: Some(id.into()),
        text: text.into(),
        signature: Some(Signature::new(P, blob)),
        redacted: false,
    })
}

fn cited_text(text: &str) -> Part {
    Part::Text(TextPart {
        text: text.into(),
        citations: vec![Citation {
            url: Some("https://example.com/a".into()),
            title: Some("A".into()),
            cited_text: None,
            start: Some(0),
            end: Some(4),
        }],
        ..TextPart::default()
    })
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A request using everything the protocol can express.
fn rich_request() -> Request {
    let mut pdf = MediaPart::base64("application/pdf", "JVBERi0x");
    pdf.filename = Some("spec.pdf".into());
    let mut image = MediaPart::url("https://example.com/chart.png");
    image.detail = Some("high".into());

    let mut request = Request::new("gpt-5", P);
    request.stream = true;
    request.system = vec![Part::text("You are a careful analyst.")];
    request.messages = vec![
        Message::new(
            Role::User,
            vec![
                Part::text("Summarise these."),
                Part::Image(image),
                Part::Document(pdf),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![
                signed_reasoning("rs_1", "Need the numbers first.", "gAAAAABblob1"),
                Part::text("Let me look things up."),
                Part::tool_call("call_a", "lookup", "{\"q\":\"revenue\"}"),
                custom_call("call_b", "run_sql", "select 1"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call_a", "42"),
                Part::ToolResult(ToolResult {
                    call_id: "call_b".into(),
                    name: None,
                    content: vec![
                        Part::text("1 row"),
                        Part::Image(MediaPart::base64("image/png", "iVBOR")),
                    ],
                    is_error: false,
                    cache_control: None,
                }),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![
                cited_text("Revenue is 42."),
                Part::Refusal(RefusalPart {
                    text: "No more.".into(),
                }),
            ],
        ),
        Message::new(
            Role::System,
            vec![Part::text("Answer in French from now on.")],
        ),
        Message::user_text("Et maintenant ?"),
        Message::user_text("Encore une question."),
    ];
    request.tools = vec![
        Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: Some("Looks up a metric".into()),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}),
            strict: Some(true),
            cache_control: None,
        }),
        Tool::Function(FunctionTool {
            name: "loose".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        }),
        Tool::Custom(CustomTool {
            name: "run_sql".into(),
            description: Some("Runs SQL".into()),
            format: Some(json!({"type": "text"})),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: P,
            raw: json!({"type": "web_search", "search_context_size": "medium"}),
        }),
    ];
    request.tool_choice = Some(ToolChoice::Tool {
        name: "lookup".into(),
    });
    request.parallel_tool_calls = Some(false);
    request.max_output_tokens = Some(2048);
    request.temperature = Some(0.25);
    request.top_p = Some(0.9);
    request.reasoning = Some(ReasoningConfig {
        depth: Some(Depth::Level(Effort::High)),
        summary: Some(Summary::Detailed),
    });
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: Some("report".into()),
        description: Some("A report".into()),
        schema: json!({"type": "object", "properties": {"total": {"type": "number"}}}),
        strict: Some(true),
    });
    request.user = Some("user-7".into());
    request.metadata = json!({"trace": "t-1"}).as_object().cloned();
    request.service_tier = Some("flex".into());
    request.prompt_cache_key = Some("cache-key".into());
    // The encoder asks for replayable reasoning; a request that already says
    // so is a fixed point.
    request.store = Some(false);
    request
        .extra
        .insert("include".into(), json!(["reasoning.encrypted_content"]));
    request.extra.insert("truncation".into(), json!("auto"));
    request
}

#[test]
fn round_trip_request_is_lossless_for_a_native_request() {
    let request = rich_request();
    assert_eq!(request_round_trip(&request), request);
}

#[test]
fn round_trip_request_preserves_every_tool_choice_and_depth() {
    for choice in [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Tool {
            name: "lookup".into(),
        },
        ToolChoice::Tool {
            name: "run_sql".into(),
        },
    ] {
        let mut request = rich_request();
        request.tool_choice = Some(choice.clone());
        assert_eq!(request_round_trip(&request).tool_choice, Some(choice));
    }
    for depth in [
        Depth::Off,
        Depth::Level(Effort::Minimal),
        Depth::Level(Effort::Low),
        Depth::Level(Effort::Medium),
        Depth::Level(Effort::High),
        Depth::Level(Effort::Xhigh),
        Depth::Level(Effort::Max),
    ] {
        let mut request = rich_request();
        request.reasoning = Some(ReasoningConfig::with_depth(depth));
        assert_eq!(
            request_round_trip(&request).reasoning,
            Some(ReasoningConfig::with_depth(depth))
        );
    }
    for format in [ResponseFormat::Text, ResponseFormat::JsonObject] {
        let mut request = rich_request();
        request.response_format = Some(format.clone());
        assert_eq!(request_round_trip(&request).response_format, Some(format));
    }
}

#[test]
fn round_trip_request_decode_encode_decode_is_stable() {
    // Start from a wire body, as a client would send it.
    let body = json!({
        "model": "gpt-5",
        "instructions": "Be brief.",
        "input": [
            {"role": "developer", "content": "Prefer tables."},
            {"role": "user", "content": [
                {"type": "input_text", "text": "Compare"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBOR", "detail": "low"}
            ]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan."}], "encrypted_content": "gAAAAABx"},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{\"a\":1}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "done"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}},
            {"role": "assistant", "content": [{"type": "output_text", "text": "Here."}]},
            {"role": "user", "content": "Thanks"}
        ],
        "tools": [
            {"type": "function", "name": "f", "description": "d", "parameters": {"type": "object"}, "strict": false},
            {"type": "web_search_preview"}
        ],
        "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": [{"type": "function", "name": "f"}]},
        "reasoning": {"effort": "medium", "summary": "auto"},
        "text": {"format": {"type": "json_object"}, "verbosity": "low"},
        "max_output_tokens": 512,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "safety_identifier": "sid",
        "truncation": "auto"
    });
    let mut first = ResponsesCodec
        .decode_request(&body, &RequestPath::default())
        .unwrap();
    let mut second = request_round_trip(&first);
    // `instructions` is one string: the leading system parts come back
    // joined, with the same text.
    assert_eq!(first.system.len(), 2);
    assert_eq!(
        second.system,
        vec![Part::text("Be brief.\n\nPrefer tables.")]
    );
    first.system.clear();
    second.system.clear();
    assert_eq!(second, first);
}

#[test]
fn round_trip_request_from_another_protocol_keeps_the_conversation() {
    // An Anthropic-shaped request: tool results and follow-up text share a
    // user turn, thinking is signed by Anthropic, tools carry no `strict`.
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.system = vec![Part::text("Be helpful.")];
    request.messages = vec![
        Message::user_text("Weather?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(Reasoning {
                    id: None,
                    text: "I should call the tool.".into(),
                    signature: Some(Signature::new(Protocol::Anthropic, "EqQBsig")),
                    redacted: false,
                }),
                Part::text("Checking."),
                Part::tool_call("toolu_1", "get_weather", "{\"city\":\"Paris\"}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("toolu_1", "18C"),
                Part::text("And tomorrow?"),
            ],
        ),
    ];
    request.tools = vec![Tool::Function(FunctionTool {
        name: "get_weather".into(),
        description: Some("Weather".into()),
        parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        strict: None,
        cache_control: None,
    })];
    request.tool_choice = Some(ToolChoice::Auto);
    request.temperature = Some(0.5);
    request.max_output_tokens = Some(1024);
    request.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::Low)));

    let back = request_round_trip(&request);
    assert_eq!(back.system_text(), "Be helpful.");
    assert_eq!(back.tool_choice, request.tool_choice);
    assert_eq!(back.temperature, request.temperature);
    assert_eq!(back.max_output_tokens, request.max_output_tokens);
    assert_eq!(back.reasoning, request.reasoning);
    assert_eq!(back.tools.len(), 1);
    assert_eq!(back.tools[0].name(), Some("get_weather"));

    // The foreign-signed reasoning cannot be replayed to this vendor; the
    // rest of the conversation survives turn for turn.
    let mut expected = request.messages.clone();
    expected[1].parts.remove(0);
    assert_eq!(normalize_turns(&back.messages), normalize_turns(&expected));
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn response_round_trip(response: &Response) -> Response {
    let body = ResponsesCodec
        .encode_response(response, &ClientCtx::new(response.model.clone()))
        .expect("encodes");
    ResponsesCodec.decode_response(&body).expect("decodes")
}

fn rich_response(finish: FinishReason, parts: Vec<Part>) -> Response {
    Response {
        id: "resp_0123abc".into(),
        model: "gpt-5".into(),
        created: 1_741_476_542,
        parts,
        finish,
        stop_sequence: None,
        usage: Usage {
            input_tokens: 300,
            cache_read_tokens: 600,
            cache_write_tokens: 100,
            output_tokens: 500,
            reasoning_tokens: 200,
        },
        service_tier: Some("default".into()),
    }
}

fn rich_parts() -> Vec<Part> {
    vec![
        signed_reasoning("rs_1", "Thinking.", "gAAAAABblob"),
        Part::text("First. "),
        cited_text("Second."),
        Part::Opaque(OpaquePart {
            origin: P,
            raw: json!({"id": "ws_1", "type": "web_search_call", "status": "completed",
                        "action": {"type": "search", "query": "q"}}),
        }),
        Part::Image(MediaPart::base64("image/png", "iVBOR")),
        Part::text("Third."),
        Part::tool_call("call_a", "lookup", "{\"q\":1}"),
        custom_call("call_b", "exec", "ls -la"),
    ]
}

#[test]
fn round_trip_response_preserves_parts_finish_and_usage() {
    let response = rich_response(FinishReason::ToolCalls, rich_parts());
    assert_eq!(response_round_trip(&response), response);
}

#[test]
fn round_trip_response_preserves_each_finish_reason() {
    let text = || vec![Part::text("Some text.")];
    let cases = vec![
        (FinishReason::Stop, text()),
        (FinishReason::Length, text()),
        (
            FinishReason::ToolCalls,
            vec![Part::tool_call("call_1", "f", "{}")],
        ),
        (FinishReason::ContentFilter, text()),
        (
            FinishReason::Refusal,
            vec![Part::Refusal(RefusalPart {
                text: "I won't.".into(),
            })],
        ),
        (FinishReason::PauseTurn, text()),
        (FinishReason::ContextWindow, text()),
        (FinishReason::Error, text()),
        (FinishReason::Other("recitation".into()), text()),
        // Cut short in the middle of a tool call.
        (
            FinishReason::Length,
            vec![Part::text("x"), Part::tool_call("call_1", "f", "{\"a\":")],
        ),
    ];
    for (finish, parts) in cases {
        let response = rich_response(finish.clone(), parts);
        let back = response_round_trip(&response);
        assert_eq!(back.finish, finish);
        assert_eq!(back.parts, response.parts, "{finish:?}");
        assert_eq!(back.usage, response.usage);
    }
}

#[test]
fn round_trip_response_mints_reasoning_ids_but_keeps_the_content() {
    let mut response = rich_response(
        FinishReason::Stop,
        vec![Part::reasoning("no id"), Part::text("x")],
    );
    response.usage = Usage::default();
    let back = response_round_trip(&response);
    match &back.parts[0] {
        Part::Reasoning(reasoning) => {
            assert_eq!(reasoning.id.as_deref(), Some("rs_0123abc_0"));
            assert_eq!(reasoning.text, "no id");
            assert_eq!(reasoning.signature, None);
        }
        other => panic!("unexpected part {other:?}"),
    }
    assert_eq!(back.parts[1], Part::text("x"));
    assert_eq!(back.usage, Usage::default());
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

fn accumulate(events: &[StreamEvent]) -> (Response, Option<ApiError>) {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    let error = acc.error().cloned();
    (acc.into_response(), error)
}

/// encode → decode → accumulate.
fn stream_round_trip(events: &[StreamEvent], model: &str) -> (Response, Option<ApiError>) {
    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new(model));
    let mut wire = Vec::new();
    for event in events {
        wire.extend(encoder.encode(event));
    }
    wire.extend(encoder.finish());

    let mut decoder = ResponsesCodec.stream_decoder();
    let mut decoded = Vec::new();
    for sse in &wire {
        // Through the wire format, as a client of the gateway would see it.
        let bytes = sse.to_bytes();
        let mut parser = switchyard_core::SseParser::new();
        for parsed in parser.push(&bytes).unwrap() {
            decoded.extend(decoder.decode(&parsed).unwrap());
        }
    }
    decoded.extend(decoder.finish());
    validate_sequence(&decoded).unwrap_or_else(|v| panic!("{v}\n{decoded:#?}"));
    accumulate(&decoded)
}

#[test]
fn round_trip_stream_from_whole_responses() {
    let cases = vec![
        rich_response(FinishReason::ToolCalls, rich_parts()),
        rich_response(FinishReason::Stop, vec![Part::text("Just text.")]),
        rich_response(FinishReason::Length, vec![Part::text("Cut o")]),
        rich_response(FinishReason::ContentFilter, vec![Part::text("Blocked he")]),
        rich_response(
            FinishReason::Refusal,
            vec![Part::Refusal(RefusalPart {
                text: "I won't.".into(),
            })],
        ),
        rich_response(
            FinishReason::Stop,
            vec![
                Part::text(""),
                signed_reasoning("rs_e", "", "gAAAAABonly"),
                Part::text("after"),
            ],
        ),
        rich_response(FinishReason::PauseTurn, vec![Part::text("paused")]),
        rich_response(
            FinishReason::Other("recitation".into()),
            vec![Part::text("x")],
        ),
    ];
    for mut response in cases {
        // A stream has no slot for the service tier.
        response.service_tier = None;
        let events = response_to_events(&response);
        validate_sequence(&events).unwrap();
        let (back, error) = stream_round_trip(&events, "gpt-5");
        assert_eq!(error, None);
        assert_eq!(back, response);
    }
}

#[test]
fn round_trip_stream_hand_written_incremental_sequence() {
    let events = vec![
        StreamEvent::Start {
            id: "resp_inc".into(),
            model: "upstream".into(),
            created: 1_700_000_123,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Reasoning {
                id: Some("rs_x".into()),
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
            signature: Signature::new(P, "gAAAAABold"),
        },
        // A later signature replaces the earlier one.
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(P, "gAAAAABnew"),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::Usage(Usage {
            input_tokens: 9,
            ..Usage::default()
        }),
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "The ".into(),
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "answer".into(),
        },
        StreamEvent::Citation {
            index: 1,
            citation: Citation {
                url: Some("https://example.com".into()),
                title: Some("Example".into()),
                cited_text: None,
                start: Some(4),
                end: Some(10),
            },
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 2,
            text: " is 42.".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "verify".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 3,
            fragment: "{\"n\"".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 3,
            fragment: ":42}".into(),
        },
        StreamEvent::BlockStop { index: 3 },
        StreamEvent::BlockStart {
            index: 4,
            block: BlockStart::ToolCall {
                id: "call_2".into(),
                name: "exec".into(),
                kind: ToolCallKind::Custom,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 4,
            fragment: "echo ".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 4,
            fragment: "42".into(),
        },
        StreamEvent::BlockStop { index: 4 },
        StreamEvent::Usage(Usage {
            input_tokens: 9,
            cache_read_tokens: 3,
            output_tokens: 30,
            reasoning_tokens: 12,
            ..Usage::default()
        }),
        StreamEvent::Finish {
            reason: FinishReason::ToolCalls,
            stop_sequence: None,
        },
    ];
    validate_sequence(&events).unwrap();
    let (mut expected, _) = accumulate(&events);
    // The client sees the model name it asked for.
    expected.model = "alias".into();
    let (back, error) = stream_round_trip(&events, "alias");
    assert_eq!(error, None);
    assert_eq!(back, expected);
    assert_eq!(back.parts.len(), 5);
}

#[test]
fn round_trip_stream_error_terminated_and_failed() {
    // Terminated by an error event: content so far and the error survive.
    let error = ApiError::rate_limit("Slow down.").with_code("rate_limit_exceeded");
    let events = vec![
        StreamEvent::Start {
            id: "resp_err".into(),
            model: "m".into(),
            created: 7,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Partial".into(),
        },
        StreamEvent::Error(error.clone()),
    ];
    let (back, seen) = stream_round_trip(&events, "m");
    assert_eq!(back.finish, FinishReason::Error);
    assert_eq!(back.text(), "Partial");
    assert_eq!(seen, Some(error));

    // A sequence that finished with `Finish { Error }` is a failed response.
    let failed = rich_response(FinishReason::Error, vec![Part::text("Partial")]);
    let (back, seen) = stream_round_trip(&response_to_events(&failed), "gpt-5");
    assert_eq!(back.finish, FinishReason::Error);
    assert_eq!(back.parts, failed.parts);
    assert_eq!(seen.map(|e| e.status), Some(502));

    // A truncated sequence reaches the client as a terminal error too.
    let truncated = &events[..3];
    let (back, seen) = stream_round_trip(truncated, "m");
    assert_eq!(back.finish, FinishReason::Error);
    assert_eq!(back.text(), "Partial");
    assert_eq!(seen.map(|e| e.status), Some(502));
}

#[test]
fn round_trip_stream_and_non_stream_decoding_agree() {
    // The terminal event of an encoded stream holds the full response; a
    // client that only reads that must see what the event stream said.
    let mut response = rich_response(FinishReason::ToolCalls, rich_parts());
    response.service_tier = None;
    let mut encoder = ResponsesCodec.stream_encoder(&ClientCtx::new("gpt-5"));
    let mut wire = Vec::new();
    for event in response_to_events(&response) {
        wire.extend(encoder.encode(&event));
    }
    let terminal: Value = serde_json::from_str(&wire.last().unwrap().data).unwrap();
    let mut from_terminal = ResponsesCodec.decode_response(&terminal).unwrap();
    from_terminal.service_tier = None;
    assert_eq!(from_terminal, response);
}
