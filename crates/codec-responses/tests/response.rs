//! `decode_response` / `encode_response`: complete response objects.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::{ClientCtx, Codec, CodecError, Protocol, Usage};

const P: Protocol = Protocol::OpenaiResponses;

fn decode(body: Value) -> Response {
    ResponsesCodec
        .decode_response(&body)
        .expect("response decodes")
}

fn encode(response: &Response) -> Value {
    ResponsesCodec
        .encode_response(response, &ClientCtx::new("gpt-5"))
        .expect("response encodes")
}

fn encode_with(response: &Response, request: Value) -> Value {
    let ctx = ClientCtx::new("gpt-5").with_request(Arc::new(request));
    ResponsesCodec
        .encode_response(response, &ctx)
        .expect("response encodes")
}

fn response(parts: Vec<Part>, finish: FinishReason) -> Response {
    Response {
        id: "resp_abc".into(),
        model: "gpt-5-2025-08-07".into(),
        created: 1_741_476_542,
        parts,
        finish,
        stop_sequence: None,
        usage: Usage {
            input_tokens: 30,
            cache_read_tokens: 6,
            cache_write_tokens: 0,
            output_tokens: 87,
            reasoning_tokens: 64,
        },
        service_tier: None,
    }
}

fn text_message(id: &str, status: &str, text: &str) -> Value {
    json!({
        "id": id, "type": "message", "status": status, "role": "assistant",
        "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": text}]
    })
}

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

#[test]
fn decode_text_response() {
    let decoded = decode(json!({
        "id": "resp_67ccd2bed1ec8190b14f964abc0542670bb6a6b452d3795b",
        "object": "response",
        "created_at": 1741476542,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-4.1-2025-04-14",
        "output": [{
            "type": "message",
            "id": "msg_67ccd2bf17f0819081ff3bb2cf6508e60bb6a6b452d3795b",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "In a peaceful grove beneath a silver moon.", "annotations": []}]
        }],
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": null, "summary": null},
        "service_tier": "default",
        "store": true,
        "temperature": 1.0,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": 1.0,
        "truncation": "disabled",
        "usage": {
            "input_tokens": 36,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 87,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 123
        },
        "user": null,
        "metadata": {}
    }));
    assert_eq!(
        decoded,
        Response {
            id: "resp_67ccd2bed1ec8190b14f964abc0542670bb6a6b452d3795b".into(),
            model: "gpt-4.1-2025-04-14".into(),
            created: 1_741_476_542,
            parts: vec![Part::text("In a peaceful grove beneath a silver moon.")],
            finish: FinishReason::Stop,
            stop_sequence: None,
            usage: Usage {
                input_tokens: 36,
                output_tokens: 87,
                ..Usage::default()
            },
            service_tier: Some("default".into()),
        }
    );
}

#[test]
fn decode_parallel_tool_calls() {
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "completed", "model": "gpt-5",
        "output": [
            {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
             "content": [{"type": "output_text", "text": "Checking both.", "annotations": []}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_a", "name": "get_weather",
             "arguments": "{\"city\":\"Paris\"}", "status": "completed"},
            {"type": "function_call", "id": "fc_2", "call_id": "call_b", "name": "get_weather",
             "arguments": "{\"city\":\"Rome\"}", "status": "completed"},
            {"type": "custom_tool_call", "id": "ctc_3", "call_id": "call_c", "name": "exec",
             "input": "ls -la", "status": "completed"}
        ]
    }));
    assert_eq!(
        decoded.parts,
        vec![
            Part::text("Checking both."),
            Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
            Part::tool_call("call_b", "get_weather", "{\"city\":\"Rome\"}"),
            Part::ToolCall(ToolCall {
                id: "call_c".into(),
                name: "exec".into(),
                arguments: "ls -la".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            }),
        ]
    );
    // `completed` with call items is the canonical "tool calls" outcome.
    assert_eq!(decoded.finish, FinishReason::ToolCalls);
}

#[test]
fn decode_reasoning_with_signature_tags_the_blob_with_this_protocol() {
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "completed", "model": "gpt-5",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [
                {"type": "summary_text", "text": "First thought."},
                {"type": "summary_text", "text": "Second thought."}
            ], "encrypted_content": "gAAAAABencrypted"},
            {"type": "reasoning", "id": "rs_2", "summary": []},
            {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
             "content": [{"type": "output_text", "text": "Answer.", "annotations": []}]}
        ]
    }));
    assert_eq!(
        decoded.parts,
        vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "First thought.\n\nSecond thought.".into(),
                signature: Some(Signature::new(P, "gAAAAABencrypted")),
                redacted: false,
            }),
            // The empty reasoning item carries nothing and is skipped.
            Part::text("Answer."),
        ]
    );
    assert_eq!(
        decoded.reasoning_text(),
        "First thought.\n\nSecond thought."
    );
}

#[test]
fn decode_refusal() {
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": "completed", "model": "gpt-5",
        "output": [{"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                    "content": [{"type": "refusal", "refusal": "I can't help with that."}]}]
    }));
    assert_eq!(
        decoded.parts,
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })]
    );
    assert_eq!(decoded.finish, FinishReason::Refusal);
}

#[test]
fn decode_each_finish_reason() {
    let finish = |extra: Value| {
        let mut body = json!({
            "id": "resp_1", "object": "response", "created_at": 1, "model": "gpt-5",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "partial"}]}]
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        decode(body).finish
    };
    assert_eq!(finish(json!({"status": "completed"})), FinishReason::Stop);
    // Compatible servers that omit `status` mean "completed".
    assert_eq!(finish(json!({})), FinishReason::Stop);
    assert_eq!(
        finish(
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}})
        ),
        FinishReason::Length
    );
    assert_eq!(
        finish(json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}})),
        FinishReason::ContentFilter
    );
    assert_eq!(
        finish(json!({"status": "incomplete", "incomplete_details": null})),
        FinishReason::Length
    );
    assert_eq!(
        finish(json!({"status": "incomplete", "incomplete_details": {"reason": "pause_turn"}})),
        FinishReason::PauseTurn
    );
    assert_eq!(
        finish(
            json!({"status": "incomplete", "incomplete_details": {"reason": "model_context_window_exceeded"}})
        ),
        FinishReason::ContextWindow
    );
    assert_eq!(
        finish(json!({"status": "incomplete", "incomplete_details": {"reason": "max_messages"}})),
        FinishReason::Other("max_messages".into())
    );
    assert_eq!(
        finish(json!({"status": "failed", "error": {"code": "server_error", "message": "boom"}})),
        FinishReason::Error
    );
    assert_eq!(
        finish(json!({"status": "cancelled"})),
        FinishReason::Other("cancelled".into())
    );
}

#[test]
fn decode_usage_converts_inclusive_totals_to_disjoint_buckets() {
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5", "output": [],
        "usage": {
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 100},
            "output_tokens": 500,
            "output_tokens_details": {"reasoning_tokens": 200},
            "total_tokens": 1500
        }
    }));
    // input_tokens includes cache reads and writes; output_tokens includes
    // reasoning. The IR buckets are disjoint.
    assert_eq!(
        decoded.usage,
        Usage {
            input_tokens: 300,
            cache_read_tokens: 600,
            cache_write_tokens: 100,
            output_tokens: 500,
            reasoning_tokens: 200,
        }
    );
    assert_eq!(decoded.usage.prompt_tokens(), 1000);
    assert_eq!(decoded.usage.total_tokens(), 1500);

    // Token counts serialised as floats and Chat-style key names (seen on
    // "compatible" servers) are understood.
    let lenient = decode(json!({
        "id": "resp_1", "status": "completed", "output": [],
        "usage": {"prompt_tokens": 12.0, "completion_tokens": 7.0,
                  "prompt_tokens_details": {"cached_tokens": 2},
                  "completion_tokens_details": {"reasoning_tokens": 3}}
    }));
    assert_eq!(
        lenient.usage,
        Usage {
            input_tokens: 10,
            cache_read_tokens: 2,
            cache_write_tokens: 0,
            output_tokens: 7,
            reasoning_tokens: 3,
        }
    );

    let none = decode(json!({"id": "resp_1", "status": "completed", "output": [], "usage": null}));
    assert_eq!(none.usage, Usage::default());
}

#[test]
fn decode_non_modelled_items_and_citations() {
    let search = json!({"type": "web_search_call", "id": "ws_1", "status": "completed",
                        "action": {"type": "search", "query": "rust 2024"}});
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5",
        "output": [
            search,
            {"type": "message", "role": "assistant", "content": [{
                "type": "output_text", "text": "Rust 2024 shipped.",
                "annotations": [{"type": "url_citation", "url": "https://blog.rust-lang.org", "title": "Rust Blog", "start_index": 0, "end_index": 9}]
            }]},
            {"type": "image_generation_call", "id": "ig_1", "status": "completed", "output_format": "webp", "result": "UklGRg=="}
        ]
    }));
    assert_eq!(
        decoded.parts,
        vec![
            Part::Opaque(OpaquePart {
                origin: P,
                raw: search.clone()
            }),
            Part::Text(TextPart {
                text: "Rust 2024 shipped.".into(),
                citations: vec![Citation {
                    url: Some("https://blog.rust-lang.org".into()),
                    title: Some("Rust Blog".into()),
                    cited_text: None,
                    start: Some(0),
                    end: Some(9),
                }],
                ..TextPart::default()
            }),
            Part::Image(MediaPart::base64("image/webp", "UklGRg==")),
        ]
    );
}

#[test]
fn decode_accepts_a_terminal_stream_event_and_mints_missing_ids() {
    let decoded = decode(json!({
        "type": "response.completed",
        "sequence_number": 12,
        "response": {
            "id": "", "object": "response", "created_at": 5, "status": "completed", "model": "gpt-5",
            "output": [
                {"type": "message", "role": "assistant", "content": "plain string content"},
                {"type": "function_call", "name": "f", "arguments": {"a": 1}}
            ]
        }
    }));
    assert!(
        decoded.id.starts_with("resp_") && decoded.id.len() == 29,
        "{}",
        decoded.id
    );
    assert_eq!(decoded.created, 5);
    assert_eq!(decoded.parts[0], Part::text("plain string content"));
    match &decoded.parts[1] {
        Part::ToolCall(call) => {
            assert!(call.id.starts_with("call_"), "{}", call.id);
            assert_eq!(call.name, "f");
            assert_eq!(call.arguments, "{\"a\":1}");
        }
        other => panic!("unexpected part {other:?}"),
    }
}

#[test]
fn decode_minimal_compatible_server_response() {
    let decoded = decode(json!({"id": "r1", "model": "llama", "output_text": "hello"}));
    assert_eq!(decoded.parts, vec![Part::text("hello")]);
    assert_eq!(decoded.finish, FinishReason::Stop);
}

#[test]
fn decode_rejects_payloads_that_are_not_responses() {
    let fails = |body: Value| match ResponsesCodec.decode_response(&body) {
        Err(CodecError::InvalidUpstream(message)) => message,
        other => panic!("expected an upstream error, got {other:?}"),
    };
    fails(json!({"id": "chatcmpl-1", "object": "chat.completion", "choices": []}));
    fails(json!("just a string"));
    fails(json!({"id": "resp_1", "object": "response", "output": "not an array"}));
    // An error envelope: the message and its code are what the caller gets.
    assert_eq!(
        fails(json!({"error": {"message": "The model is overloaded", "type": "server_error"}})),
        "The model is overloaded (server_error)"
    );
    assert_eq!(
        fails(json!({"error": "backend exploded"})),
        "backend exploded"
    );
    // A `status` that is not a response status does not make a body a
    // response.
    assert_eq!(
        fails(json!({"status": "error", "message": "no healthy upstream"})),
        "no healthy upstream"
    );
    assert_eq!(
        fails(json!({"status": 503, "detail": "Service Unavailable"})),
        "Service Unavailable"
    );
    fails(json!({"status": "error"}));
}

#[test]
fn decode_failed_response_without_output_is_an_upstream_error() {
    let fails = |body: Value| match ResponsesCodec.decode_response(&body) {
        Err(CodecError::InvalidUpstream(message)) => message,
        other => panic!("expected an upstream error, got {other:?}"),
    };
    // The vendor's shape for a generation that failed (HTTP 200).
    assert_eq!(
        fails(json!({
            "id": "resp_1", "object": "response", "created_at": 1, "status": "failed", "model": "gpt-5",
            "output": [], "usage": null, "incomplete_details": null,
            "error": {"code": "server_error", "message": "The model produced invalid content."}
        })),
        "upstream response failed: The model produced invalid content. (server_error)"
    );
    // Also when it arrives wrapped in the terminal stream event.
    assert_eq!(
        fails(json!({"type": "response.failed", "response": {
            "id": "resp_1", "status": "failed", "output": [],
            "error": {"code": "rate_limit_exceeded", "message": "Slow down."}
        }})),
        "upstream response failed: Slow down. (rate_limit_exceeded)"
    );
    // No explanation at all is still a failure, not an empty answer.
    assert_eq!(
        fails(json!({"id": "resp_1", "object": "response", "status": "failed", "output": []})),
        "upstream response failed without an error message"
    );
    assert_eq!(
        fails(
            json!({"id": "resp_1", "object": "response", "status": "failed", "error": {"code": "bio_policy"}})
        ),
        "upstream response failed: bio_policy"
    );
    // An error object on an otherwise empty "completed" response is believed.
    assert_eq!(
        fails(
            json!({"id": "resp_1", "object": "response", "status": "completed", "output": [],
                     "error": {"message": "quota exhausted", "code": "insufficient_quota"}})
        ),
        "upstream response failed: quota exhausted (insufficient_quota)"
    );
    // Credentials an upstream echoes never make it into the error.
    let leaked = fails(
        json!({"object": "response", "status": "failed", "output": [],
        "error": {"code": "server_error", "message": "call failed: Authorization: Bearer sk-live-0123456789abcdef0123"}}),
    );
    assert_eq!(
        leaked,
        "upstream response failed: call failed: Authorization: Bearer [REDACTED] (server_error)"
    );
}

#[test]
fn decode_failed_response_with_output_keeps_the_output() {
    // Like a stream that broke mid-flight: what was produced survives and
    // the finish reason says it is not the whole answer.
    let decoded = decode(json!({
        "id": "resp_1", "object": "response", "status": "failed", "model": "gpt-5",
        "error": {"code": "server_error", "message": "boom"},
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Partial"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    }));
    assert_eq!(decoded.parts, vec![Part::text("Partial")]);
    assert_eq!(decoded.finish, FinishReason::Error);
    assert_eq!(decoded.usage.output_tokens, 1);
    // An in-flight or empty-but-fine response is not a failure.
    let empty = decode(
        json!({"id": "resp_1", "object": "response", "status": "completed", "output": [], "error": null}),
    );
    assert_eq!((empty.parts.len(), empty.finish), (0, FinishReason::Stop));
    let incomplete = decode(
        json!({"id": "resp_1", "object": "response", "status": "incomplete", "output": [],
                                   "incomplete_details": {"reason": "max_output_tokens"}}),
    );
    assert_eq!(incomplete.finish, FinishReason::Length);
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

#[test]
fn encode_text_response_with_documented_defaults() {
    let encoded = encode(&response(vec![Part::text("Hi there")], FinishReason::Stop));
    assert_eq!(
        encoded,
        json!({
            "id": "resp_abc",
            "object": "response",
            "created_at": 1741476542,
            "status": "completed",
            "background": false,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "max_tool_calls": null,
            // The name the client asked for, not the upstream's.
            "model": "gpt-5",
            "output": [{
                "id": "msg_abc_0",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": "Hi there"}]
            }],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "prompt_cache_key": null,
            "reasoning": {"effort": null, "summary": null},
            "safety_identifier": null,
            "service_tier": "default",
            "store": true,
            "temperature": 1.0,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_logprobs": 0,
            "top_p": 1.0,
            "truncation": "disabled",
            "usage": {
                "input_tokens": 36,
                "input_tokens_details": {"cached_tokens": 6},
                "output_tokens": 87,
                "output_tokens_details": {"reasoning_tokens": 64},
                "total_tokens": 123
            },
            "user": null,
            "metadata": {}
        })
    );
    // The convenience field is SDK-side only.
    assert_eq!(encoded.get("output_text"), None);
}

#[test]
fn encode_echoes_request_fields_when_the_request_is_available() {
    let tools = json!([{"type": "function", "name": "f", "parameters": {"type": "object"}}]);
    let encoded = encode_with(
        &response(vec![Part::text("ok")], FinishReason::Stop),
        json!({
            "model": "gpt-5(high)",
            "input": "hi",
            "instructions": "Be brief.",
            "tools": tools,
            "tool_choice": {"type": "function", "name": "f"},
            "temperature": 0.2,
            "top_p": 0.9,
            "max_output_tokens": 256,
            "parallel_tool_calls": false,
            "reasoning": {"effort": "high"},
            "text": {"verbosity": "low"},
            "store": false,
            "metadata": {"trace": "abc"},
            "previous_response_id": "resp_prev",
            "truncation": "auto",
            "user": "user-1",
            "service_tier": "flex",
            "prompt_cache_key": "pck"
        }),
    );
    assert_eq!(encoded["instructions"], json!("Be brief."));
    assert_eq!(encoded["tools"], tools);
    assert_eq!(
        encoded["tool_choice"],
        json!({"type": "function", "name": "f"})
    );
    assert_eq!(encoded["temperature"], json!(0.2));
    assert_eq!(encoded["top_p"], json!(0.9));
    assert_eq!(encoded["max_output_tokens"], json!(256));
    assert_eq!(encoded["parallel_tool_calls"], json!(false));
    assert_eq!(
        encoded["reasoning"],
        json!({"effort": "high", "summary": null})
    );
    assert_eq!(
        encoded["text"],
        json!({"verbosity": "low", "format": {"type": "text"}})
    );
    assert_eq!(encoded["store"], json!(false));
    assert_eq!(encoded["metadata"], json!({"trace": "abc"}));
    assert_eq!(encoded["previous_response_id"], json!("resp_prev"));
    assert_eq!(encoded["truncation"], json!("auto"));
    assert_eq!(encoded["user"], json!("user-1"));
    assert_eq!(encoded["service_tier"], json!("flex"));
    assert_eq!(encoded["prompt_cache_key"], json!("pck"));
    // Never the request's `input`, and the model comes from the context.
    assert_eq!(encoded.get("input"), None);
    assert_eq!(encoded["model"], json!("gpt-5"));
}

#[test]
fn encode_parallel_tool_calls() {
    let encoded = encode(&response(
        vec![
            Part::text("Checking both."),
            Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
            Part::tool_call("call_b", "get_weather", ""),
        ],
        FinishReason::ToolCalls,
    ));
    assert_eq!(encoded["status"], json!("completed"));
    assert_eq!(
        encoded["output"],
        json!([
            text_message("msg_abc_0", "completed", "Checking both."),
            {"id": "fc_call_a", "type": "function_call", "status": "completed",
             "arguments": "{\"city\":\"Paris\"}", "call_id": "call_a", "name": "get_weather"},
            // No arguments means an empty object, which is what clients parse.
            {"id": "fc_call_b", "type": "function_call", "status": "completed",
             "arguments": "{}", "call_id": "call_b", "name": "get_weather"}
        ])
    );
}

#[test]
fn encode_reasoning_with_signature() {
    let encoded = encode(&response(
        vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_9".into()),
                text: "Thinking it through.".into(),
                signature: Some(Signature::new(P, "gAAAAABnative")),
                redacted: false,
            }),
            // A blob of another vendor travels wrapped, in the same slot.
            Part::Reasoning(Reasoning {
                id: None,
                text: "claude thoughts".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "EqQBsig")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Anthropic, "EuYBdata")),
                redacted: true,
            }),
            Part::reasoning("unsigned"),
            Part::text("Answer."),
        ],
        FinishReason::Stop,
    ));
    assert_eq!(
        encoded["output"],
        json!([
            {"id": "rs_9", "type": "reasoning", "summary": [{"type": "summary_text", "text": "Thinking it through."}],
             "encrypted_content": "gAAAAABnative"},
            {"id": "rs_abc_1", "type": "reasoning", "summary": [{"type": "summary_text", "text": "claude thoughts"}],
             "encrypted_content": "sy1.a.EqQBsig"},
            {"id": "rs_abc_2", "type": "reasoning", "summary": [], "encrypted_content": "sy1.a.redacted:EuYBdata"},
            {"id": "rs_abc_3", "type": "reasoning", "summary": [{"type": "summary_text", "text": "unsigned"}]},
            text_message("msg_abc_4", "completed", "Answer.")
        ])
    );
}

#[test]
fn encode_refusal() {
    let encoded = encode(&response(
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into(),
        })],
        FinishReason::Refusal,
    ));
    assert_eq!(encoded["status"], json!("completed"));
    assert_eq!(encoded["incomplete_details"], Value::Null);
    assert_eq!(
        encoded["output"],
        json!([{
            "id": "msg_abc_0", "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "refusal", "refusal": "I can't help with that."}]
        }])
    );
}

#[test]
fn encode_each_finish_reason() {
    let outcome = |finish: FinishReason| {
        let encoded = encode(&response(vec![Part::text("partial")], finish));
        (
            encoded["status"].clone(),
            encoded["incomplete_details"].clone(),
            encoded["error"].clone(),
            encoded["output"][0]["status"].clone(),
        )
    };
    let done = |status: &str| (json!(status), Value::Null, Value::Null, json!("completed"));
    let cut = |reason: &str| {
        (
            json!("incomplete"),
            json!({"reason": reason}),
            Value::Null,
            json!("incomplete"),
        )
    };
    assert_eq!(outcome(FinishReason::Stop), done("completed"));
    assert_eq!(outcome(FinishReason::ToolCalls), done("completed"));
    assert_eq!(outcome(FinishReason::Length), cut("max_output_tokens"));
    assert_eq!(outcome(FinishReason::ContentFilter), cut("content_filter"));
    assert_eq!(outcome(FinishReason::PauseTurn), cut("pause_turn"));
    assert_eq!(
        outcome(FinishReason::ContextWindow),
        cut("model_context_window_exceeded")
    );
    assert_eq!(
        outcome(FinishReason::Other("recitation".into())),
        cut("recitation")
    );
    // A refusal without a refusal part would read as a normal completion.
    assert_eq!(outcome(FinishReason::Refusal), cut("content_filter"));
    assert_eq!(
        outcome(FinishReason::Error),
        (
            json!("failed"),
            Value::Null,
            json!({"code": "server_error", "message": "The model failed to generate a response."}),
            json!("incomplete"),
        )
    );
}

#[test]
fn encode_usage_reports_inclusive_totals() {
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.usage = Usage {
        input_tokens: 300,
        cache_read_tokens: 600,
        cache_write_tokens: 100,
        output_tokens: 500,
        reasoning_tokens: 200,
    };
    assert_eq!(
        encode(&r)["usage"],
        json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 100},
            "output_tokens": 500,
            "output_tokens_details": {"reasoning_tokens": 200},
            "total_tokens": 1500
        })
    );

    // No usage from the upstream still yields the object clients index into.
    r.usage = Usage::default();
    assert_eq!(
        encode(&r)["usage"],
        json!({
            "input_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 0
        })
    );
}

#[test]
fn encode_ids_take_the_vendor_shape_without_mangling_native_ones() {
    let id_for = |upstream_id: &str| {
        let mut r = response(vec![Part::text("x")], FinishReason::Stop);
        r.id = upstream_id.into();
        let encoded = encode(&r);
        (
            encoded["id"].as_str().unwrap().to_string(),
            encoded["output"][0]["id"].as_str().unwrap().to_string(),
        )
    };
    assert_eq!(
        id_for("resp_0a1b2c"),
        ("resp_0a1b2c".to_string(), "msg_0a1b2c_0".to_string())
    );
    assert_eq!(
        id_for("chatcmpl-123"),
        (
            "resp_chatcmpl-123".to_string(),
            "msg_chatcmpl-123_0".to_string()
        )
    );
    assert_eq!(
        id_for("msg_01XFDUDYJgAACzvnptvVoYEL"),
        (
            "resp_msg_01XFDUDYJgAACzvnptvVoYEL".to_string(),
            "msg_msg_01XFDUDYJgAACzvnptvVoYEL_0".to_string()
        )
    );
    let (minted, _) = id_for("");
    assert!(
        minted.starts_with("resp_") && minted.len() == 29,
        "{minted}"
    );
}

#[test]
fn encode_fills_in_time_model_and_service_tier() {
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.created = 0;
    r.service_tier = Some("priority".into());
    let encoded = ResponsesCodec
        .encode_response(&r, &ClientCtx::new(""))
        .unwrap();
    assert!(encoded["created_at"].as_i64().unwrap() > 1_700_000_000);
    // Without a client-facing name the upstream's model is reported.
    assert_eq!(encoded["model"], json!("gpt-5-2025-08-07"));
    assert_eq!(encoded["service_tier"], json!("priority"));
}

#[test]
fn encode_restores_namespaces_and_custom_tools_from_the_request() {
    let request = json!({
        "model": "gpt-5",
        "input": "x",
        "tools": [
            {"type": "namespace", "name": "mcp__github", "tools": [{"type": "function", "name": "get_me"}]},
            {"type": "custom", "name": "exec"}
        ]
    });
    let encoded = encode_with(
        &response(
            vec![
                Part::tool_call("call_1", "mcp__github__get_me", "{}"),
                // An upstream without custom tools answered with the
                // function-shaped stand-in.
                Part::tool_call("toolu_2", "exec", "{\"input\":\"ls -la\"}"),
                Part::ToolCall(ToolCall {
                    id: "call_3".into(),
                    name: "exec".into(),
                    arguments: "pwd".into(),
                    kind: ToolCallKind::Custom,
                    signature: None,
                    cache_control: None,
                }),
            ],
            FinishReason::ToolCalls,
        ),
        request,
    );
    assert_eq!(
        encoded["output"],
        json!([
            {"id": "fc_call_1", "type": "function_call", "status": "completed", "arguments": "{}",
             "call_id": "call_1", "name": "get_me", "namespace": "mcp__github"},
            {"id": "ctc_toolu_2", "type": "custom_tool_call", "status": "completed", "input": "ls -la",
             "call_id": "toolu_2", "name": "exec"},
            {"id": "ctc_call_3", "type": "custom_tool_call", "status": "completed", "input": "pwd",
             "call_id": "call_3", "name": "exec"}
        ])
    );
}

#[test]
fn encode_adjacent_text_parts_share_one_message_and_other_parts_split_it() {
    let search = json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}});
    let encoded = encode(&response(
        vec![
            Part::text("One. "),
            Part::Text(TextPart {
                text: "Two.".into(),
                citations: vec![Citation {
                    url: Some("https://example.com".into()),
                    title: None,
                    cited_text: Some("quoted".into()),
                    start: Some(0),
                    end: Some(4),
                }],
                ..TextPart::default()
            }),
            Part::Opaque(OpaquePart {
                origin: P,
                raw: search.clone(),
            }),
            // Blocks of other vendors cannot be shown to a Responses client.
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "server_tool_use", "id": "srvtoolu_1"}),
            }),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
            Part::Image(MediaPart::url("https://example.com/generated.png")),
            Part::text("Three."),
        ],
        FinishReason::Stop,
    ));
    assert_eq!(
        encoded["output"],
        json!([
            {"id": "msg_abc_0", "type": "message", "status": "completed", "role": "assistant", "content": [
                {"type": "output_text", "annotations": [], "logprobs": [], "text": "One. "},
                {"type": "output_text", "annotations": [
                    {"type": "url_citation", "start_index": 0, "end_index": 4, "url": "https://example.com", "title": ""}
                ], "logprobs": [], "text": "Two."}
            ]},
            search,
            {"id": "ig_abc_2", "type": "image_generation_call", "status": "completed", "output_format": "png", "result": "iVBOR"},
            text_message("msg_abc_3", "completed", "Three.")
        ])
    );
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

#[test]
fn count_response_round_trip() {
    let body = ResponsesCodec.encode_count_response(1234).unwrap();
    assert_eq!(
        body,
        json!({"object": "response.input_tokens", "input_tokens": 1234})
    );
    assert_eq!(ResponsesCodec.decode_count_response(&body), Some(1234));
    assert_eq!(
        ResponsesCodec.decode_count_response(&json!({"usage": {"input_tokens": 9}})),
        Some(9)
    );
    assert_eq!(
        ResponsesCodec.decode_count_response(&json!({"error": "nope"})),
        None
    );
}
