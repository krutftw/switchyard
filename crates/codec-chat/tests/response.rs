//! `decode_response` and `encode_response`: complete `chat.completion`
//! objects.

mod common;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::error::CodecError;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::{Protocol, Usage};

fn decode(body: Value) -> Response {
    ChatCodec.decode_response(&body).expect("response decodes")
}

fn encode(response: &Response) -> Value {
    ChatCodec
        .encode_response(response, &ClientCtx::new("client-model"))
        .expect("response encodes")
}

fn response(parts: Vec<Part>, finish: FinishReason) -> Response {
    let mut r = Response::new("chatcmpl-abc123", "gpt-4o-2024-08-06");
    r.created = 1_741_569_952;
    r.parts = parts;
    r.finish = finish;
    r
}

/// The single choice of an encoded response.
fn message(body: &Value) -> &Value {
    &body["choices"][0]["message"]
}

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

#[test]
fn decode_openai_text_response() {
    let r = decode(json!({
        "id": "chatcmpl-B9MBs8CjcvOU2jLn4n570S5qMJKcT",
        "object": "chat.completion",
        "created": 1741569952,
        "model": "gpt-4.1-2025-04-14",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Hello! How can I assist you today?",
                "refusal": null,
                "annotations": []
            },
            "logprobs": null,
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 19,
            "completion_tokens": 10,
            "total_tokens": 29,
            "prompt_tokens_details": {"cached_tokens": 0, "audio_tokens": 0},
            "completion_tokens_details": {
                "reasoning_tokens": 0,
                "audio_tokens": 0,
                "accepted_prediction_tokens": 0,
                "rejected_prediction_tokens": 0
            }
        },
        "service_tier": "default",
        "system_fingerprint": "fp_123"
    }));
    assert_eq!(
        r,
        Response {
            id: "chatcmpl-B9MBs8CjcvOU2jLn4n570S5qMJKcT".into(),
            model: "gpt-4.1-2025-04-14".into(),
            created: 1741569952,
            parts: vec![Part::text("Hello! How can I assist you today?")],
            finish: FinishReason::Stop,
            stop_sequence: None,
            usage: Usage {
                input_tokens: 19,
                output_tokens: 10,
                ..Usage::default()
            },
            service_tier: Some("default".into()),
        }
    );
}

#[test]
fn decode_parallel_tool_calls() {
    let r = decode(json!({
        "id": "chatcmpl-1",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                    {"id": "call_b", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\": \"Rome\"}"}},
                    {"id": "call_c", "type": "custom",
                     "custom": {"name": "run_sql", "input": "SELECT 1"}}
                ]
            },
            "finish_reason": "tool_calls"
        }]
    }));
    assert_eq!(
        r.parts,
        vec![
            Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
            Part::tool_call("call_b", "get_weather", "{\"city\": \"Rome\"}"),
            Part::ToolCall(ToolCall {
                id: "call_c".into(),
                name: "run_sql".into(),
                arguments: "SELECT 1".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            }),
        ]
    );
    assert_eq!(r.finish, FinishReason::ToolCalls);
    assert_eq!(r.usage, Usage::default());
}

#[test]
fn decode_reasoning_in_every_spelling() {
    let reasoning = |message: Value| {
        decode(json!({"id": "x", "model": "m", "choices": [{"message": message}]})).parts
    };
    // DeepSeek / Kimi.
    assert_eq!(
        reasoning(json!({"role": "assistant", "reasoning_content": "think", "content": "4"})),
        vec![Part::reasoning("think"), Part::text("4")]
    );
    // OpenRouter / Ollama string.
    assert_eq!(
        reasoning(json!({"role": "assistant", "reasoning": "think", "content": "4"})),
        vec![Part::reasoning("think"), Part::text("4")]
    );
    // `reasoning_content` wins; an empty one falls back.
    assert_eq!(
        reasoning(json!({"reasoning_content": "", "reasoning": "fallback", "content": "4"})),
        vec![Part::reasoning("fallback"), Part::text("4")]
    );
    // OpenRouter sends the string and the details with the same text.
    assert_eq!(
        reasoning(json!({
            "content": "4",
            "reasoning": "think",
            "reasoning_details": [{"type": "reasoning.text", "text": "think", "index": 0}]
        })),
        vec![Part::reasoning("think"), Part::text("4")]
    );
    // Mistral: typed items inside `content`.
    assert_eq!(
        reasoning(json!({"content": [
            {"type": "thinking", "thinking": [{"type": "text", "text": "think"}]},
            {"type": "text", "text": "4"}
        ]})),
        vec![Part::reasoning("think"), Part::text("4")]
    );
}

#[test]
fn decode_reasoning_with_signature_is_tagged_as_chat_origin() {
    let r = decode(json!({
        "id": "gen-1",
        "model": "anthropic/claude-sonnet-4.5",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Done.",
                "reasoning": "Let me think.",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "Let me think.",
                     "signature": "ErUBCkYIBxgC", "format": "anthropic-claude-v1", "index": 0},
                    {"type": "reasoning.encrypted", "data": "gAAAAB", "id": "rs_1", "index": 1}
                ]
            },
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        r.parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Let me think.".into(),
                // Whatever the upstream wrapped, the blob came out of a Chat
                // endpoint and only goes back to one.
                signature: Some(Signature::new(Protocol::OpenaiChat, "ErUBCkYIBxgC")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: String::new(),
                signature: Some(Signature::new(Protocol::OpenaiChat, "gAAAAB")),
                redacted: true,
            }),
            Part::text("Done."),
        ]
    );
}

#[test]
fn decode_upstream_blob_that_looks_wrapped_is_not_unwrapped() {
    // Only client requests go through the unwrapping; an upstream cannot
    // smuggle a foreign-tagged blob in.
    let r = decode(json!({"id": "x", "model": "m", "choices": [{"message": {
        "tool_calls": [{
            "id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"},
            "extra_content": {"google": {"thought_signature": "sy1.a.zzz"}}
        }]
    }}]}));
    assert_eq!(
        r.tool_calls().next().and_then(|c| c.signature.clone()),
        Some(Signature::new(Protocol::OpenaiChat, "sy1.a.zzz"))
    );
}

#[test]
fn decode_refusal() {
    let r = decode(json!({
        "id": "chatcmpl-2",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."},
            "finish_reason": "stop"
        }]
    }));
    assert_eq!(
        r.parts,
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })]
    );
    assert_eq!(r.finish, FinishReason::Stop);
}

#[test]
fn decode_every_finish_reason() {
    let finish = |raw: Value| {
        decode(json!({"id": "x", "model": "m", "choices": [{
            "message": {"content": "hi"},
            "finish_reason": raw
        }]}))
        .finish
    };
    assert_eq!(finish(json!("stop")), FinishReason::Stop);
    assert_eq!(finish(json!("length")), FinishReason::Length);
    assert_eq!(finish(json!("content_filter")), FinishReason::ContentFilter);
    assert_eq!(finish(Value::Null), FinishReason::Stop);
    // Spellings other vendors' compatible endpoints leak.
    assert_eq!(finish(json!("end_turn")), FinishReason::Stop);
    assert_eq!(finish(json!("max_tokens")), FinishReason::Length);
    assert_eq!(finish(json!("MAX_TOKENS")), FinishReason::Length);
    assert_eq!(finish(json!("safety")), FinishReason::ContentFilter);
    assert_eq!(finish(json!("error")), FinishReason::Error);
    assert_eq!(
        finish(json!("eos_token")),
        FinishReason::Other("eos_token".into())
    );
    // `tool_calls` without any call is a plain stop.
    assert_eq!(finish(json!("tool_calls")), FinishReason::Stop);
}

#[test]
fn decode_finish_reason_is_reconciled_with_tool_calls() {
    let finish = |raw: Value| {
        decode(json!({"id": "x", "model": "m", "choices": [{
            "message": {"tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            "finish_reason": raw
        }]}))
        .finish
    };
    assert_eq!(finish(json!("tool_calls")), FinishReason::ToolCalls);
    assert_eq!(finish(json!("function_call")), FinishReason::ToolCalls);
    // Many compatible servers say `stop` (or nothing) on a tool-call turn.
    assert_eq!(finish(json!("stop")), FinishReason::ToolCalls);
    assert_eq!(finish(Value::Null), FinishReason::ToolCalls);
    // A truncated call must not look executable.
    assert_eq!(finish(json!("length")), FinishReason::Length);
}

#[test]
fn decode_legacy_function_call() {
    let r = decode(json!({"id": "x", "model": "m", "choices": [{
        "message": {"role": "assistant", "content": null,
                    "function_call": {"name": "get_time", "arguments": "{}"}},
        "finish_reason": "function_call"
    }]}));
    let call = r.tool_calls().next().expect("a call");
    assert_eq!(
        (call.name.as_str(), call.arguments.as_str()),
        ("get_time", "{}")
    );
    assert!(call.id.starts_with("call_"));
    assert_eq!(r.finish, FinishReason::ToolCalls);
}

#[test]
fn decode_usage_with_cache_and_reasoning_tokens() {
    let usage = |usage: Value| {
        decode(json!({"id": "x", "model": "m", "choices": [], "usage": usage})).usage
    };
    // OpenAI: cached tokens are part of prompt_tokens, reasoning tokens part
    // of completion_tokens.
    assert_eq!(
        usage(json!({
            "prompt_tokens": 2006,
            "completion_tokens": 300,
            "total_tokens": 2306,
            "prompt_tokens_details": {"cached_tokens": 1920},
            "completion_tokens_details": {"reasoning_tokens": 128}
        })),
        Usage {
            input_tokens: 86,
            cache_read_tokens: 1920,
            cache_write_tokens: 0,
            output_tokens: 300,
            reasoning_tokens: 128,
        }
    );
    // Cache writes are carved out of the prompt total as well.
    assert_eq!(
        usage(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 800, "cache_write_tokens": 150}
        })),
        Usage {
            input_tokens: 50,
            cache_read_tokens: 800,
            cache_write_tokens: 150,
            output_tokens: 5,
            reasoning_tokens: 0,
        }
    );
    // DeepSeek.
    assert_eq!(
        usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 60,
            "prompt_cache_miss_tokens": 40
        })),
        Usage {
            input_tokens: 40,
            cache_read_tokens: 60,
            output_tokens: 20,
            ..Usage::default()
        }
    );
    // Anthropic-style extras relayed by a compatible gateway.
    assert_eq!(
        usage(json!({
            "prompt_tokens": 500,
            "completion_tokens": 10,
            "cache_read_input_tokens": 300,
            "cache_creation_input_tokens": 100
        })),
        Usage {
            input_tokens: 100,
            cache_read_tokens: 300,
            cache_write_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: 0,
        }
    );
    // Inconsistent numbers never underflow, and floats are accepted. The
    // total says reasoning is part of the completion count, so a reasoning
    // figure above it is clamped.
    assert_eq!(
        usage(json!({
            "prompt_tokens": 500.0,
            "completion_tokens": 3,
            "total_tokens": 503,
            "prompt_tokens_details": {"cached_tokens": 800},
            "completion_tokens_details": {"reasoning_tokens": 9}
        })),
        Usage {
            input_tokens: 0,
            cache_read_tokens: 800,
            cache_write_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: 3,
        }
    );
    assert_eq!(usage(Value::Null), Usage::default());
}

#[test]
fn decode_citations_and_generated_images() {
    let r = decode(json!({"id": "x", "model": "m", "choices": [{"message": {
        "content": "Paris is the capital.",
        "annotations": [{"type": "url_citation", "url_citation": {
            "url": "https://example.com/paris", "title": "Paris", "start_index": 0, "end_index": 5
        }}],
        "images": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}, "index": 0}]
    }}]}));
    assert_eq!(
        r.parts,
        vec![
            Part::Text(TextPart {
                text: "Paris is the capital.".into(),
                cache_control: None,
                citations: vec![Citation {
                    url: Some("https://example.com/paris".into()),
                    title: Some("Paris".into()),
                    cited_text: None,
                    start: Some(0),
                    end: Some(5),
                }],
                signature: None,
            }),
            Part::Image(MediaPart::base64("image/png", "AAAA")),
        ]
    );
}

#[test]
fn decode_liberal_shapes() {
    // No id: one is minted in the vendor's shape.
    let r = decode(json!({"choices": [{"message": {"content": "hi"}}]}));
    assert!(r.id.starts_with("chatcmpl-") && r.id.len() == 33);
    assert_eq!((r.model.as_str(), r.created), ("", 0));

    // vLLM reports the matched stop string.
    let r = decode(json!({"id": "cmpl-1", "model": "m", "choices": [{
        "message": {"content": "abc"}, "finish_reason": "stop", "stop_reason": "###"
    }]}));
    assert_eq!(r.stop_sequence.as_deref(), Some("###"));

    // `choices: []` is an empty answer, not a failure.
    let r = decode(json!({"id": "x", "model": "m", "choices": []}));
    assert!(r.parts.is_empty());
    assert_eq!(r.finish, FinishReason::Stop);

    // Only the first choice is used.
    let r = decode(json!({"id": "x", "model": "m", "choices": [
        {"index": 1, "message": {"content": "second"}},
        {"index": 0, "message": {"content": "first"}}
    ]}));
    assert_eq!(r.text(), "first");
}

#[test]
fn decode_rejects_bodies_that_are_not_chat_completions() {
    let err = |body: Value| ChatCodec.decode_response(&body).expect_err("must fail");
    assert!(matches!(err(json!([1, 2])), CodecError::InvalidUpstream(_)));
    assert!(matches!(
        err(json!({"id": "x"})),
        CodecError::InvalidUpstream(_)
    ));
    match err(json!({"error": {"message": "model overloaded", "type": "server_error"}})) {
        CodecError::InvalidUpstream(message) => assert!(message.contains("model overloaded")),
        other => panic!("unexpected error {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

#[test]
fn encode_text_response_exact_shape() {
    let mut r = response(vec![Part::text("Hello!")], FinishReason::Stop);
    r.usage = Usage {
        input_tokens: 10,
        cache_read_tokens: 5,
        cache_write_tokens: 0,
        output_tokens: 7,
        reasoning_tokens: 2,
    };
    r.service_tier = Some("default".into());
    let body = encode(&r);
    assert_eq!(
        body,
        json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "created": 1741569952,
            // The name the client asked for, not the upstream's.
            "model": "client-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!", "refusal": null},
                "logprobs": null,
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 15,
                "completion_tokens": 7,
                "total_tokens": 22,
                "prompt_tokens_details": {"cached_tokens": 5},
                "completion_tokens_details": {"reasoning_tokens": 2}
            },
            "service_tier": "default"
        })
    );
    let keys: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "service_tier"
        ]
    );
}

#[test]
fn encode_usage_buckets_are_made_inclusive_again() {
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.usage = Usage {
        input_tokens: 50,
        cache_read_tokens: 800,
        cache_write_tokens: 150,
        output_tokens: 300,
        reasoning_tokens: 128,
    };
    assert_eq!(
        encode(&r)["usage"],
        json!({
            "prompt_tokens": 1000,
            "completion_tokens": 300,
            "total_tokens": 1300,
            "prompt_tokens_details": {"cached_tokens": 800, "cache_write_tokens": 150},
            "completion_tokens_details": {"reasoning_tokens": 128}
        })
    );
    // No usage reported upstream: zeros, the object is still present.
    assert_eq!(
        encode(&response(vec![], FinishReason::Stop))["usage"],
        json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 0}
        })
    );
}

#[test]
fn encode_parallel_tool_calls() {
    let r = response(
        vec![
            Part::text("Checking."),
            Part::tool_call("toolu_01", "get_weather", "{\"city\": \"Paris\"}"),
            Part::tool_call("", "ping", ""),
            Part::ToolCall(ToolCall {
                id: "c3".into(),
                name: "run_sql".into(),
                arguments: "SELECT 1".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            }),
        ],
        FinishReason::ToolCalls,
    );
    let body = encode(&r);
    let calls = message(&body)["tool_calls"].as_array().expect("tool calls");
    assert_eq!(message(&body)["content"], json!("Checking."));
    assert_eq!(
        calls[0],
        // Ids of other vendors are kept verbatim: the client sends them back.
        json!({"id": "toolu_01", "type": "function",
               "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}})
    );
    assert!(
        calls[1]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("call_"))
    );
    assert_eq!(
        calls[1]["function"],
        json!({"name": "ping", "arguments": "{}"})
    );
    assert_eq!(
        calls[2],
        json!({"id": "c3", "type": "custom", "custom": {"name": "run_sql", "input": "SELECT 1"}})
    );
    assert_eq!(body["choices"][0]["finish_reason"], json!("tool_calls"));
}

#[test]
fn encode_tool_only_turn_has_null_content() {
    let body = encode(&response(
        vec![Part::tool_call("c1", "f", "{}")],
        FinishReason::ToolCalls,
    ));
    assert_eq!(message(&body)["content"], Value::Null);
    // An empty answer is an empty string, not null.
    let body = encode(&response(vec![], FinishReason::Stop));
    assert_eq!(message(&body)["content"], json!(""));
}

#[test]
fn encode_reasoning_with_signatures() {
    let r = response(
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Claude thinks.".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "ErUB")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Anthropic, "REDACTED")),
                redacted: true,
            }),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "Summary.".into(),
                signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
                redacted: false,
            }),
            Part::text("Answer."),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        message(&encode(&r)),
        &json!({
            "role": "assistant",
            "content": "Answer.",
            "reasoning_content": "Claude thinks.\n\nSummary.",
            "reasoning_details": [
                // Foreign blobs are wrapped so they can never be replayed to
                // the wrong vendor ...
                {"type": "reasoning.text", "text": "Claude thinks.", "signature": "sy1.a.ErUB", "index": 0},
                {"type": "reasoning.encrypted", "data": "sy1.a.REDACTED", "index": 1},
                // ... and that includes the encrypted reasoning of a
                // Responses upstream: bare, it would come back as a blob a
                // Chat upstream issued and be replayed to one.
                {"type": "reasoning.text", "text": "Summary.", "signature": "sy1.r.gAAAA", "id": "rs_1", "index": 2}
            ],
            "refusal": null
        })
    );
}

#[test]
fn encode_unsigned_reasoning_has_no_details() {
    let r = response(
        vec![Part::reasoning("think"), Part::text("4")],
        FinishReason::Stop,
    );
    assert_eq!(
        message(&encode(&r)),
        &json!({"role": "assistant", "content": "4", "reasoning_content": "think", "refusal": null})
    );
}

#[test]
fn encode_tool_call_signature_is_wrapped_for_the_client() {
    let call = |origin: Protocol| {
        Part::ToolCall(ToolCall {
            id: "c1".into(),
            name: "f".into(),
            arguments: "{}".into(),
            kind: ToolCallKind::Function,
            signature: Some(Signature::new(origin, "CiQB")),
            cache_control: None,
        })
    };
    let body = encode(&response(
        vec![call(Protocol::Gemini)],
        FinishReason::ToolCalls,
    ));
    assert_eq!(
        message(&body)["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "sy1.g.CiQB"}})
    );
    let body = encode(&response(
        vec![call(Protocol::OpenaiChat)],
        FinishReason::ToolCalls,
    ));
    assert_eq!(
        message(&body)["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "CiQB"}})
    );
}

#[test]
fn encode_refusal() {
    let body = encode(&response(
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into(),
        })],
        FinishReason::Refusal,
    ));
    assert_eq!(
        message(&body),
        &json!({"role": "assistant", "content": null, "refusal": "I can't help with that."})
    );
    // A refusal the model wrote out is a completed answer: Chat Completions
    // reports it with `finish_reason: "stop"` next to `message.refusal`
    // (notes 08 §8.2: a `completed` response without tool calls is `stop`).
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));

    // A refusal without refusal text (Anthropic's `stop_reason: "refusal"`:
    // a classifier withheld the answer) is not a completed answer.
    let body = encode(&response(vec![Part::text("I can")], FinishReason::Refusal));
    assert_eq!(body["choices"][0]["finish_reason"], json!("content_filter"));
}

#[test]
fn encode_every_finish_reason() {
    let wire = |finish: FinishReason| {
        encode(&response(vec![Part::text("x")], finish))["choices"][0]["finish_reason"].clone()
    };
    assert_eq!(wire(FinishReason::Stop), json!("stop"));
    assert_eq!(wire(FinishReason::Length), json!("length"));
    assert_eq!(wire(FinishReason::ContentFilter), json!("content_filter"));
    assert_eq!(wire(FinishReason::Refusal), json!("content_filter"));
    assert_eq!(wire(FinishReason::PauseTurn), json!("stop"));
    assert_eq!(wire(FinishReason::ContextWindow), json!("length"));
    assert_eq!(wire(FinishReason::Error), json!("length"));
    assert_eq!(
        wire(FinishReason::Other("recitation".into())),
        json!("stop")
    );
    // `tool_calls` without a call would make clients wait for one.
    assert_eq!(wire(FinishReason::ToolCalls), json!("stop"));

    let with_call = |finish: FinishReason| {
        encode(&response(vec![Part::tool_call("c", "f", "{}")], finish))["choices"][0]
            ["finish_reason"]
            .clone()
    };
    assert_eq!(with_call(FinishReason::ToolCalls), json!("tool_calls"));
    // Gemini says STOP on a function-call turn; Chat clients need `tool_calls`.
    assert_eq!(with_call(FinishReason::Stop), json!("tool_calls"));
    assert_eq!(with_call(FinishReason::Length), json!("length"));
}

#[test]
fn encode_response_ids() {
    let id = |upstream: &str| {
        let mut r = response(vec![], FinishReason::Stop);
        r.id = upstream.into();
        encode(&r)["id"].as_str().expect("an id").to_string()
    };
    assert_eq!(id("chatcmpl-B9MBs8"), "chatcmpl-B9MBs8");
    assert_eq!(
        id("msg_01XFDUDYJgAACzvnptvVoYEL"),
        "chatcmpl-01XFDUDYJgAACzvnptvVoYEL"
    );
    assert_eq!(id("resp_68af4030592c"), "chatcmpl-68af4030592c");
    assert_eq!(id("7Zb2aO3wK"), "chatcmpl-7Zb2aO3wK");
    let minted = id("");
    assert!(minted.starts_with("chatcmpl-") && minted.len() == 33);
}

#[test]
fn encode_fills_created_and_model_when_unknown() {
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.created = 0;
    let body = ChatCodec
        .encode_response(&r, &ClientCtx::new(""))
        .expect("encodes");
    assert!(body["created"].as_i64().is_some_and(|t| t > 1_700_000_000));
    assert_eq!(body["model"], json!("gpt-4o-2024-08-06"));
}

#[test]
fn encode_citations_images_and_dropped_parts() {
    let cite = |start, end| Citation {
        url: Some("https://example.com".into()),
        title: None,
        cited_text: Some("quoted".into()),
        start: Some(start),
        end: Some(end),
    };
    let r = response(
        vec![
            Part::Text(TextPart {
                citations: vec![cite(0, 5)],
                ..TextPart::new("Héllo ")
            }),
            Part::Image(MediaPart::base64("image/png", "AAAA")),
            Part::Text(TextPart {
                citations: vec![cite(0, 5)],
                ..TextPart::new("world")
            }),
            Part::Audio(MediaPart::base64("audio/wav", "UklG")),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "x"}),
            }),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        message(&encode(&r)),
        &json!({
            "role": "assistant",
            "content": "Héllo world",
            "refusal": null,
            "annotations": [
                {"type": "url_citation", "url_citation": {
                    "url": "https://example.com", "title": "", "start_index": 0, "end_index": 5}},
                // Offsets are in characters of the whole message content.
                {"type": "url_citation", "url_citation": {
                    "url": "https://example.com", "title": "", "start_index": 6, "end_index": 11}}
            ],
            "images": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}, "index": 0}
            ]
        })
    );
}

#[test]
fn decode_audio_answer_keeps_its_transcript() {
    let r = decode(
        json!({"id": "x", "model": "gpt-4o-audio-preview", "choices": [{
            "message": {"role": "assistant", "content": null, "refusal": null,
                        "audio": {"id": "audio_1", "data": "UklGRg==", "expires_at": 1,
                                  "transcript": "Hello there"}},
            "finish_reason": "stop"
        }]}),
    );
    assert_eq!(r.parts, vec![Part::text("Hello there")]);
}

#[test]
fn decode_ignores_other_choices_and_unknown_message_fields() {
    let r = decode(json!({
        "id": "x", "model": "m",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi", "audio": null,
                        "function_call": null, "tool_calls": null, "annotations": null,
                        "reasoning_content": null, "provider_specific": {"a": 1}},
            "finish_reason": "stop",
            "native_finish_reason": "end_turn"
        }],
        "provider": "X",
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost": 0.0001}
    }));
    assert_eq!(r.parts, vec![Part::text("hi")]);
    assert_eq!(
        r.usage,
        Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Usage::default()
        }
    );
}
