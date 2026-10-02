//! Stream decoder: vendor transcripts -> canonical event sequences.

use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, ToolCallKind,
};
use switchyard_core::stream::{Accumulator, BlockStart, StreamEvent, validate_sequence};
use switchyard_core::{Codec, ErrorKind, Protocol, SseEvent, SseParser, Usage};

/// Runs a raw SSE transcript through the parser and the decoder, checks the
/// sequence contract and returns the events.
fn run(transcript: &str) -> Vec<StreamEvent> {
    let mut parser = SseParser::new();
    let mut decoder = GeminiCodec.stream_decoder();
    let mut events = Vec::new();
    // Feed in awkward chunk sizes to exercise the incremental parser too.
    for chunk in transcript.as_bytes().chunks(37) {
        for sse in parser.push(chunk).expect("within the size limit") {
            events.extend(decoder.decode(&sse).expect("decodable"));
        }
    }
    if let Some(sse) = parser.finish() {
        events.extend(decoder.decode(&sse).expect("decodable"));
    }
    events.extend(decoder.finish());
    assert!(decoder.finish().is_empty(), "finish() is idempotent");
    if let Err(violation) = validate_sequence(&events) {
        panic!("sequence contract violated: {violation}\n{events:#?}");
    }
    events
}

fn accumulate(events: &[StreamEvent]) -> Response {
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event);
    }
    assert!(acc.started() && acc.finished());
    acc.into_response()
}

fn start(id: &str, model: &str) -> StreamEvent {
    StreamEvent::Start {
        id: id.into(),
        model: model.into(),
        created: 0,
    }
}

fn text_delta(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        text: text.into(),
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

fn gemini_sig(data: &str) -> Signature {
    Signature::new(Protocol::Gemini, data)
}

// ---------------------------------------------------------------------------
// Text only
// ---------------------------------------------------------------------------

const TEXT_ONLY: &str = r#"data: {"candidates": [{"content": {"parts": [{"text": "Hello"}],"role": "model"},"index": 0}],"usageMetadata": {"promptTokenCount": 4,"totalTokenCount": 4,"promptTokensDetails": [{"modality": "TEXT","tokenCount": 4}]},"modelVersion": "gemini-2.5-flash","responseId": "mAitaLmk"}

data: {"candidates": [{"content": {"parts": [{"text": " there, how"}],"role": "model"},"index": 0}],"usageMetadata": {"promptTokenCount": 4,"totalTokenCount": 4},"modelVersion": "gemini-2.5-flash","responseId": "mAitaLmk"}

data: {"candidates": [{"content": {"parts": [{"text": " can I help?"}],"role": "model"},"finishReason": "STOP","index": 0}],"usageMetadata": {"promptTokenCount": 4,"candidatesTokenCount": 9,"totalTokenCount": 13},"modelVersion": "gemini-2.5-flash","responseId": "mAitaLmk"}

"#;

#[test]
fn text_only_stream() {
    let events = run(TEXT_ONLY);
    assert_eq!(
        events,
        vec![
            start("mAitaLmk", "gemini-2.5-flash"),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Hello"),
            StreamEvent::Usage(Usage {
                input_tokens: 4,
                ..Usage::default()
            }),
            // Consecutive text chunks stay in one block; identical usage is
            // not repeated.
            text_delta(0, " there, how"),
            text_delta(0, " can I help?"),
            StreamEvent::Usage(Usage {
                input_tokens: 4,
                output_tokens: 9,
                ..Usage::default()
            }),
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Stop),
        ]
    );
    let mut expected = Response::new("mAitaLmk", "gemini-2.5-flash");
    expected.parts = vec![Part::text("Hello there, how can I help?")];
    expected.usage = Usage {
        input_tokens: 4,
        output_tokens: 9,
        ..Usage::default()
    };
    assert_eq!(accumulate(&events), expected);
}

// ---------------------------------------------------------------------------
// Reasoning + text
// ---------------------------------------------------------------------------

const REASONING_AND_TEXT: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"**Weighing options**\n\nThe user wants","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":12,"totalTokenCount":12},"modelVersion":"gemini-3-pro-preview","responseId":"r-think"}

data: {"candidates":[{"content":{"parts":[{"text":" a short answer.","thought":true}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":12,"thoughtsTokenCount":40,"totalTokenCount":52},"modelVersion":"gemini-3-pro-preview","responseId":"r-think"}

data: {"candidates":[{"content":{"parts":[{"text":"The answer"}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":2,"thoughtsTokenCount":64,"totalTokenCount":78},"modelVersion":"gemini-3-pro-preview","responseId":"r-think"}

data: {"candidates":[{"content":{"parts":[{"text":" is 42."}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"thoughtsTokenCount":64,"totalTokenCount":81},"modelVersion":"gemini-3-pro-preview","responseId":"r-think"}

data: {"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"Q3VzdG9tU2lnbmF0dXJl"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":12,"cachedContentTokenCount":8,"candidatesTokenCount":5,"thoughtsTokenCount":64,"totalTokenCount":81},"modelVersion":"gemini-3-pro-preview","responseId":"r-think"}

"#;

#[test]
fn reasoning_then_text_with_trailing_signature() {
    let events = run(REASONING_AND_TEXT);
    let kinds: Vec<&StreamEvent> = events
        .iter()
        .filter(|e| !matches!(e, StreamEvent::Usage(_)))
        .collect();
    assert_eq!(
        kinds,
        vec![
            &start("r-think", "gemini-3-pro-preview"),
            &StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Reasoning {
                    id: None,
                    redacted: false
                }
            },
            &StreamEvent::ReasoningDelta {
                index: 0,
                text: "**Weighing options**\n\nThe user wants".into()
            },
            &StreamEvent::ReasoningDelta {
                index: 0,
                text: " a short answer.".into()
            },
            &StreamEvent::BlockStop { index: 0 },
            &StreamEvent::BlockStart {
                index: 1,
                block: BlockStart::Text
            },
            &text_delta(1, "The answer"),
            &text_delta(1, " is 42."),
            &StreamEvent::BlockStop { index: 1 },
            // The empty signed part that ends a Gemini 3 turn: reasoning
            // state without text.
            &StreamEvent::BlockStart {
                index: 2,
                block: BlockStart::Reasoning {
                    id: None,
                    redacted: false
                }
            },
            &StreamEvent::ReasoningSignature {
                index: 2,
                signature: gemini_sig("Q3VzdG9tU2lnbmF0dXJl")
            },
            &StreamEvent::BlockStop { index: 2 },
            &finish(FinishReason::Stop),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            Part::reasoning("**Weighing options**\n\nThe user wants a short answer."),
            Part::text("The answer is 42."),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(gemini_sig("Q3VzdG9tU2lnbmF0dXJl")),
                redacted: false,
            }),
        ]
    );
    // prompt 12 (8 of them cached), 5 visible + 64 thought tokens.
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 4,
            cache_read_tokens: 8,
            cache_write_tokens: 0,
            output_tokens: 69,
            reasoning_tokens: 64,
        }
    );
}

#[test]
fn thought_signature_on_a_thought_part_and_signature_only_parts() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"thinking","thought":true}]}}],"responseId":"r","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"thought":true,"thoughtSignature":"U0lH"}]}}]}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"more","thought":true,"thought_signature":"U0lHMg=="},{"text":""},{"text":"Answer","thoughtSignature":"bm90IGNhcnJpZWQ="}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        "\n\n",
    ));
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                // A later signature replaces the earlier one for the block.
                text: "thinkingmore".into(),
                signature: Some(gemini_sig("U0lHMg==")),
                redacted: false,
            }),
            Part::text("Answer"),
        ]
    );
    assert!(events.contains(&StreamEvent::ReasoningSignature {
        index: 0,
        signature: gemini_sig("U0lH")
    }));
}

// ---------------------------------------------------------------------------
// Text + tool calls
// ---------------------------------------------------------------------------

const TEXT_AND_TOOL_CALLS: &str = r#"data: {"candidates":[{"content":{"parts":[{"text":"I'll look up both cities."}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":50,"totalTokenCount":50},"modelVersion":"gemini-3-flash-preview","responseId":"r-tools"}

data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris","units":{"temp":"C","wind":["kmh","ms"]}},"id":"fc_001"},"thoughtSignature":"RXZ3QkNzQUI="}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":50,"totalTokenCount":50},"modelVersion":"gemini-3-flash-preview","responseId":"r-tools"}

data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Rome"},"id":"fc_002"}},{"functionCall":{"name":"get_time","id":"fc_003"}}],"role":"model"},"index":0}],"usageMetadata":{"promptTokenCount":50,"totalTokenCount":50},"modelVersion":"gemini-3-flash-preview","responseId":"r-tools"}

data: {"candidates":[{"content":{"parts":[{"text":""}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":31,"thoughtsTokenCount":20,"totalTokenCount":101},"modelVersion":"gemini-3-flash-preview","responseId":"r-tools"}

"#;

#[test]
fn text_then_multiple_tool_calls() {
    let events = run(TEXT_AND_TOOL_CALLS);
    let tool_block =
        |index: u32, id: &str, name: &str, signature: Option<Signature>, args: &str| {
            vec![
                StreamEvent::BlockStart {
                    index,
                    block: BlockStart::ToolCall {
                        id: id.into(),
                        name: name.into(),
                        kind: ToolCallKind::Function,
                        signature,
                    },
                },
                // Gemini never fragments arguments: one delta holds them all.
                StreamEvent::ToolArgsDelta {
                    index,
                    fragment: args.into(),
                },
                StreamEvent::BlockStop { index },
            ]
        };
    let mut expected = vec![
        start("r-tools", "gemini-3-flash-preview"),
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        },
        text_delta(0, "I'll look up both cities."),
        StreamEvent::Usage(Usage {
            input_tokens: 50,
            ..Usage::default()
        }),
        StreamEvent::BlockStop { index: 0 },
    ];
    expected.extend(tool_block(
        1,
        "fc_001",
        "get_weather",
        Some(gemini_sig("RXZ3QkNzQUI=")),
        r#"{"city":"Paris","units":{"temp":"C","wind":["kmh","ms"]}}"#,
    ));
    expected.extend(tool_block(
        2,
        "fc_002",
        "get_weather",
        None,
        r#"{"city":"Rome"}"#,
    ));
    expected.extend(tool_block(3, "fc_003", "get_time", None, "{}"));
    expected.push(StreamEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 51,
        reasoning_tokens: 20,
        ..Usage::default()
    }));
    // STOP after function calls is ToolCalls.
    expected.push(finish(FinishReason::ToolCalls));
    assert_eq!(events, expected);

    let response = accumulate(&events);
    assert_eq!(response.finish, FinishReason::ToolCalls);
    let calls: Vec<_> = response
        .tool_calls()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.as_str()))
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                "fc_001",
                "get_weather",
                r#"{"city":"Paris","units":{"temp":"C","wind":["kmh","ms"]}}"#
            ),
            ("fc_002", "get_weather", r#"{"city":"Rome"}"#),
            ("fc_003", "get_time", "{}"),
        ]
    );
    assert_eq!(response.text(), "I'll look up both cities.");
}

#[test]
fn tool_calls_without_ids_get_minted_ids() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{}}},{"functionCall":{"name":"a","args":{}}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2},"responseId":"r"}"#,
        "\n\n",
    ));
    let response = accumulate(&events);
    let ids: Vec<_> = response.tool_calls().map(|c| c.id.clone()).collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.iter()
            .all(|id| id.starts_with("call_") && id.len() == 29)
    );
    assert_ne!(ids[0], ids[1]);
}

// ---------------------------------------------------------------------------
// Usage placement
// ---------------------------------------------------------------------------

#[test]
fn usage_only_on_the_last_chunk() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Hi"}],"role":"model"}}],"responseId":"u1","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"!"}],"role":"model"},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"responseId":"u1","modelVersion":"m"}"#,
        "\n\n",
    ));
    assert_eq!(
        events,
        vec![
            start("u1", "m"),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Hi"),
            text_delta(0, "!"),
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            }),
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Length),
        ]
    );
}

#[test]
fn usage_in_a_chunk_of_its_own_after_the_finish_reason() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Done"}],"role":"model"},"finishReason":"STOP"}],"responseId":"u2","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1,"totalTokenCount":8},"responseId":"u2","modelVersion":"m"}"#,
        "\n\n",
    ));
    assert_eq!(
        events,
        vec![
            start("u2", "m"),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Done"),
            // The finish waits for the usage that may still follow.
            StreamEvent::Usage(Usage {
                input_tokens: 7,
                output_tokens: 1,
                ..Usage::default()
            }),
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn no_usage_at_all_finishes_when_the_stream_ends() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Done"}],"role":"model"},"finishReason":"STOP"}],"responseId":"u3","modelVersion":"m"}"#,
        "\n\n",
    ));
    assert_eq!(
        events,
        vec![
            start("u3", "m"),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Done"),
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Stop),
        ]
    );
    assert!(accumulate(&events).usage.is_empty());
}

#[test]
fn premature_finish_reason_with_placeholder_usage_does_not_end_the_stream() {
    // Shape produced by gateways that translate other vendors into Gemini:
    // every function call chunk claims STOP and carries a usage stub.
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{},"id":"t1"}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"responseId":"u4","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"b","args":{},"id":"t2"}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"responseId":"u4","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":10,"totalTokenCount":30,"trafficType":"PROVISIONED_THROUGHPUT"},"responseId":"u4","modelVersion":"m"}"#,
        "\n\n",
    ));
    let response = accumulate(&events);
    assert_eq!(
        response
            .tool_calls()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["t1", "t2"]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 10,
            ..Usage::default()
        }
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Finish { .. }))
            .count(),
        1
    );
}

#[test]
fn finish_reason_on_every_chunk_does_not_cut_the_answer_short() {
    // Old Gemini models (and several imitations) say STOP on every chunk and
    // report usage each time.
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"One, "}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}"#,
        "

",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"two, "}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7}}"#,
        "

",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"three."}],"role":"model"},"finishReason":"MAX_TOKENS","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":6,"totalTokenCount":9}}"#,
        "

",
    ));
    let response = accumulate(&events);
    assert_eq!(response.text(), "One, two, three.");
    // The last reason is the one that counts.
    assert_eq!(response.finish, FinishReason::Length);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 3,
            output_tokens: 6,
            ..Usage::default()
        }
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Finish { .. }))
            .count(),
        1
    );
}

#[test]
fn finish_is_only_emitted_when_the_stream_ends() {
    let mut parser = SseParser::new();
    let mut decoder = GeminiCodec.stream_decoder();
    let mut events = Vec::new();
    for sse in parser.push(TEXT_ONLY.as_bytes()).unwrap() {
        events.extend(decoder.decode(&sse).unwrap());
    }
    // Gemini has no terminator: until the connection closes more may come.
    assert!(!events.iter().any(|e| matches!(
        e,
        StreamEvent::Finish { .. } | StreamEvent::BlockStop { .. }
    )));
    assert_eq!(
        decoder.finish(),
        vec![
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Stop)
        ]
    );
    // Whatever arrives afterwards is ignored.
    let late = SseEvent::data(r#"{"candidates":[{"content":{"parts":[{"text":"late"}]}}]}"#);
    assert!(decoder.decode(&late).unwrap().is_empty());
    assert!(decoder.finish().is_empty());
}

// ---------------------------------------------------------------------------
// Errors, truncation, noise
// ---------------------------------------------------------------------------

#[test]
fn error_payload_in_the_middle_of_a_stream() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Partial ans"}],"role":"model"},"index":0}],"responseId":"e1","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"error":{"code":503,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"ignored after the error"}]},"finishReason":"STOP"}]}"#,
        "\n\n",
    ));
    assert_eq!(events.len(), 5);
    assert_eq!(events[3], StreamEvent::BlockStop { index: 0 });
    let StreamEvent::Error(error) = &events[4] else {
        panic!("{events:#?}")
    };
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(error.status, 503);
    assert_eq!(
        error.message,
        "The model is overloaded. Please try again later."
    );
    assert_eq!(error.code.as_deref(), Some("UNAVAILABLE"));
    let response = accumulate(&events);
    assert_eq!(response.finish, FinishReason::Error);
    assert_eq!(response.text(), "Partial ans");
}

#[test]
fn error_before_anything_else_and_named_error_events() {
    let events = run(concat!(
        r#"data: [{"error":{"code":429,"message":"Quota exceeded. Please retry in 7s.","status":"RESOURCE_EXHAUSTED"}}]"#,
        "\n\n",
    ));
    let [StreamEvent::Start { .. }, StreamEvent::Error(error)] = events.as_slice() else {
        panic!("{events:#?}")
    };
    assert_eq!(error.kind, ErrorKind::RateLimit);
    assert_eq!(error.retry_after_secs, Some(7));

    // Some gateways send `event: error` with an OpenAI-shaped body.
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"x"}]}}],"responseId":"e2"}"#,
        "\n\n",
        "event: error\n",
        r#"data: {"error":{"message":"upstream connection reset","type":"server_error"}}"#,
        "\n\n",
    ));
    let Some(StreamEvent::Error(error)) = events.last() else {
        panic!("{events:#?}")
    };
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert_eq!(error.message, "upstream connection reset");
}

#[test]
fn truncated_stream_is_closed_with_an_error_finish() {
    // The connection dropped after two chunks: no finishReason was seen.
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"thinking…","thought":true}],"role":"model"}}],"usageMetadata":{"promptTokenCount":5,"totalTokenCount":5},"responseId":"t1","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"The ans"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"responseId":"t1","modelVersion":"m"}"#,
    ));
    assert_eq!(
        &events[events.len() - 2..],
        &[
            StreamEvent::BlockStop { index: 1 },
            finish(FinishReason::Error)
        ]
    );
    let response = accumulate(&events);
    assert_eq!(response.finish, FinishReason::Error);
    assert_eq!(response.text(), "The ans");
    assert_eq!(response.usage.output_tokens, 2);
}

#[test]
fn empty_stream_still_yields_a_valid_sequence() {
    let events = run("");
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], StreamEvent::Start { id, .. } if !id.is_empty()));
    assert_eq!(events[1], finish(FinishReason::Error));
    let only_noise = run(": keep-alive\n\ndata: [DONE]\n\n");
    assert_eq!(only_noise.len(), 2);
}

#[test]
fn unknown_events_are_skipped() {
    let noisy = format!(
        ": keep-alive\n\nevent: ping\ndata: {{}}\n\ndata: not json at all\n\ndata: {{\"ping\": true}}\n\ndata: 42\n\ndata:\n\n{}data: {{\"serverInfo\":{{\"x\":1}}}}\n\ndata: [DONE]\n\n",
        TEXT_ONLY
    );
    assert_eq!(run(&noisy), run(TEXT_ONLY));
}

#[test]
fn data_prefix_left_in_the_payload_is_tolerated() {
    let mut decoder = GeminiCodec.stream_decoder();
    let events = decoder
        .decode(&SseEvent::data(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"x"}]}}],"responseId":"p","modelVersion":"m"}"#,
        ))
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], start("p", "m"));
}

// ---------------------------------------------------------------------------
// Other shapes
// ---------------------------------------------------------------------------

#[test]
fn blocked_prompt_stream() {
    let events = run(concat!(
        r#"data: {"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":9,"totalTokenCount":9},"modelVersion":"gemini-2.5-flash","responseId":"b1"}"#,
        "\n\n",
    ));
    assert_eq!(
        events,
        vec![
            start("b1", "gemini-2.5-flash"),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Refusal
            },
            text_delta(0, "The prompt was blocked by Gemini (SAFETY)."),
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Usage(Usage {
                input_tokens: 9,
                ..Usage::default()
            }),
            finish(FinishReason::ContentFilter),
        ]
    );
    assert_eq!(
        accumulate(&events).parts,
        vec![Part::Refusal(RefusalPart {
            text: "The prompt was blocked by Gemini (SAFETY).".into()
        })]
    );
}

#[test]
fn safety_stop_in_the_middle_of_an_answer() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Here is how"}],"role":"model"}}],"responseId":"s1","modelVersion":"m"}"#,
        "\n\n",
        r#"data: {"candidates":[{"finishReason":"SAFETY","index":0}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":3,"totalTokenCount":7},"responseId":"s1","modelVersion":"m"}"#,
        "\n\n",
    ));
    assert_eq!(events.last(), Some(&finish(FinishReason::ContentFilter)));
}

#[test]
fn grounded_stream_emits_citations_and_keeps_the_metadata() {
    let grounding = json!({
        "groundingChunks": [{"web": {"uri": "https://example.com/a", "title": "A"}}],
        "groundingSupports": [{"segment": {"startIndex": 0, "endIndex": 10, "text": "Rust 1.95 "}, "groundingChunkIndices": [0]}],
        "webSearchQueries": ["latest rust"]
    });
    let last = json!({
        "candidates": [{
            "content": {"parts": [{"text": "is out."}], "role": "model"},
            "finishReason": "STOP",
            "groundingMetadata": grounding
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 6, "totalTokenCount": 11},
        "responseId": "g1", "modelVersion": "m"
    });
    let transcript = format!(
        "data: {}\n\ndata: {last}\n\n",
        r#"{"candidates":[{"content":{"parts":[{"text":"Rust 1.95 "}],"role":"model"}}],"responseId":"g1","modelVersion":"m"}"#
    );
    let events = run(&transcript);
    let response = accumulate(&events);
    let Part::Text(text) = &response.parts[0] else {
        panic!("{:?}", response.parts)
    };
    assert_eq!(text.text, "Rust 1.95 is out.");
    assert_eq!(
        text.citations,
        vec![Citation {
            url: Some("https://example.com/a".into()),
            title: Some("A".into()),
            cited_text: Some("Rust 1.95 ".into()),
            start: Some(0),
            end: Some(10),
        }]
    );
    assert_eq!(
        response.parts[1],
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw: json!({"groundingMetadata": grounding})
        })
    );
}

#[test]
fn generated_image_arrives_as_a_whole_part() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Here you go:"}],"role":"model"}}],"responseId":"i1","modelVersion":"gemini-2.5-flash-image"}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1300,"totalTokenCount":1304},"responseId":"i1","modelVersion":"gemini-2.5-flash-image"}"#,
        "\n\n",
    ));
    assert_eq!(
        accumulate(&events).parts,
        vec![
            Part::text("Here you go:"),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
        ]
    );
}

#[test]
fn json_array_form_and_wrapped_chunks() {
    // Without `alt=sse` the whole stream is one JSON array.
    let array = json!([
        {"candidates": [{"content": {"parts": [{"text": "a"}], "role": "model"}}], "responseId": "arr", "modelVersion": "m"},
        {"candidates": [{"content": {"parts": [{"text": "b"}], "role": "model"}, "finishReason": "STOP"}],
         "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2, "totalTokenCount": 3}}
    ]);
    let response = accumulate(&run(&format!("data: {array}\n\n")));
    assert_eq!(response.text(), "ab");
    assert_eq!(response.id, "arr");

    let wrapped = json!({"response": {
        "candidates": [{"content": {"parts": [{"text": "w"}], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
        "responseId": "wrapped", "modelVersion": "m"
    }});
    let response = accumulate(&run(&format!("data: {wrapped}\n\n")));
    assert_eq!(
        (response.id.as_str(), response.text().as_str()),
        ("wrapped", "w")
    );
}

#[test]
fn missing_response_id_is_minted_once() {
    let events = run(concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#,
        "\n\n",
        r#"data: {"candidates":[{"content":{"parts":[{"text":"y"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        "\n\n",
    ));
    let StreamEvent::Start { id, model, .. } = &events[0] else {
        panic!()
    };
    assert_eq!(id.len(), 24);
    assert_eq!(model, "");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Start { .. }))
            .count(),
        1
    );
}
