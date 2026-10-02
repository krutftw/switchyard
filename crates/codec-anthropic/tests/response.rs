//! Complete responses: upstream bodies → canonical responses and canonical
//! responses → client bodies.

mod common;

use common::{decode_response, encode_response};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::{ClientCtx, Codec, CodecError, Protocol, Usage};

fn response(parts: Vec<Part>, finish: FinishReason) -> Response {
    let mut response = Response::new("msg_01XFDUDYJgAACzvnptvVoYEL", "claude-sonnet-4-5-20250929");
    response.parts = parts;
    response.finish = finish;
    response
}

fn reasoning(text: &str, signature: Option<Signature>, redacted: bool) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature,
        redacted,
    })
}

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

#[test]
fn decode_response_text() {
    let decoded = decode_response(&json!({
        "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "text", "text": "Hello! How can I help?"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 12, "output_tokens": 8}
    }));
    assert_eq!(
        decoded,
        Response {
            id: "msg_01XFDUDYJgAACzvnptvVoYEL".into(),
            model: "claude-sonnet-4-5-20250929".into(),
            created: 0,
            parts: vec![Part::text("Hello! How can I help?")],
            finish: FinishReason::Stop,
            stop_sequence: None,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 8,
                ..Usage::default()
            },
            service_tier: None,
        }
    );
}

#[test]
fn decode_response_parallel_tool_calls() {
    let decoded = decode_response(&json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-4-5",
        "content": [
            {"type": "text", "text": "I'll look both up."},
            {"type": "tool_use", "id": "toolu_01A", "name": "get_weather", "input": {"city": "Paris"}},
            {"type": "tool_use", "id": "toolu_01B", "name": "get_time", "input": {}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 100, "output_tokens": 50}
    }));
    assert_eq!(
        decoded.parts,
        vec![
            Part::text("I'll look both up."),
            Part::tool_call("toolu_01A", "get_weather", r#"{"city":"Paris"}"#),
            Part::tool_call("toolu_01B", "get_time", "{}"),
        ]
    );
    assert_eq!(decoded.finish, FinishReason::ToolCalls);
}

#[test]
fn decode_response_reasoning_with_signature_is_tagged_anthropic() {
    let decoded = decode_response(&json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-4-5",
        "content": [
            {"type": "thinking", "thinking": "The user wants X.", "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pk"},
            {"type": "thinking", "thinking": "", "signature": "ErUBCkYIBxgCIkD"},
            {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rLVyIw"},
            {"type": "text", "text": "Here is X."}
        ],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 200,
                  "output_tokens_details": {"thinking_tokens": 150}}
    }));
    assert_eq!(
        decoded.parts,
        vec![
            reasoning(
                "The user wants X.",
                Some(Signature::new(
                    Protocol::Anthropic,
                    "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pk"
                )),
                false
            ),
            // `display: "omitted"`: no text, only the signature.
            reasoning(
                "",
                Some(Signature::new(Protocol::Anthropic, "ErUBCkYIBxgCIkD")),
                false
            ),
            reasoning(
                "",
                Some(Signature::new(
                    Protocol::Anthropic,
                    "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rLVyIw"
                )),
                true
            ),
            Part::text("Here is X."),
        ]
    );
    assert_eq!(decoded.usage.reasoning_tokens, 150);
    assert_eq!(decoded.usage.output_tokens, 200);
}

#[test]
fn decode_response_refusal() {
    let decoded = decode_response(&json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-5",
        "content": [{"type": "text", "text": "I can't continue with"}],
        "stop_reason": "refusal", "stop_sequence": null,
        "stop_details": {"type": "refusal", "category": "cyber", "explanation": null},
        "usage": {"input_tokens": 30, "output_tokens": 6}
    }));
    assert_eq!(decoded.finish, FinishReason::Refusal);
    assert_eq!(decoded.parts, vec![Part::text("I can't continue with")]);
    // A refusal before any output.
    let decoded = decode_response(&json!({
        "id": "msg_02", "type": "message", "role": "assistant", "model": "claude-opus-5",
        "content": [], "stop_reason": "refusal", "stop_sequence": null,
        "usage": {"input_tokens": 30, "output_tokens": 0}
    }));
    assert_eq!(decoded.finish, FinishReason::Refusal);
    assert!(decoded.parts.is_empty());
}

#[test]
fn decode_response_every_stop_reason() {
    let finish_of = |stop_reason: Value, stop_sequence: Value| {
        let decoded = decode_response(&json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": stop_reason, "stop_sequence": stop_sequence,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }));
        (decoded.finish, decoded.stop_sequence)
    };
    assert_eq!(
        finish_of(json!("end_turn"), Value::Null),
        (FinishReason::Stop, None)
    );
    assert_eq!(
        finish_of(json!("stop_sequence"), json!("END")),
        (FinishReason::Stop, Some("END".to_string()))
    );
    assert_eq!(
        finish_of(json!("max_tokens"), Value::Null),
        (FinishReason::Length, None)
    );
    assert_eq!(
        finish_of(json!("tool_use"), Value::Null),
        (FinishReason::ToolCalls, None)
    );
    assert_eq!(
        finish_of(json!("pause_turn"), Value::Null),
        (FinishReason::PauseTurn, None)
    );
    assert_eq!(
        finish_of(json!("refusal"), Value::Null),
        (FinishReason::Refusal, None)
    );
    assert_eq!(
        finish_of(json!("model_context_window_exceeded"), Value::Null),
        (FinishReason::ContextWindow, None)
    );
    // The enum is open: unknown reasons are kept as spelled.
    assert_eq!(
        finish_of(json!("something_new"), Value::Null),
        (FinishReason::Other("something_new".into()), None)
    );
    // Missing reason: inferred from the content.
    assert_eq!(
        finish_of(Value::Null, Value::Null),
        (FinishReason::Stop, None)
    );
    let decoded = decode_response(&json!({
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}]
    }));
    assert_eq!(decoded.finish, FinishReason::ToolCalls);
}

#[test]
fn decode_response_usage_buckets_are_taken_as_disjoint() {
    let decoded = decode_response(&json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-5",
        "content": [{"type": "text", "text": "x"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {
            "input_tokens": 2095,
            "output_tokens": 503,
            "cache_creation_input_tokens": 2051,
            "cache_read_input_tokens": 1000,
            "cache_creation": {"ephemeral_5m_input_tokens": 2051, "ephemeral_1h_input_tokens": 0},
            "output_tokens_details": {"thinking_tokens": 120},
            "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 2},
            "service_tier": "standard",
            "inference_geo": "global"
        }
    }));
    // Anthropic's input_tokens already EXCLUDES cache reads and writes.
    assert_eq!(
        decoded.usage,
        Usage {
            input_tokens: 2095,
            cache_read_tokens: 1000,
            cache_write_tokens: 2051,
            output_tokens: 503,
            reasoning_tokens: 120,
        }
    );
    assert_eq!(decoded.usage.prompt_tokens(), 5146);
    assert_eq!(decoded.usage.total_tokens(), 5649);
    assert_eq!(decoded.service_tier.as_deref(), Some("standard"));
}

#[test]
fn decode_response_usage_tolerates_floats_nulls_and_absence() {
    let decoded = decode_response(&json!({
        "content": [{"type": "text", "text": "x"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 12.0, "output_tokens": 8.0,
                  "cache_creation_input_tokens": null, "cache_read_input_tokens": null}
    }));
    assert_eq!(
        decoded.usage,
        Usage {
            input_tokens: 12,
            output_tokens: 8,
            ..Usage::default()
        }
    );
    let decoded = decode_response(&json!({
        "content": [{"type": "text", "text": "x"}], "stop_reason": "end_turn"
    }));
    assert!(decoded.usage.is_empty());
}

#[test]
fn decode_response_server_tool_blocks_are_opaque() {
    let server_tool_use = json!({"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
                                 "input": {"query": "weather"}});
    let result = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01",
                        "content": [{"type": "web_search_result", "url": "https://w.example",
                                     "title": "W", "encrypted_content": "enc", "page_age": null}]});
    let decoded = decode_response(&json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "m",
        "content": [
            server_tool_use.clone(), result.clone(),
            {"type": "text", "text": "It is sunny.", "citations": [
                {"type": "web_search_result_location", "url": "https://w.example", "title": "W",
                 "encrypted_index": "idx", "cited_text": "sunny"}]}
        ],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }));
    assert_eq!(
        decoded.parts,
        vec![
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: server_tool_use
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: result
            }),
            Part::Text(TextPart {
                text: "It is sunny.".into(),
                cache_control: None,
                citations: vec![Citation {
                    url: Some("https://w.example".into()),
                    title: Some("W".into()),
                    cited_text: Some("sunny".into()),
                    start: None,
                    end: None,
                }],
                signature: None,
            }),
        ]
    );
    // Server tool activity is not a pending client tool call.
    assert_eq!(decoded.finish, FinishReason::Stop);
}

#[test]
fn decode_response_mints_missing_ids() {
    let decoded = decode_response(&json!({
        "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "name": "f", "input": {"a": 1}}],
        "stop_reason": "tool_use"
    }));
    assert!(decoded.id.starts_with("msg_") && decoded.id.len() > 4);
    let call = decoded.tool_calls().next().unwrap();
    assert!(call.id.starts_with("toolu_") && call.id.len() > 6);
    // An id the upstream did send is never touched.
    let decoded = decode_response(&json!({
        "id": "gen-1758112233-abc", "content": [], "stop_reason": "end_turn"
    }));
    assert_eq!(decoded.id, "gen-1758112233-abc");
}

#[test]
fn decode_response_from_compatible_servers() {
    // String content, OpenAI-shaped usage and finish reason.
    let decoded = decode_response(&json!({
        "id": "msg_x", "type": "message", "role": "assistant", "model": "deepseek-chat",
        "content": "plain string",
        "stop_reason": "length",
        "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                  "prompt_tokens_details": {"cached_tokens": 60}}
    }));
    assert_eq!(decoded.parts, vec![Part::text("plain string")]);
    assert_eq!(decoded.finish, FinishReason::Length);
    assert_eq!(
        decoded.usage,
        Usage {
            input_tokens: 40,
            cache_read_tokens: 60,
            output_tokens: 20,
            ..Usage::default()
        }
    );
    // Thinking without a signature (third-party "Anthropic-compatible" APIs).
    let decoded = decode_response(&json!({
        "id": "msg_x", "type": "message", "role": "assistant", "model": "kimi",
        "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "ok"}],
        "stop_reason": "end_turn"
    }));
    assert_eq!(
        decoded.parts,
        vec![reasoning("hmm", None, false), Part::text("ok")]
    );
}

#[test]
fn decode_response_rejects_bodies_that_are_not_messages() {
    let codec = AnthropicCodec;
    assert!(matches!(
        codec.decode_response(&json!("text")),
        Err(CodecError::InvalidUpstream(_))
    ));
    assert!(matches!(
        codec.decode_response(&json!({"choices": [{"message": {"content": "hi"}}]})),
        Err(CodecError::InvalidUpstream(_))
    ));
    let err = codec
        .decode_response(&json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))
        .unwrap_err();
    assert_eq!(
        err,
        CodecError::upstream("upstream answered with an error body: Overloaded")
    );
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

#[test]
fn encode_response_text_exact_body() {
    let mut resp = response(vec![Part::text("Hello!")], FinishReason::Stop);
    resp.usage = Usage {
        input_tokens: 12,
        output_tokens: 8,
        ..Usage::default()
    };
    assert_eq!(
        encode_response(&resp, "sonnet"),
        json!({
            "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
            "type": "message",
            "role": "assistant",
            "model": "sonnet",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 8}
        })
    );
}

#[test]
fn encode_response_reports_the_clients_model_name() {
    let resp = response(vec![Part::text("x")], FinishReason::Stop);
    assert_eq!(
        encode_response(&resp, "my-alias")["model"],
        json!("my-alias")
    );
    // Without a client name the upstream's is used.
    let body = AnthropicCodec
        .encode_response(&resp, &ClientCtx::new(""))
        .unwrap();
    assert_eq!(body["model"], json!("claude-sonnet-4-5-20250929"));
}

#[test]
fn encode_response_ids_take_the_vendor_shape_without_mangling_native_ones() {
    let mut resp = response(vec![Part::text("x")], FinishReason::Stop);
    assert_eq!(
        encode_response(&resp, "m")["id"],
        json!("msg_01XFDUDYJgAACzvnptvVoYEL")
    );
    resp.id = "chatcmpl-CaBc123".into();
    assert_eq!(encode_response(&resp, "m")["id"], json!("msg_CaBc123"));
    resp.id = "resp_0a1b2c".into();
    assert_eq!(encode_response(&resp, "m")["id"], json!("msg_0a1b2c"));
    resp.id = "kR3xZ".into();
    assert_eq!(encode_response(&resp, "m")["id"], json!("msg_kR3xZ"));
    resp.id = String::new();
    let id = encode_response(&resp, "m")["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(id.starts_with("msg_") && id.len() == 28);
}

#[test]
fn encode_response_parallel_tool_calls() {
    let resp = response(
        vec![
            Part::text("Checking."),
            Part::tool_call("call_abc", "get_weather", r#"{"city":"Paris"}"#),
            Part::tool_call("toolu_01B", "get_time", ""),
            Part::tool_call("", "broken_args", "{\"a\":"),
            Part::ToolCall(ToolCall {
                id: "call_custom".into(),
                name: "apply_patch".into(),
                arguments: "*** Begin Patch".into(),
                kind: ToolCallKind::Custom,
                signature: Some(Signature::new(Protocol::Gemini, "thought")),
                cache_control: None,
            }),
        ],
        FinishReason::ToolCalls,
    );
    let body = encode_response(&resp, "m");
    let content = body["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": "Checking."}));
    // Call ids of other vendors are kept verbatim: the client sends them back.
    assert_eq!(
        content[1],
        json!({"type": "tool_use", "id": "call_abc", "name": "get_weather", "input": {"city": "Paris"}})
    );
    assert_eq!(
        content[2],
        json!({"type": "tool_use", "id": "toolu_01B", "name": "get_time", "input": {}})
    );
    // A missing id is minted; unparsable arguments are wrapped, not lost.
    assert!(content[3]["id"].as_str().unwrap().starts_with("toolu_"));
    assert_eq!(content[3]["input"], json!({"input": "{\"a\":"}));
    // A `tool_use` block has no field for the signature another vendor put
    // on the call, so it travels on a text-less `thinking` block right ahead
    // of the call, marked as a call signature and tagged with its origin.
    assert_eq!(
        content[4],
        json!({"type": "thinking", "thinking": "", "signature": "sy1.g.call:thought"})
    );
    assert_eq!(
        content[5],
        json!({"type": "tool_use", "id": "call_custom", "name": "apply_patch",
               "input": {"input": "*** Begin Patch"}})
    );
    assert_eq!(content.len(), 6);
    assert_eq!(body["stop_reason"], json!("tool_use"));
}

#[test]
fn encode_response_reasoning_signatures() {
    let resp = response(
        vec![
            reasoning(
                "native",
                Some(Signature::new(Protocol::Anthropic, "EqQBCgIYAhIM")),
                false,
            ),
            reasoning(
                "from gemini",
                Some(Signature::new(Protocol::Gemini, "CiQBabc+/=")),
                false,
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::OpenaiResponses, "gAAAAAB")),
                false,
            ),
            reasoning("unsigned", None, false),
            reasoning(
                "",
                Some(Signature::new(Protocol::Anthropic, "EmwKAhgB")),
                true,
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::OpenaiResponses, "enc")),
                true,
            ),
            // Nothing to show and nothing to replay.
            reasoning("", None, false),
            reasoning("", None, true),
            Part::text("Answer"),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode_response(&resp, "m")["content"],
        json!([
            {"type": "thinking", "thinking": "native", "signature": "EqQBCgIYAhIM"},
            // Foreign blobs are wrapped so they can never be replayed to Anthropic.
            {"type": "thinking", "thinking": "from gemini", "signature": "sy1.g.CiQBabc+/="},
            {"type": "thinking", "thinking": "", "signature": "sy1.r.gAAAAAB"},
            // Unsigned reasoning is still shown, with an empty signature.
            {"type": "thinking", "thinking": "unsigned", "signature": ""},
            {"type": "redacted_thinking", "data": "EmwKAhgB"},
            {"type": "redacted_thinking", "data": "sy1.r.enc"},
            {"type": "text", "text": "Answer"}
        ])
    );
}

#[test]
fn encode_response_refusal_is_text_plus_stop_reason() {
    let resp = response(
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into(),
        })],
        FinishReason::Stop,
    );
    let body = encode_response(&resp, "m");
    assert_eq!(
        body["content"],
        json!([{"type": "text", "text": "I can't help with that."}])
    );
    assert_eq!(body["stop_reason"], json!("refusal"));
    let resp = response(vec![], FinishReason::Refusal);
    let body = encode_response(&resp, "m");
    assert_eq!(body["content"], json!([]));
    assert_eq!(body["stop_reason"], json!("refusal"));
}

#[test]
fn encode_response_every_finish_reason() {
    let stop = |finish: FinishReason, stop_sequence: Option<&str>| {
        let mut resp = response(vec![Part::text("x")], finish);
        resp.stop_sequence = stop_sequence.map(str::to_string);
        let body = encode_response(&resp, "m");
        (body["stop_reason"].clone(), body["stop_sequence"].clone())
    };
    assert_eq!(
        stop(FinishReason::Stop, None),
        (json!("end_turn"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::Stop, Some("END")),
        (json!("stop_sequence"), json!("END"))
    );
    assert_eq!(
        stop(FinishReason::Length, None),
        (json!("max_tokens"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::ToolCalls, None),
        (json!("tool_use"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::PauseTurn, None),
        (json!("pause_turn"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::Refusal, None),
        (json!("refusal"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::ContentFilter, None),
        (json!("refusal"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::ContextWindow, None),
        (json!("model_context_window_exceeded"), Value::Null)
    );
    assert_eq!(
        stop(FinishReason::Other("RECITATION".into()), None),
        (json!("end_turn"), Value::Null)
    );
    // A failed generation has no stop reason.
    assert_eq!(stop(FinishReason::Error, None), (Value::Null, Value::Null));
    // A stop sequence is only reported with its own stop reason.
    assert_eq!(
        stop(FinishReason::Length, Some("END")),
        (json!("max_tokens"), Value::Null)
    );
}

#[test]
fn encode_response_plain_stop_with_a_pending_tool_call_is_tool_use() {
    // Some upstreams report "stop" next to tool calls; Messages clients key
    // their tool loop on stop_reason.
    let resp = response(
        vec![Part::tool_call("call_1", "f", "{}")],
        FinishReason::Stop,
    );
    assert_eq!(
        encode_response(&resp, "m")["stop_reason"],
        json!("tool_use")
    );
    let resp = response(
        vec![Part::tool_call("call_1", "f", "{")],
        FinishReason::Length,
    );
    assert_eq!(
        encode_response(&resp, "m")["stop_reason"],
        json!("max_tokens")
    );
}

#[test]
fn encode_response_usage_exact() {
    let mut resp = response(vec![Part::text("x")], FinishReason::Stop);
    resp.usage = Usage {
        input_tokens: 2095,
        cache_read_tokens: 1000,
        cache_write_tokens: 2051,
        output_tokens: 503,
        reasoning_tokens: 120,
    };
    resp.service_tier = Some("priority".into());
    assert_eq!(
        encode_response(&resp, "m")["usage"],
        json!({
            "input_tokens": 2095,
            "cache_creation_input_tokens": 2051,
            "cache_read_input_tokens": 1000,
            "output_tokens": 503,
            "output_tokens_details": {"thinking_tokens": 120},
            "service_tier": "priority"
        })
    );
    // Another vendor's tier name is not an Anthropic value.
    resp.service_tier = Some("flex".into());
    assert!(
        encode_response(&resp, "m")["usage"]
            .get("service_tier")
            .is_none()
    );
}

#[test]
fn encode_response_citations_are_best_effort() {
    let resp = response(
        vec![Part::Text(TextPart {
            text: "Rust is fast.".into(),
            cache_control: None,
            citations: vec![
                Citation {
                    url: Some("https://rust-lang.org".into()),
                    title: Some("Rust".into()),
                    cited_text: Some("fast".into()),
                    start: Some(8),
                    end: Some(12),
                },
                Citation {
                    url: None,
                    title: Some("Manual".into()),
                    cited_text: Some("blazingly".into()),
                    start: None,
                    end: None,
                },
                Citation::default(),
            ],
            signature: None,
        })],
        FinishReason::Stop,
    );
    assert_eq!(
        encode_response(&resp, "m")["content"],
        json!([{"type": "text", "text": "Rust is fast.", "citations": [
            {"type": "web_search_result_location", "url": "https://rust-lang.org", "title": "Rust",
             "encrypted_index": "", "cited_text": "fast"},
            {"type": "char_location", "cited_text": "blazingly", "document_index": 0,
             "document_title": "Manual", "start_char_index": 0, "end_char_index": 9}
        ]}])
    );
}

#[test]
fn encode_response_drops_what_a_message_cannot_carry() {
    let native =
        json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}});
    let resp = response(
        vec![
            Part::text(""),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: native.clone(),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::OpenaiResponses,
                raw: json!({"type": "web_search_call", "id": "ws_1"}),
            }),
            Part::tool_result_text("x", "y"),
            Part::text("done"),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode_response(&resp, "m")["content"],
        json!([native, {"type": "text", "text": "done"}])
    );
}
