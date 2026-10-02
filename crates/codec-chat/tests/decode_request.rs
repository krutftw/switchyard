//! `decode_request`: Chat Completions request bodies into the IR.

mod common;

use common::decode_request;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_chat::ChatCodec;
use switchyard_core::Protocol;
use switchyard_core::codec::{Codec, RequestPath};
use switchyard_core::error::CodecError;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, MediaSource, Message, Part,
    Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, TextPart, Tool, ToolCall,
    ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};

fn tool_result(call_id: &str, name: Option<&str>, text: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: name.map(str::to_string),
        content: vec![Part::text(text)],
        is_error: false,
        cache_control: None,
    })
}

// ---------------------------------------------------------------------------
// Plain text and system prompts
// ---------------------------------------------------------------------------

#[test]
fn plain_text_request() {
    let req = decode_request(json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hello"}]
    }));
    let mut expected = Request::new("gpt-4o", Protocol::OpenaiChat);
    expected.messages.push(Message::user_text("Hello"));
    assert_eq!(req, expected);
}

#[test]
fn leading_system_and_developer_messages_become_request_system() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "You are terse."},
            {"role": "developer", "content": [
                {"type": "text", "text": "Answer in French."},
                {"type": "text", "text": "Never apologise."}
            ]},
            {"role": "user", "content": "Hi"}
        ]
    }));
    assert_eq!(
        req.system,
        vec![
            Part::text("You are terse."),
            Part::text("Answer in French."),
            Part::text("Never apologise.")
        ]
    );
    assert_eq!(req.messages, vec![Message::user_text("Hi")]);
    assert_eq!(
        req.system_text(),
        "You are terse.\n\nAnswer in French.\n\nNever apologise."
    );
}

#[test]
fn later_system_messages_stay_in_the_conversation() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "lead"},
            {"role": "user", "content": "one"},
            {"role": "system", "content": "mid", "name": "policy"},
            {"role": "developer", "content": "mid dev"},
            {"role": "user", "content": "two"}
        ]
    }));
    assert_eq!(req.system, vec![Part::text("lead")]);
    assert_eq!(
        req.messages,
        vec![
            Message::user_text("one"),
            Message {
                role: Role::System,
                parts: vec![Part::text("mid")],
                name: Some("policy".into()),
            },
            Message::new(Role::System, vec![Part::text("mid dev")]),
            Message::user_text("two"),
        ]
    );
}

#[test]
fn system_only_request_has_no_messages() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "system", "content": "just this"}]
    }));
    assert_eq!(req.system, vec![Part::text("just this")]);
    assert!(req.messages.is_empty());
}

#[test]
fn cache_control_markers_are_kept_on_parts_and_messages() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "big prompt", "cache_control": {"type": "ephemeral"}},
            {"role": "user", "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "text", "text": "b", "cache_control": {"type": "bogus"}}
            ]}
        ]
    }));
    assert_eq!(
        req.system[0].cache_control(),
        Some(&json!({"type": "ephemeral"}))
    );
    assert_eq!(
        req.messages[0].parts[0].cache_control(),
        Some(&json!({"type": "ephemeral", "ttl": "1h"}))
    );
    // Malformed markers are not forwarded to a vendor that validates them.
    assert_eq!(req.messages[0].parts[1].cache_control(), None);
}

// ---------------------------------------------------------------------------
// Multimodal content
// ---------------------------------------------------------------------------

#[test]
fn image_by_url_and_by_data_uri() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What is this?"},
            {"type": "image_url", "image_url": {"url": "https://example.com/cat.png", "detail": "low"}},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}},
            {"type": "image_url", "image_url": "https://example.com/bare.png"}
        ]}]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![
            Part::text("What is this?"),
            Part::Image(MediaPart {
                detail: Some("low".into()),
                ..MediaPart::url("https://example.com/cat.png")
            }),
            Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
            Part::Image(MediaPart::url("https://example.com/bare.png")),
        ]
    );
}

#[test]
fn input_audio_part() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
            {"type": "input_audio", "input_audio": {"data": "SUQz", "format": "mp3"}}
        ]}]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![
            Part::Audio(MediaPart::base64("audio/wav", "UklGRg==")),
            Part::Audio(MediaPart::base64("audio/mpeg", "SUQz")),
        ]
    );
}

#[test]
fn file_parts_pdf_by_data_uri_bare_base64_and_file_id() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "file", "file": {
                "filename": "report.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0x"
            }},
            {"type": "file", "file": {"filename": "notes.txt", "file_data": "aGVsbG8="}},
            {"type": "file", "file": {"file_id": "file-abc123"}},
            {"type": "file", "file": {"filename": "scan.png", "file_data": "data:image/png;base64,iVBOR"}}
        ]}]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![
            Part::Document(MediaPart {
                filename: Some("report.pdf".into()),
                ..MediaPart::base64("application/pdf", "JVBERi0x")
            }),
            // Bare base64: the media type comes from the file name.
            Part::Document(MediaPart {
                filename: Some("notes.txt".into()),
                ..MediaPart::base64("text/plain", "aGVsbG8=")
            }),
            Part::Document(MediaPart {
                source: MediaSource::FileRef {
                    id: "file-abc123".into()
                },
                media_type: None,
                filename: None,
                detail: None,
                cache_control: None,
            }),
            // An image uploaded as a file is still an image.
            Part::Image(MediaPart {
                filename: Some("scan.png".into()),
                ..MediaPart::base64("image/png", "iVBOR")
            }),
        ]
    );
}

#[test]
fn unknown_content_part_types_are_kept_opaque() {
    let part = json!({"type": "vendor_widget", "vendor_widget": {"ref": "w-1"}});
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [part.clone()]}]
    }));
    match &req.messages[0].parts[0] {
        Part::Opaque(o) => {
            assert_eq!(o.origin, Protocol::OpenaiChat);
            assert_eq!(o.raw, part);
        }
        other => panic!("expected an opaque part, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tools and tool choice
// ---------------------------------------------------------------------------

fn weather_tools() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "get_weather",
            "description": "Current weather for a city",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false
            },
            "strict": true
        }},
        {"type": "function", "function": {"name": "ping"}},
        {"type": "custom", "custom": {
            "name": "run_sql",
            "description": "Runs SQL",
            "format": {"type": "grammar", "grammar": {"syntax": "lark", "definition": "start: /.+/"}}
        }}
    ])
}

#[test]
fn function_and_custom_tools() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": weather_tools(),
        "parallel_tool_calls": false
    }));
    assert_eq!(
        req.tools,
        vec![
            Tool::Function(FunctionTool {
                name: "get_weather".into(),
                description: Some("Current weather for a city".into()),
                parameters: json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                    "additionalProperties": false
                }),
                strict: Some(true),
                cache_control: None,
            }),
            Tool::Function(FunctionTool {
                name: "ping".into(),
                description: None,
                parameters: Value::Null,
                strict: None,
                cache_control: None,
            }),
            Tool::Custom(CustomTool {
                name: "run_sql".into(),
                description: Some("Runs SQL".into()),
                format: Some(json!({
                    "type": "grammar",
                    "grammar": {"syntax": "lark", "definition": "start: /.+/"}
                })),
            }),
        ]
    );
    assert_eq!(req.tool_choice, None);
    assert_eq!(req.parallel_tool_calls, Some(false));
}

#[test]
fn deprecated_functions_and_function_call() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "functions": [{"name": "lookup", "description": "d", "parameters": {"type": "object"}}],
        "function_call": {"name": "lookup"}
    }));
    assert_eq!(
        req.tools,
        vec![Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: Some("d".into()),
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
        })]
    );
    assert_eq!(
        req.tool_choice,
        Some(ToolChoice::Tool {
            name: "lookup".into()
        })
    );
    // None of the deprecated fields leak into `extra`.
    assert!(req.extra.is_empty());
}

fn choice(tool_choice: Value) -> (Option<ToolChoice>, Vec<String>) {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": weather_tools(),
        "tool_choice": tool_choice
    }));
    let names = req
        .tools
        .iter()
        .filter_map(|t| t.name().map(str::to_string))
        .collect();
    (req.tool_choice, names)
}

#[test]
fn tool_choice_variants() {
    let all = vec![
        "get_weather".to_string(),
        "ping".to_string(),
        "run_sql".to_string(),
    ];
    assert_eq!(choice(json!("none")), (Some(ToolChoice::None), all.clone()));
    assert_eq!(choice(json!("auto")), (Some(ToolChoice::Auto), all.clone()));
    assert_eq!(
        choice(json!("required")),
        (Some(ToolChoice::Required), all.clone())
    );
    assert_eq!(choice(Value::Null), (None, all.clone()));
    assert_eq!(
        choice(json!({"type": "function", "function": {"name": "get_weather"}})),
        (
            Some(ToolChoice::Tool {
                name: "get_weather".into()
            }),
            all.clone()
        )
    );
    assert_eq!(
        choice(json!({"type": "custom", "custom": {"name": "run_sql"}})),
        (
            Some(ToolChoice::Tool {
                name: "run_sql".into()
            }),
            all.clone()
        )
    );
    // Object spellings of the simple modes.
    assert_eq!(
        choice(json!({"type": "none"})),
        (Some(ToolChoice::None), all.clone())
    );
    assert_eq!(
        choice(json!({"type": "auto"})),
        (Some(ToolChoice::Auto), all.clone())
    );
    // A restriction that cannot be understood must not become permission.
    assert_eq!(
        choice(json!({"type": "function", "function": {}})),
        (Some(ToolChoice::None), all.clone())
    );
    assert_eq!(
        choice(json!({"type": "mystery"})),
        (Some(ToolChoice::None), all)
    );
}

#[test]
fn allowed_tools_narrows_the_tool_list() {
    let allowed = |mode: &str, names: &[&str]| {
        json!({
            "type": "allowed_tools",
            "allowed_tools": {
                "mode": mode,
                "tools": names
                    .iter()
                    .map(|n| json!({"type": "function", "function": {"name": n}}))
                    .collect::<Vec<_>>()
            }
        })
    };
    assert_eq!(
        choice(allowed("auto", &["ping"])),
        (Some(ToolChoice::Auto), vec!["ping".to_string()])
    );
    assert_eq!(
        choice(allowed("required", &["ping", "get_weather"])),
        (
            Some(ToolChoice::Required),
            vec!["get_weather".to_string(), "ping".to_string()]
        )
    );
    // Nothing allowed means no tool may be called.
    assert_eq!(
        choice(allowed("auto", &[])),
        (Some(ToolChoice::None), vec![])
    );
}

#[test]
fn web_search_options_become_a_builtin_tool() {
    let req = decode_request(json!({
        "model": "gpt-4o-search-preview",
        "messages": [{"role": "user", "content": "news?"}],
        "web_search_options": {"search_context_size": "low"}
    }));
    assert_eq!(
        req.tools,
        vec![Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::OpenaiChat,
            raw: json!({"web_search_options": {"search_context_size": "low"}}),
        })]
    );
    assert!(req.extra.is_empty());
}

// ---------------------------------------------------------------------------
// Tool-call conversations
// ---------------------------------------------------------------------------

#[test]
fn multi_turn_tool_conversation() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "Weather in Paris and Rome?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}},
                {"id": "call_2", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "18C"},
            {"role": "tool", "tool_call_id": "call_2", "content": [
                {"type": "text", "text": "24C"},
                {"type": "text", "text": " sunny"}
            ]},
            {"role": "assistant", "content": "Paris 18C, Rome 24C and sunny."},
            {"role": "user", "content": "Thanks"}
        ]
    }));
    assert_eq!(
        req.messages,
        vec![
            Message::user_text("Weather in Paris and Rome?"),
            Message::new(
                Role::Assistant,
                vec![
                    // Arguments are kept byte for byte.
                    Part::tool_call("call_1", "get_weather", "{\"city\": \"Paris\"}"),
                    Part::tool_call("call_2", "get_weather", "{\"city\":\"Rome\"}"),
                ]
            ),
            // Consecutive tool messages are one IR user message.
            Message::new(
                Role::User,
                vec![
                    tool_result("call_1", None, "18C"),
                    Part::ToolResult(ToolResult {
                        call_id: "call_2".into(),
                        name: None,
                        content: vec![Part::text("24C"), Part::text(" sunny")],
                        is_error: false,
                        cache_control: None,
                    }),
                ]
            ),
            Message::assistant_text("Paris 18C, Rome 24C and sunny."),
            Message::user_text("Thanks"),
        ]
    );
    assert!(req.has_tool_traffic());
    assert_eq!(req.tool_name_for_call("call_2"), Some("get_weather"));
}

#[test]
fn tool_message_followed_by_user_text_is_a_separate_message() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok", "name": "f"},
            {"role": "user", "content": "and now?"}
        ]
    }));
    assert_eq!(
        req.messages[1..],
        [
            Message::new(Role::User, vec![tool_result("c1", Some("f"), "ok")]),
            Message::user_text("and now?"),
        ]
    );
}

#[test]
fn tool_messages_without_ids_pair_with_pending_calls_in_order() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{}"}}
            ]},
            {"role": "tool", "content": "first"},
            {"role": "tool", "content": "second"}
        ]
    }));
    let ids: Vec<&str> = req.messages[1]
        .tool_results()
        .map(|r| r.call_id.as_str())
        .collect();
    assert_eq!(ids, ["c1", "c2"]);
}

#[test]
fn custom_tool_call_in_history() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "assistant", "tool_calls": [
            {"id": "c9", "type": "custom", "custom": {"name": "run_sql", "input": "SELECT 1"}}
        ]}]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![Part::ToolCall(ToolCall {
            id: "c9".into(),
            name: "run_sql".into(),
            arguments: "SELECT 1".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })]
    );
}

#[test]
fn legacy_function_call_and_function_role_are_paired_by_name() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "time?"},
            {"role": "assistant", "content": null,
             "function_call": {"name": "get_time", "arguments": "{\"tz\":\"UTC\"}"}},
            {"role": "function", "name": "get_time", "content": "12:00"}
        ]
    }));
    let call = req.messages[1].tool_calls().next().expect("a tool call");
    assert_eq!(call.name, "get_time");
    assert_eq!(call.arguments, "{\"tz\":\"UTC\"}");
    assert!(call.id.starts_with("call_"));
    let result = req.messages[2]
        .tool_results()
        .next()
        .expect("a tool result");
    assert_eq!(result.call_id, call.id);
    assert_eq!(result.name.as_deref(), Some("get_time"));
    assert_eq!(result.text(), "12:00");
    assert_eq!(req.messages[2].role, Role::User);
    // The assistant's `function_call` is a call, not a tool choice.
    assert_eq!(req.tool_choice, None);
}

#[test]
fn tool_call_without_id_gets_one_and_object_arguments_are_serialised() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "assistant", "tool_calls": [
            {"type": "function", "function": {"name": "f", "arguments": {"a": 1}}},
            {"id": "c2", "function": {"name": "g"}},
            {"id": "c3", "function": {"arguments": "{}"}}
        ]}]
    }));
    let calls: Vec<&ToolCall> = req.messages[0].tool_calls().collect();
    // The nameless call is unusable and dropped.
    assert_eq!(calls.len(), 2);
    assert!(calls[0].id.starts_with("call_"));
    assert_eq!(calls[0].arguments, "{\"a\":1}");
    assert_eq!(
        (calls[1].id.as_str(), calls[1].arguments.as_str()),
        ("c2", "")
    );
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

fn reasoning_of(extra: Value) -> Option<ReasoningConfig> {
    let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    for (key, value) in extra.as_object().expect("an object") {
        body[key] = value.clone();
    }
    decode_request(body).reasoning
}

#[test]
fn reasoning_effort_in_every_spelling() {
    let depth = |extra: Value| reasoning_of(extra).and_then(|r| r.depth);
    // OpenAI.
    for (wire, effort) in [
        ("minimal", Effort::Minimal),
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("xhigh", Effort::Xhigh),
        ("max", Effort::Max),
    ] {
        assert_eq!(
            depth(json!({"reasoning_effort": wire})),
            Some(Depth::Level(effort))
        );
    }
    assert_eq!(depth(json!({"reasoning_effort": "none"})), Some(Depth::Off));
    assert_eq!(
        depth(json!({"reasoning_effort": "HIGH"})),
        Some(Depth::Level(Effort::High))
    );
    // OpenRouter.
    assert_eq!(
        depth(json!({"reasoning": {"effort": "low"}})),
        Some(Depth::Level(Effort::Low))
    );
    assert_eq!(
        depth(json!({"reasoning": {"max_tokens": 2000}})),
        Some(Depth::Budget(2000))
    );
    assert_eq!(
        depth(json!({"reasoning": {"enabled": false}})),
        Some(Depth::Off)
    );
    assert_eq!(
        depth(json!({"reasoning": {"enabled": true}})),
        Some(Depth::Auto)
    );
    // Anthropic-style, as DeepSeek / Zhipu / Doubao accept it.
    assert_eq!(
        depth(json!({"thinking": {"type": "disabled"}})),
        Some(Depth::Off)
    );
    assert_eq!(
        depth(json!({"thinking": {"type": "enabled"}})),
        Some(Depth::Auto)
    );
    assert_eq!(
        depth(json!({"thinking": {"type": "enabled", "budget_tokens": 4096}})),
        Some(Depth::Budget(4096))
    );
    // Qwen / DashScope.
    assert_eq!(depth(json!({"enable_thinking": false})), Some(Depth::Off));
    assert_eq!(depth(json!({"enable_thinking": true})), Some(Depth::Auto));
    assert_eq!(
        depth(json!({"enable_thinking": true, "thinking_budget": 1024})),
        Some(Depth::Budget(1024))
    );
    // Google's OpenAI-compatible endpoint.
    assert_eq!(
        depth(json!({"extra_body": {"google": {"thinking_config": {"thinking_budget": -1}}}})),
        Some(Depth::Auto)
    );
    assert_eq!(
        depth(json!({"extra_body": {"google": {"thinking_config": {"thinking_level": "high"}}}})),
        Some(Depth::Level(Effort::High))
    );
    assert_eq!(
        depth(json!({"extra_body": {"google": {"thinking_config": {"thinking_budget": 0}}}})),
        Some(Depth::Off)
    );
}

#[test]
fn no_reasoning_fields_means_no_reasoning_config() {
    assert_eq!(reasoning_of(json!({})), None);
    assert_eq!(reasoning_of(json!({"reasoning_effort": null})), None);
    assert_eq!(reasoning_of(json!({"reasoning_effort": "bogus"})), None);
}

#[test]
fn reasoning_visibility_is_read_separately_from_depth() {
    assert_eq!(
        reasoning_of(json!({"reasoning_effort": "high"})),
        Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::High)),
            summary: None
        })
    );
    assert_eq!(
        reasoning_of(json!({"reasoning_effort": "low", "include_reasoning": true})),
        Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Auto)
        })
    );
    assert_eq!(
        reasoning_of(json!({"reasoning": {"effort": "high", "exclude": true}})),
        Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::High)),
            summary: Some(Summary::Off)
        })
    );
    assert_eq!(
        reasoning_of(json!({"reasoning": {"summary": "detailed"}})),
        Some(ReasoningConfig {
            depth: None,
            summary: Some(Summary::Detailed)
        })
    );
}

#[test]
fn assistant_history_reasoning_content_is_an_unsigned_reasoning_part() {
    let req = decode_request(json!({
        "model": "deepseek-reasoner",
        "messages": [
            {"role": "user", "content": "2+2?"},
            {"role": "assistant", "reasoning_content": "Simple sum.", "content": "4"},
            {"role": "assistant", "reasoning": "OpenRouter spelling", "content": "x"}
        ]
    }));
    assert_eq!(
        req.messages[1].parts,
        vec![Part::reasoning("Simple sum."), Part::text("4")]
    );
    assert_eq!(
        req.messages[2].parts,
        vec![Part::reasoning("OpenRouter spelling"), Part::text("x")]
    );
}

#[test]
fn reasoning_details_blobs_go_through_the_signature_wrapper() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{
            "role": "assistant",
            "content": "done",
            "reasoning_details": [
                {"type": "reasoning.text", "text": "claude thought", "signature": "sy1.a.ErUB", "index": 0},
                {"type": "reasoning.encrypted", "data": "sy1.r.gAAAA", "id": "rs_1", "index": 1},
                {"type": "reasoning.text", "text": "native", "signature": "plain-sig", "index": 2}
            ]
        }]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "claude thought".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "ErUB")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: Some("rs_1".into()),
                text: String::new(),
                signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
                redacted: true,
            }),
            // A blob without a wrapper is native to the protocol it arrived in.
            Part::Reasoning(Reasoning {
                id: None,
                text: "native".into(),
                signature: Some(Signature::new(Protocol::OpenaiChat, "plain-sig")),
                redacted: false,
            }),
            Part::text("done"),
        ]
    );
}

#[test]
fn reasoning_content_fills_details_that_only_carry_a_signature() {
    // What a client echoes back after a stream: the text it concatenated
    // from `reasoning_content` deltas plus the signature-only detail.
    let req = decode_request(json!({
        "model": "m",
        "messages": [{
            "role": "assistant",
            "content": "ok",
            "reasoning_content": "the thought",
            "reasoning_details": [{"type": "reasoning.text", "signature": "sy1.a.SIG", "index": 0}]
        }]
    }));
    assert_eq!(
        req.messages[0].parts[0],
        Part::Reasoning(Reasoning {
            id: None,
            text: "the thought".into(),
            signature: Some(Signature::new(Protocol::Anthropic, "SIG")),
            redacted: false,
        })
    );
}

#[test]
fn tool_call_thought_signature_is_unwrapped() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "assistant", "tool_calls": [
            {"id": "c1", "type": "function",
             "function": {"name": "f", "arguments": "{}"},
             "extra_content": {"google": {"thought_signature": "sy1.g.CiQB"}}},
            {"id": "c2", "type": "function",
             "function": {"name": "f", "arguments": "{}"},
             "extra_content": {"google": {"thought_signature": "native"}}}
        ]}]
    }));
    let calls: Vec<&ToolCall> = req.messages[0].tool_calls().collect();
    assert_eq!(
        calls[0].signature,
        Some(Signature::new(Protocol::Gemini, "CiQB"))
    );
    assert_eq!(
        calls[1].signature,
        Some(Signature::new(Protocol::OpenaiChat, "native"))
    );
}

// ---------------------------------------------------------------------------
// Structured output and sampling
// ---------------------------------------------------------------------------

#[test]
fn response_format_variants() {
    let format = |v: Value| {
        decode_request(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": v
        }))
        .response_format
    };
    assert_eq!(format(json!({"type": "text"})), Some(ResponseFormat::Text));
    assert_eq!(
        format(json!({"type": "json_object"})),
        Some(ResponseFormat::JsonObject)
    );
    assert_eq!(
        format(json!({"type": "json_schema", "json_schema": {
            "name": "answer",
            "description": "The answer",
            "strict": true,
            "schema": {"type": "object", "properties": {"n": {"type": "integer"}}}
        }})),
        Some(ResponseFormat::JsonSchema {
            name: Some("answer".into()),
            description: Some("The answer".into()),
            schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            strict: Some(true),
        })
    );
    // A schema format without a schema can only mean "some JSON".
    assert_eq!(
        format(json!({"type": "json_schema", "json_schema": {"name": "x"}})),
        Some(ResponseFormat::JsonObject)
    );
    assert_eq!(format(Value::Null), None);
    assert_eq!(format(json!({"type": "yaml"})), None);
}

#[test]
fn sampling_and_bookkeeping_fields() {
    let req = decode_request(json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 256,
        "temperature": 0.2,
        "top_p": 0.9,
        "n": 2,
        "stop": ["END", "STOP"],
        "seed": 42,
        "presence_penalty": 0.5,
        "frequency_penalty": -0.5,
        "user": "user-123",
        "metadata": {"trace": "abc"},
        "service_tier": "flex",
        "prompt_cache_key": "conv-9",
        "store": true,
        "stream": true,
        "stream_options": {"include_usage": true}
    }));
    assert_eq!(req.max_output_tokens, Some(256));
    assert_eq!(req.temperature, Some(0.2));
    assert_eq!(req.top_p, Some(0.9));
    assert_eq!(req.candidate_count, Some(2));
    assert_eq!(req.stop, vec!["END".to_string(), "STOP".to_string()]);
    assert_eq!(req.seed, Some(42));
    assert_eq!(req.presence_penalty, Some(0.5));
    assert_eq!(req.frequency_penalty, Some(-0.5));
    assert_eq!(req.user.as_deref(), Some("user-123"));
    assert_eq!(req.metadata, json!({"trace": "abc"}).as_object().cloned());
    assert_eq!(req.service_tier.as_deref(), Some("flex"));
    assert_eq!(req.prompt_cache_key.as_deref(), Some("conv-9"));
    assert_eq!(req.store, Some(true));
    assert!(req.stream);
    assert!(req.extra.is_empty());
}

#[test]
fn max_tokens_spellings_and_float_numbers() {
    let limit = |extra: Value| {
        let mut body = json!({"model": "m", "messages": []});
        for (key, value) in extra.as_object().expect("an object") {
            body[key] = value.clone();
        }
        decode_request(body).max_output_tokens
    };
    assert_eq!(limit(json!({"max_tokens": 100})), Some(100));
    assert_eq!(limit(json!({"max_completion_tokens": 200})), Some(200));
    // The current field wins over the deprecated one.
    assert_eq!(
        limit(json!({"max_tokens": 100, "max_completion_tokens": 200})),
        Some(200)
    );
    assert_eq!(limit(json!({"max_tokens": 100.0})), Some(100));
    assert_eq!(limit(json!({"max_tokens": null})), None);
    assert_eq!(limit(json!({})), None);
}

#[test]
fn stop_as_a_single_string() {
    let req = decode_request(json!({"model": "m", "messages": [], "stop": "\n\n"}));
    assert_eq!(req.stop, vec!["\n\n".to_string()]);
}

#[test]
fn stream_only_for_the_literal_true() {
    let stream =
        |v: Value| decode_request(json!({"model": "m", "messages": [], "stream": v})).stream;
    assert!(stream(json!(true)));
    assert!(!stream(json!(false)));
    assert!(!stream(json!("true")));
    assert!(!stream(json!(1)));
    assert!(!stream(Value::Null));
}

// ---------------------------------------------------------------------------
// Odd but legal inputs
// ---------------------------------------------------------------------------

#[test]
fn null_fields_are_absent() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi", "name": null}],
        "temperature": null,
        "top_p": null,
        "stop": null,
        "tools": null,
        "tool_choice": null,
        "response_format": null,
        "user": null,
        "metadata": null,
        "seed": null,
        "n": null,
        "parallel_tool_calls": null,
        "stream": null,
        "reasoning_effort": null,
        "web_search_options": null
    }));
    let mut expected = Request::new("m", Protocol::OpenaiChat);
    expected.messages.push(Message::user_text("hi"));
    assert_eq!(req, expected);
}

#[test]
fn empty_and_null_content() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": ""},
            {"role": "assistant", "content": null},
            {"role": "user", "content": []},
            {"role": "assistant"},
            {"role": "user", "content": [{"type": "text", "text": ""}, {"type": "text", "text": "x"}]}
        ]
    }));
    assert_eq!(
        req.messages,
        vec![
            Message::new(Role::User, vec![]),
            Message::new(Role::Assistant, vec![]),
            Message::new(Role::User, vec![]),
            Message::new(Role::Assistant, vec![]),
            // Empty text blocks carry nothing and upset stricter protocols.
            Message::user_text("x"),
        ]
    );
}

#[test]
fn assistant_content_as_part_array_with_refusal() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "text", "text": "part one "},
                {"type": "text", "text": "part two"},
                {"type": "refusal", "refusal": "but not that"}
            ]},
            {"role": "assistant", "content": null, "refusal": "I can't help with that."}
        ]
    }));
    assert_eq!(
        req.messages[0].parts,
        vec![
            Part::text("part one "),
            Part::text("part two"),
            Part::Refusal(RefusalPart {
                text: "but not that".into()
            }),
        ]
    );
    assert_eq!(
        req.messages[1].parts,
        vec![Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })]
    );
}

#[test]
fn name_field_and_unknown_roles() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "hi", "name": "alice"},
            {"role": "assistant", "content": "yo", "name": "bot"},
            {"content": "no role"},
            {"role": "critic", "content": "unknown role"}
        ]
    }));
    assert_eq!(req.messages[0].name.as_deref(), Some("alice"));
    assert_eq!(req.messages[1].name.as_deref(), Some("bot"));
    assert_eq!(req.messages[2], Message::user_text("no role"));
    assert_eq!(req.messages[3], Message::user_text("unknown role"));
}

#[test]
fn tool_content_shapes() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "a", "content": null},
            {"role": "tool", "tool_call_id": "b", "content": {"temperature": 18}},
            {"role": "tool", "tool_call_id": "c", "content": [
                {"type": "text", "text": "see image", "cache_control": {"type": "ephemeral"}},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}
        ]
    }));
    let results: Vec<&ToolResult> = req.messages[1].tool_results().collect();
    assert_eq!(results[0].content, vec![]);
    // Structured output sent without string encoding is kept as JSON text.
    assert_eq!(results[1].content, vec![Part::text("{\"temperature\":18}")]);
    assert_eq!(
        results[2].content,
        vec![
            Part::Text(TextPart::new("see image")),
            Part::Image(MediaPart::base64("image/png", "AAAA")),
        ]
    );
    // A cache marker inside a tool result moves to the result itself.
    assert_eq!(results[2].cache_control, Some(json!({"type": "ephemeral"})));
}

#[test]
fn unknown_top_level_fields_land_in_extra() {
    let req = decode_request(json!({
        "model": "m",
        "messages": [],
        "logprobs": true,
        "top_logprobs": 3,
        "logit_bias": {"50256": -100},
        "modalities": ["text"],
        "prediction": {"type": "content", "content": "x"},
        "verbosity": "low",
        "safety_identifier": "u1",
        "some_vendor_flag": {"a": 1}
    }));
    let keys: Vec<&str> = req.extra.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "logprobs",
            "top_logprobs",
            "logit_bias",
            "modalities",
            "prediction",
            "verbosity",
            "safety_identifier",
            "some_vendor_flag"
        ]
    );
    assert_eq!(req.extra["some_vendor_flag"], json!({"a": 1}));
}

#[test]
fn model_can_come_from_the_path() {
    let path = RequestPath {
        model: Some("from-path"),
        stream: Some(true),
    };
    let req = ChatCodec
        .decode_request(&json!({"messages": []}), &path)
        .expect("decodes");
    assert_eq!(req.model, "from-path");
    assert!(req.stream);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn decode_err(body: Value) -> CodecError {
    ChatCodec
        .decode_request(&body, &RequestPath::default())
        .expect_err("must not decode")
}

#[test]
fn requests_that_cannot_be_understood_are_rejected() {
    assert!(matches!(
        decode_err(json!("text")),
        CodecError::InvalidRequest { .. }
    ));
    assert_eq!(
        decode_err(json!({"model": "m"})),
        CodecError::invalid_param("messages", "`messages` is required")
    );
    assert_eq!(
        decode_err(json!({"model": "m", "messages": "hi"})),
        CodecError::invalid_param("messages", "`messages` must be an array")
    );
    assert_eq!(
        decode_err(json!({"model": "m", "messages": ["hi"]})),
        CodecError::invalid_param("messages[0]", "each message must be an object")
    );
}
