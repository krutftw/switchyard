//! Upstream side: canonical requests → Messages request bodies, asserted as
//! exact JSON.

mod common;

use common::{encode_request, encode_request_with};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, CustomTool, FunctionTool, MediaPart, MediaSource, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, TextPart,
    Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{
    Depth, Effort, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

/// A request as another protocol's decoder would hand it over.
fn request(messages: Vec<Message>) -> Request {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.messages = messages;
    request
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn function_tool(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: None,
        parameters: Value::Null,
        strict: None,
        cache_control: None,
    })
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
// Skeleton, max_tokens, scalars
// ---------------------------------------------------------------------------

#[test]
fn encode_plain_text_request() {
    let body = encode_request(&request(vec![Message::user_text("Hello")]));
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]
        })
    );
}

#[test]
fn encode_max_tokens_precedence() {
    let base = request(vec![Message::user_text("hi")]);
    let known = UpstreamCtx {
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    // Nothing known: the conservative default.
    assert_eq!(encode_request(&base)["max_tokens"], json!(4096));
    // The model's output limit when the client gave none.
    assert_eq!(
        encode_request_with(&base, &known)["max_tokens"],
        json!(64_000)
    );
    // The client's limit.
    let mut limited = base.clone();
    limited.max_output_tokens = Some(1000);
    assert_eq!(encode_request(&limited)["max_tokens"], json!(1000));
    assert_eq!(
        encode_request_with(&limited, &known)["max_tokens"],
        json!(1000)
    );
    // …capped at what the model can produce.
    limited.max_output_tokens = Some(200_000);
    assert_eq!(
        encode_request_with(&limited, &known)["max_tokens"],
        json!(64_000)
    );
    // Thinking needs room: a larger default when nothing is known.
    let mut thinking = base.clone();
    thinking.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)));
    assert_eq!(encode_request(&thinking)["max_tokens"], json!(32_000));
    thinking.reasoning = Some(ReasoningConfig::with_depth(Depth::Off));
    assert_eq!(encode_request(&thinking)["max_tokens"], json!(4096));
}

#[test]
fn encode_scalar_params() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.stream = true;
    req.max_output_tokens = Some(512);
    req.top_k = Some(40);
    req.stop = vec!["END".into(), "".into(), "STOP".into()];
    req.user = Some("user-42".into());
    req.service_tier = Some("default".into());
    req.seed = Some(7);
    req.presence_penalty = Some(0.5);
    req.frequency_penalty = Some(0.5);
    req.candidate_count = Some(1);
    req.store = Some(true);
    req.prompt_cache_key = Some("k".into());
    req.metadata = Some(json!({"trace": "abc"}).as_object().unwrap().clone());
    assert_eq!(
        encode_request(&req),
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 512,
            "messages": [user("hi")],
            "stop_sequences": ["END", "STOP"],
            "top_k": 40,
            "metadata": {"user_id": "user-42"},
            "service_tier": "standard_only",
            "stream": true
        })
    );
}

#[test]
fn encode_sampling_never_sends_temperature_and_top_p_together() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.temperature = Some(0.3);
    req.top_p = Some(0.9);
    let body = encode_request(&req);
    assert_eq!(body["temperature"], json!(0.3));
    assert!(body.get("top_p").is_none());
    // top_p alone is fine.
    req.temperature = None;
    let body = encode_request(&req);
    assert_eq!(body["top_p"], json!(0.9));
    // OpenAI's 0..2 range is clamped to Anthropic's 0..1.
    req.temperature = Some(1.7);
    req.top_p = None;
    assert_eq!(encode_request(&req)["temperature"], json!(1.0));
}

#[test]
fn encode_sampling_is_dropped_while_thinking_is_active() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.temperature = Some(0.3);
    req.top_p = Some(0.9);
    req.top_k = Some(5);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::Medium)));
    let body = encode_request(&req);
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "messages": [user("hi")],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "medium"}
        })
    );
    // Explicitly disabled thinking keeps sampling (minus the top_p clash).
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Off));
    let body = encode_request(&req);
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert_eq!(body["temperature"], json!(0.3));
    assert_eq!(body["top_k"], json!(5));
    assert!(body.get("top_p").is_none());
}

#[test]
fn encode_forwards_native_extras_only_for_anthropic_sources() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.extra.insert("speed".into(), json!("fast"));
    req.extra.insert("container".into(), json!("container_01"));
    req.extra.insert("logit_bias".into(), json!({"1": 2}));
    req.extra.insert("betas".into(), json!(["x"]));
    // Decoded from Chat Completions: nothing is copied.
    let body = encode_request(&req);
    assert!(body.get("speed").is_none() && body.get("container").is_none());
    // Decoded from a Messages client: known request fields come back.
    req.source = Protocol::Anthropic;
    let body = encode_request(&req);
    assert_eq!(body["speed"], json!("fast"));
    assert_eq!(body["container"], json!("container_01"));
    assert!(body.get("logit_bias").is_none());
    assert!(body.get("betas").is_none());
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

#[test]
fn encode_system_blocks_keep_cache_control() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.system = vec![
        Part::Text(TextPart {
            text: "You are a helpful assistant.".into(),
            cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
            citations: vec![],
            signature: None,
        }),
        Part::text(""),
        Part::text("Answer briefly."),
        Part::Image(MediaPart::url("https://e.com/x.png")),
    ];
    assert_eq!(
        encode_request(&req)["system"],
        json!([
            {"type": "text", "text": "You are a helpful assistant.",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "text", "text": "Answer briefly."}
        ])
    );
}

#[test]
fn encode_no_system_key_without_system_text() {
    let body = encode_request(&request(vec![Message::user_text("hi")]));
    assert!(body.get("system").is_none());
}

// ---------------------------------------------------------------------------
// Content parts
// ---------------------------------------------------------------------------

#[test]
fn encode_multimodal_user_content() {
    let req = request(vec![Message::new(
        Role::User,
        vec![
            Part::text("Describe these."),
            Part::Image(MediaPart {
                detail: Some("high".into()),
                ..MediaPart::url("https://example.com/cat.png")
            }),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
            // A data URI that reached the IR as a URL becomes inline data.
            Part::Image(MediaPart::url("data:image/webp;base64,UklGRg==")),
            Part::Document(MediaPart {
                filename: Some("report.pdf".into()),
                ..MediaPart::base64("application/pdf", "JVBERi0xLjQ=")
            }),
            Part::Document(MediaPart::url("https://example.com/paper.pdf")),
            Part::Document(MediaPart::base64("text/plain", "cGxhaW4gd29yZHM=")),
            Part::Audio(MediaPart::base64("audio/wav", "UklGRg==")),
            Part::Document(MediaPart::base64("application/zip", "UEsDBA==")),
        ],
    )]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "Describe these."},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/cat.png"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "UklGRg=="}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf",
                                             "data": "JVBERi0xLjQ="}, "title": "report.pdf"},
            {"type": "document", "source": {"type": "url", "url": "https://example.com/paper.pdf"}},
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "plain words"}}
        ]}])
    );
}

#[test]
fn encode_file_references_only_for_anthropic_sources() {
    let file = |id: &str| MediaPart {
        source: MediaSource::FileRef { id: id.into() },
        media_type: None,
        filename: None,
        detail: None,
        cache_control: None,
    };
    let parts = vec![
        Part::text("see"),
        Part::Image(file("file_img")),
        Part::Document(file("file_doc")),
    ];
    // An OpenAI file id means nothing to Anthropic.
    let foreign = request(vec![Message::new(Role::User, parts.clone())]);
    assert_eq!(encode_request(&foreign)["messages"], json!([user("see")]));
    let mut native = foreign.clone();
    native.source = Protocol::Anthropic;
    assert_eq!(
        encode_request(&native)["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "see"},
            {"type": "image", "source": {"type": "file", "file_id": "file_img"}},
            {"type": "document", "source": {"type": "file", "file_id": "file_doc"}}
        ]}])
    );
}

#[test]
fn encode_cache_control_on_parts() {
    let cache = json!({"type": "ephemeral"});
    let req = request(vec![
        Message::new(
            Role::User,
            vec![
                Part::Text(TextPart {
                    text: "big context".into(),
                    cache_control: Some(cache.clone()),
                    citations: vec![],
                    signature: None,
                }),
                Part::Image(MediaPart {
                    cache_control: Some(cache.clone()),
                    ..MediaPart::base64("image/png", "iVBOR")
                }),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![Part::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
                kind: ToolCallKind::Function,
                signature: None,
                cache_control: Some(cache.clone()),
            })],
        ),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "toolu_1".into(),
                name: None,
                content: vec![Part::text("found")],
                is_error: false,
                cache_control: Some(cache.clone()),
            })],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "big context", "cache_control": {"type": "ephemeral"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"},
                 "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {},
                 "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found",
                 "cache_control": {"type": "ephemeral"}}
            ]}
        ])
    );
}

#[test]
fn encode_tool_result_carrying_images_hoists_nested_cache_control() {
    let req = request(vec![
        Message::user_text("screenshot?"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "screenshot", "")],
        ),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "call_1".into(),
                name: None,
                content: vec![
                    Part::text("here it is"),
                    Part::Image(MediaPart {
                        cache_control: Some(json!({"type": "ephemeral"})),
                        ..MediaPart::base64("image/png", "iVBOR")
                    }),
                    Part::Image(MediaPart::url("https://example.com/shot.png")),
                ],
                is_error: false,
                cache_control: None,
            })],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "call_1", "content": [
                {"type": "text", "text": "here it is"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/shot.png"}}
            ], "cache_control": {"type": "ephemeral"}}
        ]})
    );
}

#[test]
fn encode_tool_result_shapes() {
    let result = |id: &str, content: Vec<Part>, is_error: bool| {
        Part::ToolResult(ToolResult {
            call_id: id.into(),
            name: None,
            content,
            is_error,
            cache_control: None,
        })
    };
    let req = request(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("a", "f", "{}"),
                Part::tool_call("b", "f", "{}"),
                Part::tool_call("c", "f", "{}"),
                Part::tool_call("d", "f", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                result("a", vec![Part::text("single text is a string")], false),
                result("b", vec![], false),
                result("c", vec![], true),
                result("d", vec![Part::text("boom")], true),
            ],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "a", "content": "single text is a string"},
            {"type": "tool_result", "tool_use_id": "b"},
            // The API refuses an error result without content.
            {"type": "tool_result", "tool_use_id": "c", "content": "Tool call failed.", "is_error": true},
            {"type": "tool_result", "tool_use_id": "d", "content": "boom", "is_error": true}
        ])
    );
}

#[test]
fn encode_text_citations_are_not_replayed() {
    let req = request(vec![
        Message::user_text("q"),
        Message::new(
            Role::Assistant,
            vec![Part::Text(TextPart {
                text: "cited answer".into(),
                cache_control: None,
                citations: vec![Citation {
                    url: Some("https://e.com".into()),
                    ..Citation::default()
                }],
                signature: Some(Signature::new(Protocol::Gemini, "sig")),
            })],
        ),
        Message::user_text("more"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][1],
        json!({"role": "assistant", "content": [{"type": "text", "text": "cited answer"}]})
    );
}

// ---------------------------------------------------------------------------
// Tools and tool choice
// ---------------------------------------------------------------------------

#[test]
fn encode_tools_of_every_kind() {
    let mut req = request(vec![Message::user_text("hi")]);
    let native_search = json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 2});
    req.tools = vec![
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Get the weather".into()),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}},
                               "required": ["city"]}),
            strict: Some(true),
            cache_control: Some(json!({"type": "ephemeral"})),
        }),
        // No parameters: the mandatory schema is the empty object schema.
        function_tool("mcp.server:ping"),
        Tool::Custom(CustomTool {
            name: "apply_patch".into(),
            description: Some("Apply a patch".into()),
            format: Some(json!({"type": "grammar", "syntax": "lark", "definition": "start: /.+/"})),
        }),
        // A builtin of this family travels verbatim.
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: native_search.clone(),
        }),
        // Another vendor's code interpreter has no declaration here.
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::CodeExecution,
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "code_interpreter", "container": {"type": "auto"}}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::Other("file_search".into()),
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "file_search"}),
        }),
    ];
    // `strict` of another protocol's client is not forwarded: Anthropic holds
    // a strict tool to its structured-output dialect and per-request limits
    // that schemas written for OpenAI's strict mode do not meet.
    assert_eq!(
        encode_request(&req)["tools"],
        json!([
            {"name": "get_weather", "description": "Get the weather",
             "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
             "cache_control": {"type": "ephemeral"}},
            {"name": "mcp_server_ping", "input_schema": {"type": "object", "properties": {}}},
            {"name": "apply_patch", "description": "Apply a patch",
             "input_schema": {"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}},
            native_search
        ])
    );
    // A Messages client wrote its `strict` for this API: it is kept.
    req.source = Protocol::Anthropic;
    assert_eq!(
        encode_request(&req)["tools"][0],
        json!({"name": "get_weather", "description": "Get the weather",
               "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
               "strict": true, "cache_control": {"type": "ephemeral"}})
    );
}

#[test]
fn encode_foreign_web_search_becomes_the_basic_anthropic_tool() {
    let mut req = request(vec![Message::user_text("news?")]);
    req.tools = vec![
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "web_search_preview", "search_context_size": "low"}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Gemini,
            raw: json!({"googleSearch": {}}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebFetch,
            origin: Protocol::Gemini,
            raw: json!({"urlContext": {}}),
        }),
    ];
    assert_eq!(
        encode_request(&req)["tools"],
        json!([{"type": "web_search_20250305", "name": "web_search"}])
    );
}

#[test]
fn encode_tool_choice_variants() {
    let choice = |tool_choice: Option<ToolChoice>, parallel: Option<bool>| {
        let mut req = request(vec![Message::user_text("hi")]);
        req.tools = vec![function_tool("get_weather")];
        req.tool_choice = tool_choice;
        req.parallel_tool_calls = parallel;
        encode_request(&req).get("tool_choice").cloned()
    };
    assert_eq!(choice(None, None), None);
    assert_eq!(choice(None, Some(true)), None);
    assert_eq!(
        choice(Some(ToolChoice::Auto), None),
        Some(json!({"type": "auto"}))
    );
    assert_eq!(
        choice(Some(ToolChoice::Required), None),
        Some(json!({"type": "any"}))
    );
    assert_eq!(
        choice(Some(ToolChoice::None), None),
        Some(json!({"type": "none"}))
    );
    assert_eq!(
        choice(
            Some(ToolChoice::Tool {
                name: "get.weather".into()
            }),
            None
        ),
        Some(json!({"type": "tool", "name": "get_weather"}))
    );
    // parallel_tool_calls: false
    assert_eq!(
        choice(None, Some(false)),
        Some(json!({"type": "auto", "disable_parallel_tool_use": true}))
    );
    assert_eq!(
        choice(Some(ToolChoice::Required), Some(false)),
        Some(json!({"type": "any", "disable_parallel_tool_use": true}))
    );
    assert_eq!(
        choice(Some(ToolChoice::None), Some(false)),
        Some(json!({"type": "none"}))
    );
}

#[test]
fn encode_tool_choice_is_omitted_without_tools() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.tool_choice = Some(ToolChoice::Required);
    req.parallel_tool_calls = Some(false);
    let body = encode_request(&req);
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

#[test]
fn encode_multi_turn_tool_conversation_with_foreign_ids() {
    let req = request(vec![
        Message::user_text("Weather in Paris, and the time?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("Let me check."),
                Part::tool_call("call.1:x", "get_weather", r#"{"city":"Paris"}"#),
                Part::tool_call("call_2", "get_time", ""),
            ],
        ),
        // Results arrive in a different order, without tool names, in two
        // separate messages, followed by more user text.
        Message::new(Role::User, vec![Part::tool_result_text("call_2", "12:00")]),
        Message::new(
            Role::User,
            vec![Part::tool_result_text("call.1:x", "sunny")],
        ),
        Message::user_text("Thanks. Summarise."),
        Message::assistant_text("Sunny at noon."),
        Message::user_text("Great."),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            user("Weather in Paris, and the time?"),
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "call_1_x", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_use", "id": "call_2", "name": "get_time", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_2", "content": "12:00"},
                {"type": "tool_result", "tool_use_id": "call_1_x", "content": "sunny"},
                {"type": "text", "text": "Thanks. Summarise."}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "Sunny at noon."}]},
            user("Great.")
        ])
    );
}

#[test]
fn encode_tool_results_move_ahead_of_other_user_content() {
    let req = request(vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("toolu_1", "f", "{}")]),
        Message::new(
            Role::User,
            vec![
                Part::text("before"),
                Part::tool_result_text("toolu_1", "42"),
                Part::text("after"),
            ],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
            {"type": "text", "text": "before"},
            {"type": "text", "text": "after"}
        ])
    );
}

#[test]
fn encode_tool_arguments_are_always_an_object() {
    let custom = Part::ToolCall(ToolCall {
        id: "c".into(),
        name: "apply_patch".into(),
        arguments: "*** Begin Patch".into(),
        kind: ToolCallKind::Custom,
        signature: Some(Signature::new(Protocol::Gemini, "thought-sig")),
        cache_control: None,
    });
    let req = request(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("a", "f", "[1,2]"),
                Part::tool_call("b", "f", "not json"),
                custom,
                Part::tool_call("d", "f", "  "),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("a", "1"),
                Part::tool_result_text("b", "2"),
                Part::tool_result_text("c", "3"),
                Part::tool_result_text("d", "4"),
            ],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][1]["content"],
        json!([
            {"type": "tool_use", "id": "a", "name": "f", "input": {"input": [1, 2]}},
            {"type": "tool_use", "id": "b", "name": "f", "input": {"input": "not json"}},
            {"type": "tool_use", "id": "c", "name": "apply_patch", "input": {"input": "*** Begin Patch"}},
            {"type": "tool_use", "id": "d", "name": "f", "input": {}}
        ])
    );
}

#[test]
fn encode_tool_ids_that_collide_after_sanitising_stay_distinct() {
    let req = request(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("call.1", "f", "{}"),
                Part::tool_call("call:1", "f", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call:1", "second"),
                Part::tool_result_text("call.1", "first"),
            ],
        ),
    ]);
    let body = encode_request(&req);
    let calls = body["messages"][1]["content"].as_array().unwrap();
    let results = body["messages"][2]["content"].as_array().unwrap();
    let (first, second) = (
        calls[0]["id"].as_str().unwrap(),
        calls[1]["id"].as_str().unwrap(),
    );
    assert_eq!(first, "call_1");
    assert_ne!(first, second);
    assert!(
        second
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
    assert_eq!(results[0]["tool_use_id"], json!(second));
    assert_eq!(results[0]["content"], json!("second"));
    assert_eq!(results[1]["tool_use_id"], json!(first));
    // Deterministic: the same request encodes the same way.
    assert_eq!(encode_request(&req), body);
}

#[test]
fn encode_orphan_tool_result_becomes_user_text() {
    // The call was trimmed from the history by the client.
    let req = request(vec![
        Message::user_text("hello"),
        Message::assistant_text("hi"),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call_gone", "42 degrees"),
                Part::tool_result_text("call_gone_too", ""),
                Part::text("so?"),
            ],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2],
        json!({"role": "user", "content": [
            {"type": "text", "text": "42 degrees"},
            {"type": "text", "text": "Tool result was empty."},
            {"type": "text", "text": "so?"}
        ]})
    );
}

#[test]
fn encode_unanswered_tool_call_gets_an_error_result() {
    // Two calls, one answered, then the user moved on.
    let req = request(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("toolu_a", "f", "{}"),
                Part::tool_call("toolu_b", "f", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("toolu_b", "ok"),
                Part::text("never mind a"),
            ],
        ),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_b", "content": "ok"},
            {"type": "tool_result", "tool_use_id": "toolu_a", "is_error": true,
             "content": "Tool call was interrupted before any output was recorded."},
            {"type": "text", "text": "never mind a"}
        ])
    );
    // A conversation that ends on a tool call gets the result in a new turn.
    let req = request(vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("toolu_a", "f", "{}")]),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_a", "is_error": true,
             "content": "Tool call was interrupted before any output was recorded."}
        ]})
    );
}

#[test]
fn encode_mid_conversation_system_messages_become_tagged_user_text() {
    let req = request(vec![
        Message::user_text("one"),
        Message::new(
            Role::System,
            vec![Part::text("Be careful."), Part::text("And brief.")],
        ),
        Message::user_text("two"),
        Message::assistant_text("ok"),
        Message::new(Role::System, vec![Part::text("Now be verbose.")]),
        Message::new(Role::System, vec![Part::text("  ")]),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "one"},
                {"type": "text", "text": "<system>\nBe careful.\nAnd brief.\n</system>"},
                {"type": "text", "text": "two"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "<system>\nNow be verbose.\n</system>"}
            ]}
        ])
    );
}

#[test]
fn encode_mid_conversation_system_message_does_not_split_a_tool_exchange() {
    let req = request(vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("toolu_1", "f", "{}")]),
        Message::new(Role::System, vec![Part::text("reminder")]),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_1", "done")]),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "done"},
            {"type": "text", "text": "<system>\nreminder\n</system>"}
        ]})
    );
}

#[test]
fn encode_consecutive_same_role_messages_are_merged() {
    let req = request(vec![
        Message::user_text("a"),
        Message::user_text("b"),
        Message::new(Role::User, vec![]),
        Message::assistant_text("c"),
        Message::assistant_text("d"),
        Message::user_text("e"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "c"}, {"type": "text", "text": "d"}]},
            user("e")
        ])
    );
}

#[test]
fn encode_turns_emptied_by_dropped_parts_do_not_break_alternation() {
    // The middle user turn only has audio, which Messages cannot carry.
    let req = request(vec![
        Message::user_text("a"),
        Message::assistant_text("b"),
        Message::new(
            Role::User,
            vec![Part::Audio(MediaPart::base64("audio/wav", "UklGRg=="))],
        ),
        Message::assistant_text("c"),
        Message::user_text("d"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            user("a"),
            {"role": "assistant", "content": [{"type": "text", "text": "b"}, {"type": "text", "text": "c"}]},
            user("d")
        ])
    );
}

#[test]
fn encode_conversation_must_open_with_a_user_turn() {
    // A leading assistant turn gets a minimal user turn in front of it.
    let req = request(vec![
        Message::assistant_text("Hi, how can I help?"),
        Message::user_text("yo"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([
            user("(continued)"),
            {"role": "assistant", "content": [{"type": "text", "text": "Hi, how can I help?"}]},
            user("yo")
        ])
    );
    // No messages at all (system-only request).
    let mut req = request(vec![]);
    req.system = vec![Part::text("You are a poet.")];
    assert_eq!(
        encode_request(&req)["messages"],
        json!([user("(continued)")])
    );
}

#[test]
fn encode_trailing_assistant_prefill_is_made_valid() {
    let req = request(vec![
        Message::user_text("Write JSON"),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("{\"answer\":  \n"),
                reasoning(
                    "dangling",
                    Some(Signature::new(Protocol::Anthropic, "sig")),
                    false,
                ),
            ],
        ),
    ]);
    // No trailing thinking block, no trailing whitespace.
    assert_eq!(
        encode_request(&req)["messages"][1],
        json!({"role": "assistant", "content": [{"type": "text", "text": "{\"answer\":"}]})
    );
}

// ---------------------------------------------------------------------------
// Reasoning parts, opaque parts, refusals
// ---------------------------------------------------------------------------

#[test]
fn encode_reasoning_parts_only_with_an_anthropic_signature() {
    let req = request(vec![
        Message::user_text("think"),
        Message::new(
            Role::Assistant,
            vec![
                reasoning(
                    "native",
                    Some(Signature::new(Protocol::Anthropic, "EqQBCgIYAhIM")),
                    false,
                ),
                reasoning(
                    "",
                    Some(Signature::new(Protocol::Anthropic, "EmwKAhgBEgy3")),
                    true,
                ),
                // Foreign, unsigned and empty-signature reasoning is dropped:
                // Anthropic rejects thinking it did not sign.
                reasoning(
                    "gemini",
                    Some(Signature::new(Protocol::Gemini, "CiQB")),
                    false,
                ),
                reasoning(
                    "openai",
                    Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
                    false,
                ),
                reasoning(
                    "",
                    Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
                    true,
                ),
                reasoning("unsigned", None, false),
                reasoning(
                    "blank",
                    Some(Signature::new(Protocol::Anthropic, "")),
                    false,
                ),
                Part::text("Answer"),
                Part::tool_call("toolu_1", "f", "{}"),
            ],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_1", "ok")]),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "native", "signature": "EqQBCgIYAhIM"},
            {"type": "redacted_thinking", "data": "EmwKAhgBEgy3"},
            {"type": "text", "text": "Answer"},
            {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}
        ]})
    );
}

#[test]
fn encode_assistant_turn_of_only_foreign_reasoning_disappears() {
    let req = request(vec![
        Message::user_text("a"),
        Message::new(
            Role::Assistant,
            vec![reasoning(
                "x",
                Some(Signature::new(Protocol::Gemini, "CiQB")),
                false,
            )],
        ),
        Message::user_text("b"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "a"}, {"type": "text", "text": "b"}
        ]}])
    );
}

#[test]
fn encode_opaque_parts_only_for_their_own_family() {
    let server_tool_use = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                                 "input": {"query": "rust"}});
    let req = request(vec![
        Message::user_text("search"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Opaque(OpaquePart {
                    origin: Protocol::Anthropic,
                    raw: server_tool_use.clone(),
                }),
                Part::Opaque(OpaquePart {
                    origin: Protocol::OpenaiResponses,
                    raw: json!({"type": "web_search_call", "id": "ws_1"}),
                }),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Gemini,
                    raw: json!({"executableCode": {"code": "1+1"}}),
                }),
                Part::Refusal(RefusalPart {
                    text: "I can't help with that.".into(),
                }),
            ],
        ),
        Message::user_text("ok"),
    ]);
    assert_eq!(
        encode_request(&req)["messages"][1],
        json!({"role": "assistant", "content": [
            server_tool_use,
            {"type": "text", "text": "I can't help with that."}
        ]})
    );
}

// ---------------------------------------------------------------------------
// Structured output
// ---------------------------------------------------------------------------

#[test]
fn encode_json_schema_uses_native_structured_output() {
    let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}},
                        "required": ["answer"], "additionalProperties": false});
    let mut req = request(vec![Message::user_text("hi")]);
    req.response_format = Some(ResponseFormat::JsonSchema {
        name: Some("answer".into()),
        description: Some("An answer".into()),
        schema: schema.clone(),
        strict: Some(true),
    });
    let body = encode_request(&req);
    assert_eq!(
        body["output_config"],
        json!({"format": {"type": "json_schema", "schema": schema}})
    );
    assert!(body.get("system").is_none());
}

#[test]
fn encode_json_object_mode_becomes_a_system_instruction() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.system = vec![Part::text("You are terse.")];
    req.response_format = Some(ResponseFormat::JsonObject);
    let body = encode_request(&req);
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "You are terse."},
            {"type": "text", "text": "Respond with a single valid JSON object and nothing else: \
                                      no explanations and no markdown code fences."}
        ])
    );
    assert!(body.get("output_config").is_none());
    // Plain text format: nothing at all.
    req.response_format = Some(ResponseFormat::Text);
    assert_eq!(
        encode_request(&req)["system"],
        json!([{"type": "text", "text": "You are terse."}])
    );
}

#[test]
fn encode_json_schema_and_effort_share_output_config() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema: json!({"type": "object"}),
        strict: None,
    });
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::Low)));
    // The schema was written for another protocol, so it is fitted to the
    // dialect `output_config.format` takes: objects are closed.
    let fitted = json!({"type": "object", "additionalProperties": false});
    assert_eq!(
        encode_request(&req)["output_config"],
        json!({"format": {"type": "json_schema", "schema": fitted}, "effort": "low"})
    );
    // Turning thinking off removes the effort but not the format.
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Off));
    assert_eq!(
        encode_request(&req)["output_config"],
        json!({"format": {"type": "json_schema", "schema": fitted}})
    );
    // A Messages client's own schema is sent as written.
    req.source = Protocol::Anthropic;
    assert_eq!(
        encode_request(&req)["output_config"],
        json!({"format": {"type": "json_schema", "schema": {"type": "object"}}})
    );
}

// ---------------------------------------------------------------------------
// Reasoning settings
// ---------------------------------------------------------------------------

fn with_reasoning(depth: Depth) -> Request {
    let mut req = request(vec![Message::user_text("hi")]);
    req.reasoning = Some(ReasoningConfig::with_depth(depth));
    req
}

#[test]
fn encode_reasoning_for_an_unknown_model() {
    let body = encode_request(&with_reasoning(Depth::Off));
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert!(body.get("output_config").is_none());

    let body = encode_request(&with_reasoning(Depth::Level(Effort::High)));
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "high"}));

    // The API has no "minimal": the lowest effort it knows is "low".
    let body = encode_request(&with_reasoning(Depth::Level(Effort::Minimal)));
    assert_eq!(body["output_config"], json!({"effort": "low"}));

    let body = encode_request(&with_reasoning(Depth::Auto));
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert!(body.get("output_config").is_none());

    let body = encode_request(&with_reasoning(Depth::Budget(10_000)));
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 10_000})
    );
    assert_eq!(body["max_tokens"], json!(32_000));
}

#[test]
fn encode_reasoning_for_a_budget_only_model() {
    let caps = ThinkingSupport {
        min: 1024,
        max: 128_000,
        zero_allowed: true,
        dynamic_allowed: false,
        levels: vec![],
    };
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    // A level that slipped through unfitted is written as its table budget.
    let body = encode_request_with(&with_reasoning(Depth::Level(Effort::High)), &ctx);
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 24_576})
    );
    assert_eq!(body["max_tokens"], json!(64_000));
    assert!(body.get("output_config").is_none());
    // "Let the provider decide" cannot be said to such a model: say nothing.
    let body = encode_request_with(&with_reasoning(Depth::Auto), &ctx);
    assert!(body.get("thinking").is_none());
    // A budget under the API minimum is raised to it.
    let body = encode_request_with(&with_reasoning(Depth::Budget(100)), &ctx);
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );
}

#[test]
fn encode_reasoning_for_a_level_model() {
    let caps = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High, Effort::Max]);
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let body = encode_request_with(&with_reasoning(Depth::Level(Effort::Xhigh)), &ctx);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "max"}));
    // No budget range: a budget becomes the nearest effort.
    let body = encode_request_with(&with_reasoning(Depth::Budget(8192)), &ctx);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "medium"}));
    let body = encode_request_with(&with_reasoning(Depth::Auto), &ctx);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert!(body.get("output_config").is_none());
}

#[test]
fn encode_reasoning_is_removed_for_a_model_that_cannot_think() {
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    let mut req = with_reasoning(Depth::Level(Effort::High));
    req.temperature = Some(0.5);
    let body = encode_request_with(&req, &ctx);
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    assert_eq!(body["max_tokens"], json!(4096));
    assert_eq!(body["temperature"], json!(0.5));
}

#[test]
fn encode_budget_must_stay_below_max_tokens() {
    let ctx = UpstreamCtx {
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    // Fits: untouched.
    let mut req = with_reasoning(Depth::Budget(8192));
    req.max_output_tokens = Some(16_000);
    let body = encode_request_with(&req, &ctx);
    assert_eq!(
        (&body["max_tokens"], &body["thinking"]["budget_tokens"]),
        (&json!(16_000), &json!(8192))
    );
    // Too large for the client's limit: the budget shrinks under it.
    req.max_output_tokens = Some(4096);
    let body = encode_request_with(&req, &ctx);
    assert_eq!(
        (&body["max_tokens"], &body["thinking"]["budget_tokens"]),
        (&json!(4096), &json!(4095))
    );
    // The limit is under the smallest budget the API takes: raise the limit
    // to the model's instead.
    req.max_output_tokens = Some(1000);
    let body = encode_request_with(&req, &ctx);
    assert_eq!(
        (&body["max_tokens"], &body["thinking"]["budget_tokens"]),
        (&json!(64_000), &json!(8192))
    );
    // No client limit: the model's limit is used and bounds the budget.
    let req = with_reasoning(Depth::Budget(100_000));
    let body = encode_request_with(&req, &ctx);
    assert_eq!(
        (&body["max_tokens"], &body["thinking"]["budget_tokens"]),
        (&json!(64_000), &json!(63_999))
    );
}

#[test]
fn encode_forced_tool_choice_removes_thinking() {
    for choice in [
        ToolChoice::Required,
        ToolChoice::Tool {
            name: "get_weather".into(),
        },
    ] {
        let mut req = with_reasoning(Depth::Level(Effort::High));
        req.tools = vec![function_tool("get_weather")];
        req.tool_choice = Some(choice);
        req.temperature = Some(0.2);
        let body = encode_request(&req);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        // With thinking gone the sampling parameter is legal again.
        assert_eq!(body["temperature"], json!(0.2));
    }
    // auto / none keep thinking.
    let mut req = with_reasoning(Depth::Level(Effort::High));
    req.tools = vec![function_tool("get_weather")];
    req.tool_choice = Some(ToolChoice::Auto);
    assert_eq!(
        encode_request(&req)["thinking"],
        json!({"type": "adaptive"})
    );
    // An explicit "off" is compatible with forced tool use.
    let mut req = with_reasoning(Depth::Off);
    req.tools = vec![function_tool("get_weather")];
    req.tool_choice = Some(ToolChoice::Required);
    assert_eq!(
        encode_request(&req)["thinking"],
        json!({"type": "disabled"})
    );
}

#[test]
fn encode_summary_intent_becomes_thinking_display() {
    let mut req = with_reasoning(Depth::Level(Effort::High));
    req.reasoning.as_mut().unwrap().summary = Some(Summary::Detailed);
    assert_eq!(
        encode_request(&req)["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    req.reasoning.as_mut().unwrap().summary = Some(Summary::Off);
    assert_eq!(
        encode_request(&req)["thinking"],
        json!({"type": "adaptive", "display": "omitted"})
    );
    // `display` is invalid next to `disabled`…
    let mut req = with_reasoning(Depth::Off);
    req.reasoning.as_mut().unwrap().summary = Some(Summary::Auto);
    assert_eq!(
        encode_request(&req)["thinking"],
        json!({"type": "disabled"})
    );
    // …and a summary intent alone never switches thinking on.
    let mut req = request(vec![Message::user_text("hi")]);
    req.reasoning = Some(ReasoningConfig {
        depth: None,
        summary: Some(Summary::Auto),
    });
    assert!(encode_request(&req).get("thinking").is_none());
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

#[test]
fn encode_count_request_is_the_subset_the_endpoint_accepts() {
    let mut req = request(vec![Message::new(
        Role::User,
        vec![
            Part::text("How many tokens?"),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
            // URL media is rejected by the counting endpoint.
            Part::Image(MediaPart::url("https://example.com/x.png")),
        ],
    )]);
    req.system = vec![Part::text("Be brief.")];
    req.stream = true;
    req.max_output_tokens = Some(500);
    req.temperature = Some(0.5);
    req.user = Some("u".into());
    req.stop = vec!["X".into()];
    req.tools = vec![
        function_tool("get_weather"),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_search_20250305", "name": "web_search"}),
        }),
    ];
    req.tool_choice = Some(ToolChoice::Auto);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)));
    let body = AnthropicCodec
        .encode_count_request(&req, &UpstreamCtx::default())
        .expect("anthropic has a counting endpoint");
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": "Be brief."}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "How many tokens?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
            ]}],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"}
        })
    );
}
