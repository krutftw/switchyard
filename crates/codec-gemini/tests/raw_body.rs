//! The raw-body helpers used on the passthrough path, the Vertex adaptation
//! and the model listings.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::{
    GeminiCodec, SKIP_SIGNATURE, adapt_for_vertex, sanitize_function_name,
};
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{
    Codec, CodecError, ModelInfo, Protocol, RequestMeta, RequestPath, UpstreamCtx,
};

fn user(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

// ---------------------------------------------------------------------------
// request_meta / set_request_model
// ---------------------------------------------------------------------------

#[test]
fn protocol_is_gemini() {
    assert_eq!(GeminiCodec.protocol(), Protocol::Gemini);
}

#[test]
fn request_meta_reads_the_url() {
    let body = json!({"contents": [user("hi")]});
    let meta = |model: Option<&str>, stream: Option<bool>| {
        GeminiCodec.request_meta(&body, &RequestPath { model, stream })
    };
    assert_eq!(
        meta(Some("gemini-2.5-pro"), Some(true)),
        Ok(RequestMeta {
            model: "gemini-2.5-pro".into(),
            stream: true
        })
    );
    assert_eq!(
        meta(Some("team/gemini-3-flash(high)"), Some(false)),
        Ok(RequestMeta {
            model: "team/gemini-3-flash(high)".into(),
            stream: false
        })
    );
    assert_eq!(
        meta(Some("gemini-2.5-pro"), None),
        Ok(RequestMeta {
            model: "gemini-2.5-pro".into(),
            stream: false
        })
    );
    assert!(matches!(
        meta(None, Some(true)),
        Err(CodecError::InvalidRequest { param: Some(ref p), .. }) if p == "model"
    ));
    assert!(matches!(
        meta(Some("  "), None),
        Err(CodecError::InvalidRequest { .. })
    ));
}

#[test]
fn request_meta_falls_back_to_the_body() {
    let none = RequestPath::default();
    assert_eq!(
        GeminiCodec.request_meta(
            &json!({"model": "models/gemini-2.5-flash", "contents": []}),
            &none
        ),
        Ok(RequestMeta {
            model: "gemini-2.5-flash".into(),
            stream: false
        })
    );
    assert_eq!(
        GeminiCodec.request_meta(&json!({"model": "gemini-2.5-flash", "stream": true}), &none),
        Ok(RequestMeta {
            model: "gemini-2.5-flash".into(),
            stream: true
        })
    );
    // The URL wins over the body for both facts.
    assert_eq!(
        GeminiCodec.request_meta(
            &json!({"model": "models/from-body", "stream": true}),
            &RequestPath {
                model: Some("from-url"),
                stream: Some(false)
            }
        ),
        Ok(RequestMeta {
            model: "from-url".into(),
            stream: false
        })
    );
    for body in [
        json!({"model": 7}),
        json!({"model": ""}),
        json!({"model": "models/"}),
        json!("x"),
        json!(null),
    ] {
        assert!(GeminiCodec.request_meta(&body, &none).is_err(), "{body}");
    }
}

#[test]
fn set_request_model_only_touches_an_existing_model_field() {
    let mut without = json!({"contents": [user("hi")]});
    GeminiCodec.set_request_model(&mut without, "gemini-2.5-pro");
    assert_eq!(without, json!({"contents": [user("hi")]}));

    let mut prefixed = json!({"model": "models/alias", "contents": []});
    GeminiCodec.set_request_model(&mut prefixed, "gemini-2.5-pro");
    assert_eq!(
        prefixed,
        json!({"model": "models/gemini-2.5-pro", "contents": []})
    );

    let mut bare = json!({"model": "alias", "contents": []});
    GeminiCodec.set_request_model(&mut bare, "models/gemini-2.5-pro");
    assert_eq!(bare, json!({"model": "gemini-2.5-pro", "contents": []}));

    let mut not_an_object = json!([1, 2]);
    GeminiCodec.set_request_model(&mut not_an_object, "x");
    assert_eq!(not_an_object, json!([1, 2]));
}

// ---------------------------------------------------------------------------
// rewrite_response_model
// ---------------------------------------------------------------------------

#[test]
fn rewrite_response_model_on_full_responses() {
    let mut body = json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "modelVersion: x"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 1, "totalTokenCount": 1},
        "modelVersion": "gemini-2.5-pro-preview-06-05",
        "responseId": "r1"
    });
    GeminiCodec.rewrite_response_model(&mut body, "my-alias");
    assert_eq!(body["modelVersion"], "my-alias");
    // Only the field is touched, never content that mentions it.
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "modelVersion: x"
    );
    assert_eq!(body["responseId"], "r1");
}

#[test]
fn rewrite_response_model_on_stream_payloads() {
    let mut chunk = json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}, "index": 0}],
        "modelVersion": "gemini-2.5-flash",
        "responseId": "r2"
    });
    GeminiCodec.rewrite_response_model(&mut chunk, "fast");
    assert_eq!(chunk["modelVersion"], "fast");

    // Wrapped chunks and the JSON-array stream form.
    let mut wrapped = json!({"response": {"candidates": [], "modelVersion": "up"}});
    GeminiCodec.rewrite_response_model(&mut wrapped, "alias");
    assert_eq!(
        wrapped,
        json!({"response": {"candidates": [], "modelVersion": "alias"}})
    );
    let mut array = json!([{"modelVersion": "up"}, {"candidates": []}, {"modelVersion": "up"}]);
    GeminiCodec.rewrite_response_model(&mut array, "alias");
    assert_eq!(
        array,
        json!([{"modelVersion": "alias"}, {"candidates": []}, {"modelVersion": "alias"}])
    );
}

#[test]
fn rewrite_response_model_leaves_payloads_without_a_model_alone() {
    for payload in [
        json!({"error": {"code": 429, "message": "slow", "status": "RESOURCE_EXHAUSTED"}}),
        json!({"candidates": [{"content": {"parts": [{"text": "x"}]}}]}),
        json!({"totalTokens": 12}),
        json!({"modelVersion": 3}),
        json!("text"),
        json!(null),
    ] {
        let mut rewritten = payload.clone();
        GeminiCodec.rewrite_response_model(&mut rewritten, "alias");
        assert_eq!(rewritten, payload);
    }
}

// ---------------------------------------------------------------------------
// read_reasoning
// ---------------------------------------------------------------------------

#[test]
fn read_reasoning_in_every_spelling() {
    let read = |body: Value| GeminiCodec.read_reasoning(&body);
    assert_eq!(read(json!({"contents": []})), ReasoningConfig::default());
    assert_eq!(
        read(json!({"generationConfig": {"temperature": 1}})),
        ReasoningConfig::default()
    );
    assert_eq!(
        read(json!({"generationConfig": {"thinkingConfig": {}}})),
        ReasoningConfig::default()
    );
    assert_eq!(
        read(
            json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 4096, "includeThoughts": true}}})
        ),
        ReasoningConfig {
            depth: Some(Depth::Budget(4096)),
            summary: Some(Summary::Auto)
        }
    );
    assert_eq!(
        read(json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}})),
        ReasoningConfig::with_depth(Depth::Off)
    );
    assert_eq!(
        read(json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}})),
        ReasoningConfig::with_depth(Depth::Auto)
    );
    assert_eq!(
        read(json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "LOW"}}})),
        ReasoningConfig::with_depth(Depth::Level(Effort::Low))
    );
    assert_eq!(
        read(
            json!({"generation_config": {"thinking_config": {"thinking_level": "high", "include_thoughts": false}}})
        ),
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::High)),
            summary: Some(Summary::Off)
        }
    );
    assert_eq!(
        read(json!({"generationConfig": {"thinking_config": {"thinking_budget": 512}}})),
        ReasoningConfig::with_depth(Depth::Budget(512))
    );
    assert_eq!(
        read(
            json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "minimal", "thinkingBudget": 9000}}})
        ),
        ReasoningConfig::with_depth(Depth::Level(Effort::Minimal))
    );
    assert_eq!(read(json!("not an object")), ReasoningConfig::default());
}

// ---------------------------------------------------------------------------
// write_reasoning
// ---------------------------------------------------------------------------

fn write(mut body: Value, depth: Fitted, ctx: &UpstreamCtx<'_>) -> Value {
    GeminiCodec.write_reasoning(&mut body, depth, ctx);
    body
}

fn thinking_config(body: &Value) -> Value {
    body["generationConfig"]["thinkingConfig"].clone()
}

#[test]
fn write_reasoning_every_depth_on_an_unknown_model() {
    let ctx = UpstreamCtx::default();
    let empty = || json!({"contents": [user("hi")]});
    assert_eq!(
        write(empty(), Fitted::Use(Depth::Budget(8192)), &ctx),
        json!({"contents": [user("hi")], "generationConfig": {"thinkingConfig": {"thinkingBudget": 8192}}})
    );
    assert_eq!(
        thinking_config(&write(empty(), Fitted::Use(Depth::Auto), &ctx)),
        json!({"thinkingBudget": -1})
    );
    assert_eq!(
        thinking_config(&write(empty(), Fitted::Use(Depth::Off), &ctx)),
        json!({"thinkingBudget": 0})
    );
    for (effort, level) in [
        (Effort::Minimal, "minimal"),
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::Xhigh, "high"),
        (Effort::Max, "high"),
    ] {
        assert_eq!(
            thinking_config(&write(empty(), Fitted::Use(Depth::Level(effort)), &ctx)),
            json!({"thinkingLevel": level}),
            "{effort:?}"
        );
    }
    // Nothing to strip, nothing created.
    assert_eq!(write(empty(), Fitted::Strip, &ctx), empty());
}

#[test]
fn write_reasoning_replaces_whatever_depth_was_there() {
    let ctx = UpstreamCtx::default();
    let body = || {
        json!({
            "contents": [user("hi")],
            "generationConfig": {
                "temperature": 0.5,
                "thinkingConfig": {"thinkingBudget": 1024, "thinking_level": "low", "include_thoughts": true}
            }
        })
    };
    // Exactly one of budget/level survives; the snake-case duplicates go and
    // an explicit includeThoughts is kept under its camel-case name.
    assert_eq!(
        write(body(), Fitted::Use(Depth::Level(Effort::High)), &ctx)["generationConfig"],
        json!({"temperature": 0.5, "thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}})
    );
    assert_eq!(
        write(body(), Fitted::Use(Depth::Budget(2048)), &ctx)["generationConfig"],
        json!({"temperature": 0.5, "thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}})
    );
    assert_eq!(
        write(body(), Fitted::Strip, &ctx)["generationConfig"],
        json!({"temperature": 0.5, "thinkingConfig": {"includeThoughts": true}})
    );
}

#[test]
fn write_reasoning_strip_removes_empty_containers() {
    let ctx = UpstreamCtx::default();
    assert_eq!(
        write(
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}}),
            Fitted::Strip,
            &ctx
        ),
        json!({"contents": []})
    );
    assert_eq!(
        write(
            json!({"contents": [], "generationConfig": {"topK": 5, "thinkingConfig": {"thinking_budget": 100}}}),
            Fitted::Strip,
            &ctx
        ),
        json!({"contents": [], "generationConfig": {"topK": 5}})
    );
}

#[test]
fn write_reasoning_on_a_model_that_does_not_think() {
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    // `includeThoughts` alone is rejected by such models: everything goes.
    assert_eq!(
        write(
            json!({"contents": [], "generationConfig": {
                "maxOutputTokens": 10,
                "thinkingConfig": {"thinkingBudget": 1024, "includeThoughts": true}
            }}),
            Fitted::Strip,
            &ctx
        ),
        json!({"contents": [], "generationConfig": {"maxOutputTokens": 10}})
    );
}

#[test]
fn write_reasoning_with_known_capabilities() {
    let budget_only = ThinkingSupport {
        min: 128,
        max: 32768,
        zero_allowed: false,
        dynamic_allowed: true,
        levels: vec![],
    };
    let level_only = ThinkingSupport::levels(&[Effort::Low, Effort::High]);
    let hybrid = ThinkingSupport {
        min: 128,
        max: 32768,
        zero_allowed: false,
        dynamic_allowed: true,
        levels: vec![Effort::Low, Effort::High],
    };
    let ctx = |caps| UpstreamCtx {
        thinking: ModelThinking::Supported(caps),
        ..UpstreamCtx::default()
    };
    let empty = || json!({"contents": []});
    // Budget-only (2.5): a level that slipped through becomes a budget.
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Level(Effort::Medium)),
            &ctx(&budget_only)
        )),
        json!({"thinkingBudget": 8192})
    );
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Level(Effort::Max)),
            &ctx(&budget_only)
        )),
        json!({"thinkingBudget": 32768})
    );
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Budget(128)),
            &ctx(&budget_only)
        )),
        json!({"thinkingBudget": 128})
    );
    // Level-only: a budget that slipped through becomes a level.
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Budget(20000)),
            &ctx(&level_only)
        )),
        json!({"thinkingLevel": "high"})
    );
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Level(Effort::Low)),
            &ctx(&level_only)
        )),
        json!({"thinkingLevel": "low"})
    );
    // Hybrid (3.x): the caller's form is kept; dynamic is always a budget.
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Budget(4096)),
            &ctx(&hybrid)
        )),
        json!({"thinkingBudget": 4096})
    );
    assert_eq!(
        thinking_config(&write(
            empty(),
            Fitted::Use(Depth::Level(Effort::High)),
            &ctx(&hybrid)
        )),
        json!({"thinkingLevel": "high"})
    );
    assert_eq!(
        thinking_config(&write(empty(), Fitted::Use(Depth::Auto), &ctx(&hybrid))),
        json!({"thinkingBudget": -1})
    );
}

#[test]
fn write_reasoning_keeps_a_snake_case_generation_config_in_place() {
    let body = json!({"contents": [], "generation_config": {"max_output_tokens": 64, "thinking_config": {"thinking_budget": 1}}});
    assert_eq!(
        write(
            body,
            Fitted::Use(Depth::Budget(2048)),
            &UpstreamCtx::default()
        ),
        json!({"contents": [], "generation_config": {"max_output_tokens": 64, "thinkingConfig": {"thinkingBudget": 2048}}})
    );
}

#[test]
fn written_reasoning_reads_back() {
    for depth in [
        Depth::Off,
        Depth::Auto,
        Depth::Budget(777),
        Depth::Level(Effort::Medium),
    ] {
        let body = write(
            json!({"contents": []}),
            Fitted::Use(depth),
            &UpstreamCtx::default(),
        );
        assert_eq!(
            GeminiCodec.read_reasoning(&body),
            ReasoningConfig::with_depth(depth)
        );
    }
    let mut not_an_object = json!([]);
    GeminiCodec.write_reasoning(
        &mut not_an_object,
        Fitted::Use(Depth::Auto),
        &UpstreamCtx::default(),
    );
    assert_eq!(not_an_object, json!([]));
}

// ---------------------------------------------------------------------------
// prepare_passthrough
// ---------------------------------------------------------------------------

fn prepare(mut body: Value, ctx: &UpstreamCtx<'_>) -> Value {
    GeminiCodec.prepare_passthrough(&mut body, true, ctx);
    body
}

#[test]
fn prepare_passthrough_leaves_a_well_formed_request_untouched() {
    let body = json!({
        "systemInstruction": {"parts": [{"text": "Be brief."}]},
        "contents": [
            user("Weather?"),
            {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "fc1"}, "thoughtSignature": "U0lH"},
                {"functionCall": {"name": "get_weather", "args": {"city": "Rome"}, "id": "fc2"}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"result": "18C"}, "id": "fc1"}},
                {"functionResponse": {"name": "get_weather", "response": {"result": "24C"}, "id": "fc2"}}
            ]},
            {"parts": [{"text": "no role is a user turn"}]}
        ],
        "tools": [{"functionDeclarations": [{"name": "get_weather", "parameters": {"type": "OBJECT"}}]}],
        "generationConfig": {"maxOutputTokens": 1024, "thinkingConfig": {"thinkingBudget": 512}},
        "safetySettings": []
    });
    let ctx = UpstreamCtx {
        max_output_tokens: Some(65_536),
        ..UpstreamCtx::default()
    };
    assert_eq!(prepare(body.clone(), &ctx), body);
    assert_eq!(prepare(body.clone(), &UpstreamCtx::default()), body);
}

#[test]
fn prepare_passthrough_signs_the_first_unsigned_call_of_each_model_turn() {
    let body = json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"text": "calling"},
            {"functionCall": {"name": "a", "args": {}}},
            {"functionCall": {"name": "b", "args": {}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "a", "response": {"result": "1"}}},
            {"functionResponse": {"name": "b", "response": {"result": "2"}}}
        ]},
        {"role": "model", "parts": [{"function_call": {"name": "c", "args": {}}, "thought_signature": "bmF0aXZl"}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "c", "response": {"result": "3"}}}]}
    ]});
    let prepared = prepare(body, &UpstreamCtx::default());
    assert_eq!(
        prepared["contents"][1]["parts"],
        json!([
            {"text": "calling"},
            {"functionCall": {"name": "a", "args": {}}, "thoughtSignature": SKIP_SIGNATURE},
            {"functionCall": {"name": "b", "args": {}}}
        ])
    );
    // A native signature, in any spelling, is never touched.
    assert_eq!(
        prepared["contents"][3]["parts"],
        json!([{"function_call": {"name": "c", "args": {}}, "thought_signature": "bmF0aXZl"}])
    );
}

#[test]
fn prepare_passthrough_repairs_roles_names_and_the_first_turn() {
    let body = json!({"contents": [
        {"role": "assistant", "parts": [{"functionCall": {"name": "lookup", "args": {}}, "thoughtSignature": "c2ln"}]},
        {"role": "function", "parts": [{"functionResponse": {"name": "", "response": {"result": "ok"}}}]},
        {"role": "bot", "parts": [{"text": "after a user turn an unknown role is the model"}]},
        {"role": "tool", "parts": [{"functionResponse": {"name": "kept", "response": {}}}]},
        {"role": "MODEL", "parts": [{"text": "upper-case is valid"}]}
    ]});
    assert_eq!(
        prepare(body, &UpstreamCtx::default())["contents"],
        json!([
            // The conversation started with the model: an empty user turn
            // goes in front.
            {"role": "user", "parts": [{"text": ""}]},
            {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {}}, "thoughtSignature": "c2ln"}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "lookup", "response": {"result": "ok"}}}]},
            {"role": "model", "parts": [{"text": "after a user turn an unknown role is the model"}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "kept", "response": {}}}]},
            {"role": "MODEL", "parts": [{"text": "upper-case is valid"}]}
        ])
    );
}

#[test]
fn prepare_passthrough_caps_max_output_tokens() {
    let ctx = UpstreamCtx {
        max_output_tokens: Some(8192),
        ..UpstreamCtx::default()
    };
    let body = json!({"contents": [user("hi")], "generationConfig": {"maxOutputTokens": 100_000, "temperature": 1}});
    assert_eq!(
        prepare(body.clone(), &ctx)["generationConfig"],
        json!({"maxOutputTokens": 8192, "temperature": 1})
    );
    // Unknown limit: untouched.
    assert_eq!(prepare(body.clone(), &UpstreamCtx::default()), body);
    let snake = json!({"contents": [user("hi")], "generation_config": {"max_output_tokens": 9000}});
    assert_eq!(
        prepare(snake, &ctx)["generation_config"]["max_output_tokens"],
        8192
    );
}

#[test]
fn prepare_passthrough_tolerates_anything() {
    for body in [
        json!(null),
        json!([]),
        json!({}),
        json!({"contents": "text"}),
        json!({"contents": [1, null, "x", {}]}),
    ] {
        assert_eq!(prepare(body.clone(), &UpstreamCtx::default()), body);
    }
}

// ---------------------------------------------------------------------------
// adapt_for_vertex
// ---------------------------------------------------------------------------

#[test]
fn adapt_for_vertex_removes_call_ids() {
    let mut body = json!({
        "contents": [
            user("go"),
            {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {"id": "an argument called id"}, "id": "fc1"}, "thoughtSignature": "c2ln"}]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "f", "response": {"id": "a result field"}, "id": "fc1"}},
                {"function_response": {"name": "g", "response": {}, "id": "fc2"}}
            ]}
        ],
        "generationConfig": {"temperature": 0}
    });
    adapt_for_vertex(&mut body);
    assert_eq!(
        body,
        json!({
            "contents": [
                user("go"),
                {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {"id": "an argument called id"}}, "thoughtSignature": "c2ln"}]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "f", "response": {"id": "a result field"}}},
                    {"function_response": {"name": "g", "response": {}}}
                ]}
            ],
            "generationConfig": {"temperature": 0}
        })
    );
}

#[test]
fn adapt_for_vertex_unwraps_the_count_tokens_request() {
    let mut body = json!({
        "generateContentRequest": {
            "model": "models/gemini-2.5-pro",
            "contents": [user("count me")],
            "systemInstruction": {"parts": [{"text": "sys"}]}
        }
    });
    adapt_for_vertex(&mut body);
    assert_eq!(
        body,
        json!({"contents": [user("count me")], "systemInstruction": {"parts": [{"text": "sys"}]}})
    );
    let mut untouched = json!({"contents": [user("plain")]});
    adapt_for_vertex(&mut untouched);
    assert_eq!(untouched, json!({"contents": [user("plain")]}));
    let mut odd = json!("not a body");
    adapt_for_vertex(&mut odd);
    assert_eq!(odd, json!("not a body"));
}

// ---------------------------------------------------------------------------
// Model listings
// ---------------------------------------------------------------------------

#[test]
fn model_listing() {
    let known = ModelInfo {
        id: "gemini-2.5-pro".into(),
        display_name: Some("Gemini 2.5 Pro".into()),
        description: Some("Stable release of Gemini 2.5 Pro".into()),
        owned_by: Some("google".into()),
        created: Some(1_750_000_000),
        context_window: Some(1_048_576),
        max_output_tokens: Some(65_536),
        thinking: Some(ThinkingSupport::budget(128, 32768)),
        known: true,
    };
    let no_thinking = ModelInfo {
        id: "gpt-4o-mini".into(),
        known: true,
        ..ModelInfo::default()
    };
    let methods = json!(["generateContent", "countTokens", "streamGenerateContent"]);
    assert_eq!(
        GeminiCodec.encode_models(&[known.clone(), no_thinking, ModelInfo::bare("team/my-alias")]),
        json!({"models": [
            {
                "name": "models/gemini-2.5-pro",
                "version": "001",
                "displayName": "Gemini 2.5 Pro",
                "description": "Stable release of Gemini 2.5 Pro",
                "inputTokenLimit": 1_048_576,
                "outputTokenLimit": 65_536,
                "supportedGenerationMethods": methods,
                "thinking": true
            },
            {
                "name": "models/gpt-4o-mini",
                "version": "001",
                "displayName": "gpt-4o-mini",
                "description": "gpt-4o-mini",
                "supportedGenerationMethods": methods,
                "thinking": false
            },
            {
                // Nothing is known about this one: no limits, no thinking flag.
                "name": "models/team/my-alias",
                "version": "001",
                "displayName": "team/my-alias",
                "description": "team/my-alias",
                "supportedGenerationMethods": methods
            }
        ]})
    );
    assert_eq!(GeminiCodec.encode_models(&[]), json!({"models": []}));
    // The single-model form is the same entry, and a `models/` prefix is
    // never doubled.
    assert_eq!(
        GeminiCodec.encode_model(&known),
        GeminiCodec.encode_models(std::slice::from_ref(&known))["models"][0]
    );
    assert_eq!(
        GeminiCodec.encode_model(&ModelInfo::bare("models/x"))["name"],
        "models/x"
    );
}

#[test]
fn function_name_sanitiser_is_exported() {
    assert_eq!(sanitize_function_name("mcp/server get"), "mcp_server_get");
    assert_eq!(sanitize_function_name("9lives"), "_9lives");
    assert_eq!(
        sanitize_function_name("ok.name:with-dash"),
        "ok.name:with-dash"
    );
}
