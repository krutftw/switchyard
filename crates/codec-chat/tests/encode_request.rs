//! `encode_request`: IR requests (from any source protocol) into Chat
//! Completions bodies for an upstream.

mod common;

use common::{encode_request, encoded_messages};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::Protocol;
use switchyard_core::codec::{Codec, MaxTokensField, Quirks, UpstreamCtx};
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, MediaSource, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, TextPart,
    Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ModelThinking, ReasoningConfig, ThinkingSupport};

fn request(source: Protocol) -> Request {
    Request::new("gpt-4o", source)
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

fn result(call_id: &str, content: Vec<Part>) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: None,
        content,
        is_error: false,
        cache_control: None,
    })
}

// ---------------------------------------------------------------------------
// Skeleton and scalar parameters
// ---------------------------------------------------------------------------

#[test]
fn plain_text() {
    let mut req = request(Protocol::OpenaiChat);
    req.messages.push(Message::user_text("Hello"));
    let body = encode_request(&req);
    assert_eq!(
        body,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        })
    );
    // Key order is part of what is sent.
    assert_eq!(
        body.to_string(),
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}],"stream":false}"#
    );
}

#[test]
fn system_prompt_forms() {
    let mut req = request(Protocol::Anthropic);
    req.system = vec![Part::text("You are terse.")];
    req.messages.push(Message::user_text("Hi"));
    assert_eq!(
        encoded_messages(&req),
        json!([
            {"role": "system", "content": "You are terse."},
            {"role": "user", "content": "Hi"}
        ])
    );

    // Several blocks stay separate; cache markers and non-text parts are dropped.
    req.system = vec![
        Part::Text(TextPart {
            cache_control: Some(json!({"type": "ephemeral"})),
            ..TextPart::new("Block one.")
        }),
        Part::Image(MediaPart::url("https://example.com/x.png")),
        Part::text("Block two."),
        Part::text(""),
    ];
    assert_eq!(
        encoded_messages(&req)[0],
        json!({"role": "system", "content": [
            {"type": "text", "text": "Block one."},
            {"type": "text", "text": "Block two."}
        ]})
    );

    req.system = vec![];
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": "Hi"}])
    );
}

#[test]
fn mid_conversation_system_messages_keep_their_position_and_role() {
    let mut req = request(Protocol::Anthropic);
    req.system = vec![Part::text("lead")];
    req.messages = vec![
        Message::user_text("one"),
        Message {
            role: Role::System,
            parts: vec![Part::text("be careful now")],
            name: Some("policy".into()),
        },
        Message::user_text("two"),
    ];
    assert_eq!(
        encoded_messages(&req),
        json!([
            {"role": "system", "content": "lead"},
            {"role": "user", "content": "one"},
            {"role": "system", "content": "be careful now", "name": "policy"},
            {"role": "user", "content": "two"}
        ])
    );
}

#[test]
fn sampling_parameters() {
    let mut req = request(Protocol::Gemini);
    req.messages.push(Message::user_text("hi"));
    req.max_output_tokens = Some(256);
    req.temperature = Some(0.2);
    req.top_p = Some(0.9);
    req.top_k = Some(40);
    req.candidate_count = Some(2);
    req.stop = vec!["END".into()];
    req.seed = Some(7);
    req.presence_penalty = Some(0.5);
    req.frequency_penalty = Some(-0.5);
    req.user = Some("u1".into());
    req.prompt_cache_key = Some("k".into());
    req.store = Some(false);
    assert_eq!(
        encode_request(&req),
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "max_completion_tokens": 256,
            "temperature": 0.2,
            "top_p": 0.9,
            // `top_k` from another protocol is not an OpenAI parameter.
            "n": 2,
            "stop": ["END"],
            "seed": 7,
            "presence_penalty": 0.5,
            "frequency_penalty": -0.5,
            "user": "u1",
            "prompt_cache_key": "k",
            "store": false,
            "stream": false
        })
    );
}

#[test]
fn top_k_is_replayed_only_for_chat_clients() {
    let mut req = request(Protocol::OpenaiChat);
    req.top_k = Some(40);
    assert_eq!(encode_request(&req)["top_k"], json!(40));
}

#[test]
fn max_tokens_field_follows_the_quirk() {
    let mut req = request(Protocol::Anthropic);
    req.max_output_tokens = Some(1000);
    let legacy = UpstreamCtx {
        quirks: Quirks {
            max_tokens_field: MaxTokensField::MaxTokens,
            stream_usage: true,
        },
        ..UpstreamCtx::default()
    };
    let body = ChatCodec.encode_request(&req, &legacy).expect("encodes");
    assert_eq!(body["max_tokens"], json!(1000));
    assert!(body.get("max_completion_tokens").is_none());

    let body = encode_request(&req);
    assert_eq!(body["max_completion_tokens"], json!(1000));
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn streaming_asks_for_usage_unless_the_upstream_cannot() {
    let mut req = request(Protocol::Anthropic);
    req.stream = true;
    let body = encode_request(&req);
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));

    let no_usage = UpstreamCtx {
        quirks: Quirks {
            stream_usage: false,
            ..Quirks::default()
        },
        ..UpstreamCtx::default()
    };
    let body = ChatCodec.encode_request(&req, &no_usage).expect("encodes");
    assert_eq!(body["stream"], json!(true));
    assert!(body.get("stream_options").is_none());

    // `stream_options` on a non-streaming request is an error upstream.
    req.stream = false;
    assert!(encode_request(&req).get("stream_options").is_none());
}

#[test]
fn metadata_and_service_tier_are_vendor_scoped() {
    let mut req = request(Protocol::OpenaiResponses);
    req.metadata = json!({"trace": "abc"}).as_object().cloned();
    req.service_tier = Some("flex".into());
    req.store = Some(true);
    let body = encode_request(&req);
    assert_eq!(body["metadata"], json!({"trace": "abc"}));
    assert_eq!(body["service_tier"], json!("flex"));

    // Chat rejects `metadata` unless the completion is stored; Responses
    // has no such rule, so its metadata only travels with `store: true`.
    for store in [None, Some(false)] {
        req.store = store;
        assert!(encode_request(&req).get("metadata").is_none(), "{store:?}");
    }
    // What a Chat client wrote is replayed as written.
    let mut req = request(Protocol::OpenaiChat);
    req.metadata = json!({"trace": "abc"}).as_object().cloned();
    assert_eq!(encode_request(&req)["metadata"], json!({"trace": "abc"}));

    // Anthropic's metadata and tier names mean nothing to OpenAI.
    let mut req = request(Protocol::Anthropic);
    req.metadata = json!({"user_id": "u"}).as_object().cloned();
    req.service_tier = Some("standard_only".into());
    let body = encode_request(&req);
    assert!(body.get("metadata").is_none());
    assert!(body.get("service_tier").is_none());

    req.service_tier = Some("auto".into());
    assert_eq!(encode_request(&req)["service_tier"], json!("auto"));
}

#[test]
fn known_chat_extras_are_replayed_only_for_openai_sources() {
    let extras = json!({
        "logprobs": true,
        "top_logprobs": 3,
        "logit_bias": {"50256": -100},
        "modalities": ["text"],
        "audio": {"voice": "alloy", "format": "wav"},
        "prediction": {"type": "content", "content": "x"},
        "verbosity": "low",
        "safety_identifier": "u1",
        "vendor_flag": true,
        "extra_body": {"x": 1}
    });
    let mut req = request(Protocol::OpenaiChat);
    req.extra = extras.as_object().cloned().expect("an object");
    let body = encode_request(&req);
    for key in [
        "logprobs",
        "top_logprobs",
        "logit_bias",
        "modalities",
        "audio",
        "prediction",
        "verbosity",
        "safety_identifier",
    ] {
        assert_eq!(body[key], extras[key], "{key}");
    }
    assert!(body.get("vendor_flag").is_none());
    assert!(body.get("extra_body").is_none());

    // Responses has `top_logprobs` without the `logprobs` switch Chat needs.
    let mut req = request(Protocol::OpenaiResponses);
    req.extra.insert("top_logprobs".into(), json!(5));
    let body = encode_request(&req);
    assert_eq!(body["logprobs"], json!(true));
    assert_eq!(body["top_logprobs"], json!(5));

    let mut req = request(Protocol::Gemini);
    req.extra = extras.as_object().cloned().expect("an object");
    assert_eq!(
        encode_request(&req),
        json!({"model": "gpt-4o", "messages": [], "stream": false})
    );
}

// ---------------------------------------------------------------------------
// Reasoning depth
// ---------------------------------------------------------------------------

fn effort(depth: Depth, thinking: ModelThinking<'_>) -> Option<Value> {
    let mut req = request(Protocol::Anthropic);
    req.reasoning = Some(ReasoningConfig::with_depth(depth));
    let ctx = UpstreamCtx {
        thinking,
        ..UpstreamCtx::default()
    };
    ChatCodec
        .encode_request(&req, &ctx)
        .expect("encodes")
        .get("reasoning_effort")
        .cloned()
}

#[test]
fn reasoning_depth_is_written_as_reasoning_effort() {
    let unknown = ModelThinking::Unknown;
    assert_eq!(
        effort(Depth::Level(Effort::High), unknown),
        Some(json!("high"))
    );
    assert_eq!(
        effort(Depth::Level(Effort::Max), unknown),
        Some(json!("max"))
    );
    assert_eq!(effort(Depth::Budget(4000), unknown), Some(json!("medium")));
    assert_eq!(
        effort(Depth::Budget(100_000), unknown),
        Some(json!("xhigh"))
    );
    assert_eq!(effort(Depth::Off, unknown), Some(json!("none")));
    assert_eq!(effort(Depth::Auto, unknown), None);
    assert_eq!(
        effort(Depth::Level(Effort::High), ModelThinking::Unsupported),
        None
    );

    let caps = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let supported = ModelThinking::Supported(&caps);
    assert_eq!(
        effort(Depth::Level(Effort::Xhigh), supported),
        Some(json!("high"))
    );
    assert_eq!(effort(Depth::Budget(100), supported), Some(json!("low")));
}

#[test]
fn no_reasoning_config_writes_nothing() {
    let mut req = request(Protocol::OpenaiChat);
    assert!(encode_request(&req).get("reasoning_effort").is_none());
    req.reasoning = Some(ReasoningConfig::default());
    assert!(encode_request(&req).get("reasoning_effort").is_none());
}

// ---------------------------------------------------------------------------
// Content parts
// ---------------------------------------------------------------------------

#[test]
fn multimodal_user_content() {
    let mut req = request(Protocol::Anthropic);
    req.messages.push(Message::new(
        Role::User,
        vec![
            Part::text("Look:"),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
            Part::Image(MediaPart {
                detail: Some("high".into()),
                ..MediaPart::url("https://example.com/a.jpg")
            }),
            Part::Audio(MediaPart::base64("audio/mpeg", "SUQz")),
            Part::Document(MediaPart::base64("application/pdf", "JVBERi0x")),
            Part::Document(MediaPart {
                filename: Some("notes.txt".into()),
                ..MediaPart::base64("text/plain", "aGk=")
            }),
        ],
    ));
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": [
            {"type": "text", "text": "Look:"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR"}},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.jpg", "detail": "high"}},
            {"type": "input_audio", "input_audio": {"data": "SUQz", "format": "mp3"}},
            {"type": "file", "file": {
                "filename": "document.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0x"
            }},
            {"type": "file", "file": {
                "filename": "notes.txt",
                "file_data": "data:text/plain;base64,aGk="
            }}
        ]}])
    );
}

#[test]
fn provider_file_handles_only_travel_within_the_openai_family() {
    let file = Part::Document(MediaPart {
        source: MediaSource::FileRef {
            id: "file-abc".into(),
        },
        media_type: None,
        filename: None,
        detail: None,
        cache_control: None,
    });
    let mut req = request(Protocol::OpenaiResponses);
    req.messages.push(Message::new(
        Role::User,
        vec![Part::text("read"), file.clone()],
    ));
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": [
            {"type": "text", "text": "read"},
            {"type": "file", "file": {"file_id": "file-abc"}}
        ]}])
    );

    // A Gemini file URI or an Anthropic file id is not an OpenAI file.
    let mut req = request(Protocol::Gemini);
    req.messages
        .push(Message::new(Role::User, vec![Part::text("read"), file]));
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": "read"}])
    );
}

#[test]
fn document_by_url_is_named_instead_of_silently_dropped() {
    let mut req = request(Protocol::Anthropic);
    req.messages.push(Message::new(
        Role::User,
        vec![Part::Document(MediaPart::url(
            "https://example.com/spec.pdf",
        ))],
    ));
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": "[File: https://example.com/spec.pdf]"}])
    );
}

#[test]
fn cache_control_markers_are_not_sent() {
    let mut req = request(Protocol::Anthropic);
    req.messages.push(Message::new(
        Role::User,
        vec![
            Part::Text(TextPart {
                cache_control: Some(json!({"type": "ephemeral"})),
                ..TextPart::new("cached prefix")
            }),
            Part::Image(MediaPart {
                cache_control: Some(json!({"type": "ephemeral"})),
                ..MediaPart::url("https://example.com/a.png")
            }),
        ],
    ));
    req.tools.push(Tool::Function(FunctionTool {
        name: "f".into(),
        description: None,
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
        cache_control: Some(json!({"type": "ephemeral"})),
    }));
    let body = encode_request(&req);
    assert!(!body.to_string().contains("cache_control"));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "cached prefix"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
        ]}])
    );
}

#[test]
fn opaque_parts_only_return_to_the_protocol_that_made_them() {
    let chat_part = json!({"type": "video_url", "video_url": {"url": "https://example.com/v.mp4"}});
    let mut req = request(Protocol::OpenaiChat);
    req.messages.push(Message::new(
        Role::User,
        vec![
            Part::Opaque(OpaquePart {
                origin: Protocol::OpenaiChat,
                raw: chat_part.clone(),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "search_result", "source": "x"}),
            }),
        ],
    ));
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": [chat_part]}])
    );
}

#[test]
fn empty_and_consecutive_messages() {
    let mut req = request(Protocol::OpenaiChat);
    req.messages = vec![
        Message::user_text("one"),
        Message {
            name: Some("alice".into()),
            ..Message::user_text("two")
        },
        Message::assistant_text("a"),
        Message::assistant_text("b"),
        Message::new(Role::User, vec![]),
        Message::new(Role::Assistant, vec![]),
    ];
    // Chat allows consecutive messages of one role: nothing is merged.
    assert_eq!(
        encoded_messages(&req),
        json!([
            {"role": "user", "content": "one"},
            {"role": "user", "content": "two", "name": "alice"},
            {"role": "assistant", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": ""},
            {"role": "assistant", "content": ""}
        ])
    );
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[test]
fn tools_and_tool_choice_variants() {
    let mut req = request(Protocol::Anthropic);
    req.messages.push(Message::user_text("hi"));
    req.tools = vec![
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Weather".into()),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            strict: Some(true),
            cache_control: None,
        }),
        function_tool("ping"),
        Tool::Custom(CustomTool {
            name: "run_sql".into(),
            description: Some("Runs SQL".into()),
            format: Some(json!({"type": "text"})),
        }),
    ];
    req.parallel_tool_calls = Some(false);
    let body = encode_request(&req);
    assert_eq!(
        body["tools"],
        json!([
            {"type": "function", "function": {
                "name": "get_weather",
                "description": "Weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                "strict": true
            }},
            // A schema is mandatory: "no parameters" becomes the empty object schema.
            {"type": "function", "function": {
                "name": "ping",
                "parameters": {"type": "object", "properties": {}}
            }},
            {"type": "custom", "custom": {
                "name": "run_sql",
                "description": "Runs SQL",
                "format": {"type": "text"}
            }}
        ])
    );
    assert_eq!(body["parallel_tool_calls"], json!(false));
    assert!(body.get("tool_choice").is_none());

    let mut choice = |c: ToolChoice| {
        req.tool_choice = Some(c);
        encode_request(&req)["tool_choice"].clone()
    };
    assert_eq!(choice(ToolChoice::Auto), json!("auto"));
    assert_eq!(choice(ToolChoice::None), json!("none"));
    assert_eq!(choice(ToolChoice::Required), json!("required"));
    assert_eq!(
        choice(ToolChoice::Tool {
            name: "get_weather".into()
        }),
        json!({"type": "function", "function": {"name": "get_weather"}})
    );
    assert_eq!(
        choice(ToolChoice::Tool {
            name: "run_sql".into()
        }),
        json!({"type": "custom", "custom": {"name": "run_sql"}})
    );
}

#[test]
fn tool_choice_without_tools_is_omitted() {
    let mut req = request(Protocol::Anthropic);
    req.tool_choice = Some(ToolChoice::Auto);
    req.parallel_tool_calls = Some(true);
    assert_eq!(
        encode_request(&req),
        json!({"model": "gpt-4o", "messages": [], "stream": false})
    );
}

#[test]
fn builtin_tools_of_other_families_are_dropped() {
    let mut req = request(Protocol::Anthropic);
    req.tools = vec![
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::CodeExecution,
            origin: Protocol::Gemini,
            raw: json!({"codeExecution": {}}),
        }),
        function_tool("ping"),
    ];
    req.tool_choice = Some(ToolChoice::Auto);
    let body = encode_request(&req);
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "function": {
            "name": "ping",
            "parameters": {"type": "object", "properties": {}}
        }}])
    );
    assert!(body.get("web_search_options").is_none());

    // With only foreign built-ins nothing tool-related is left to send.
    req.tools.pop();
    let body = encode_request(&req);
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[test]
fn openai_family_web_search_becomes_web_search_options() {
    // Declared by a Chat client: replayed verbatim.
    let mut req = request(Protocol::OpenaiChat);
    req.tools = vec![Tool::Builtin(BuiltinTool {
        kind: BuiltinKind::WebSearch,
        origin: Protocol::OpenaiChat,
        raw: json!({"web_search_options": {"search_context_size": "low"}}),
    })];
    let body = encode_request(&req);
    assert_eq!(
        body["web_search_options"],
        json!({"search_context_size": "low"})
    );
    assert!(body.get("tools").is_none());

    // Declared by a Responses client for a search model: the location nests
    // under `approximate`.
    let mut req = request(Protocol::OpenaiResponses);
    req.model = "gpt-4o-search-preview".into();
    req.tools = vec![Tool::Builtin(BuiltinTool {
        kind: BuiltinKind::WebSearch,
        origin: Protocol::OpenaiResponses,
        raw: json!({
            "type": "web_search",
            "search_context_size": "high",
            "user_location": {"type": "approximate", "country": "FR", "city": "Paris"}
        }),
    })];
    assert_eq!(
        encode_request(&req)["web_search_options"],
        json!({
            "search_context_size": "high",
            "user_location": {
                "type": "approximate",
                "approximate": {"country": "FR", "city": "Paris"}
            }
        })
    );

    // Any other Chat model rejects `web_search_options` outright, so the
    // same declaration (Codex sends it with every request) is dropped.
    req.model = "gpt-4o".into();
    let body = encode_request(&req);
    assert!(body.get("web_search_options").is_none());
    assert!(body.get("tools").is_none());
}

// ---------------------------------------------------------------------------
// Tool-call conversations
// ---------------------------------------------------------------------------

#[test]
fn multi_turn_tool_conversation() {
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![
        Message::user_text("Weather in Paris and Rome?"),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("Let me check."),
                Part::tool_call("toolu_1", "get_weather", "{\"city\": \"Paris\"}"),
                Part::tool_call("toolu_2", "get_weather", ""),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                result("toolu_1", vec![Part::text("18C")]),
                result("toolu_2", vec![Part::text("24C"), Part::text("sunny")]),
                Part::text("Which is warmer?"),
            ],
        ),
        Message::assistant_text("Rome."),
    ];
    assert_eq!(
        encoded_messages(&req),
        json!([
            {"role": "user", "content": "Weather in Paris and Rome?"},
            {"role": "assistant", "content": "Let me check.", "tool_calls": [
                {"id": "toolu_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}},
                // Empty arguments mean "no arguments"; Chat wants a JSON document.
                {"id": "toolu_2", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{}"}}
            ]},
            // One tool message per result, in order, then the rest of the turn.
            {"role": "tool", "tool_call_id": "toolu_1", "content": "18C"},
            {"role": "tool", "tool_call_id": "toolu_2", "content": "24C\n\nsunny"},
            {"role": "user", "content": "Which is warmer?"},
            {"role": "assistant", "content": "Rome."}
        ])
    );
}

#[test]
fn user_message_with_only_tool_results_adds_no_empty_user_message() {
    let mut req = request(Protocol::Gemini);
    req.messages = vec![
        Message::new(Role::Assistant, vec![Part::tool_call("c1", "f", "{}")]),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "c1".into(),
                // Gemini supplies the name; Chat tool messages have no use for it.
                name: Some("f".into()),
                content: vec![Part::text("ok")],
                is_error: true,
                cache_control: Some(json!({"type": "ephemeral"})),
            })],
        ),
    ];
    assert_eq!(
        encoded_messages(&req),
        json!([
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ])
    );
}

#[test]
fn tool_result_images_are_relayed_in_a_following_user_message() {
    let image = Part::Image(MediaPart::base64("image/png", "AAAA"));
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("c1", "screenshot", "{}"),
                Part::tool_call("c2", "screenshot", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                result("c1", vec![Part::text("Here it is"), image.clone()]),
                // Image only: the tool message still needs some text.
                result(
                    "c2",
                    vec![Part::Image(MediaPart::url("https://example.com/s.png"))],
                ),
                Part::text("What do you see?"),
            ],
        ),
    ];
    let messages = encoded_messages(&req);
    assert_eq!(
        messages.as_array().expect("an array")[1..],
        [
            json!({"role": "tool", "tool_call_id": "c1", "content": "Here it is"}),
            json!({"role": "tool", "tool_call_id": "c2", "content":
                "[The tool returned non-text content; it is attached to the next user message.]"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "Content returned by the preceding tool call(s):"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/s.png"}},
                {"type": "text", "text": "What do you see?"}
            ]}),
        ]
    );
}

#[test]
fn tool_result_without_a_call_id_is_shown_as_user_text() {
    let mut req = request(Protocol::Gemini);
    req.messages = vec![Message::new(
        Role::User,
        vec![
            result("", vec![Part::text("orphan output")]),
            result("", vec![]),
        ],
    )];
    // A `tool` message that answers no call is rejected upstream.
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "user", "content": "orphan output"}])
    );
}

#[test]
fn tool_messages_are_moved_up_behind_the_call_they_answer() {
    let mut req = request(Protocol::OpenaiResponses);
    req.messages = vec![
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("c1", "a", "{}"),
                Part::tool_call("c2", "b", "{}"),
            ],
        ),
        Message::user_text("while you wait"),
        Message::new(
            Role::User,
            vec![
                result("c2", vec![Part::text("B")]),
                result("c1", vec![Part::text("A")]),
            ],
        ),
    ];
    let roles: Vec<(String, String)> = encoded_messages(&req)
        .as_array()
        .expect("an array")
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or_default().to_string(),
                m["tool_call_id"]
                    .as_str()
                    .or(m["content"].as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("assistant".to_string(), String::new()),
            // Relative order of the results is kept.
            ("tool".to_string(), "c2".to_string()),
            ("tool".to_string(), "c1".to_string()),
            ("user".to_string(), "while you wait".to_string()),
        ]
    );
}

#[test]
fn incomplete_tool_histories_are_left_in_input_order() {
    let mut req = request(Protocol::OpenaiResponses);
    req.messages = vec![
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("c1", "a", "{}"),
                Part::tool_call("c2", "b", "{}"),
            ],
        ),
        Message::user_text("interruption"),
        Message::new(Role::User, vec![result("c1", vec![Part::text("A")])]),
    ];
    let messages = encoded_messages(&req);
    let roles: Vec<&str> = messages
        .as_array()
        .expect("an array")
        .iter()
        .map(|m| m["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(roles, ["assistant", "user", "tool"]);
}

#[test]
fn custom_tool_call_in_history() {
    let mut req = request(Protocol::OpenaiResponses);
    req.messages = vec![Message::new(
        Role::Assistant,
        vec![Part::ToolCall(ToolCall {
            id: "c9".into(),
            name: "run_sql".into(),
            arguments: "SELECT 1".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })],
    )];
    assert_eq!(
        encoded_messages(&req),
        json!([{"role": "assistant", "content": "", "tool_calls": [
            {"id": "c9", "type": "custom", "custom": {"name": "run_sql", "input": "SELECT 1"}}
        ]}])
    );
}

#[test]
fn assistant_text_refusal_and_dropped_parts() {
    let mut req = request(Protocol::OpenaiResponses);
    req.messages = vec![
        Message::new(
            Role::Assistant,
            vec![
                Part::text("before "),
                Part::tool_call("c1", "f", "{}"),
                Part::text("after"),
                Part::Refusal(RefusalPart { text: "no".into() }),
                // Generated media and foreign blocks have no place here.
                Part::Image(MediaPart::base64("image/png", "AAAA")),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Anthropic,
                    raw: json!({"type": "x"}),
                }),
            ],
        ),
        // A turn whose every part was dropped disappears instead of becoming
        // an empty assistant message.
        Message::new(
            Role::Assistant,
            vec![Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "x"}),
            })],
        ),
    ];
    assert_eq!(
        encoded_messages(&req),
        json!([{
            "role": "assistant",
            "content": "before after",
            "refusal": "no",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]
        }])
    );
}

// ---------------------------------------------------------------------------
// Reasoning and signatures in history
// ---------------------------------------------------------------------------

fn assistant_with(parts: Vec<Part>) -> Value {
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![Message::new(Role::Assistant, parts)];
    encoded_messages(&req)
}

#[test]
fn unsigned_reasoning_is_replayed_as_reasoning_content() {
    assert_eq!(
        assistant_with(vec![
            Part::reasoning("first thought"),
            Part::reasoning("second thought"),
            Part::text("answer"),
        ]),
        json!([{
            "role": "assistant",
            "content": "answer",
            "reasoning_content": "first thought\n\nsecond thought"
        }])
    );
}

#[test]
fn chat_issued_reasoning_blobs_return_in_reasoning_details() {
    assert_eq!(
        assistant_with(vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "thought".into(),
                signature: Some(Signature::new(Protocol::OpenaiChat, "sig-1")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: String::new(),
                signature: Some(Signature::new(Protocol::OpenaiChat, "enc-2")),
                redacted: true,
            }),
            Part::text("answer"),
        ]),
        json!([{
            "role": "assistant",
            "content": "answer",
            "reasoning_content": "thought",
            "reasoning_details": [
                {"type": "reasoning.text", "text": "thought", "signature": "sig-1", "index": 0},
                {"type": "reasoning.encrypted", "data": "enc-2", "id": "rs_1", "index": 1}
            ]
        }])
    );
}

#[test]
fn responses_issued_blob_is_dropped_but_its_summary_text_is_kept() {
    // Same vendor family, but `encrypted_content` has no slot in Chat.
    assert_eq!(
        assistant_with(vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: "summary".into(),
                signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
                redacted: false,
            }),
            Part::text("answer"),
        ]),
        json!([{"role": "assistant", "content": "answer", "reasoning_content": "summary"}])
    );
}

#[test]
fn foreign_signed_reasoning_is_dropped_entirely() {
    let body = assistant_with(vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "claude's private thought".into(),
            signature: Some(Signature::new(Protocol::Anthropic, "ErUBCkYIBxgC")),
            redacted: false,
        }),
        Part::Reasoning(Reasoning {
            id: None,
            text: String::new(),
            signature: Some(Signature::new(Protocol::Anthropic, "redacted-payload")),
            redacted: true,
        }),
        Part::Reasoning(Reasoning {
            id: None,
            text: "gemini thought".into(),
            signature: Some(Signature::new(Protocol::Gemini, "CiQB")),
            redacted: false,
        }),
        Part::text("answer"),
    ]);
    assert_eq!(body, json!([{"role": "assistant", "content": "answer"}]));
}

#[test]
fn assistant_turn_of_only_foreign_reasoning_disappears() {
    assert_eq!(
        assistant_with(vec![Part::Reasoning(Reasoning {
            id: None,
            text: "t".into(),
            signature: Some(Signature::new(Protocol::Anthropic, "sig")),
            redacted: false,
        })]),
        json!([])
    );
}

#[test]
fn tool_call_signatures_are_family_scoped() {
    let call = |id: &str, origin: Protocol| {
        Part::ToolCall(ToolCall {
            id: id.into(),
            name: "f".into(),
            arguments: "{}".into(),
            kind: ToolCallKind::Function,
            signature: Some(Signature::new(origin, "SIG")),
            cache_control: None,
        })
    };
    assert_eq!(
        assistant_with(vec![
            call("c1", Protocol::OpenaiChat),
            call("c2", Protocol::Gemini),
            call("c3", Protocol::Anthropic),
        ]),
        json!([{"role": "assistant", "content": "", "tool_calls": [
            // Issued by a Chat upstream (Google's compatible endpoint): replayed.
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"},
             "extra_content": {"google": {"thought_signature": "SIG"}}},
            // Foreign blobs are dropped; the call itself stays.
            {"id": "c2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
            {"id": "c3", "type": "function", "function": {"name": "f", "arguments": "{}"}}
        ]}])
    );
}

// ---------------------------------------------------------------------------
// Structured output
// ---------------------------------------------------------------------------

#[test]
fn response_format_variants() {
    let mut req = request(Protocol::Gemini);
    let mut format = |f: ResponseFormat| {
        req.response_format = Some(f);
        encode_request(&req)["response_format"].clone()
    };
    assert_eq!(format(ResponseFormat::Text), json!({"type": "text"}));
    assert_eq!(
        format(ResponseFormat::JsonObject),
        json!({"type": "json_object"})
    );
    assert_eq!(
        format(ResponseFormat::JsonSchema {
            name: Some("answer".into()),
            description: Some("The answer".into()),
            schema: json!({"type": "object"}),
            strict: Some(true),
        }),
        // A schema written for another protocol is normalised like tool
        // parameters are.
        json!({"type": "json_schema", "json_schema": {
            "name": "answer",
            "description": "The answer",
            "schema": {"type": "object", "properties": {}},
            "strict": true
        }})
    );
    // Chat requires a name; Gemini's response schema has none.
    assert_eq!(
        format(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            schema: json!({"type": "object"}),
            strict: None,
        }),
        json!({"type": "json_schema", "json_schema": {
            "name": "response",
            "schema": {"type": "object", "properties": {}}
        }})
    );
}
