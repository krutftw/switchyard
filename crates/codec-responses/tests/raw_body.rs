//! The inspect / patch helpers used for same-protocol passthrough, plus the
//! model listings.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{
    Codec, CodecError, ModelInfo, Protocol, RequestMeta, RequestPath, UpstreamCtx,
};

fn meta(body: Value) -> Result<RequestMeta, CodecError> {
    ResponsesCodec.request_meta(&body, &RequestPath::default())
}

fn written(mut body: Value, depth: Fitted, thinking: ModelThinking<'_>) -> Value {
    let ctx = UpstreamCtx {
        thinking,
        ..UpstreamCtx::default()
    };
    ResponsesCodec.write_reasoning(&mut body, depth, &ctx);
    body
}

#[test]
fn protocol_is_openai_responses() {
    assert_eq!(ResponsesCodec.protocol(), Protocol::OpenaiResponses);
}

// ---------------------------------------------------------------------------
// request_meta / set_request_model
// ---------------------------------------------------------------------------

#[test]
fn request_meta_reads_model_and_stream() {
    assert_eq!(
        meta(json!({"model": "team/gpt-5(high)", "input": "x", "stream": true})).unwrap(),
        RequestMeta {
            model: "team/gpt-5(high)".into(),
            stream: true
        }
    );
    assert_eq!(
        meta(json!({"model": "gpt-5", "input": "x"})).unwrap(),
        RequestMeta {
            model: "gpt-5".into(),
            stream: false
        }
    );
    // Only a literal `true` streams.
    assert!(
        !meta(json!({"model": "gpt-5", "stream": "true"}))
            .unwrap()
            .stream
    );
    assert!(
        !meta(json!({"model": "gpt-5", "stream": null}))
            .unwrap()
            .stream
    );
    // The body does not need to be a valid request for routing to work.
    assert_eq!(meta(json!({"model": "gpt-5"})).unwrap().model, "gpt-5");
}

#[test]
fn request_meta_falls_back_to_the_path_and_rejects_unroutable_bodies() {
    let path = RequestPath {
        model: Some("from-path"),
        stream: Some(true),
    };
    assert_eq!(
        ResponsesCodec
            .request_meta(&json!({"input": "x"}), &path)
            .unwrap(),
        RequestMeta {
            model: "from-path".into(),
            stream: true
        }
    );
    // The body wins when it speaks.
    assert_eq!(
        ResponsesCodec
            .request_meta(&json!({"model": "from-body", "stream": false}), &path)
            .unwrap(),
        RequestMeta {
            model: "from-body".into(),
            stream: false
        }
    );

    let param = |result: Result<RequestMeta, CodecError>| match result {
        Err(CodecError::InvalidRequest { param, .. }) => param,
        other => panic!("expected an invalid-request error, got {other:?}"),
    };
    assert_eq!(param(meta(json!({"input": "x"}))).as_deref(), Some("model"));
    assert_eq!(
        param(meta(json!({"model": "", "input": "x"}))).as_deref(),
        Some("model")
    );
    assert_eq!(
        param(meta(json!({"model": ["a"], "input": "x"}))).as_deref(),
        Some("model")
    );
    assert_eq!(param(meta(json!("not an object"))), None);
}

#[test]
fn set_request_model_replaces_only_the_model() {
    let mut body = json!({"model": "alias(high)", "input": "x", "reasoning": {"effort": "low"}});
    ResponsesCodec.set_request_model(&mut body, "gpt-5-2025-08-07");
    assert_eq!(
        body,
        json!({"model": "gpt-5-2025-08-07", "input": "x", "reasoning": {"effort": "low"}})
    );
    // Key order of a forwarded body is preserved.
    assert_eq!(body.as_object().unwrap().keys().next().unwrap(), "model");

    let mut missing = json!({"input": "x"});
    ResponsesCodec.set_request_model(&mut missing, "m");
    assert_eq!(missing["model"], json!("m"));

    let mut not_an_object = json!([1, 2]);
    ResponsesCodec.set_request_model(&mut not_an_object, "m");
    assert_eq!(not_an_object, json!([1, 2]));
}

// ---------------------------------------------------------------------------
// rewrite_response_model
// ---------------------------------------------------------------------------

#[test]
fn rewrite_response_model_on_a_full_response() {
    let mut body = json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5-2025-08-07",
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "the model said hi"}]}]
    });
    ResponsesCodec.rewrite_response_model(&mut body, "my-alias");
    assert_eq!(body["model"], json!("my-alias"));
    assert_eq!(
        body["output"][0]["content"][0]["text"],
        json!("the model said hi")
    );
}

#[test]
fn rewrite_response_model_on_single_stream_event_payloads() {
    for kind in [
        "response.created",
        "response.in_progress",
        "response.completed",
        "response.incomplete",
        "response.failed",
    ] {
        let mut event = json!({
            "type": kind, "sequence_number": 3,
            "response": {"id": "resp_1", "object": "response", "model": "gpt-5-2025-08-07", "output": []}
        });
        ResponsesCodec.rewrite_response_model(&mut event, "my-alias");
        assert_eq!(event["response"]["model"], json!("my-alias"), "{kind}");
        assert_eq!(event.get("model"), None, "{kind}");
    }

    // Events without a model are left exactly as they were.
    let untouched = [
        json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_1",
               "output_index": 0, "content_index": 0, "delta": "model"}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"id": "fc_1", "type": "function_call", "name": "model", "arguments": ""}}),
        json!({"type": "response.created", "response": {"id": "resp_1", "output": []}}),
        json!({"type": "error", "code": "model_not_found", "message": "no such model"}),
        json!("[DONE]"),
    ];
    for original in untouched {
        let mut event = original.clone();
        ResponsesCodec.rewrite_response_model(&mut event, "my-alias");
        assert_eq!(event, original);
    }
}

// ---------------------------------------------------------------------------
// read_reasoning
// ---------------------------------------------------------------------------

#[test]
fn read_reasoning_reads_effort_and_summary() {
    let read = |body: Value| ResponsesCodec.read_reasoning(&body);
    assert_eq!(
        read(json!({"model": "m", "input": "x"})),
        ReasoningConfig::default()
    );
    assert!(read(json!({"model": "m", "reasoning": {}})).is_empty());
    assert_eq!(
        read(json!({"reasoning": {"effort": "high", "summary": "concise"}})),
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::High)),
            summary: Some(Summary::Concise),
        }
    );
    assert_eq!(
        read(json!({"reasoning": {"effort": "none"}})),
        ReasoningConfig::with_depth(Depth::Off)
    );
    assert_eq!(
        read(json!({"reasoning": {"effort": "xhigh", "generate_summary": "detailed"}})),
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::Xhigh)),
            summary: Some(Summary::Detailed),
        }
    );
    // `summary` wins over its deprecated alias; null means "off".
    assert_eq!(
        read(json!({"reasoning": {"summary": null, "generate_summary": "detailed"}})).summary,
        Some(Summary::Off)
    );
    // Chat's spelling is not a Responses field.
    assert!(read(json!({"reasoning_effort": "high"})).is_empty());
    assert!(read(json!("garbage")).is_empty());
}

// ---------------------------------------------------------------------------
// write_reasoning
// ---------------------------------------------------------------------------

fn body_with(reasoning: Value) -> Value {
    json!({"model": "m", "input": "x", "reasoning": reasoning})
}

#[test]
fn write_reasoning_every_depth_variant() {
    let unknown = ModelThinking::Unknown;
    let both = || body_with(json!({"effort": "low", "summary": "auto"}));
    let effort = |depth: Depth| written(both(), Fitted::Use(depth), unknown)["reasoning"].clone();

    assert_eq!(
        effort(Depth::Off),
        json!({"effort": "none", "summary": "auto"})
    );
    // No "dynamic" effort exists: the field goes, the model default applies.
    assert_eq!(effort(Depth::Auto), json!({"summary": "auto"}));
    for level in Effort::ALL {
        assert_eq!(
            effort(Depth::Level(level)),
            json!({"effort": level.as_str(), "summary": "auto"})
        );
    }
    // Budgets are bucketed into levels.
    for (budget, expected) in [
        (1, "minimal"),
        (512, "minimal"),
        (1024, "low"),
        (8192, "medium"),
        (24576, "high"),
        (50_000, "xhigh"),
    ] {
        assert_eq!(
            effort(Depth::Budget(budget)),
            json!({"effort": expected, "summary": "auto"}),
            "budget {budget}"
        );
    }
}

#[test]
fn write_reasoning_strip_keeps_the_summary_and_removes_an_emptied_object() {
    let unknown = ModelThinking::Unknown;
    assert_eq!(
        written(
            body_with(json!({"effort": "high", "summary": "detailed"})),
            Fitted::Strip,
            unknown
        ),
        body_with(json!({"summary": "detailed"}))
    );
    assert_eq!(
        written(body_with(json!({"effort": "high"})), Fitted::Strip, unknown),
        json!({"model": "m", "input": "x"})
    );
    // Same for "auto", which is also the absence of an effort.
    assert_eq!(
        written(
            body_with(json!({"effort": "high"})),
            Fitted::Use(Depth::Auto),
            unknown
        ),
        json!({"model": "m", "input": "x"})
    );
    // Nothing to strip: nothing changes, and nothing is invented.
    assert_eq!(
        written(json!({"model": "m", "input": "x"}), Fitted::Strip, unknown),
        json!({"model": "m", "input": "x"})
    );
    assert_eq!(
        written(
            json!({"model": "m", "input": "x"}),
            Fitted::Use(Depth::Auto),
            unknown
        ),
        json!({"model": "m", "input": "x"})
    );
}

#[test]
fn write_reasoning_creates_the_object_when_needed() {
    let unknown = ModelThinking::Unknown;
    assert_eq!(
        written(
            json!({"model": "m", "input": "x"}),
            Fitted::Use(Depth::Level(Effort::Medium)),
            unknown
        ),
        body_with(json!({"effort": "medium"}))
    );
    // A malformed `reasoning` value is replaced.
    assert_eq!(
        written(
            body_with(json!("high")),
            Fitted::Use(Depth::Level(Effort::Low)),
            unknown
        ),
        body_with(json!({"effort": "low"}))
    );
    // Not a request body at all: left alone.
    assert_eq!(
        written(json!([1]), Fitted::Use(Depth::Level(Effort::Low)), unknown),
        json!([1])
    );
}

#[test]
fn write_reasoning_with_model_thinking_supported_and_unsupported() {
    let body = || body_with(json!({"effort": "low", "summary": "auto"}));

    let levels = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let supported = ModelThinking::Supported(&levels);
    let effort = |depth: Depth| {
        written(body(), Fitted::Use(depth), supported)["reasoning"]["effort"].clone()
    };
    // Clamped into the model's level set.
    assert_eq!(effort(Depth::Level(Effort::Max)), json!("high"));
    assert_eq!(effort(Depth::Level(Effort::Xhigh)), json!("high"));
    assert_eq!(effort(Depth::Level(Effort::Minimal)), json!("low"));
    assert_eq!(effort(Depth::Level(Effort::Medium)), json!("medium"));
    assert_eq!(effort(Depth::Budget(100)), json!("low"));
    assert_eq!(effort(Depth::Budget(100_000)), json!("high"));
    assert_eq!(effort(Depth::Off), json!("none"));

    // A budget-only description has no level set to clamp to.
    let range = ThinkingSupport::budget(1024, 32_000);
    assert_eq!(
        written(
            body(),
            Fitted::Use(Depth::Level(Effort::Max)),
            ModelThinking::Supported(&range)
        )["reasoning"]["effort"],
        json!("max")
    );

    // A model known not to reason gets no effort, whatever was asked.
    assert_eq!(
        written(
            body(),
            Fitted::Use(Depth::Level(Effort::High)),
            ModelThinking::Unsupported
        ),
        body_with(json!({"summary": "auto"}))
    );
    assert_eq!(
        written(body(), Fitted::Strip, supported),
        body_with(json!({"summary": "auto"}))
    );
}

#[test]
fn write_reasoning_keeps_configuration_update_items_consistent() {
    let body = json!({
        "model": "m",
        "reasoning": {"effort": "low"},
        "input": [
            {"role": "user", "content": "x"},
            {"type": "configuration_update", "reasoning": {"effort": "xhigh"}},
            {"type": "configuration_update", "verbosity": "low"}
        ]
    });
    let high = written(
        body.clone(),
        Fitted::Use(Depth::Level(Effort::High)),
        ModelThinking::Unknown,
    );
    assert_eq!(high["reasoning"], json!({"effort": "high"}));
    assert_eq!(
        high["input"][1],
        json!({"type": "configuration_update", "reasoning": {"effort": "high"}})
    );
    assert_eq!(
        high["input"][2],
        json!({"type": "configuration_update", "verbosity": "low"})
    );
    // What was written is what is read back.
    assert_eq!(
        ResponsesCodec.read_reasoning(&high),
        ReasoningConfig::with_depth(Depth::Level(Effort::High))
    );

    let stripped = written(body, Fitted::Strip, ModelThinking::Unknown);
    assert_eq!(stripped.get("reasoning"), None);
    assert_eq!(
        stripped["input"][1],
        json!({"type": "configuration_update", "reasoning": {}})
    );
    assert!(ResponsesCodec.read_reasoning(&stripped).is_empty());
}

#[test]
fn write_then_read_reasoning_round_trips_levels() {
    for level in Effort::ALL {
        let body = written(
            json!({"model": "m", "input": "x"}),
            Fitted::Use(Depth::Level(level)),
            ModelThinking::Unknown,
        );
        assert_eq!(
            ResponsesCodec.read_reasoning(&body),
            ReasoningConfig::with_depth(Depth::Level(level))
        );
    }
}

// ---------------------------------------------------------------------------
// prepare_passthrough
// ---------------------------------------------------------------------------

#[test]
fn prepare_passthrough_only_aligns_the_stream_flag() {
    let prepared = |mut body: Value, stream: bool| {
        ResponsesCodec.prepare_passthrough(&mut body, stream, &UpstreamCtx::default());
        body
    };
    let body = json!({
        "model": "m", "input": [{"role": "user", "content": "x"}], "store": true,
        "previous_response_id": "resp_1", "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": "high"}, "tools": [{"type": "web_search_preview"}], "unknown_field": 1
    });

    let mut streaming = body.clone();
    streaming["stream"] = json!(true);
    assert_eq!(prepared(body.clone(), true), streaming);
    assert_eq!(prepared(streaming.clone(), true), streaming);

    // Not streaming: an absent flag stays absent, a present one is corrected.
    assert_eq!(prepared(body.clone(), false), body);
    let mut not_streaming = body.clone();
    not_streaming["stream"] = json!(false);
    assert_eq!(prepared(streaming, false), not_streaming);

    assert_eq!(
        prepared(json!("not an object"), true),
        json!("not an object")
    );
}

// ---------------------------------------------------------------------------
// Model listings
// ---------------------------------------------------------------------------

#[test]
fn encode_models_in_the_openai_list_shape() {
    let models = vec![
        ModelInfo {
            id: "gpt-5".into(),
            display_name: Some("GPT-5".into()),
            owned_by: Some("openai".into()),
            created: Some(1_754_500_000),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
            known: true,
            ..ModelInfo::default()
        },
        ModelInfo::bare("team-a/my-alias"),
    ];
    assert_eq!(
        ResponsesCodec.encode_models(&models),
        json!({
            "object": "list",
            "data": [
                {"id": "gpt-5", "object": "model", "created": 1754500000, "owned_by": "openai"},
                {"id": "team-a/my-alias", "object": "model", "created": 0, "owned_by": "switchyard"}
            ]
        })
    );
    assert_eq!(
        ResponsesCodec.encode_models(&[]),
        json!({"object": "list", "data": []})
    );
    assert_eq!(
        ResponsesCodec.encode_model(&models[0]),
        json!({"id": "gpt-5", "object": "model", "created": 1754500000, "owned_by": "openai"})
    );
}
