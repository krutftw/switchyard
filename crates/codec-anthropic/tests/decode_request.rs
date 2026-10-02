//! Client side: Messages request bodies → canonical requests.

mod common;

use common::decode_request;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, FunctionTool, MediaPart, MediaSource, Message, OpaquePart,
    Part, Reasoning, Request, ResponseFormat, Role, Signature, TextPart, Tool, ToolCall,
    ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::{Codec, CodecError, Protocol, RequestPath};

fn reasoning_of(body: Value) -> Option<ReasoningConfig> {
    let mut full = json!({"model": "m", "max_tokens": 64000, "messages": []});
    for (key, value) in body.as_object().unwrap() {
        full[key] = value.clone();
    }
    decode_request(&full).reasoning
}

fn tool_choice_of(choice: Value) -> (Option<ToolChoice>, Option<bool>) {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 10, "messages": [], "tool_choice": choice
    }));
    (request.tool_choice, request.parallel_tool_calls)
}

// ---------------------------------------------------------------------------
// Plain text and scalars
// ---------------------------------------------------------------------------

#[test]
fn decode_plain_text_request() {
    let request = decode_request(&json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "Hello, Claude"}]
    }));
    let mut expected = Request::new("claude-sonnet-4-5", Protocol::Anthropic);
    expected.max_output_tokens = Some(1024);
    expected.messages = vec![Message::user_text("Hello, Claude")];
    assert_eq!(request, expected);
}

#[test]
fn decode_sampling_and_scalar_params() {
    let request = decode_request(&json!({
        "model": "claude-opus-4-5",
        "max_tokens": 2048.0,
        "temperature": 0.7,
        "top_p": 0.9,
        "top_k": 40,
        "stop_sequences": ["END", "STOP"],
        "metadata": {"user_id": "user-123", "trace": "abc"},
        "service_tier": "standard_only",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}]
    }));
    assert_eq!(request.max_output_tokens, Some(2048));
    assert_eq!(request.temperature, Some(0.7));
    assert_eq!(request.top_p, Some(0.9));
    assert_eq!(request.top_k, Some(40));
    assert_eq!(request.stop, vec!["END".to_string(), "STOP".to_string()]);
    assert_eq!(request.user.as_deref(), Some("user-123"));
    assert_eq!(
        request.metadata,
        Some(json!({"trace": "abc"}).as_object().unwrap().clone())
    );
    assert_eq!(request.service_tier.as_deref(), Some("standard_only"));
    assert!(request.stream);
    assert_eq!(request.source, Protocol::Anthropic);
}

#[test]
fn decode_stream_flag_only_for_literal_true() {
    for (value, expected) in [
        (json!(true), true),
        (json!(false), false),
        (json!("true"), false),
        (json!(1), false),
        (Value::Null, false),
    ] {
        let request = decode_request(&json!({
            "model": "m", "max_tokens": 1, "messages": [], "stream": value
        }));
        assert_eq!(request.stream, expected, "stream = {value}");
    }
}

#[test]
fn decode_unknown_top_level_fields_go_to_extra() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "speed": "fast",
        "container": "container_01",
        "mcp_servers": [{"type": "url", "url": "https://mcp.example.com", "name": "x"}],
        "betas": ["files-api-2025-04-14"],
        "something_null": null
    }));
    assert_eq!(request.extra.get("speed"), Some(&json!("fast")));
    assert_eq!(request.extra.get("container"), Some(&json!("container_01")));
    assert!(request.extra.contains_key("mcp_servers"));
    assert!(request.extra.contains_key("betas"));
    assert!(!request.extra.contains_key("something_null"));
    assert!(!request.extra.contains_key("model"));
}

// ---------------------------------------------------------------------------
// System prompt forms
// ---------------------------------------------------------------------------

#[test]
fn decode_system_as_string() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "system": "You are terse.", "messages": []
    }));
    assert_eq!(request.system, vec![Part::text("You are terse.")]);
    assert_eq!(request.system_text(), "You are terse.");
}

#[test]
fn decode_system_as_block_array_keeps_cache_control_per_block() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "system": [
            {"type": "text", "text": "Long reference document"},
            {"type": "text", "text": "Instructions", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "text", "text": ""},
            "bare string"
        ]
    }));
    assert_eq!(
        request.system,
        vec![
            Part::text("Long reference document"),
            Part::Text(TextPart {
                text: "Instructions".into(),
                cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
                citations: vec![],
                signature: None,
            }),
            Part::text("bare string"),
        ]
    );
}

#[test]
fn decode_system_absent_null_or_empty_is_no_system() {
    for system in [Value::Null, json!(""), json!([])] {
        let request = decode_request(&json!({
            "model": "m", "max_tokens": 1, "messages": [], "system": system
        }));
        assert!(request.system.is_empty());
    }
    let request = decode_request(&json!({"model": "m", "max_tokens": 1, "messages": []}));
    assert!(request.system.is_empty());
}

#[test]
fn decode_mid_conversation_system_message_stays_in_place_as_system_role() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [
            {"role": "user", "content": "one"},
            {"role": "system", "content": "Be careful from now on."},
            {"role": "developer", "content": [{"type": "text", "text": "And brief."},
                                              {"type": "image", "source": {"type": "url", "url": "https://e.com/x.png"}}]},
            {"role": "user", "content": "two"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("one"),
            Message::new(Role::System, vec![Part::text("Be careful from now on.")]),
            Message::new(Role::System, vec![Part::text("And brief.")]),
            Message::user_text("two"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Multimodal content
// ---------------------------------------------------------------------------

#[test]
fn decode_images_by_base64_url_data_uri_and_file() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is in these?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/cat.png"},
             "cache_control": {"type": "ephemeral"}},
            {"type": "image", "source": {"type": "url", "url": "data:image/png;base64,iVBORw0KGgo="}},
            {"type": "image", "source": {"type": "file", "file_id": "file_011CNha8iCJcU1wXNR6q4V8w"}}
        ]}]
    }));
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::text("What is in these?"),
            Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
            Part::Image(MediaPart {
                cache_control: Some(json!({"type": "ephemeral"})),
                ..MediaPart::url("https://example.com/cat.png")
            }),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
            Part::Image(MediaPart {
                source: MediaSource::FileRef {
                    id: "file_011CNha8iCJcU1wXNR6q4V8w".into()
                },
                media_type: None,
                filename: None,
                detail: None,
                cache_control: None,
            }),
        ]
    );
}

#[test]
fn decode_documents_pdf_text_url_and_file() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [
            {"type": "document", "title": "Q3 report",
             "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQ="},
             "cache_control": {"type": "ephemeral"}},
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "plain words"}},
            {"type": "document", "source": {"type": "url", "url": "https://example.com/paper.pdf"}},
            {"type": "document", "source": {"type": "file", "file_id": "file_abc"}, "title": "Uploaded"}
        ]}]
    }));
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::Document(MediaPart {
                filename: Some("Q3 report".into()),
                cache_control: Some(json!({"type": "ephemeral"})),
                ..MediaPart::base64("application/pdf", "JVBERi0xLjQ=")
            }),
            // Text documents are stored as bytes: base64("plain words").
            Part::Document(MediaPart::base64("text/plain", "cGxhaW4gd29yZHM=")),
            Part::Document(MediaPart {
                media_type: Some("application/pdf".into()),
                ..MediaPart::url("https://example.com/paper.pdf")
            }),
            Part::Document(MediaPart {
                source: MediaSource::FileRef {
                    id: "file_abc".into()
                },
                media_type: None,
                filename: Some("Uploaded".into()),
                detail: None,
                cache_control: None,
            }),
        ]
    );
}

#[test]
fn decode_document_with_custom_content_source_is_kept_opaque() {
    let block = json!({"type": "document", "source": {"type": "content", "content": [
        {"type": "text", "text": "chunk"}]}});
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [block.clone()]}]
    }));
    // The block itself for an Anthropic upstream, followed by the rendering
    // every other upstream is shown in its place (they drop foreign opaque
    // blocks, which would silently remove what the user attached).
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: block
            }),
            Part::text("<document>\nchunk\n</document>"),
        ]
    );
}

#[test]
fn decode_opaque_user_block_without_readable_content_has_no_rendering() {
    // A custom-content document that holds nothing a model could read, and a
    // block of a kind the codec does not know: opaque, and nothing else.
    let empty = json!({"type": "document", "source": {"type": "content", "content": [
        {"type": "text", "text": "  "}]}});
    let unknown = json!({"type": "container_upload", "file_id": "file_1"});
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [empty.clone(), unknown.clone()]}]
    }));
    let opaque = |raw: Value| {
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw,
        })
    };
    assert_eq!(
        request.messages[0].parts,
        vec![opaque(empty), opaque(unknown)]
    );
}

#[test]
fn decode_text_block_with_citations_and_cache_control() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "assistant", "content": [
            {"type": "text", "text": "The sky is blue.",
             "cache_control": {"type": "ephemeral"},
             "citations": [
                {"type": "web_search_result_location", "url": "https://sky.example", "title": "Sky",
                 "encrypted_index": "Eo8BCio=", "cited_text": "blue"},
                {"type": "char_location", "cited_text": "the sky", "document_index": 0,
                 "document_title": "Doc", "start_char_index": 0, "end_char_index": 7}
             ]}
        ]}, {"role": "user", "content": "ok"}]
    }));
    assert_eq!(
        request.messages[0].parts,
        vec![Part::Text(TextPart {
            text: "The sky is blue.".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
            citations: vec![
                Citation {
                    url: Some("https://sky.example".into()),
                    title: Some("Sky".into()),
                    cited_text: Some("blue".into()),
                    start: None,
                    end: None,
                },
                Citation {
                    url: None,
                    title: Some("Doc".into()),
                    cited_text: Some("the sky".into()),
                    start: None,
                    end: None,
                },
            ],
            signature: None,
        })]
    );
}

// ---------------------------------------------------------------------------
// Tools and tool choice
// ---------------------------------------------------------------------------

#[test]
fn decode_custom_tools() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "tools": [
            {"name": "get_weather", "description": "Get the weather",
             "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
             "strict": true, "cache_control": {"type": "ephemeral"}},
            {"type": "custom", "name": "no_schema"},
            {"description": "nameless tools are skipped"},
            "not an object"
        ]
    }));
    assert_eq!(
        request.tools,
        vec![
            Tool::Function(FunctionTool {
                name: "get_weather".into(),
                description: Some("Get the weather".into()),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}},
                                   "required": ["city"]}),
                strict: Some(true),
                cache_control: Some(json!({"type": "ephemeral"})),
            }),
            Tool::Function(FunctionTool {
                name: "no_schema".into(),
                description: None,
                parameters: Value::Null,
                strict: None,
                cache_control: None,
            }),
        ]
    );
}

#[test]
fn decode_server_tools_by_type_prefix_keeps_raw() {
    let web_search = json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3,
                            "allowed_domains": ["example.com"]});
    let web_fetch = json!({"type": "web_fetch_20250910", "name": "web_fetch"});
    let code = json!({"type": "code_execution_20250825", "name": "code_execution"});
    let bash = json!({"type": "bash_20250124", "name": "bash"});
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "tools": [web_search.clone(), web_fetch.clone(), code.clone(), bash.clone()]
    }));
    let builtin = |kind, raw: &Value| {
        Tool::Builtin(BuiltinTool {
            kind,
            origin: Protocol::Anthropic,
            raw: raw.clone(),
        })
    };
    assert_eq!(
        request.tools,
        vec![
            builtin(BuiltinKind::WebSearch, &web_search),
            builtin(BuiltinKind::WebFetch, &web_fetch),
            builtin(BuiltinKind::CodeExecution, &code),
            builtin(BuiltinKind::Other("bash_20250124".into()), &bash),
        ]
    );
}

#[test]
fn decode_tool_choice_variants() {
    assert_eq!(
        tool_choice_of(json!({"type": "auto"})),
        (Some(ToolChoice::Auto), None)
    );
    assert_eq!(
        tool_choice_of(json!({"type": "any"})),
        (Some(ToolChoice::Required), None)
    );
    assert_eq!(
        tool_choice_of(json!({"type": "none"})),
        (Some(ToolChoice::None), None)
    );
    assert_eq!(
        tool_choice_of(json!({"type": "tool", "name": "get_weather"})),
        (
            Some(ToolChoice::Tool {
                name: "get_weather".into()
            }),
            None
        )
    );
    // Bare strings are accepted too.
    assert_eq!(
        tool_choice_of(json!("auto")),
        (Some(ToolChoice::Auto), None)
    );
    assert_eq!(
        tool_choice_of(json!("any")),
        (Some(ToolChoice::Required), None)
    );
    // Absent / null: nothing.
    assert_eq!(tool_choice_of(Value::Null), (None, None));
}

#[test]
fn decode_tool_choice_disable_parallel_tool_use() {
    assert_eq!(
        tool_choice_of(json!({"type": "auto", "disable_parallel_tool_use": true})),
        (Some(ToolChoice::Auto), Some(false))
    );
    assert_eq!(
        tool_choice_of(json!({"type": "any", "disable_parallel_tool_use": true})),
        (Some(ToolChoice::Required), Some(false))
    );
    assert_eq!(
        tool_choice_of(json!({"type": "auto", "disable_parallel_tool_use": false})),
        (Some(ToolChoice::Auto), None)
    );
}

#[test]
fn decode_tool_choice_fails_closed_on_unknown_restrictions() {
    assert_eq!(
        tool_choice_of(json!({"type": "tool"})),
        (Some(ToolChoice::None), None)
    );
    assert_eq!(
        tool_choice_of(json!({"type": "something_new"})),
        (Some(ToolChoice::None), None)
    );
}

// ---------------------------------------------------------------------------
// Multi-turn tool conversation
// ---------------------------------------------------------------------------

#[test]
fn decode_multi_turn_tool_call_conversation() {
    let request = decode_request(&json!({
        "model": "claude-sonnet-4-5", "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Weather in Paris and Rome?"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_01A", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_use", "id": "toolu_01B", "name": "get_weather", "input": {"city": "Rome"},
                 "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "18°C, cloudy"},
                {"type": "tool_result", "tool_use_id": "toolu_01B", "is_error": true,
                 "content": [{"type": "text", "text": "station offline"}]},
                {"type": "text", "text": "Summarise please."}
            ]},
            {"role": "assistant", "content": "Paris is 18°C; Rome is unknown."}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("Weather in Paris and Rome?"),
            Message::new(
                Role::Assistant,
                vec![
                    Part::text("Let me check."),
                    Part::tool_call("toolu_01A", "get_weather", r#"{"city":"Paris"}"#),
                    Part::ToolCall(ToolCall {
                        id: "toolu_01B".into(),
                        name: "get_weather".into(),
                        arguments: r#"{"city":"Rome"}"#.into(),
                        kind: ToolCallKind::Function,
                        signature: None,
                        cache_control: Some(json!({"type": "ephemeral"})),
                    }),
                ]
            ),
            Message::new(
                Role::User,
                vec![
                    Part::tool_result_text("toolu_01A", "18°C, cloudy"),
                    Part::ToolResult(ToolResult {
                        call_id: "toolu_01B".into(),
                        name: None,
                        content: vec![Part::text("station offline")],
                        is_error: true,
                        cache_control: None,
                    }),
                    Part::text("Summarise please."),
                ]
            ),
            Message::assistant_text("Paris is 18°C; Rome is unknown."),
        ]
    );
    assert!(request.has_tool_traffic());
    assert_eq!(request.tool_name_for_call("toolu_01B"), Some("get_weather"));
}

#[test]
fn decode_tool_result_with_images_and_hoisted_cache_control() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "screenshot attached", "cache_control": {"type": "ephemeral"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
            ]},
            {"type": "tool_result", "tool_use_id": "toolu_2", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "tool_result", "tool_use_id": "toolu_3", "content": ""},
            {"type": "tool_result", "tool_use_id": "toolu_4", "content": null}
        ]}]
    }));
    let result = |id: &str, content: Vec<Part>, cache: Option<Value>| {
        Part::ToolResult(ToolResult {
            call_id: id.into(),
            name: None,
            content,
            is_error: false,
            cache_control: cache,
        })
    };
    assert_eq!(
        request.messages[0].parts,
        vec![
            // The API rejects cache_control inside tool_result content, so a
            // marker found there moves to the result itself.
            result(
                "toolu_1",
                vec![
                    Part::text("screenshot attached"),
                    Part::Image(MediaPart::base64("image/png", "iVBOR")),
                ],
                Some(json!({"type": "ephemeral"}))
            ),
            result(
                "toolu_2",
                vec![],
                Some(json!({"type": "ephemeral", "ttl": "1h"}))
            ),
            result("toolu_3", vec![], None),
            result("toolu_4", vec![], None),
        ]
    );
}

#[test]
fn decode_tool_use_input_oddities() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "id": "a", "name": "f"},
            {"type": "tool_use", "id": "b", "name": "f", "input": null},
            {"type": "tool_use", "id": "c", "name": "f", "input": {}},
            {"type": "tool_use", "id": "d", "name": "f", "input": "{\"x\": 1}"}
        ]}]
    }));
    let arguments: Vec<String> = request.messages[0]
        .tool_calls()
        .map(|call| call.arguments.clone())
        .collect();
    assert_eq!(arguments, vec!["{}", "{}", "{}", "{\"x\": 1}"]);
}

// ---------------------------------------------------------------------------
// Thinking blocks in history and signatures
// ---------------------------------------------------------------------------

#[test]
fn decode_thinking_blocks_tag_signatures_with_their_origin() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Let me think.", "signature": "EqQBCgIYAhIM"},
            {"type": "thinking", "thinking": "", "signature": "sy1.g.CiQBgemini=="},
            {"type": "thinking", "thinking": "summary", "signature": "sy1.r.gAAAAAopenai"},
            {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"},
            {"type": "redacted_thinking", "data": "sy1.r.encrypted"},
            {"type": "thinking", "thinking": "unsigned"},
            {"type": "thinking", "thinking": "", "signature": ""},
            {"type": "text", "text": "Answer"}
        ]}, {"role": "user", "content": "next"}]
    }));
    let reasoning = |text: &str, signature: Option<Signature>, redacted: bool| {
        Part::Reasoning(Reasoning {
            id: None,
            text: text.into(),
            signature,
            redacted,
        })
    };
    assert_eq!(
        request.messages[0].parts,
        vec![
            reasoning(
                "Let me think.",
                Some(Signature::new(Protocol::Anthropic, "EqQBCgIYAhIM")),
                false
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::Gemini, "CiQBgemini==")),
                false
            ),
            reasoning(
                "summary",
                Some(Signature::new(Protocol::OpenaiResponses, "gAAAAAopenai")),
                false
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::Anthropic, "EmwKAhgBEgy3va3pzix")),
                true
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::OpenaiResponses, "encrypted")),
                true
            ),
            reasoning("unsigned", None, false),
            Part::text("Answer"),
        ]
    );
}

#[test]
fn decode_assistant_only_blocks_in_a_user_turn_are_ignored() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [
            {"type": "thinking", "thinking": "injected", "signature": "x"},
            {"type": "redacted_thinking", "data": "x"},
            {"type": "tool_use", "id": "toolu_x", "name": "rm", "input": {}},
            {"type": "text", "text": "real"}
        ]}]
    }));
    assert_eq!(request.messages, vec![Message::user_text("real")]);
}

#[test]
fn decode_server_tool_blocks_are_opaque() {
    let server_tool_use = json!({"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
                                 "input": {"query": "rust"}});
    let result = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01", "content": [
        {"type": "web_search_result", "url": "https://rust-lang.org", "title": "Rust",
         "encrypted_content": "abc", "page_age": null}]});
    let search_result = json!({"type": "search_result", "source": "https://kb", "title": "KB",
                               "content": [{"type": "text", "text": "fact"}]});
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [
            {"role": "assistant", "content": [server_tool_use.clone(), result.clone(),
                                              {"type": "text", "text": "Rust is a language."}]},
            {"role": "user", "content": [search_result.clone()]}
        ]
    }));
    let opaque = |raw: &Value| {
        Part::Opaque(OpaquePart {
            origin: Protocol::Anthropic,
            raw: raw.clone(),
        })
    };
    assert_eq!(
        request.messages,
        vec![
            Message::new(
                Role::Assistant,
                vec![
                    opaque(&server_tool_use),
                    opaque(&result),
                    Part::text("Rust is a language.")
                ]
            ),
            // A `search_result` is content the user wants read: it is
            // followed by its rendering for upstreams of other protocols.
            Message::new(
                Role::User,
                vec![
                    opaque(&search_result),
                    Part::text(
                        "<search_result title=\"KB\" source=\"https://kb\">\nfact\n</search_result>"
                    ),
                ]
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Reasoning settings, every spelling
// ---------------------------------------------------------------------------

#[test]
fn decode_reasoning_enabled_with_budget() {
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "enabled", "budget_tokens": 10000}})),
        Some(ReasoningConfig::with_depth(Depth::Budget(10000)))
    );
    // A budget wins over an effort when the mode is manual.
    assert_eq!(
        reasoning_of(
            json!({"thinking": {"type": "enabled", "budget_tokens": 2048.0},
                            "output_config": {"effort": "high"}})
        ),
        Some(ReasoningConfig::with_depth(Depth::Budget(2048)))
    );
}

#[test]
fn decode_reasoning_disabled() {
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "disabled"}})),
        Some(ReasoningConfig::with_depth(Depth::Off))
    );
    // `disabled` wins over a stray budget and over an effort.
    assert_eq!(
        reasoning_of(
            json!({"thinking": {"type": "disabled", "budget_tokens": 5000},
                            "output_config": {"effort": "high"}})
        ),
        Some(ReasoningConfig::with_depth(Depth::Off))
    );
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "enabled", "budget_tokens": 0}})),
        Some(ReasoningConfig::with_depth(Depth::Off))
    );
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "between_tools"}})),
        Some(ReasoningConfig::with_depth(Depth::Off))
    );
}

#[test]
fn decode_reasoning_adaptive_with_and_without_effort() {
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "adaptive"}})),
        Some(ReasoningConfig::with_depth(Depth::Auto))
    );
    for (effort, expected) in [
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("xhigh", Effort::Xhigh),
        ("max", Effort::Max),
        (" HIGH ", Effort::High),
    ] {
        assert_eq!(
            reasoning_of(json!({"thinking": {"type": "adaptive"},
                                "output_config": {"effort": effort}})),
            Some(ReasoningConfig::with_depth(Depth::Level(expected))),
            "effort {effort}"
        );
    }
    // In adaptive mode a budget is ignored.
    assert_eq!(
        reasoning_of(
            json!({"thinking": {"type": "adaptive", "budget_tokens": 4096},
                            "output_config": {"effort": "low"}})
        ),
        Some(ReasoningConfig::with_depth(Depth::Level(Effort::Low)))
    );
    // `auto` is an alias some clients use.
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "auto"}})),
        Some(ReasoningConfig::with_depth(Depth::Auto))
    );
}

#[test]
fn decode_reasoning_enabled_without_budget() {
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "enabled"}})),
        Some(ReasoningConfig::with_depth(Depth::Auto))
    );
    assert_eq!(
        reasoning_of(
            json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "medium"}})
        ),
        Some(ReasoningConfig::with_depth(Depth::Level(Effort::Medium)))
    );
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "enabled", "budget_tokens": -1}})),
        Some(ReasoningConfig::with_depth(Depth::Auto))
    );
}

#[test]
fn decode_reasoning_effort_alone() {
    assert_eq!(
        reasoning_of(json!({"output_config": {"effort": "high"}})),
        Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)))
    );
}

#[test]
fn decode_reasoning_absent_is_none() {
    assert_eq!(reasoning_of(json!({})), None);
    assert_eq!(reasoning_of(json!({"thinking": null})), None);
    assert_eq!(
        reasoning_of(json!({"output_config": {"format": {"type": "json_schema", "schema": {}}}})),
        None
    );
}

#[test]
fn decode_reasoning_display_is_the_summary_intent() {
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "adaptive", "display": "summarized"}})),
        Some(ReasoningConfig {
            depth: Some(Depth::Auto),
            summary: Some(Summary::Auto),
        })
    );
    assert_eq!(
        reasoning_of(
            json!({"thinking": {"type": "enabled", "budget_tokens": 2000, "display": "omitted"}})
        ),
        Some(ReasoningConfig {
            depth: Some(Depth::Budget(2000)),
            summary: Some(Summary::Off),
        })
    );
    // `display` means nothing while thinking is off.
    assert_eq!(
        reasoning_of(json!({"thinking": {"type": "disabled", "display": "summarized"}})),
        Some(ReasoningConfig::with_depth(Depth::Off))
    );
}

// ---------------------------------------------------------------------------
// Structured output
// ---------------------------------------------------------------------------

#[test]
fn decode_structured_output_json_schema() {
    let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}},
                        "required": ["answer"], "additionalProperties": false});
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "output_config": {"format": {"type": "json_schema", "schema": schema.clone()}, "effort": "low"}
    }));
    assert_eq!(
        request.response_format,
        Some(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            schema: schema.clone(),
            strict: None,
        })
    );
    assert_eq!(
        request.reasoning,
        Some(ReasoningConfig::with_depth(Depth::Level(Effort::Low)))
    );
    // The deprecated top-level spelling.
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "output_format": {"type": "json_schema", "schema": schema.clone()}
    }));
    assert!(matches!(
        request.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert!(!request.extra.contains_key("output_format"));
}

// ---------------------------------------------------------------------------
// Odd but legal inputs
// ---------------------------------------------------------------------------

#[test]
fn decode_string_and_array_content_are_equivalent() {
    let from_string = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]
    }));
    let from_array = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    }));
    assert_eq!(from_string.messages, from_array.messages);
}

#[test]
fn decode_empty_content_and_null_fields() {
    let request = decode_request(&json!({
        "model": "m",
        "max_tokens": null,
        "temperature": null,
        "top_p": null,
        "top_k": null,
        "stop_sequences": null,
        "system": null,
        "tools": null,
        "tool_choice": null,
        "thinking": null,
        "metadata": null,
        "service_tier": null,
        "messages": [
            {"role": "user", "content": ""},
            {"role": "user", "content": []},
            {"role": "user", "content": null},
            {"role": "user"},
            {"role": "user", "content": [{"type": "text", "text": ""}]},
            "garbage",
            {"role": "user", "content": [{"type": "text", "text": "kept"}, null, 5]}
        ]
    }));
    let mut expected = Request::new("m", Protocol::Anthropic);
    expected.messages = vec![Message::user_text("kept")];
    assert_eq!(request, expected);
}

#[test]
fn decode_metadata_without_user_id() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [], "metadata": {"user_id": "  "}
    }));
    assert_eq!(request.user, None);
    assert_eq!(request.metadata, None);
}

#[test]
fn decode_role_spellings() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1,
        "messages": [
            {"role": "USER", "content": "a"},
            {"role": "Assistant", "content": "b"},
            {"content": "c"}
        ]
    }));
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
}

#[test]
fn decode_rejects_only_what_cannot_be_understood() {
    let codec = AnthropicCodec;
    let path = RequestPath::default();
    let err = codec.decode_request(&json!("nope"), &path).unwrap_err();
    assert!(matches!(err, CodecError::InvalidRequest { .. }));
    let err = codec
        .decode_request(&json!({"model": "m", "max_tokens": 1}), &path)
        .unwrap_err();
    assert_eq!(
        err,
        CodecError::invalid_param("messages", "`messages` is required")
    );
    let err = codec
        .decode_request(&json!({"model": "m", "messages": "hello"}), &path)
        .unwrap_err();
    assert_eq!(
        err,
        CodecError::invalid_param("messages", "`messages` must be an array")
    );
    // A missing model is the router's problem, not the decoder's.
    let request = codec
        .decode_request(&json!({"messages": []}), &path)
        .unwrap();
    assert_eq!(request.model, "");
}
