//! `decode_response` / `encode_response`.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, OpaquePart, Part, Reasoning, RefusalPart, Response,
    Signature, TextPart, ToolCall, ToolCallKind, ToolResult,
};
use switchyard_core::{ClientCtx, Codec, CodecError, Protocol, Usage};

fn decode(body: Value) -> Response {
    GeminiCodec
        .decode_response(&body)
        .expect("response decodes")
}

fn encode(response: &Response) -> Value {
    GeminiCodec
        .encode_response(response, &ClientCtx::new("gemini-pro-alias"))
        .expect("response encodes")
}

fn response(parts: Vec<Part>, finish: FinishReason) -> Response {
    let mut r = Response::new("resp-1", "gemini-2.5-pro");
    r.parts = parts;
    r.finish = finish;
    r
}

fn gemini_sig(data: &str) -> Option<Signature> {
    Some(Signature::new(Protocol::Gemini, data))
}

fn reasoning(text: &str, signature: Option<Signature>) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature,
        redacted: false,
    })
}

fn candidate(parts: Value, finish: &str) -> Value {
    json!({
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": finish, "index": 0}],
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp-1"
    })
}

// ---------------------------------------------------------------------------
// decode_response
// ---------------------------------------------------------------------------

#[test]
fn decode_text_response() {
    let decoded = decode(json!({
        "candidates": [{
            "content": {"parts": [{"text": "Hello! How can I help?"}], "role": "model"},
            "finishReason": "STOP",
            "index": 0,
            "avgLogprobs": -0.12,
            "safetyRatings": [{"category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE"}]
        }],
        "usageMetadata": {
            "promptTokenCount": 8,
            "candidatesTokenCount": 7,
            "totalTokenCount": 15,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 8}]
        },
        "modelVersion": "gemini-2.5-flash",
        "responseId": "mAitaLmkHPPlz7IPvtfUqQ4"
    }));
    let mut expected = Response::new("mAitaLmkHPPlz7IPvtfUqQ4", "gemini-2.5-flash");
    expected.parts = vec![Part::text("Hello! How can I help?")];
    expected.finish = FinishReason::Stop;
    expected.usage = Usage {
        input_tokens: 8,
        output_tokens: 7,
        ..Usage::default()
    };
    assert_eq!(decoded, expected);
}

#[test]
fn decode_parallel_tool_calls() {
    let decoded = decode(candidate(
        json!([
            {"text": "Checking both."},
            {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "fc-1"}, "thoughtSignature": "U0lHMQ=="},
            {"functionCall": {"name": "get_weather", "args": {"city": "Rome"}}},
            {"functionCall": {"name": "ping"}}
        ]),
        "STOP",
    ));
    // Gemini says STOP; the canonical reason is ToolCalls.
    assert_eq!(decoded.finish, FinishReason::ToolCalls);
    assert_eq!(decoded.parts[0], Part::text("Checking both."));
    let calls: Vec<&ToolCall> = decoded.tool_calls().collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].id, "fc-1");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, r#"{"city":"Paris"}"#);
    assert_eq!(calls[0].signature, gemini_sig("U0lHMQ=="));
    assert_eq!(calls[0].kind, ToolCallKind::Function);
    // No id from the upstream: one is minted.
    assert!(
        calls[1].id.starts_with("call_") && calls[1].id.len() == 29,
        "{}",
        calls[1].id
    );
    assert_ne!(calls[1].id, calls[2].id);
    assert_eq!(calls[1].signature, None);
    assert_eq!(calls[2].arguments, "{}");
}

#[test]
fn decode_reasoning_with_signature() {
    let decoded = decode(candidate(
        json!([
            {"text": "step 1\n", "thought": true},
            {"text": "step 2", "thought": true, "thoughtSignature": "U0lH"},
            {"text": "new thought", "thought": true},
            {"thought_signature": "VFJBSUw="},
            {"text": "visible "},
            {"text": "answer"}
        ]),
        "STOP",
    ));
    assert_eq!(
        decoded.parts,
        vec![
            reasoning("step 1\nstep 2", gemini_sig("U0lH")),
            reasoning("new thought", gemini_sig("VFJBSUw=")),
            Part::text("visible answer"),
        ]
    );
    assert_eq!(decoded.reasoning_text(), "step 1\nstep 2new thought");
    assert_eq!(decoded.finish, FinishReason::Stop);
}

#[test]
fn decode_signatures_on_text_parts() {
    let decoded = decode(candidate(
        json!([
            {"text": "First.", "thoughtSignature": "QQ=="},
            {"text": " Second."},
            {"text": " Third."},
            {"text": "", "thoughtSignature": "Qg=="},
            {"text": ""},
            {"text": "x", "thoughtSignature": "skip_thought_signature_validator"}
        ]),
        "STOP",
    ));
    assert_eq!(
        decoded.parts,
        vec![
            // A signed part is never merged: the signature belongs to it.
            Part::Text(TextPart {
                text: "First.".into(),
                signature: gemini_sig("QQ=="),
                ..TextPart::default()
            }),
            Part::text(" Second. Third."),
            // The empty signed part Gemini 3 ends a turn with.
            reasoning("", gemini_sig("Qg==")),
            // A bypass literal is not a signature.
            Part::text("x"),
        ]
    );
}

#[test]
fn decode_blocked_prompt_is_a_refusal() {
    let decoded = decode(json!({
        "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"},
        "usageMetadata": {"promptTokenCount": 12, "totalTokenCount": 12},
        "modelVersion": "gemini-2.5-flash",
        "responseId": "blocked-1"
    }));
    assert_eq!(decoded.finish, FinishReason::ContentFilter);
    assert_eq!(
        decoded.parts,
        vec![Part::Refusal(RefusalPart {
            text: "The prompt was blocked by Gemini (PROHIBITED_CONTENT).".into()
        })]
    );
    assert_eq!(decoded.usage.input_tokens, 12);

    let with_message = decode(json!({
        "candidates": [],
        "promptFeedback": {"blockReason": "SAFETY", "blockReasonMessage": "Blocked for safety."}
    }));
    assert_eq!(
        with_message.parts,
        vec![Part::Refusal(RefusalPart {
            text: "Blocked for safety.".into()
        })]
    );
}

#[test]
fn decode_safety_stop_without_content() {
    let decoded = decode(json!({
        "candidates": [{"finishReason": "SAFETY", "index": 0, "finishMessage": "Response blocked."}],
        "modelVersion": "m", "responseId": "r"
    }));
    assert_eq!(decoded.finish, FinishReason::ContentFilter);
    assert_eq!(
        decoded.parts,
        vec![Part::Refusal(RefusalPart {
            text: "Response blocked.".into()
        })]
    );

    let bare = decode(json!({"candidates": [{"content": {}, "finishReason": "RECITATION"}]}));
    assert_eq!(bare.finish, FinishReason::ContentFilter);
    assert!(bare.parts.is_empty());
}

#[test]
fn decode_every_finish_reason() {
    let finish = |reason: Value, parts: Value| {
        decode(json!({"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": reason}]})).finish
    };
    let text = json!([{"text": "x"}]);
    for (reason, expected) in [
        ("STOP", FinishReason::Stop),
        ("MAX_TOKENS", FinishReason::Length),
        ("SAFETY", FinishReason::ContentFilter),
        ("RECITATION", FinishReason::ContentFilter),
        ("BLOCKLIST", FinishReason::ContentFilter),
        ("PROHIBITED_CONTENT", FinishReason::ContentFilter),
        ("SPII", FinishReason::ContentFilter),
        ("IMAGE_SAFETY", FinishReason::ContentFilter),
        ("MALFORMED_FUNCTION_CALL", FinishReason::Error),
        ("UNEXPECTED_TOOL_CALL", FinishReason::Error),
        ("MISSING_THOUGHT_SIGNATURE", FinishReason::Error),
        ("FINISH_REASON_UNSPECIFIED", FinishReason::Stop),
        ("OTHER", FinishReason::Other("OTHER".into())),
        ("LANGUAGE", FinishReason::Other("LANGUAGE".into())),
        ("NO_IMAGE", FinishReason::Other("NO_IMAGE".into())),
        ("stop", FinishReason::Stop),
    ] {
        assert_eq!(finish(json!(reason), text.clone()), expected, "{reason}");
    }
    // Absent or null reason on a complete response is a normal stop.
    assert_eq!(finish(Value::Null, text.clone()), FinishReason::Stop);
    assert_eq!(
        decode(json!({"candidates": [{"content": {"parts": [{"text": "x"}]}}]})).finish,
        FinishReason::Stop
    );

    // Function calls turn STOP, MAX_TOKENS and unknown reasons into
    // ToolCalls, but never hide a safety block or an error.
    let call = json!([{"functionCall": {"name": "f", "args": {}}}]);
    assert_eq!(finish(json!("STOP"), call.clone()), FinishReason::ToolCalls);
    assert_eq!(
        finish(json!("MAX_TOKENS"), call.clone()),
        FinishReason::ToolCalls
    );
    assert_eq!(
        finish(json!("OTHER"), call.clone()),
        FinishReason::ToolCalls
    );
    assert_eq!(
        finish(json!("SAFETY"), call.clone()),
        FinishReason::ContentFilter
    );
    assert_eq!(
        finish(json!("MALFORMED_FUNCTION_CALL"), call),
        FinishReason::Error
    );
}

#[test]
fn decode_usage_buckets() {
    let usage = |meta: Value| {
        decode(json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "STOP"}], "usageMetadata": meta}))
            .usage
    };
    // promptTokenCount includes cached tokens; candidatesTokenCount excludes
    // thoughts.
    assert_eq!(
        usage(json!({
            "promptTokenCount": 100,
            "cachedContentTokenCount": 60,
            "candidatesTokenCount": 20,
            "thoughtsTokenCount": 30,
            "totalTokenCount": 150
        })),
        Usage {
            input_tokens: 40,
            cache_read_tokens: 60,
            cache_write_tokens: 0,
            output_tokens: 50,
            reasoning_tokens: 30,
        }
    );
    // Thoughts only (the answer was cut off while thinking).
    assert_eq!(
        usage(json!({"promptTokenCount": 10, "thoughtsTokenCount": 42, "totalTokenCount": 52})),
        Usage {
            input_tokens: 10,
            output_tokens: 42,
            reasoning_tokens: 42,
            ..Usage::default()
        }
    );
    // Tool-use prompt tokens are prompt tokens too.
    assert_eq!(
        usage(
            json!({"promptTokenCount": 10, "toolUsePromptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 18})
        ),
        Usage {
            input_tokens: 15,
            output_tokens: 3,
            ..Usage::default()
        }
    );
    // snake_case and float counts.
    assert_eq!(
        usage(
            json!({"prompt_token_count": 9.0, "cached_content_token_count": 4, "candidates_token_count": 2})
        ),
        Usage {
            input_tokens: 5,
            cache_read_tokens: 4,
            output_tokens: 2,
            ..Usage::default()
        }
    );
    // More cached than prompt (never negative).
    assert_eq!(
        usage(json!({"promptTokenCount": 5, "cachedContentTokenCount": 9})),
        Usage {
            input_tokens: 0,
            cache_read_tokens: 9,
            ..Usage::default()
        }
    );
    // A server that folds thoughts into candidates is not double counted.
    assert_eq!(
        usage(
            json!({"promptTokenCount": 10, "candidatesTokenCount": 50, "thoughtsTokenCount": 30, "totalTokenCount": 60})
        ),
        Usage {
            input_tokens: 10,
            output_tokens: 50,
            reasoning_tokens: 30,
            ..Usage::default()
        }
    );
}

#[test]
fn decode_grounding_metadata_into_citations_and_keeps_it() {
    let grounding = json!({
        "webSearchQueries": ["café opening hours"],
        "groundingChunks": [
            {"web": {"uri": "https://example.com/a", "title": "A"}},
            {"web": {"uri": "https://example.com/b", "title": "B"}}
        ],
        "groundingSupports": [
            {"segment": {"endIndex": 13, "text": "Le café ouvre"}, "groundingChunkIndices": [0]},
            {"segment": {"startIndex": 14, "endIndex": 20, "text": "à 8h."}, "groundingChunkIndices": [1, 0]}
        ]
    });
    let decoded = decode(json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Le café ouvre à 8h."}]},
            "finishReason": "STOP",
            "groundingMetadata": grounding
        }]
    }));
    let Part::Text(text) = &decoded.parts[0] else {
        panic!("{:?}", decoded.parts)
    };
    let cite = |url: &str, title: &str, cited: &str, start: u64, end: u64| Citation {
        url: Some(url.into()),
        title: Some(title.into()),
        cited_text: Some(cited.into()),
        start: Some(start),
        end: Some(end),
    };
    // Gemini's offsets are bytes ("é" and "à" are two each); citations use
    // characters.
    assert_eq!(
        text.citations,
        vec![
            cite("https://example.com/a", "A", "Le café ouvre", 0, 12),
            cite("https://example.com/b", "B", "à 8h.", 13, 18),
            cite("https://example.com/a", "A", "à 8h.", 13, 18),
        ]
    );
    assert_eq!(
        decoded.parts[1],
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw: json!({"groundingMetadata": grounding})
        })
    );
}

#[test]
fn decode_grounding_sources_without_supports() {
    let decoded = decode(json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Answer."}]},
            "finishReason": "STOP",
            "groundingMetadata": {"groundingChunks": [{"web": {"uri": "https://example.com", "title": "Example"}}]}
        }]
    }));
    let Part::Text(text) = &decoded.parts[0] else {
        panic!()
    };
    assert_eq!(
        text.citations,
        vec![Citation {
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            ..Citation::default()
        }]
    );
}

#[test]
fn decode_media_and_unknown_parts() {
    let code = json!({"executableCode": {"language": "PYTHON", "code": "print(2)"}});
    let decoded = decode(candidate(
        json!([
            {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
            code.clone(),
            {"fileData": {"mimeType": "video/mp4", "fileUri": "https://example.com/v.mp4"}},
            {"text": "Done."}
        ]),
        "STOP",
    ));
    assert_eq!(
        decoded.parts[0],
        Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo="))
    );
    assert_eq!(
        decoded.parts[1],
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw: code
        })
    );
    assert!(
        matches!(&decoded.parts[2], Part::Document(m) if m.media_type.as_deref() == Some("video/mp4"))
    );
    assert_eq!(decoded.parts[3], Part::text("Done."));
}

#[test]
fn decode_is_liberal_about_envelopes_and_ids() {
    // Relays that wrap the payload.
    let wrapped = decode(json!({"response": candidate(json!([{"text": "hi"}]), "STOP")}));
    assert_eq!(wrapped.text(), "hi");
    assert_eq!(wrapped.id, "resp-1");
    // Vertex adds createTime.
    let vertex = decode(json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
        "createTime": "2024-01-01T00:00:00.123456Z",
        "modelVersion": "gemini-2.5-pro",
        "responseId": "v1",
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2, "trafficType": "ON_DEMAND"}
    }));
    assert_eq!(vertex.created, 1_704_067_200);
    // No responseId: one is minted. No modelVersion: empty model.
    let anonymous = decode(json!({"candidates": [{"content": {"parts": [{"text": "hi"}]}}]}));
    assert_eq!(anonymous.id.len(), 24);
    assert!(anonymous.id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(anonymous.model, "");
    // Only the first candidate is used.
    let two = decode(json!({"candidates": [
        {"content": {"parts": [{"text": "first"}]}, "finishReason": "STOP", "index": 0},
        {"content": {"parts": [{"text": "second"}]}, "finishReason": "MAX_TOKENS", "index": 1}
    ]}));
    assert_eq!(two.text(), "first");
    assert_eq!(two.finish, FinishReason::Stop);
}

#[test]
fn decode_rejects_payloads_that_are_not_responses() {
    for body in [
        json!("text"),
        json!([]),
        json!({}),
        json!({"unrelated": true}),
        json!({"error": {"code": 500, "message": "boom", "status": "INTERNAL"}}),
    ] {
        let err = GeminiCodec.decode_response(&body).unwrap_err();
        assert!(
            matches!(err, CodecError::InvalidUpstream(_)),
            "{body}: {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// encode_response
// ---------------------------------------------------------------------------

#[test]
fn encode_text_response() {
    let mut r = response(vec![Part::text("Hello!")], FinishReason::Stop);
    r.usage = Usage {
        input_tokens: 8,
        output_tokens: 7,
        ..Usage::default()
    };
    assert_eq!(
        encode(&r),
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hello!"}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 7, "totalTokenCount": 15},
            // The name the client asked for, not the upstream's.
            "modelVersion": "gemini-pro-alias",
            "responseId": "resp-1"
        })
    );
}

#[test]
fn encode_parallel_tool_calls() {
    let call = |id: &str, args: &str, signature: Option<Signature>| {
        Part::ToolCall(ToolCall {
            id: id.into(),
            name: "get_weather".into(),
            arguments: args.into(),
            kind: ToolCallKind::Function,
            signature,
            cache_control: None,
        })
    };
    let r = response(
        vec![
            Part::text("Checking."),
            call("fc-1", r#"{"city":"Paris"}"#, gemini_sig("U0lHMQ==")),
            call("toolu_01ABC", "", None),
            call(
                "call_x",
                "not json",
                Some(Signature::new(Protocol::Anthropic, "ErAC")),
            ),
        ],
        FinishReason::ToolCalls,
    );
    assert_eq!(
        encode(&r)["candidates"][0],
        json!({
            "content": {"role": "model", "parts": [
                {"text": "Checking."},
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "fc-1"}, "thoughtSignature": "U0lHMQ=="},
                {"functionCall": {"name": "get_weather", "args": {}, "id": "toolu_01ABC"}},
                // base64("sy1.a.ErAC"): the field is `bytes`, so the tagged
                // foreign blob is armoured.
                {"functionCall": {"name": "get_weather", "args": {"input": "not json"}, "id": "call_x"}, "thoughtSignature": "c3kxLmEuRXJBQw=="}
            ]},
            // Gemini has no tool-call finish reason.
            "finishReason": "STOP",
            "index": 0
        })
    );
}

#[test]
fn encode_reasoning_with_signatures() {
    let r = response(
        vec![
            reasoning("gemini thought", gemini_sig("R1NJRw==")),
            reasoning(
                "claude thought",
                Some(Signature::new(Protocol::Anthropic, "ErACkgE=")),
            ),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: String::new(),
                signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAAAB")),
                redacted: true,
            }),
            reasoning("unsigned", None),
            reasoning("", None),
            Part::Text(TextPart {
                text: "Answer.".into(),
                signature: gemini_sig("VFNJRw=="),
                ..TextPart::default()
            }),
            reasoning("", gemini_sig("VFJBSUw=")),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode(&r)["candidates"][0]["content"]["parts"],
        json!([
            {"text": "gemini thought", "thought": true, "thoughtSignature": "R1NJRw=="},
            // Foreign blobs are tagged so they are recognised when replayed,
            // and armoured because `thoughtSignature` must be base64:
            // base64("sy1.a.ErACkgE="). The payload of withheld reasoning is
            // marked as such inside the tag, because a Gemini part has no
            // other way to say "this blob is the reasoning":
            // base64("sy1.r.redacted:gAAAAAB").
            {"text": "claude thought", "thought": true, "thoughtSignature": "c3kxLmEuRXJBQ2tnRT0="},
            {"text": "", "thoughtSignature": "c3kxLnIucmVkYWN0ZWQ6Z0FBQUFBQg=="},
            {"text": "unsigned", "thought": true},
            {"text": "Answer.", "thoughtSignature": "VFNJRw=="},
            {"text": "", "thoughtSignature": "VFJBSUw="}
        ])
    );
}

#[test]
fn encode_refusal() {
    let r = response(
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into(),
        })],
        FinishReason::Refusal,
    );
    assert_eq!(
        encode(&r)["candidates"][0],
        json!({
            "content": {"role": "model", "parts": [{"text": "I can't help with that."}]},
            "finishReason": "SAFETY",
            "index": 0
        })
    );
}

#[test]
fn encode_every_finish_reason() {
    for (reason, expected) in [
        (FinishReason::Stop, "STOP"),
        (FinishReason::ToolCalls, "STOP"),
        (FinishReason::PauseTurn, "STOP"),
        (FinishReason::Length, "MAX_TOKENS"),
        (FinishReason::ContextWindow, "MAX_TOKENS"),
        (FinishReason::ContentFilter, "SAFETY"),
        (FinishReason::Refusal, "SAFETY"),
        (FinishReason::Error, "OTHER"),
        (FinishReason::Other("RECITATION".into()), "RECITATION"),
        (FinishReason::Other("language".into()), "LANGUAGE"),
        (
            FinishReason::Other("model_context_window_exceeded".into()),
            "OTHER",
        ),
        (
            FinishReason::Other("FINISH_REASON_UNSPECIFIED".into()),
            "OTHER",
        ),
    ] {
        let r = response(vec![Part::text("x")], reason.clone());
        assert_eq!(
            encode(&r)["candidates"][0]["finishReason"],
            expected,
            "{reason:?}"
        );
    }
}

#[test]
fn encode_usage_in_gemini_convention() {
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.usage = Usage {
        input_tokens: 40,
        cache_read_tokens: 60,
        cache_write_tokens: 5,
        output_tokens: 50,
        reasoning_tokens: 30,
    };
    assert_eq!(
        encode(&r)["usageMetadata"],
        json!({
            // Everything on the prompt side, cached or not.
            "promptTokenCount": 105,
            "cachedContentTokenCount": 60,
            // Output without the thoughts, which Gemini reports separately.
            "candidatesTokenCount": 20,
            "thoughtsTokenCount": 30,
            "totalTokenCount": 155
        })
    );
    r.usage = Usage::default();
    assert_eq!(
        encode(&r)["usageMetadata"],
        json!({"promptTokenCount": 0, "candidatesTokenCount": 0, "totalTokenCount": 0})
    );
}

#[test]
fn encode_ids_and_model() {
    // Ids of any shape pass through untouched.
    for id in [
        "chatcmpl-abc123",
        "msg_01XYZ",
        "resp_68a1",
        "mAitaLmkHPPlz7IPvtfUqQ4",
    ] {
        let mut r = response(vec![Part::text("x")], FinishReason::Stop);
        r.id = id.into();
        assert_eq!(encode(&r)["responseId"], id);
    }
    // An empty id is replaced by a fresh one.
    let mut r = response(vec![Part::text("x")], FinishReason::Stop);
    r.id = String::new();
    let first = encode(&r)["responseId"].as_str().unwrap().to_string();
    let second = encode(&r)["responseId"].as_str().unwrap().to_string();
    assert_eq!(first.len(), 24);
    assert_ne!(first, second);
    // Without a client model name the upstream's is reported.
    let body = GeminiCodec
        .encode_response(&r, &ClientCtx::new(""))
        .unwrap();
    assert_eq!(body["modelVersion"], "gemini-2.5-pro");
}

#[test]
fn encode_parts_gemini_cannot_express_are_dropped() {
    let gemini_code = json!({"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "2\n"}});
    let r = response(
        vec![
            Part::text(""),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
            Part::Opaque(OpaquePart {
                origin: Protocol::Gemini,
                raw: gemini_code.clone(),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []}),
            }),
            Part::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: None,
                content: vec![Part::text("not an assistant part")],
                is_error: false,
                cache_control: None,
            }),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode(&r)["candidates"][0]["content"]["parts"],
        json!([{"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}}, gemini_code])
    );
    // No parts at all still yields a well-formed candidate.
    let empty = response(vec![], FinishReason::ContentFilter);
    assert_eq!(
        encode(&empty)["candidates"][0],
        json!({"content": {"role": "model", "parts": []}, "finishReason": "SAFETY", "index": 0})
    );
}

#[test]
fn encode_custom_tool_call() {
    let r = response(
        vec![Part::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "apply_patch".into(),
            arguments: "{\"looks\": \"like json\"}".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })],
        FinishReason::ToolCalls,
    );
    assert_eq!(
        encode(&r)["candidates"][0]["content"]["parts"][0]["functionCall"],
        json!({"name": "apply_patch", "args": {"input": "{\"looks\": \"like json\"}"}, "id": "call_1"})
    );
}

#[test]
fn encode_gemini_grounding_metadata_goes_back_on_the_candidate() {
    let grounding = json!({"webSearchQueries": ["q"], "groundingChunks": [{"web": {"uri": "https://e.com", "title": "E"}}]});
    let r = response(
        vec![
            Part::Text(TextPart {
                text: "Answer.".into(),
                citations: vec![Citation {
                    url: Some("https://e.com".into()),
                    ..Citation::default()
                }],
                ..TextPart::default()
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Gemini,
                raw: json!({"groundingMetadata": grounding}),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Gemini,
                raw: json!({"urlContextMetadata": {"urlMetadata": []}}),
            }),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode(&r)["candidates"][0],
        json!({
            "content": {"role": "model", "parts": [{"text": "Answer."}]},
            "finishReason": "STOP",
            "index": 0,
            "groundingMetadata": grounding,
            "urlContextMetadata": {"urlMetadata": []}
        })
    );
}

#[test]
fn encode_foreign_citations_become_grounding_metadata() {
    let cite = |url: &str, start: Option<u64>, end: Option<u64>, cited: Option<&str>| Citation {
        url: Some(url.into()),
        title: Some("Source".into()),
        cited_text: cited.map(str::to_owned),
        start,
        end,
    };
    let r = response(
        vec![
            Part::text("Intro. "),
            Part::Text(TextPart {
                text: "Le café ouvre à 8h.".into(),
                citations: vec![
                    cite(
                        "https://example.com/a",
                        Some(0),
                        Some(12),
                        Some("Le café ouvre"),
                    ),
                    cite("https://example.com/b", Some(13), Some(18), None),
                    cite("https://example.com/a", None, None, None),
                ],
                ..TextPart::default()
            }),
        ],
        FinishReason::Stop,
    );
    assert_eq!(
        encode(&r)["candidates"][0]["groundingMetadata"],
        json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/a", "title": "Source"}},
                {"web": {"uri": "https://example.com/b", "title": "Source"}}
            ],
            "groundingSupports": [
                {"segment": {"partIndex": 1, "startIndex": 0, "endIndex": 13, "text": "Le café ouvre"}, "groundingChunkIndices": [0]},
                {"segment": {"partIndex": 1, "startIndex": 14, "endIndex": 20}, "groundingChunkIndices": [1]}
            ]
        })
    );
}
