//! Round-trip properties (DESIGN.md section 3): requests, responses and
//! streams survive encode -> decode.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    FinishReason, FunctionTool, MediaPart, Message, OpaquePart, Part, Reasoning, Request, Response,
    ResponseFormat, Role, Signature, TextPart, Tool, ToolCall, ToolCallKind, ToolChoice,
    ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::stream::{
    Accumulator, BlockStart, StreamEvent, response_to_events, validate_sequence,
};
use switchyard_core::{
    ApiError, ClientCtx, Codec, ErrorKind, Protocol, RequestPath, SseParser, UpstreamCtx, Usage,
};

const MODEL: &str = "gemini-2.5-pro";

fn path() -> RequestPath<'static> {
    RequestPath {
        model: Some(MODEL),
        stream: Some(false),
    }
}

fn round_trip_request(request: &Request) -> Request {
    let body = GeminiCodec
        .encode_request(request, &UpstreamCtx::default())
        .unwrap();
    assert!(body.get("model").is_none() && body.get("stream").is_none());
    GeminiCodec.decode_request(&body, &path()).unwrap()
}

fn gemini_sig(data: &str) -> Option<Signature> {
    Some(Signature::new(Protocol::Gemini, data))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A request using everything Gemini can express, as a Gemini client sends it.
fn native_body() -> Value {
    json!({
        "systemInstruction": {"parts": [{"text": "You are a travel assistant."}, {"text": "Be concise."}]},
        "contents": [
            {"role": "user", "parts": [
                {"text": "Compare the weather."},
                {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
                {"fileData": {"mimeType": "application/pdf", "fileUri": "https://example.com/brochure.pdf"}},
                {"fileData": {"mimeType": "video/mp4", "fileUri": "gs://bucket/clip.mp4"}}
            ]},
            {"role": "model", "parts": [
                {"text": "Planning the lookups.", "thought": true, "thoughtSignature": "VGhvdWdodFNpZw=="},
                {"text": "Let me check both cities."},
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}, "thoughtSignature": "Q2FsbFNpZw=="},
                {"functionCall": {"name": "get_weather", "args": {"city": "Rome"}}},
                {"functionCall": {"name": "get_time", "args": {}, "id": "fc-explicit"}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"result": "18C"}}},
                {"functionResponse": {"name": "get_weather", "response": {"temp": 24, "sky": "clear"}}},
                {"functionResponse": {"name": "get_time", "response": {"error": "clock offline"}, "id": "fc-explicit"}}
            ]},
            {"role": "model", "parts": [
                {"text": "Paris 18C, Rome 24C."},
                {"text": "", "thoughtSignature": "VHJhaWxpbmdTaWc="}
            ]},
            {"role": "user", "parts": [{"text": "And tomorrow?"}]}
        ],
        "tools": [
            {"functionDeclarations": [
                {"name": "get_weather", "description": "Weather for a city", "parameters": {
                    "type": "OBJECT",
                    "properties": {"city": {"type": "STRING"}, "days": {"type": "INTEGER", "minimum": 1}},
                    "required": ["city"]
                }},
                {"name": "get_time", "parametersJsonSchema": {"type": "object", "properties": {}}}
            ]}
        ],
        "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}},
        "generationConfig": {
            "temperature": 0.4,
            "topP": 0.9,
            "topK": 32,
            "maxOutputTokens": 4096,
            "stopSequences": ["END"],
            "candidateCount": 1,
            "seed": 7,
            "presencePenalty": 0.25,
            "frequencyPenalty": 0.5,
            "responseMimeType": "application/json",
            "responseSchema": {"type": "OBJECT", "properties": {"summary": {"type": "STRING"}}},
            "responseModalities": ["TEXT"],
            "thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}
        },
        "safetySettings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_ONLY_HIGH"}],
        "cachedContent": "cachedContents/abc123",
        "labels": {"team": "travel"},
        "serviceTier": "flex"
    })
}

#[test]
fn native_request_round_trips_completely() {
    let original = GeminiCodec.decode_request(&native_body(), &path()).unwrap();
    // Sanity: the fixture really exercises what it claims to.
    assert_eq!(original.messages.len(), 5);
    assert_eq!(original.tools.len(), 2);
    assert_eq!(
        original.tool_choice,
        Some(ToolChoice::Tool {
            name: "get_weather".into()
        })
    );
    assert_eq!(
        original.reasoning,
        Some(ReasoningConfig {
            depth: Some(Depth::Budget(2048)),
            summary: Some(Summary::Auto)
        })
    );
    assert!(matches!(
        original.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));

    let again = round_trip_request(&original);
    assert_eq!(again, original);
    // And once more: encoding is stable.
    assert_eq!(round_trip_request(&again), original);
}

#[test]
fn native_request_body_is_stable_after_one_pass() {
    let request = GeminiCodec.decode_request(&native_body(), &path()).unwrap();
    let first = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .unwrap();
    let second = GeminiCodec
        .encode_request(
            &GeminiCodec.decode_request(&first, &path()).unwrap(),
            &UpstreamCtx::default(),
        )
        .unwrap();
    assert_eq!(first, second);
    // Native signatures and the explicit call id went through untouched.
    let model_turn = &first["contents"][1]["parts"];
    assert_eq!(model_turn[0]["thoughtSignature"], "VGhvdWdodFNpZw==");
    assert_eq!(model_turn[2]["thoughtSignature"], "Q2FsbFNpZw==");
    assert!(model_turn[3].get("thoughtSignature").is_none());
    assert_eq!(model_turn[4]["functionCall"]["id"], "fc-explicit");
    assert_eq!(
        first["contents"][2]["parts"][2]["functionResponse"]["id"],
        "fc-explicit"
    );
    assert_eq!(
        first["contents"][3]["parts"][1],
        json!({"text": "", "thoughtSignature": "VHJhaWxpbmdTaWc="})
    );
}

/// Replaces call ids by their position, so conversations whose ids cannot be
/// expressed in Gemini (it has none of its own) can still be compared.
fn positional_ids(messages: &[Message]) -> Vec<Message> {
    let mut seen: Vec<String> = Vec::new();
    let mut position = |id: &str| {
        let index = match seen.iter().position(|known| known == id) {
            Some(index) => index,
            None => {
                seen.push(id.to_string());
                seen.len() - 1
            }
        };
        format!("#{index}")
    };
    let mut out = messages.to_vec();
    for message in &mut out {
        for part in &mut message.parts {
            match part {
                Part::ToolCall(call) => call.id = position(&call.id),
                Part::ToolResult(result) => result.call_id = position(&result.call_id),
                _ => {}
            }
        }
    }
    out
}

#[test]
fn foreign_request_round_trips_messages_tools_choice_sampling_and_depth() {
    let mut request = Request::new(MODEL, Protocol::OpenaiChat);
    request.system = vec![Part::text("Be helpful.")];
    request.messages = vec![
        Message::user_text("Find flights and hotels."),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("Searching."),
                Part::tool_call("call_AbC123", "search_flights", r#"{"to":"Paris"}"#),
                Part::tool_call(
                    "call_XyZ789",
                    "search_hotels",
                    r#"{"city":"Paris","stars":4}"#,
                ),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::ToolResult(ToolResult {
                    call_id: "call_AbC123".into(),
                    name: Some("search_flights".into()),
                    content: vec![Part::text(r#"{"flights":3}"#)],
                    is_error: false,
                    cache_control: None,
                }),
                Part::ToolResult(ToolResult {
                    call_id: "call_XyZ789".into(),
                    name: Some("search_hotels".into()),
                    content: vec![Part::text("no rooms available")],
                    is_error: true,
                    cache_control: None,
                }),
            ],
        ),
        Message::assistant_text("Three flights, no hotels."),
        Message::new(
            Role::User,
            vec![
                Part::text("What about this one?"),
                Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
            ],
        ),
    ];
    request.tools = vec![
        Tool::Function(FunctionTool {
            name: "search_flights".into(),
            description: Some("Find flights".into()),
            parameters: json!({"type": "object", "properties": {"to": {"type": "string"}}, "required": ["to"]}),
            strict: None,
            cache_control: None,
        }),
        Tool::Function(FunctionTool {
            name: "search_hotels".into(),
            description: Some("Find hotels".into()),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}, "stars": {"type": "integer", "minimum": 1, "maximum": 5}}
            }),
            strict: None,
            cache_control: None,
        }),
    ];
    request.tool_choice = Some(ToolChoice::Tool {
        name: "search_hotels".into(),
    });
    request.temperature = Some(0.3);
    request.top_p = Some(0.8);
    request.top_k = Some(20);
    request.max_output_tokens = Some(2048);
    request.stop = vec!["DONE".into()];
    request.seed = Some(11);
    request.presence_penalty = Some(0.1);
    request.frequency_penalty = Some(0.2);
    request.reasoning = Some(ReasoningConfig {
        depth: Some(Depth::Level(Effort::Low)),
        summary: Some(Summary::Off),
    });
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema: json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}),
        strict: None,
    });

    let decoded = round_trip_request(&request);
    assert_eq!(decoded.source, Protocol::Gemini);
    assert_eq!(decoded.system, request.system);
    assert_eq!(
        positional_ids(&decoded.messages),
        positional_ids(&request.messages)
    );
    assert_eq!(decoded.tools, request.tools);
    assert_eq!(decoded.tool_choice, request.tool_choice);
    assert_eq!(decoded.temperature, request.temperature);
    assert_eq!(decoded.top_p, request.top_p);
    assert_eq!(decoded.top_k, request.top_k);
    assert_eq!(decoded.max_output_tokens, request.max_output_tokens);
    assert_eq!(decoded.stop, request.stop);
    assert_eq!(decoded.seed, request.seed);
    assert_eq!(decoded.presence_penalty, request.presence_penalty);
    assert_eq!(decoded.frequency_penalty, request.frequency_penalty);
    assert_eq!(decoded.reasoning, request.reasoning);
    assert_eq!(decoded.response_format, request.response_format);
}

#[test]
fn every_reasoning_depth_round_trips() {
    for depth in [
        Depth::Off,
        Depth::Auto,
        Depth::Budget(1),
        Depth::Budget(24576),
        Depth::Level(Effort::Minimal),
        Depth::Level(Effort::Low),
        Depth::Level(Effort::Medium),
        Depth::Level(Effort::High),
    ] {
        for summary in [None, Some(Summary::Auto), Some(Summary::Off)] {
            let mut request = Request::new(MODEL, Protocol::Anthropic);
            request.messages = vec![Message::user_text("hi")];
            request.reasoning = Some(ReasoningConfig {
                depth: Some(depth),
                summary,
            });
            assert_eq!(
                round_trip_request(&request).reasoning,
                request.reasoning,
                "{depth:?} {summary:?}"
            );
        }
    }
}

#[test]
fn every_tool_choice_round_trips() {
    for choice in [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Tool { name: "f".into() },
    ] {
        let mut request = Request::new(MODEL, Protocol::OpenaiResponses);
        request.messages = vec![Message::user_text("hi")];
        request.tools = vec![Tool::Function(FunctionTool {
            name: "f".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            strict: None,
            cache_control: None,
        })];
        request.tool_choice = Some(choice.clone());
        let decoded = round_trip_request(&request);
        assert_eq!(decoded.tool_choice, Some(choice));
        assert_eq!(decoded.tools, request.tools);
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn responses() -> Vec<Response> {
    let base = |parts: Vec<Part>, finish: FinishReason, usage: Usage| {
        let mut r = Response::new("resp-rt", MODEL);
        r.parts = parts;
        r.finish = finish;
        r.usage = usage;
        r
    };
    let call = |id: &str, name: &str, args: &str, signature: Option<Signature>| {
        Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
            kind: ToolCallKind::Function,
            signature,
            cache_control: None,
        })
    };
    let thought = |text: &str, signature: Option<Signature>| {
        Part::Reasoning(Reasoning {
            id: None,
            text: text.into(),
            signature,
            redacted: false,
        })
    };
    vec![
        base(
            vec![Part::text("Plain answer.")],
            FinishReason::Stop,
            Usage {
                input_tokens: 12,
                output_tokens: 3,
                ..Usage::default()
            },
        ),
        base(
            vec![
                thought("Thinking it through.", gemini_sig("VGhpbmtTaWc=")),
                Part::text("The answer is 4."),
                thought("", gemini_sig("VHJhaWxTaWc=")),
            ],
            FinishReason::Length,
            Usage {
                input_tokens: 40,
                cache_read_tokens: 60,
                cache_write_tokens: 0,
                output_tokens: 50,
                reasoning_tokens: 30,
            },
        ),
        base(
            vec![
                Part::text("Calling tools."),
                call(
                    "fc_1",
                    "get_weather",
                    r#"{"city":"Paris","days":[1,2]}"#,
                    gemini_sig("Q2FsbFNpZw=="),
                ),
                call("toolu_01XYZ", "get_time", "{}", None),
            ],
            FinishReason::ToolCalls,
            Usage {
                input_tokens: 7,
                output_tokens: 21,
                reasoning_tokens: 9,
                ..Usage::default()
            },
        ),
        base(
            vec![
                Part::text("Here is the chart:"),
                Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Gemini,
                    raw: json!({"executableCode": {"language": "PYTHON", "code": "plot()"}}),
                }),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Gemini,
                    raw: json!({"groundingMetadata": {"webSearchQueries": ["chart data"]}}),
                }),
            ],
            FinishReason::Stop,
            Usage {
                input_tokens: 5,
                output_tokens: 1300,
                ..Usage::default()
            },
        ),
        base(
            vec![],
            FinishReason::ContentFilter,
            Usage {
                input_tokens: 9,
                ..Usage::default()
            },
        ),
        base(
            vec![Part::text("cut")],
            FinishReason::Other("RECITATION_LIKE".into()),
            Usage::default(),
        ),
    ]
}

#[test]
fn responses_round_trip_parts_finish_and_usage() {
    for (index, response) in responses().into_iter().enumerate() {
        let body = GeminiCodec
            .encode_response(&response, &ClientCtx::new(MODEL))
            .unwrap();
        let decoded = GeminiCodec.decode_response(&body).unwrap();
        // The last fixture's reason has no Gemini spelling.
        let expected_finish = match &response.finish {
            FinishReason::Other(_) => FinishReason::Other("OTHER".into()),
            other => other.clone(),
        };
        assert_eq!(decoded.parts, response.parts, "parts of response #{index}");
        assert_eq!(
            decoded.finish, expected_finish,
            "finish of response #{index}"
        );
        assert_eq!(decoded.usage, response.usage, "usage of response #{index}");
        assert_eq!(decoded.id, response.id);
        assert_eq!(decoded.model, response.model);
    }
}

#[test]
fn upstream_response_survives_re_encoding_for_a_gemini_client() {
    let upstream = json!({
        "candidates": [{
            "content": {"parts": [
                {"text": "Thinking.", "thought": true},
                {"functionCall": {"name": "lookup", "args": {"q": "x"}, "id": "fc-9"}, "thoughtSignature": "U0lH"},
                {"functionCall": {"name": "lookup", "args": {"q": "y"}, "id": "fc-10"}}
            ], "role": "model"},
            "finishReason": "STOP",
            "index": 0,
            "groundingMetadata": {"webSearchQueries": ["x", "y"]}
        }],
        "usageMetadata": {"promptTokenCount": 30, "cachedContentTokenCount": 10, "candidatesTokenCount": 12, "thoughtsTokenCount": 8, "totalTokenCount": 50},
        "modelVersion": "gemini-3-pro-preview",
        "responseId": "up-1"
    });
    let decoded = GeminiCodec.decode_response(&upstream).unwrap();
    let re_encoded = GeminiCodec
        .encode_response(&decoded, &ClientCtx::new("gemini-3-pro-preview"))
        .unwrap();
    assert_eq!(re_encoded, upstream);
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// encoder -> SSE bytes -> parser -> decoder -> accumulator.
fn through_the_wire(events: &[StreamEvent], model: &str) -> (Vec<StreamEvent>, Response) {
    validate_sequence(events).expect("input sequence is valid");
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new(model));
    let mut bytes = Vec::new();
    for event in events {
        for sse in encoder.encode(event) {
            bytes.extend_from_slice(&sse.to_bytes());
        }
    }
    for sse in encoder.finish() {
        bytes.extend_from_slice(&sse.to_bytes());
    }
    let mut parser = SseParser::new();
    let mut decoder = GeminiCodec.stream_decoder();
    let mut decoded = Vec::new();
    for chunk in bytes.chunks(64) {
        for sse in parser.push(chunk).unwrap() {
            decoded.extend(decoder.decode(&sse).unwrap());
        }
    }
    if let Some(sse) = parser.finish() {
        decoded.extend(decoder.decode(&sse).unwrap());
    }
    decoded.extend(decoder.finish());
    validate_sequence(&decoded).expect("decoded sequence is valid");
    let mut acc = Accumulator::new();
    for event in &decoded {
        acc.push(event);
    }
    (decoded, acc.into_response())
}

fn accumulate(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    acc.into_response()
}

#[test]
fn replayed_responses_round_trip_through_the_stream_codecs() {
    for (index, response) in responses().into_iter().enumerate() {
        // Finish reasons Gemini cannot spell do not round-trip by design.
        if matches!(response.finish, FinishReason::Other(_)) {
            continue;
        }
        let events = response_to_events(&response);
        let (_, streamed) = through_the_wire(&events, MODEL);
        assert_eq!(streamed, response, "response #{index}");
    }
}

#[test]
fn incremental_sequence_round_trips_through_the_stream_codecs() {
    let events = vec![
        StreamEvent::Start {
            id: "stream-1".into(),
            model: MODEL.into(),
            created: 0,
        },
        StreamEvent::Usage(Usage {
            input_tokens: 20,
            cache_read_tokens: 5,
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
            text: "First I ".into(),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "consider…".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Gemini, "U0lHTkFUVVJF"),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "Sure — ".into(),
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "calling two tools.".into(),
        },
        StreamEvent::BlockStop { index: 1 },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "alpha".into(),
                kind: ToolCallKind::Function,
                signature: Some(Signature::new(Protocol::Gemini, "Q0FMTA==")),
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 2,
            fragment: "{\"a\":".into(),
        },
        StreamEvent::ToolArgsDelta {
            index: 2,
            fragment: "[1,2,{\"b\":\"c\"}]}".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::ToolCall {
                id: "call_2".into(),
                name: "beta".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 3,
            fragment: "{}".into(),
        },
        StreamEvent::BlockStop { index: 3 },
        StreamEvent::BlockStart {
            index: 4,
            block: BlockStart::Whole {
                part: Part::Image(MediaPart::base64("image/png", "AAAA")),
            },
        },
        StreamEvent::BlockStop { index: 4 },
        StreamEvent::Usage(Usage {
            output_tokens: 33,
            reasoning_tokens: 12,
            ..Usage::default()
        }),
        StreamEvent::Finish {
            reason: FinishReason::ToolCalls,
            stop_sequence: None,
        },
    ];
    let (_, streamed) = through_the_wire(&events, MODEL);
    assert_eq!(streamed, accumulate(&events));
    assert_eq!(streamed.tool_calls().count(), 2);
    assert_eq!(
        streamed.usage,
        Usage {
            input_tokens: 20,
            cache_read_tokens: 5,
            cache_write_tokens: 0,
            output_tokens: 33,
            reasoning_tokens: 12,
        }
    );
}

#[test]
fn error_terminated_sequence_round_trips_as_an_error() {
    let events = vec![
        StreamEvent::Start {
            id: "stream-err".into(),
            model: MODEL.into(),
            created: 0,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Half an ans".into(),
        },
        StreamEvent::Error(ApiError::unavailable("the upstream went away")),
    ];
    let (decoded, streamed) = through_the_wire(&events, MODEL);
    let Some(StreamEvent::Error(error)) = decoded.last() else {
        panic!("{decoded:#?}")
    };
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(error.status, 503);
    assert_eq!(error.message, "the upstream went away");
    assert_eq!(streamed.text(), "Half an ans");
    assert_eq!(streamed.finish, FinishReason::Error);
}

#[test]
fn signed_text_survives_a_non_stream_round_trip_only() {
    // The stream model has no signature slot on text blocks, so a signature
    // on non-empty text is carried by complete responses only.
    let mut response = Response::new("resp-sig", MODEL);
    response.parts = vec![Part::Text(TextPart {
        text: "Signed text.".into(),
        signature: gemini_sig("VGV4dFNpZw=="),
        ..TextPart::default()
    })];
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new(MODEL))
        .unwrap();
    assert_eq!(
        GeminiCodec.decode_response(&body).unwrap().parts,
        response.parts
    );
    let (_, streamed) = through_the_wire(&response_to_events(&response), MODEL);
    assert_eq!(streamed.parts, vec![Part::text("Signed text.")]);
}
