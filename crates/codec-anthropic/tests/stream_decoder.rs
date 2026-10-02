//! Upstream side: vendor SSE transcripts → canonical stream events. Every
//! sequence is checked against the contract by `common::decode_stream`.

mod common;

use common::{accumulate, decode_stream, sse};
use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    Citation, FinishReason, OpaquePart, Part, Reasoning, Response, Signature, TextPart,
    ToolCallKind,
};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::{Codec, ErrorKind, Protocol, SseEvent, Usage};

fn start(id: &str, model: &str) -> StreamEvent {
    StreamEvent::Start {
        id: id.into(),
        model: model.into(),
        created: 0,
    }
}

fn usage(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Usage(Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    })
}

fn text_delta(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        text: text.into(),
    }
}

fn args_delta(index: u32, fragment: &str) -> StreamEvent {
    StreamEvent::ToolArgsDelta {
        index,
        fragment: fragment.into(),
    }
}

fn finish(reason: FinishReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        stop_sequence: None,
    }
}

const TEXT_ONLY: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5-20250929","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}

"#;

#[test]
fn stream_decoder_text_only() {
    let events = decode_stream(TEXT_ONLY);
    assert_eq!(
        events,
        vec![
            start(
                "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY",
                "claude-sonnet-4-5-20250929"
            ),
            usage(25, 1),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Hello"),
            text_delta(0, "!"),
            StreamEvent::BlockStop { index: 0 },
            usage(25, 15),
            finish(FinishReason::Stop),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(
        response,
        Response {
            id: "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY".into(),
            model: "claude-sonnet-4-5-20250929".into(),
            created: 0,
            parts: vec![Part::text("Hello!")],
            finish: FinishReason::Stop,
            stop_sequence: None,
            usage: Usage {
                input_tokens: 25,
                output_tokens: 15,
                ..Usage::default()
            },
            service_tier: None,
        }
    );
}

const THINKING_THEN_TEXT: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Thinking","type":"message","role":"assistant","content":[],"model":"claude-opus-4-5-20251101","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":40,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":3}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me solve this step by step:\n\n1. First break down 27 * 453"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"\n2. 453 = 400 + 50 + 3"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"27 * 453 = 12,231"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":210,"output_tokens_details":{"thinking_tokens":180}}}

event: message_stop
data: {"type":"message_stop"}

"#;

#[test]
fn stream_decoder_reasoning_then_text() {
    let events = decode_stream(THINKING_THEN_TEXT);
    let signature = Signature::new(
        Protocol::Anthropic,
        "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds",
    );
    assert_eq!(
        events[2..7],
        [
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Reasoning {
                    id: None,
                    redacted: false
                }
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "Let me solve this step by step:\n\n1. First break down 27 * 453".into()
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "\n2. 453 = 400 + 50 + 3".into()
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: signature.clone()
            },
            StreamEvent::BlockStop { index: 0 },
        ]
    );
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Let me solve this step by step:\n\n1. First break down 27 * 453\n2. 453 = 400 + 50 + 3"
                    .into(),
                signature: Some(signature),
                redacted: false,
            }),
            Part::text("27 * 453 = 12,231"),
        ]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 40,
            output_tokens: 210,
            reasoning_tokens: 180,
            ..Usage::default()
        }
    );
    assert_eq!(response.finish, FinishReason::Stop);
}

#[test]
fn stream_decoder_omitted_thinking_and_redacted_thinking() {
    // display: "omitted" — one empty thinking_delta, then the signature.
    let events = decode_stream(
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-opus-4-8","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"ErUBCkYIBxgCIkD"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"EmwKAhgBEgy3va3pzix"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Done."}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":90}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    assert_eq!(
        accumulate(&events).parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Anthropic, "ErUBCkYIBxgCIkD")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Anthropic, "EmwKAhgBEgy3va3pzix")),
                redacted: true,
            }),
            Part::text("Done."),
        ]
    );
}

const TEXT_AND_TOOLS: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_014p7gG3wDgGV9EUtLvnow3U","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","stop_sequence":null,"usage":{"input_tokens":472,"output_tokens":2},"content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Okay, let's check"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" both."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01T1x1fJ34qAmk2tNTrN7Up6","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" Francisco, CA\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_02","name":"get_time","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_03","name":"get_news","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"topic\": \"tech\", \"n\""}}

event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":": 3}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":3}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}

"#;

#[test]
fn stream_decoder_text_and_multiple_tool_calls_with_fragmented_arguments() {
    let events = decode_stream(TEXT_AND_TOOLS);
    let tool = |index: u32, id: &str, name: &str| StreamEvent::BlockStart {
        index,
        block: BlockStart::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolCallKind::Function,
            signature: None,
        },
    };
    assert_eq!(
        events,
        vec![
            start("msg_014p7gG3wDgGV9EUtLvnow3U", "claude-sonnet-4-5-20250929"),
            usage(472, 2),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text
            },
            text_delta(0, "Okay, let's check"),
            text_delta(0, " both."),
            StreamEvent::BlockStop { index: 0 },
            tool(1, "toolu_01T1x1fJ34qAmk2tNTrN7Up6", "get_weather"),
            args_delta(1, "{\"location\":"),
            args_delta(1, " \"San"),
            args_delta(1, " Francisco, CA\"}"),
            StreamEvent::BlockStop { index: 1 },
            tool(2, "toolu_02", "get_time"),
            StreamEvent::BlockStop { index: 2 },
            tool(3, "toolu_03", "get_news"),
            args_delta(3, "{\"topic\": \"tech\", \"n\""),
            args_delta(3, ": 3}"),
            StreamEvent::BlockStop { index: 3 },
            usage(472, 89),
            finish(FinishReason::ToolCalls),
        ]
    );
    let response = accumulate(&events);
    assert_eq!(
        response.parts,
        vec![
            Part::text("Okay, let's check both."),
            // Argument text is passed through byte for byte.
            Part::tool_call(
                "toolu_01T1x1fJ34qAmk2tNTrN7Up6",
                "get_weather",
                "{\"location\": \"San Francisco, CA\"}"
            ),
            Part::tool_call("toolu_02", "get_time", ""),
            Part::tool_call("toolu_03", "get_news", "{\"topic\": \"tech\", \"n\": 3}"),
        ]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
}

// ---------------------------------------------------------------------------
// Usage placement
// ---------------------------------------------------------------------------

fn text_stream(message_start_usage: &str, message_delta_usage: &str) -> String {
    format!(
        r#"event: message_start
data: {{"type":"message_start","message":{{"id":"msg_u","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null{message_start_usage}}}}}

event: content_block_start
data: {{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}}

event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"ok"}}}}

event: content_block_stop
data: {{"type":"content_block_stop","index":0}}

event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}}{message_delta_usage}}}

event: message_stop
data: {{"type":"message_stop"}}

"#
    )
}

#[test]
fn stream_decoder_usage_in_message_start_and_message_delta() {
    // Input side (with cache) at the start, cumulative output at the end,
    // which also repeats the input side.
    let events = decode_stream(&text_stream(
        r#","usage":{"input_tokens":13,"cache_creation_input_tokens":31,"cache_read_input_tokens":22000,"output_tokens":1}"#,
        r#","usage":{"input_tokens":13,"cache_creation_input_tokens":31,"cache_read_input_tokens":22000,"output_tokens":4,"server_tool_use":{"web_search_requests":1}}"#,
    ));
    let usages: Vec<&StreamEvent> = events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Usage(_)))
        .collect();
    assert_eq!(
        usages,
        vec![
            &StreamEvent::Usage(Usage {
                input_tokens: 13,
                cache_read_tokens: 22000,
                cache_write_tokens: 31,
                output_tokens: 1,
                reasoning_tokens: 0,
            }),
            &StreamEvent::Usage(Usage {
                input_tokens: 13,
                cache_read_tokens: 22000,
                cache_write_tokens: 31,
                output_tokens: 4,
                reasoning_tokens: 0,
            }),
        ]
    );
    // The buckets are disjoint: nothing is counted twice.
    let total = accumulate(&events).usage;
    assert_eq!(total.prompt_tokens(), 22044);
    assert_eq!(total.total_tokens(), 22048);
}

#[test]
fn stream_decoder_usage_only_in_message_delta() {
    // Gateways that translate from other vendors only know usage at the end.
    let events = decode_stream(&text_stream(
        r#","usage":{"input_tokens":0,"output_tokens":0}"#,
        r#","usage":{"input_tokens":50,"output_tokens":7,"cache_read_input_tokens":800}"#,
    ));
    assert!(
        matches!(events[1], StreamEvent::BlockStart { .. }),
        "no empty Usage event"
    );
    assert_eq!(
        accumulate(&events).usage,
        Usage {
            input_tokens: 50,
            cache_read_tokens: 800,
            output_tokens: 7,
            ..Usage::default()
        }
    );
}

#[test]
fn stream_decoder_usage_only_in_message_start_or_absent() {
    let events = decode_stream(&text_stream(
        r#","usage":{"input_tokens":9,"output_tokens":2}"#,
        "",
    ));
    assert_eq!(
        accumulate(&events).usage,
        Usage {
            input_tokens: 9,
            output_tokens: 2,
            ..Usage::default()
        }
    );
    let events = decode_stream(&text_stream("", ""));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Usage(_)))
    );
    let response = accumulate(&events);
    assert!(response.usage.is_empty());
    assert_eq!(response.text(), "ok");
    assert_eq!(response.finish, FinishReason::Stop);
}

#[test]
fn stream_decoder_several_message_deltas_keep_the_last_totals() {
    let wire = text_stream(r#","usage":{"input_tokens":5,"output_tokens":1}"#, r#","usage":{"output_tokens":3}"#)
        .replace(
            "event: message_stop",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\nevent: message_stop",
        );
    let events = decode_stream(&wire);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Finish { .. }))
            .count(),
        1
    );
    assert_eq!(accumulate(&events).usage.output_tokens, 9);
}

// ---------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------

#[test]
fn stream_decoder_stop_reasons_and_stop_sequence() {
    let finish_of = |delta: &str| {
        let wire = text_stream("", "")
            .replace(r#"{"stop_reason":"end_turn","stop_sequence":null}"#, delta);
        decode_stream(&wire).pop().unwrap()
    };
    assert_eq!(
        finish_of(r#"{"stop_reason":"stop_sequence","stop_sequence":"END"}"#),
        StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: Some("END".into())
        }
    );
    assert_eq!(
        finish_of(r#"{"stop_reason":"max_tokens","stop_sequence":null}"#),
        finish(FinishReason::Length)
    );
    assert_eq!(
        finish_of(r#"{"stop_reason":"pause_turn","stop_sequence":null}"#),
        finish(FinishReason::PauseTurn)
    );
    assert_eq!(
        finish_of(r#"{"stop_reason":"refusal","stop_sequence":null}"#),
        finish(FinishReason::Refusal)
    );
    assert_eq!(
        finish_of(r#"{"stop_reason":"model_context_window_exceeded","stop_sequence":null}"#),
        finish(FinishReason::ContextWindow)
    );
    assert_eq!(
        finish_of(r#"{"stop_reason":null}"#),
        finish(FinishReason::Stop)
    );
}

// ---------------------------------------------------------------------------
// Errors and truncation
// ---------------------------------------------------------------------------

#[test]
fn stream_decoder_error_event_mid_stream() {
    let wire = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_e","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}}

event: error
data: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" ignored"}}

"#;
    let events = decode_stream(wire);
    // The open block is closed, the error is terminal, nothing follows it.
    assert_eq!(
        events[events.len() - 2],
        StreamEvent::BlockStop { index: 0 }
    );
    let StreamEvent::Error(error) = events.last().unwrap() else {
        panic!("expected a terminal error, got {:?}", events.last());
    };
    // 529 overloaded is the gateway's 503.
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(error.status, 503);
    assert_eq!(error.message, "Overloaded");
    assert_eq!(error.code.as_deref(), Some("overloaded_error"));
    let response = accumulate(&events);
    assert_eq!(response.text(), "Partial");
    assert_eq!(response.finish, FinishReason::Error);
}

#[test]
fn stream_decoder_error_types_map_to_kinds() {
    let error_of = |payload: &str| {
        let events = decode_stream(&format!("event: error\ndata: {payload}\n\n"));
        // An error before message_start still yields a valid sequence.
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        match events.last().unwrap() {
            StreamEvent::Error(error) => error.clone(),
            other => panic!("expected error, got {other:?}"),
        }
    };
    let rate = error_of(
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit. Please try again in 12s."}}"#,
    );
    assert_eq!((rate.kind, rate.status), (ErrorKind::RateLimit, 429));
    assert_eq!(rate.retry_after_secs, Some(12));
    let invalid =
        error_of(r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#);
    assert_eq!(
        (invalid.kind, invalid.status),
        (ErrorKind::InvalidRequest, 400)
    );
    let api = error_of(
        r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#,
    );
    assert_eq!((api.kind, api.status), (ErrorKind::Upstream, 502));
    // The upstream's credential problem is not the client's.
    let auth = error_of(
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    );
    assert_eq!((auth.kind, auth.status), (ErrorKind::Upstream, 502));
    let timeout = error_of(r#"{"type":"error","error":{"type":"timeout_error","message":"slow"}}"#);
    assert_eq!(timeout.kind, ErrorKind::Timeout);
    // An OpenAI-shaped error from a compatible gateway, sent without a type.
    let foreign = error_of(
        r#"{"error":{"message":"upstream exploded","type":"server_error","code":"boom"}}"#,
    );
    assert_eq!(foreign.kind, ErrorKind::Upstream);
    assert_eq!(foreign.message, "upstream exploded");
}

#[test]
fn stream_decoder_truncated_stream_without_terminal_event() {
    let wire = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_t","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_t","name":"write_file","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"/tmp/x\", \"content\": \"half"}}

"#;
    let events = decode_stream(wire);
    assert_eq!(
        events[events.len() - 2..],
        [
            StreamEvent::BlockStop { index: 0 },
            finish(FinishReason::Error)
        ]
    );
    let response = accumulate(&events);
    assert_eq!(response.finish, FinishReason::Error);
    assert_eq!(
        response.parts,
        vec![Part::tool_call(
            "toolu_t",
            "write_file",
            "{\"path\": \"/tmp/x\", \"content\": \"half"
        )]
    );
}

#[test]
fn stream_decoder_missing_message_stop_after_message_delta_is_complete() {
    let wire = text_stream(
        r#","usage":{"input_tokens":5,"output_tokens":1}"#,
        r#","usage":{"output_tokens":3}"#,
    )
    .replace(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "",
    );
    let events = decode_stream(&wire);
    assert_eq!(events.last().unwrap(), &finish(FinishReason::Stop));
}

#[test]
fn stream_decoder_empty_stream() {
    let events = decode_stream("");
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], StreamEvent::Start { id, .. } if id.starts_with("msg_")));
    assert_eq!(events[1], finish(FinishReason::Error));
}

#[test]
fn stream_decoder_ignores_everything_after_the_terminal_event() {
    let mut decoder = AnthropicCodec.stream_decoder();
    for event in sse(TEXT_ONLY) {
        decoder.decode(&event).unwrap();
    }
    for late in sse(TEXT_ONLY) {
        assert!(decoder.decode(&late).unwrap().is_empty());
    }
    assert!(decoder.finish().is_empty());
}

// ---------------------------------------------------------------------------
// Unknown events and blocks
// ---------------------------------------------------------------------------

#[test]
fn stream_decoder_unknown_events_and_server_tool_blocks() {
    let wire = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_ws","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":1}}}

event: brand_new_event
data: {"type":"brand_new_event","payload":{"x":1}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_014hJH82Qum7Td6UV8gDXThB","name":"web_search","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"weather"}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" NYC today\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_014hJH82Qum7Td6UV8gDXThB","content":[{"type":"web_search_result","title":"Weather in New York City","url":"https://weather.example/nyc","encrypted_content":"EqgfCioIARgBIiQ3YTAwMjY1Mi1m","page_age":null}]}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"hologram","frames":3}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"hologram_delta","frame":"AAAA"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":"","citations":[]}}

event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","cited_text":"72°F and sunny","url":"https://weather.example/nyc","title":"Weather in New York City","encrypted_index":"Eo8BCioIAhgBIiQyYjQ0OWJm"}}}

event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"It is 72°F and sunny."}}

event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"future_delta","value":1}}

event: content_block_stop
data: {"type":"content_block_stop","index":3}

: keep-alive comment

data: this is not json

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2500,"output_tokens":60,"server_tool_use":{"web_search_requests":1}}}

event: message_stop
data: {"type":"message_stop"}

"#;
    let events = decode_stream(wire);
    let response = accumulate(&events);
    let opaque = |raw: serde_json::Value| {
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw,
        })
    };
    assert_eq!(
        response.parts,
        vec![
            // The streamed input is folded back into the block.
            opaque(
                json!({"type": "server_tool_use", "id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB",
                          "name": "web_search", "input": {"query": "weather NYC today"}})
            ),
            opaque(json!({"type": "web_search_tool_result",
                          "tool_use_id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB",
                          "content": [{"type": "web_search_result", "title": "Weather in New York City",
                                       "url": "https://weather.example/nyc",
                                       "encrypted_content": "EqgfCioIARgBIiQ3YTAwMjY1Mi1m",
                                       "page_age": null}]})),
            opaque(json!({"type": "hologram", "frames": 3})),
            Part::Text(TextPart {
                text: "It is 72°F and sunny.".into(),
                cache_control: None,
                citations: vec![Citation {
                    url: Some("https://weather.example/nyc".into()),
                    title: Some("Weather in New York City".into()),
                    cited_text: Some("72°F and sunny".into()),
                    start: None,
                    end: None,
                }],
                signature: None,
            }),
        ]
    );
    // Opaque blocks are whole blocks: start immediately followed by stop.
    assert!(matches!(
        &events[2..4],
        [
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Whole { .. }
            },
            StreamEvent::BlockStop { index: 0 }
        ]
    ));
    // Server tool use is not a pending client tool call.
    assert_eq!(response.finish, FinishReason::Stop);
    assert_eq!(response.usage.input_tokens, 2500);
}

// ---------------------------------------------------------------------------
// Sloppy "compatible" servers
// ---------------------------------------------------------------------------

#[test]
fn stream_decoder_tool_input_delivered_whole_in_block_start() {
    let wire = r#"data: {"type":"message_start","message":{"id":"","type":"message","role":"assistant","content":[],"model":"glm-4.6"}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"lookup","input":{"q":"rust"}}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":3,"output_tokens":9}}

data: {"type":"message_stop"}

"#;
    let events = decode_stream(wire);
    let response = accumulate(&events);
    // Empty ids are minted in the vendor's shape.
    assert!(response.id.starts_with("msg_") && response.id.len() > 4);
    let call = response.tool_calls().next().unwrap();
    assert!(call.id.starts_with("toolu_") && call.id.len() > 6);
    assert_eq!(call.name, "lookup");
    assert_eq!(call.arguments, r#"{"q":"rust"}"#);
    assert_eq!(response.finish, FinishReason::ToolCalls);
}

#[test]
fn stream_decoder_without_message_start_or_block_boundaries() {
    // Deltas only: the decoder supplies Start and the block boundaries.
    let wire = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}

data: {"type":"message_stop"}

data: [DONE]

"#;
    let events = decode_stream(wire);
    assert!(
        matches!(&events[0], StreamEvent::Start { id, model, .. } if id.starts_with("msg_") && model.is_empty())
    );
    assert_eq!(
        accumulate(&events).parts,
        vec![
            Part::Reasoning(Reasoning {
                text: "hmm".into(),
                ..Reasoning::default()
            }),
            Part::text("Hi there"),
        ]
    );
}

#[test]
fn stream_decoder_missing_content_block_stop_between_blocks() {
    let wire = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_m","type":"message","role":"assistant","content":[],"model":"m"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hello"}}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"f","input":{}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}

event: message_stop
data: {"type":"message_stop"}

"#;
    let events = decode_stream(wire);
    assert_eq!(
        accumulate(&events).parts,
        vec![Part::text("Hello"), Part::tool_call("toolu_1", "f", "")]
    );
}

#[test]
fn stream_decoder_event_name_is_used_when_the_payload_has_no_type() {
    let events = common::decode_events(&[
        SseEvent::named("message_start", r#"{"message":{"id":"msg_n","model":"m"}}"#),
        SseEvent::named(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        SseEvent::named(
            "content_block_delta",
            r#"{"index":0,"delta":{"text":"typeless"}}"#,
        ),
        SseEvent::named("content_block_stop", r#"{"index":0}"#),
        SseEvent::named("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#),
        SseEvent::named("message_stop", "{}"),
    ]);
    let response = accumulate(&events);
    assert_eq!(response.id, "msg_n");
    assert_eq!(response.text(), "typeless");
    assert_eq!(response.finish, FinishReason::Stop);
}
