//! The inspect / patch helpers used for same-protocol passthrough, model
//! listings, and the trait's static facts.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::Protocol;
use switchyard_core::codec::{
    Codec, MaxTokensField, Quirks, RequestMeta, RequestPath, UpstreamCtx,
};
use switchyard_core::error::CodecError;
use switchyard_core::ir::Request;
use switchyard_core::model::ModelInfo;
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};

// ---------------------------------------------------------------------------
// request_meta / set_request_model
// ---------------------------------------------------------------------------

fn meta(body: Value) -> Result<RequestMeta, CodecError> {
    ChatCodec.request_meta(&body, &RequestPath::default())
}

#[test]
fn request_meta_reads_model_and_stream() {
    assert_eq!(
        meta(json!({"model": "gpt-4o(high)", "stream": true, "messages": []})),
        Ok(RequestMeta {
            model: "gpt-4o(high)".into(),
            stream: true
        })
    );
    assert_eq!(
        meta(json!({"model": "team/claude"})),
        Ok(RequestMeta {
            model: "team/claude".into(),
            stream: false
        })
    );
    // Only the JSON literal `true` streams.
    for not_true in [json!(false), json!("true"), json!(1), Value::Null] {
        assert_eq!(
            meta(json!({"model": "m", "stream": not_true})).map(|m| m.stream),
            Ok(false)
        );
    }
}

#[test]
fn request_meta_rejects_bodies_without_a_usable_model() {
    assert_eq!(
        meta(json!({"messages": []})),
        Err(CodecError::invalid_param("model", "`model` is required"))
    );
    assert_eq!(
        meta(json!({"model": "  "})),
        Err(CodecError::invalid_param("model", "`model` is required"))
    );
    assert_eq!(
        meta(json!({"model": 42})),
        Err(CodecError::invalid_param(
            "model",
            "`model` must be a string"
        ))
    );
    assert_eq!(
        meta(json!([1, 2, 3])),
        Err(CodecError::invalid("request body must be a JSON object"))
    );
}

#[test]
fn request_meta_falls_back_to_the_path() {
    let path = RequestPath {
        model: Some("path-model"),
        stream: Some(true),
    };
    assert_eq!(
        ChatCodec.request_meta(&json!({"stream": false}), &path),
        Ok(RequestMeta {
            model: "path-model".into(),
            stream: true
        })
    );
    // A model in the body wins.
    assert_eq!(
        ChatCodec
            .request_meta(&json!({"model": "body-model"}), &path)
            .map(|m| m.model),
        Ok("body-model".to_string())
    );
}

#[test]
fn set_request_model_replaces_in_place() {
    let mut body = json!({"model": "alias(high)", "messages": [], "temperature": 1});
    ChatCodec.set_request_model(&mut body, "gpt-4o-2024-08-06");
    assert_eq!(
        body.to_string(),
        r#"{"model":"gpt-4o-2024-08-06","messages":[],"temperature":1}"#
    );
    let mut body = json!({"messages": []});
    ChatCodec.set_request_model(&mut body, "m");
    assert_eq!(body, json!({"messages": [], "model": "m"}));
    let mut not_an_object = json!("text");
    ChatCodec.set_request_model(&mut not_an_object, "m");
    assert_eq!(not_an_object, json!("text"));
}

// ---------------------------------------------------------------------------
// rewrite_response_model
// ---------------------------------------------------------------------------

#[test]
fn rewrite_response_model_on_full_responses_and_stream_chunks() {
    let mut response = json!({
        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o-2024-08-06",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "the model is gpt-4o"},
                     "finish_reason": "stop"}]
    });
    ChatCodec.rewrite_response_model(&mut response, "my-alias");
    assert_eq!(response["model"], json!("my-alias"));
    // Only the top-level field is touched.
    assert_eq!(
        response["choices"][0]["message"]["content"],
        json!("the model is gpt-4o")
    );
    assert_eq!(
        response
            .as_object()
            .expect("object")
            .keys()
            .collect::<Vec<_>>(),
        ["id", "object", "created", "model", "choices"]
    );

    let mut chunk = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
        "model": "gpt-4o-2024-08-06",
        "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]
    });
    ChatCodec.rewrite_response_model(&mut chunk, "my-alias");
    assert_eq!(chunk["model"], json!("my-alias"));

    let mut usage_chunk =
        json!({"id": "x", "model": "up", "choices": [], "usage": {"total_tokens": 1}});
    ChatCodec.rewrite_response_model(&mut usage_chunk, "my-alias");
    assert_eq!(usage_chunk["model"], json!("my-alias"));
}

#[test]
fn rewrite_response_model_leaves_payloads_without_a_model_alone() {
    for payload in [
        json!({"error": {"message": "boom", "type": "server_error"}}),
        json!({"model": null, "choices": []}),
        json!("[DONE]"),
        json!([{"model": "nested"}]),
    ] {
        let mut patched = payload.clone();
        ChatCodec.rewrite_response_model(&mut patched, "my-alias");
        assert_eq!(patched, payload);
    }
}

// ---------------------------------------------------------------------------
// read_reasoning
// ---------------------------------------------------------------------------

#[test]
fn read_reasoning_from_raw_bodies() {
    let read = |body: Value| ChatCodec.read_reasoning(&body);
    assert_eq!(read(json!({"model": "m"})), ReasoningConfig::default());
    assert!(read(json!({"model": "m"})).is_empty());
    assert_eq!(read(json!("not an object")), ReasoningConfig::default());
    assert_eq!(
        read(json!({"reasoning_effort": "medium"})),
        ReasoningConfig::with_depth(Depth::Level(Effort::Medium))
    );
    assert_eq!(
        read(json!({"reasoning_effort": "none"})),
        ReasoningConfig::with_depth(Depth::Off)
    );
    assert_eq!(
        read(json!({"reasoning": {"max_tokens": 8000, "exclude": false}})),
        ReasoningConfig {
            depth: Some(Depth::Budget(8000)),
            summary: Some(Summary::Auto)
        }
    );
    assert_eq!(
        read(json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})),
        ReasoningConfig::with_depth(Depth::Budget(2048))
    );
    assert_eq!(
        read(json!({"enable_thinking": true, "thinking_budget": 512})),
        ReasoningConfig::with_depth(Depth::Budget(512))
    );
    assert_eq!(
        read(json!({"extra_body": {"google": {"thinking_config": {
            "thinking_level": "low", "include_thoughts": true
        }}}})),
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Auto)
        }
    );
    assert_eq!(
        read(json!({"include_reasoning": false})),
        ReasoningConfig {
            depth: None,
            summary: Some(Summary::Off)
        }
    );
}

// ---------------------------------------------------------------------------
// write_reasoning
// ---------------------------------------------------------------------------

fn written(mut body: Value, depth: Fitted, thinking: ModelThinking<'_>) -> Value {
    let ctx = UpstreamCtx {
        thinking,
        ..UpstreamCtx::default()
    };
    ChatCodec.write_reasoning(&mut body, depth, &ctx);
    body
}

fn effort_written(depth: Depth, thinking: ModelThinking<'_>) -> Option<Value> {
    written(
        json!({"model": "m", "reasoning_effort": "medium"}),
        Fitted::Use(depth),
        thinking,
    )
    .get("reasoning_effort")
    .cloned()
}

#[test]
fn write_reasoning_every_depth_for_an_unknown_model() {
    let unknown = ModelThinking::Unknown;
    assert_eq!(effort_written(Depth::Off, unknown), Some(json!("none")));
    // Chat cannot say "provider decides": the field is removed.
    assert_eq!(effort_written(Depth::Auto, unknown), None);
    for effort in Effort::ALL {
        assert_eq!(
            effort_written(Depth::Level(effort), unknown),
            Some(json!(effort.as_str()))
        );
    }
    // Budgets are bucketed.
    for (budget, wire) in [
        (1, "minimal"),
        (512, "minimal"),
        (1024, "low"),
        (8192, "medium"),
        (24576, "high"),
        (64000, "xhigh"),
    ] {
        assert_eq!(
            effort_written(Depth::Budget(budget), unknown),
            Some(json!(wire)),
            "{budget}"
        );
    }
}

#[test]
fn write_reasoning_strip_removes_every_depth_field() {
    let body = json!({
        "model": "m",
        "messages": [],
        "reasoning_effort": "high",
        "reasoning": {"effort": "high", "max_tokens": 4000, "exclude": true},
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "enable_thinking": true,
        "thinking_budget": 2048,
        "extra_body": {"google": {"thinking_config": {"thinking_budget": 2048, "include_thoughts": true}}}
    });
    assert_eq!(
        written(body, Fitted::Strip, ModelThinking::Unknown),
        json!({
            "model": "m",
            "messages": [],
            // Visibility switches are not depth settings and stay.
            "reasoning": {"exclude": true},
            "extra_body": {"google": {"thinking_config": {"include_thoughts": true}}}
        })
    );
}

#[test]
fn write_reasoning_replaces_alternate_spellings_with_reasoning_effort() {
    let body = json!({
        "model": "m",
        "reasoning": {"effort": "high"},
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "enable_thinking": true,
        "thinking_budget": 2048
    });
    assert_eq!(
        written(
            body,
            Fitted::Use(Depth::Level(Effort::Low)),
            ModelThinking::Unknown
        ),
        json!({"model": "m", "reasoning_effort": "low"})
    );
}

#[test]
fn write_reasoning_adds_the_field_when_absent() {
    assert_eq!(
        written(
            json!({"model": "m"}),
            Fitted::Use(Depth::Level(Effort::High)),
            ModelThinking::Unknown
        ),
        json!({"model": "m", "reasoning_effort": "high"})
    );
    // Nothing to remove, nothing to add.
    assert_eq!(
        written(
            json!({"model": "m"}),
            Fitted::Use(Depth::Auto),
            ModelThinking::Unknown
        ),
        json!({"model": "m"})
    );
    assert_eq!(
        written(json!({"model": "m"}), Fitted::Strip, ModelThinking::Unknown),
        json!({"model": "m"})
    );
    // Not an object: left alone.
    assert_eq!(
        written(json!([1]), Fitted::Use(Depth::Off), ModelThinking::Unknown),
        json!([1])
    );
}

#[test]
fn write_reasoning_for_a_model_with_known_levels() {
    let caps = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let supported = ModelThinking::Supported(&caps);
    assert_eq!(
        effort_written(Depth::Level(Effort::Medium), supported),
        Some(json!("medium"))
    );
    // Levels the model lacks move to the nearest one it has.
    assert_eq!(
        effort_written(Depth::Level(Effort::Max), supported),
        Some(json!("high"))
    );
    assert_eq!(
        effort_written(Depth::Level(Effort::Minimal), supported),
        Some(json!("low"))
    );
    assert_eq!(
        effort_written(Depth::Budget(100_000), supported),
        Some(json!("high"))
    );
    // The model cannot switch reasoning off: its lowest level is the floor.
    assert_eq!(effort_written(Depth::Off, supported), Some(json!("low")));
    assert_eq!(effort_written(Depth::Auto, supported), None);

    let caps = ThinkingSupport {
        zero_allowed: true,
        ..ThinkingSupport::levels(&[Effort::Minimal, Effort::Low, Effort::Medium, Effort::High])
    };
    assert_eq!(
        effort_written(Depth::Off, ModelThinking::Supported(&caps)),
        Some(json!("none"))
    );

    // A budget-only model reached over Chat still takes effort strings.
    let caps = ThinkingSupport::budget(128, 32768);
    let supported = ModelThinking::Supported(&caps);
    assert_eq!(
        effort_written(Depth::Budget(2000), supported),
        Some(json!("medium"))
    );
    assert_eq!(
        effort_written(Depth::Level(Effort::Xhigh), supported),
        Some(json!("xhigh"))
    );
}

#[test]
fn write_reasoning_for_a_model_that_does_not_reason() {
    assert_eq!(
        effort_written(Depth::Level(Effort::High), ModelThinking::Unsupported),
        None
    );
    assert_eq!(effort_written(Depth::Off, ModelThinking::Unsupported), None);
}

#[test]
fn write_then_read_is_consistent() {
    for depth in [
        Depth::Off,
        Depth::Level(Effort::Low),
        Depth::Level(Effort::Max),
    ] {
        let body = written(
            json!({"model": "m"}),
            Fitted::Use(depth),
            ModelThinking::Unknown,
        );
        assert_eq!(ChatCodec.read_reasoning(&body).depth, Some(depth));
    }
}

// ---------------------------------------------------------------------------
// prepare_passthrough
// ---------------------------------------------------------------------------

fn prepared(mut body: Value, stream: bool, quirks: Quirks) -> Value {
    let ctx = UpstreamCtx {
        quirks,
        ..UpstreamCtx::default()
    };
    ChatCodec.prepare_passthrough(&mut body, stream, &ctx);
    body
}

#[test]
fn prepare_passthrough_asks_for_stream_usage() {
    let quirks = Quirks::default();
    assert_eq!(
        prepared(json!({"model": "m", "stream": true}), true, quirks),
        json!({"model": "m", "stream": true, "stream_options": {"include_usage": true}})
    );
    // Existing options are kept; an explicit `false` is overridden because
    // the gateway needs the numbers.
    assert_eq!(
        prepared(
            json!({"model": "m", "stream": true,
                   "stream_options": {"include_usage": false, "include_obfuscation": false}}),
            true,
            quirks
        ),
        json!({"model": "m", "stream": true,
               "stream_options": {"include_usage": true, "include_obfuscation": false}})
    );
    assert_eq!(
        prepared(
            json!({"model": "m", "stream": true, "stream_options": null}),
            true,
            quirks
        ),
        json!({"model": "m", "stream": true, "stream_options": {"include_usage": true}})
    );
}

#[test]
fn prepare_passthrough_leaves_non_streaming_requests_alone() {
    let body = json!({"model": "m", "messages": [], "temperature": 0.3, "unknown_field": {"x": 1}});
    assert_eq!(prepared(body.clone(), false, Quirks::default()), body);
}

#[test]
fn prepare_passthrough_for_upstreams_without_stream_usage() {
    let quirks = Quirks {
        stream_usage: false,
        ..Quirks::default()
    };
    assert_eq!(
        prepared(json!({"model": "m", "stream": true}), true, quirks),
        json!({"model": "m", "stream": true})
    );
    // Such an upstream rejects the option, even when the client sent it.
    assert_eq!(
        prepared(
            json!({"model": "m", "stream": true, "stream_options": {"include_usage": true}}),
            true,
            quirks
        ),
        json!({"model": "m", "stream": true})
    );
}

#[test]
fn prepare_passthrough_moves_the_token_limit_to_the_field_the_upstream_knows() {
    let legacy = Quirks {
        max_tokens_field: MaxTokensField::MaxTokens,
        stream_usage: true,
    };
    assert_eq!(
        prepared(
            json!({"model": "m", "max_completion_tokens": 100}),
            false,
            legacy
        ),
        json!({"model": "m", "max_tokens": 100})
    );
    assert_eq!(
        prepared(json!({"model": "m", "max_tokens": 100}), false, legacy),
        json!({"model": "m", "max_tokens": 100})
    );
    // Both present: the upstream's own field wins, the other is removed.
    assert_eq!(
        prepared(
            json!({"model": "m", "max_tokens": 50, "max_completion_tokens": 100}),
            false,
            legacy
        ),
        json!({"model": "m", "max_tokens": 50})
    );

    let modern = Quirks::default();
    assert_eq!(
        prepared(json!({"model": "m", "max_tokens": 100}), false, modern),
        json!({"model": "m", "max_completion_tokens": 100})
    );
    assert_eq!(
        prepared(
            json!({"model": "m", "max_completion_tokens": 100}),
            false,
            modern
        ),
        json!({"model": "m", "max_completion_tokens": 100})
    );
    assert_eq!(
        prepared(json!({"model": "m", "max_tokens": null}), false, modern),
        json!({"model": "m"})
    );
}

#[test]
fn prepare_passthrough_ignores_non_objects() {
    assert_eq!(
        prepared(json!("text"), true, Quirks::default()),
        json!("text")
    );
}

// ---------------------------------------------------------------------------
// Models, protocol, counting
// ---------------------------------------------------------------------------

#[test]
fn model_listing_in_openai_shape() {
    let models = [
        ModelInfo {
            id: "gpt-4.1".into(),
            display_name: Some("GPT-4.1".into()),
            owned_by: Some("openai".into()),
            created: Some(1744675200),
            context_window: Some(1_000_000),
            known: true,
            ..ModelInfo::default()
        },
        ModelInfo::bare("team-a/my-alias"),
    ];
    let list = ChatCodec.encode_models(&models);
    assert_eq!(
        list,
        json!({
            "object": "list",
            "data": [
                {"id": "gpt-4.1", "object": "model", "created": 1744675200, "owned_by": "openai"},
                {"id": "team-a/my-alias", "object": "model", "created": 0, "owned_by": "switchyard"}
            ]
        })
    );
    assert_eq!(
        list.to_string(),
        r#"{"object":"list","data":[{"id":"gpt-4.1","object":"model","created":1744675200,"owned_by":"openai"},{"id":"team-a/my-alias","object":"model","created":0,"owned_by":"switchyard"}]}"#
    );
    assert_eq!(ChatCodec.encode_model(&models[0]), list["data"][0]);
    assert_eq!(
        ChatCodec.encode_models(&[]),
        json!({"object": "list", "data": []})
    );
}

#[test]
fn protocol_and_token_counting() {
    assert_eq!(ChatCodec.protocol(), Protocol::OpenaiChat);
    // Chat Completions has no token-counting endpoint.
    let request = Request::new("m", Protocol::OpenaiChat);
    assert_eq!(
        ChatCodec.encode_count_request(&request, &UpstreamCtx::default()),
        None
    );
    assert_eq!(ChatCodec.encode_count_response(42), None);
    assert_eq!(
        ChatCodec.decode_count_response(&json!({"input_tokens": 42})),
        None
    );
}
