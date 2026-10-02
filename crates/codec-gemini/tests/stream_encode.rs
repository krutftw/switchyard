//! Stream encoder: canonical event sequences -> Gemini SSE chunks.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, ToolCall, ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events, validate_sequence};
use switchyard_core::{ApiError, ClientCtx, Codec, Protocol, SseEvent, Usage};

const ALIAS: &str = "my-gemini";

/// Encodes a sequence and returns the JSON payload of every wire event,
/// checking that Gemini's framing rules hold: data-only events, no `[DONE]`.
fn wire(events: &[StreamEvent], finish: bool) -> Vec<Value> {
    wire_with(events, finish, ALIAS)
}

fn wire_with(events: &[StreamEvent], finish: bool, model: &str) -> Vec<Value> {
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new(model));
    let mut out: Vec<SseEvent> = Vec::new();
    for event in events {
        out.extend(encoder.encode(event));
    }
    if finish {
        out.extend(encoder.finish());
        assert!(encoder.finish().is_empty(), "finish() is idempotent");
    }
    out.iter()
        .map(|sse| {
            assert_eq!(sse.event, None, "Gemini streams have no event names");
            assert!(!sse.is_done_marker(), "Gemini streams have no [DONE]");
            assert!(!sse.data.contains('\n'), "one line per payload");
            serde_json::from_str(&sse.data).expect("payload is JSON")
        })
        .collect()
}

fn chunk(part: Value) -> Value {
    json!({
        "candidates": [{"content": {"role": "model", "parts": [part]}, "index": 0}],
        "modelVersion": ALIAS,
        "responseId": "resp-1"
    })
}

fn last_chunk(finish: &str, usage: Value) -> Value {
    json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": finish, "index": 0}],
        "usageMetadata": usage,
        "modelVersion": ALIAS,
        "responseId": "resp-1"
    })
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "resp-1".into(),
        model: "gemini-2.5-pro".into(),
        created: 1_700_000_000,
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn text_block(index: u32, deltas: &[&str]) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::BlockStart {
        index,
        block: BlockStart::Text,
    }];
    events.extend(deltas.iter().map(|text| StreamEvent::TextDelta {
        index,
        text: (*text).into(),
    }));
    events.push(StreamEvent::BlockStop { index });
    events
}

const ZERO_USAGE: fn() -> Value =
    || json!({"promptTokenCount": 0, "candidatesTokenCount": 0, "totalTokenCount": 0});

// ---------------------------------------------------------------------------
// From complete responses
// ---------------------------------------------------------------------------

#[test]
fn text_response_replayed_as_a_stream() {
    let mut response = Response::new("resp-1", "gemini-2.5-pro");
    response.parts = vec![Part::text("Hello there")];
    response.usage = Usage {
        input_tokens: 4,
        output_tokens: 2,
        ..Usage::default()
    };
    let events = response_to_events(&response);
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Hello there"})),
            last_chunk(
                "STOP",
                json!({"promptTokenCount": 4, "candidatesTokenCount": 2, "totalTokenCount": 6})
            ),
        ]
    );
}

#[test]
fn reasoning_text_and_tool_calls_replayed_as_a_stream() {
    let mut response = Response::new("resp-1", "gemini-2.5-pro");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "Need the weather.".into(),
            signature: Some(Signature::new(Protocol::Gemini, "R1NJRw==")),
            redacted: false,
        }),
        Part::text("Checking."),
        Part::ToolCall(ToolCall {
            id: "fc_1".into(),
            name: "get_weather".into(),
            arguments: r#"{"city":"Paris"}"#.into(),
            kind: ToolCallKind::Function,
            signature: Some(Signature::new(Protocol::Gemini, "Q1NJRw==")),
            cache_control: None,
        }),
        Part::tool_call("fc_2", "get_time", ""),
    ];
    response.finish = FinishReason::ToolCalls;
    response.usage = Usage {
        input_tokens: 40,
        cache_read_tokens: 60,
        cache_write_tokens: 0,
        output_tokens: 50,
        reasoning_tokens: 30,
    };
    let events = response_to_events(&response);
    validate_sequence(&events).unwrap();
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Need the weather.", "thought": true})),
            chunk(json!({"text": "", "thought": true, "thoughtSignature": "R1NJRw=="})),
            chunk(json!({"text": "Checking."})),
            chunk(json!({
                "functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "fc_1"},
                "thoughtSignature": "Q1NJRw=="
            })),
            chunk(json!({"functionCall": {"name": "get_time", "args": {}, "id": "fc_2"}})),
            last_chunk(
                "STOP",
                json!({
                    "promptTokenCount": 100,
                    "cachedContentTokenCount": 60,
                    "candidatesTokenCount": 20,
                    "thoughtsTokenCount": 30,
                    "totalTokenCount": 150
                })
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Hand-written incremental sequences
// ---------------------------------------------------------------------------

#[test]
fn text_deltas_become_one_chunk_each() {
    let mut events = vec![start()];
    events.extend(text_block(0, &["Hel", "", "lo", " world"]));
    events.push(StreamEvent::Usage(Usage {
        input_tokens: 3,
        ..Usage::default()
    }));
    events.push(StreamEvent::Usage(Usage {
        output_tokens: 2,
        ..Usage::default()
    }));
    events.push(finish(FinishReason::Length));
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Hel"})),
            chunk(json!({"text": "lo"})),
            chunk(json!({"text": " world"})),
            // Running usage totals are merged and reported once, at the end.
            last_chunk(
                "MAX_TOKENS",
                json!({"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5})
            ),
        ]
    );
}

#[test]
fn fragmented_tool_arguments_are_buffered_into_one_function_call() {
    let tool_start =
        |index: u32, id: &str, name: &str, signature: Option<Signature>| StreamEvent::BlockStart {
            index,
            block: BlockStart::ToolCall {
                id: id.into(),
                name: name.into(),
                kind: ToolCallKind::Function,
                signature,
            },
        };
    let fragment = |index: u32, fragment: &str| StreamEvent::ToolArgsDelta {
        index,
        fragment: fragment.into(),
    };
    let mut events = vec![start()];
    events.extend(text_block(0, &["Let me check."]));
    events.extend([
        // Arguments arrive in pieces, as OpenAI and Anthropic stream them.
        tool_start(1, "call_abc", "get_weather", None),
        fragment(1, "{\"ci"),
        fragment(1, "ty\": \"Par"),
        fragment(1, "is\", \"days\": [1, 2"),
        fragment(1, "]}"),
        StreamEvent::BlockStop { index: 1 },
        // A call whose signature came from another vendor.
        tool_start(
            2,
            "toolu_01",
            "get_time",
            Some(Signature::new(Protocol::Anthropic, "ErAC")),
        ),
        StreamEvent::BlockStop { index: 2 },
        // Arguments that never became valid JSON are still delivered.
        tool_start(3, "call_bad", "broken", None),
        fragment(3, "{\"unterminated\": "),
        StreamEvent::BlockStop { index: 3 },
        finish(FinishReason::ToolCalls),
    ]);
    validate_sequence(&events).unwrap();
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Let me check."})),
            chunk(
                json!({"functionCall": {"name": "get_weather", "args": {"city": "Paris", "days": [1, 2]}, "id": "call_abc"}})
            ),
            chunk(json!({
                "functionCall": {"name": "get_time", "args": {}, "id": "toolu_01"},
                // base64("sy1.a.ErAC")
                "thoughtSignature": "c3kxLmEuRXJBQw=="
            })),
            chunk(
                json!({"functionCall": {"name": "broken", "args": {"input": "{\"unterminated\": "}, "id": "call_bad"}})
            ),
            last_chunk("STOP", ZERO_USAGE()),
        ]
    );
}

#[test]
fn reasoning_deltas_and_signatures() {
    let reasoning_start = |index: u32, redacted: bool| StreamEvent::BlockStart {
        index,
        block: BlockStart::Reasoning {
            id: Some("rs_1".into()),
            redacted,
        },
    };
    let events = vec![
        start(),
        reasoning_start(0, false),
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "Step 1. ".into(),
        },
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "Step 2.".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: Signature::new(Protocol::Anthropic, "ErACkgE="),
        },
        StreamEvent::BlockStop { index: 0 },
        // Redacted reasoning: only the encrypted payload.
        reasoning_start(1, true),
        StreamEvent::ReasoningSignature {
            index: 1,
            signature: Signature::new(Protocol::OpenaiResponses, "gAAAAAB"),
        },
        StreamEvent::BlockStop { index: 1 },
        // Reasoning with nothing to show produces nothing.
        reasoning_start(2, false),
        StreamEvent::BlockStop { index: 2 },
        finish(FinishReason::Stop),
    ];
    validate_sequence(&events).unwrap();
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Step 1. ", "thought": true})),
            chunk(json!({"text": "Step 2.", "thought": true})),
            // base64("sy1.a.ErACkgE=") and, for the withheld reasoning, the
            // payload marked as such: base64("sy1.r.redacted:gAAAAAB")
            chunk(json!({"text": "", "thought": true, "thoughtSignature": "c3kxLmEuRXJBQ2tnRT0="})),
            chunk(json!({"text": "", "thoughtSignature": "c3kxLnIucmVkYWN0ZWQ6Z0FBQUFBQg=="})),
            last_chunk("STOP", ZERO_USAGE()),
        ]
    );
}

#[test]
fn refusal_block_is_delivered_as_text_with_a_safety_finish() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Refusal,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "I can't help with that.".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Refusal),
    ];
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "I can't help with that."})),
            last_chunk("SAFETY", ZERO_USAGE())
        ]
    );
}

#[test]
fn whole_parts() {
    let whole = |index: u32, part: Part| {
        [
            StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole { part },
            },
            StreamEvent::BlockStop { index },
        ]
    };
    let code = json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}});
    let grounding = json!({"webSearchQueries": ["q"]});
    let mut events = vec![start()];
    events.extend(whole(
        0,
        Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
    ));
    events.extend(whole(
        1,
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw: code.clone(),
        }),
    ));
    // Another vendor's block has no Gemini form.
    events.extend(whole(
        2,
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw: json!({"type": "server_tool_use"}),
        }),
    ));
    // Candidate-level metadata is held back for the last chunk.
    events.extend(whole(
        3,
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw: json!({"groundingMetadata": grounding}),
        }),
    ));
    events.push(finish(FinishReason::Stop));
    validate_sequence(&events).unwrap();
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}})),
            chunk(code),
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": ""}]},
                    "finishReason": "STOP",
                    "index": 0,
                    "groundingMetadata": grounding
                }],
                "usageMetadata": ZERO_USAGE(),
                "modelVersion": ALIAS,
                "responseId": "resp-1"
            }),
        ]
    );
}

#[test]
fn citations_become_grounding_metadata_on_the_last_chunk() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Le café ouvre".into(),
        },
        StreamEvent::Citation {
            index: 0,
            citation: Citation {
                url: Some("https://example.com/a".into()),
                title: Some("A".into()),
                cited_text: None,
                start: Some(3),
                end: Some(7),
            },
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Stop),
    ];
    validate_sequence(&events).unwrap();
    let out = wire(&events, true);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[1]["candidates"][0]["groundingMetadata"],
        json!({
            "groundingChunks": [{"web": {"uri": "https://example.com/a", "title": "A"}}],
            // Character offsets 3..7 ("café") are bytes 3..8.
            "groundingSupports": [{"segment": {"startIndex": 3, "endIndex": 8}, "groundingChunkIndices": [0]}]
        })
    );
}

#[test]
fn wire_bytes_follow_gemini_key_order() {
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new(ALIAS));
    let mut out = Vec::new();
    let mut events = vec![start()];
    events.extend(text_block(0, &["Hi"]));
    events.push(StreamEvent::Usage(Usage {
        input_tokens: 1,
        output_tokens: 1,
        ..Usage::default()
    }));
    events.push(finish(FinishReason::Stop));
    for event in &events {
        out.extend(encoder.encode(event));
    }
    let bytes: Vec<String> = out
        .iter()
        .map(|sse| String::from_utf8(sse.to_bytes().to_vec()).unwrap())
        .collect();
    assert_eq!(
        bytes,
        [
            concat!(
                r#"data: {"candidates":[{"content":{"parts":[{"text":"Hi"}],"role":"model"},"index":0}],"#,
                r#""modelVersion":"my-gemini","responseId":"resp-1"}"#,
                "

"
            ),
            concat!(
                r#"data: {"candidates":[{"content":{"parts":[{"text":""}],"role":"model"},"finishReason":"STOP","index":0}],"#,
                r#""usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2},"#,
                r#""modelVersion":"my-gemini","responseId":"resp-1"}"#,
                "

"
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Terminators
// ---------------------------------------------------------------------------

#[test]
fn error_terminated_sequence() {
    let mut events = vec![start()];
    events.extend(text_block(0, &["Partial"]));
    events.push(StreamEvent::Error(
        ApiError::unavailable("upstream overloaded")
            .with_retry_after(std::time::Duration::from_secs(5)),
    ));
    // Nothing may follow a terminal event; if something does it is ignored.
    events.push(StreamEvent::TextDelta {
        index: 0,
        text: "late".into(),
    });
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "Partial"})),
            json!({"error": {
                "code": 503,
                "message": "upstream overloaded",
                "status": "UNAVAILABLE",
                "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "5s"}]
            }}),
        ]
    );
}

#[test]
fn error_as_the_only_event() {
    let events = vec![StreamEvent::Error(ApiError::rate_limit("slow down"))];
    assert_eq!(
        wire(&events, true),
        vec![
            json!({"error": {"code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED"}})
        ]
    );
}

#[test]
fn truncated_sequence_is_closed_with_an_abnormal_finish() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "The ans".into(),
        },
        StreamEvent::Usage(Usage {
            input_tokens: 9,
            output_tokens: 2,
            ..Usage::default()
        }),
        StreamEvent::BlockStop { index: 0 },
        // A tool call that never completed must not be delivered.
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::ToolCall {
                id: "c1".into(),
                name: "f".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 1,
            fragment: "{\"half\":".into(),
        },
    ];
    // Without `finish()` nothing closes the stream.
    assert_eq!(
        wire(&events, false),
        vec![chunk(json!({"text": "The ans"}))]
    );
    assert_eq!(
        wire(&events, true),
        vec![
            chunk(json!({"text": "The ans"})),
            last_chunk(
                "OTHER",
                json!({"promptTokenCount": 9, "candidatesTokenCount": 2, "totalTokenCount": 11})
            ),
        ]
    );
}

#[test]
fn nothing_is_emitted_for_a_sequence_that_never_started() {
    assert!(wire(&[], true).is_empty());
}

#[test]
fn a_finished_stream_has_no_extra_terminator() {
    let mut events = vec![start()];
    events.extend(text_block(0, &["ok"]));
    events.push(finish(FinishReason::Stop));
    assert_eq!(wire(&events, true), wire(&events, false));
}

// ---------------------------------------------------------------------------
// Ids and model names
// ---------------------------------------------------------------------------

#[test]
fn every_chunk_carries_the_same_id_and_the_clients_model_name() {
    let mut events = vec![StreamEvent::Start {
        // The upstream had not sent an id.
        id: String::new(),
        model: "upstream-model-id".into(),
        created: 0,
    }];
    events.extend(text_block(0, &["a", "b"]));
    events.push(finish(FinishReason::Stop));
    let out = wire(&events, true);
    assert_eq!(out.len(), 3);
    let id = out[0]["responseId"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 24, "an id is minted: {id}");
    for payload in &out {
        assert_eq!(payload["responseId"], id.as_str());
        assert_eq!(payload["modelVersion"], ALIAS);
    }
    // Without a client-facing name the upstream's model is reported.
    let out = wire_with(&events, true, "");
    assert!(
        out.iter()
            .all(|payload| payload["modelVersion"] == "upstream-model-id")
    );
}

#[test]
fn finish_reasons_in_the_last_chunk() {
    for (reason, expected) in [
        (FinishReason::Stop, "STOP"),
        (FinishReason::ToolCalls, "STOP"),
        (FinishReason::Length, "MAX_TOKENS"),
        (FinishReason::ContentFilter, "SAFETY"),
        (FinishReason::Error, "OTHER"),
        (FinishReason::Other("RECITATION".into()), "RECITATION"),
    ] {
        let out = wire(&[start(), finish(reason.clone())], true);
        assert_eq!(out, vec![last_chunk(expected, ZERO_USAGE())], "{reason:?}");
    }
}

#[test]
fn whole_refusal_part_is_rendered_as_text() {
    let events = vec![
        start(),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Whole {
                part: Part::Refusal(RefusalPart { text: "No.".into() }),
            },
        },
        StreamEvent::BlockStop { index: 0 },
        finish(FinishReason::Refusal),
    ];
    assert_eq!(wire(&events, true)[0], chunk(json!({"text": "No."})));
}
