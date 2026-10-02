//! `decode_request`: client Responses bodies → IR.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, MediaSource, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, Tool,
    ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::{Codec, CodecError, Protocol, RequestPath};

const P: Protocol = Protocol::OpenaiResponses;

fn try_decode(body: Value) -> Result<Request, CodecError> {
    ResponsesCodec.decode_request(&body, &RequestPath::default())
}

fn decode(body: Value) -> Request {
    try_decode(body).expect("request decodes")
}

fn user(parts: Vec<Part>) -> Message {
    Message::new(Role::User, parts)
}

fn assistant(parts: Vec<Part>) -> Message {
    Message::new(Role::Assistant, parts)
}

// ---------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------

#[test]
fn decode_plain_text_string_input() {
    let request = decode(json!({"model": "gpt-5", "input": "Hello there"}));
    let mut expected = Request::new("gpt-5", P);
    expected.messages.push(Message::user_text("Hello there"));
    assert_eq!(request, expected);
}

#[test]
fn decode_plain_text_item_list_with_and_without_type() {
    let typed = decode(json!({
        "model": "gpt-5",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
    }));
    let untyped = decode(json!({
        "model": "gpt-5",
        "stream": true,
        "input": [{"role": "user", "content": "Hi"}]
    }));
    assert_eq!(typed, untyped);
    assert!(typed.stream);
    assert_eq!(typed.messages, vec![Message::user_text("Hi")]);
    assert_eq!(typed.source, P);
}

#[test]
fn decode_consecutive_user_messages_stay_separate() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "one"},
            {"role": "user", "content": "two"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![Message::user_text("one"), Message::user_text("two")]
    );
}

// ---------------------------------------------------------------------------
// System prompt forms
// ---------------------------------------------------------------------------

#[test]
fn decode_system_prompt_from_instructions() {
    let request = decode(json!({"model": "m", "instructions": "Be brief.", "input": "hi"}));
    assert_eq!(request.system, vec![Part::text("Be brief.")]);
    assert_eq!(request.messages, vec![Message::user_text("hi")]);
}

#[test]
fn decode_system_prompt_from_leading_system_and_developer_messages() {
    let request = decode(json!({
        "model": "m",
        "instructions": "From instructions.",
        "input": [
            {"type": "message", "role": "system", "content": "From system."},
            {"type": "message", "role": "developer", "content": [
                {"type": "input_text", "text": "From developer."}
            ]},
            {"type": "message", "role": "user", "content": "hi"}
        ]
    }));
    assert_eq!(
        request.system,
        vec![
            Part::text("From instructions."),
            Part::text("From system."),
            Part::text("From developer."),
        ]
    );
    assert_eq!(
        request.system_text(),
        "From instructions.\n\nFrom system.\n\nFrom developer."
    );
    assert_eq!(request.messages, vec![Message::user_text("hi")]);
}

#[test]
fn decode_system_message_mid_conversation_stays_in_place() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "hi"},
            {"role": "developer", "content": "Switch to French."},
            {"role": "user", "content": "again"}
        ]
    }));
    assert!(request.system.is_empty());
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("hi"),
            Message::new(Role::System, vec![Part::text("Switch to French.")]),
            Message::user_text("again"),
        ]
    );
}

#[test]
fn decode_instructions_null_and_empty_are_no_system_prompt() {
    assert!(
        decode(json!({"model": "m", "instructions": null, "input": "x"}))
            .system
            .is_empty()
    );
    assert!(
        decode(json!({"model": "m", "instructions": "", "input": "x"}))
            .system
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

#[test]
fn decode_multimodal_image_by_url_and_by_data_uri() {
    let request = decode(json!({
        "model": "m",
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "Compare"},
            {"type": "input_image", "image_url": "https://example.com/a.png", "detail": "high"},
            {"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/4AAQ"},
            {"type": "input_image", "file_id": "file-abc"}
        ]}]
    }));
    let mut by_url = MediaPart::url("https://example.com/a.png");
    by_url.detail = Some("high".into());
    assert_eq!(
        request.messages,
        vec![user(vec![
            Part::text("Compare"),
            Part::Image(by_url),
            Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
            Part::Image(MediaPart {
                source: MediaSource::FileRef {
                    id: "file-abc".into()
                },
                media_type: None,
                filename: None,
                detail: None,
                cache_control: None,
            }),
        ])]
    );
}

#[test]
fn decode_multimodal_pdf_file_in_every_form() {
    let request = decode(json!({
        "model": "m",
        "input": [{"role": "user", "content": [
            {"type": "input_file", "filename": "report.pdf", "file_data": "data:application/pdf;base64,JVBERi0x"},
            {"type": "input_file", "filename": "notes.txt", "file_data": "aGVsbG8="},
            {"type": "input_file", "file_url": "https://example.com/doc.pdf"},
            {"type": "input_file", "file_id": "file-123"}
        ]}]
    }));
    let mut inline = MediaPart::base64("application/pdf", "JVBERi0x");
    inline.filename = Some("report.pdf".into());
    // Raw base64 without a data: prefix takes its type from the file name.
    let mut raw = MediaPart::base64("text/plain", "aGVsbG8=");
    raw.filename = Some("notes.txt".into());
    assert_eq!(
        request.messages,
        vec![user(vec![
            Part::Document(inline),
            Part::Document(raw),
            Part::Document(MediaPart::url("https://example.com/doc.pdf")),
            Part::Document(MediaPart {
                source: MediaSource::FileRef {
                    id: "file-123".into()
                },
                media_type: None,
                filename: None,
                detail: None,
                cache_control: None,
            }),
        ])]
    );
}

#[test]
fn decode_multimodal_audio() {
    let request = decode(json!({
        "model": "m",
        "input": [{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "mp3"}}
        ]}]
    }));
    assert_eq!(
        request.messages,
        vec![user(vec![Part::Audio(MediaPart::base64(
            "audio/mpeg",
            "UklGRg=="
        ))])]
    );
}

// ---------------------------------------------------------------------------
// Tools and tool_choice
// ---------------------------------------------------------------------------

#[test]
fn decode_tools_of_every_kind() {
    let schema =
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]});
    let request = decode(json!({
        "model": "m",
        "input": "x",
        "tools": [
            {"type": "function", "name": "get_weather", "description": "Weather by city", "parameters": schema, "strict": true},
            {"name": "untyped", "parameters": {"type": "object"}},
            {"type": "function", "function": {"name": "nested", "description": "Chat style", "parameters": {"type": "object", "properties": {}}}},
            {"type": "custom", "name": "run_sql", "description": "Runs SQL", "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}},
            {"type": "web_search"},
            {"type": "web_search_preview", "search_context_size": "low"},
            {"type": "code_interpreter", "container": {"type": "auto"}},
            {"type": "file_search", "vector_store_ids": ["vs_1"]},
            {"type": "namespace", "name": "mcp__github", "description": "GitHub", "tools": [
                {"type": "function", "name": "get_me", "parameters": {"type": "object"}}
            ]}
        ]
    }));
    let builtin = |kind: BuiltinKind, raw: Value| {
        Tool::Builtin(BuiltinTool {
            kind,
            origin: P,
            raw,
        })
    };
    assert_eq!(
        request.tools,
        vec![
            Tool::Function(FunctionTool {
                name: "get_weather".into(),
                description: Some("Weather by city".into()),
                parameters: schema.clone(),
                strict: Some(true),
                cache_control: None,
            }),
            Tool::Function(FunctionTool {
                name: "untyped".into(),
                description: None,
                parameters: json!({"type": "object"}),
                strict: None,
                cache_control: None,
            }),
            Tool::Function(FunctionTool {
                name: "nested".into(),
                description: Some("Chat style".into()),
                parameters: json!({"type": "object", "properties": {}}),
                strict: None,
                cache_control: None,
            }),
            Tool::Custom(CustomTool {
                name: "run_sql".into(),
                description: Some("Runs SQL".into()),
                format: Some(
                    json!({"type": "grammar", "syntax": "lark", "definition": "start: /.+/"})
                ),
            }),
            builtin(BuiltinKind::WebSearch, json!({"type": "web_search"})),
            builtin(
                BuiltinKind::WebSearch,
                json!({"type": "web_search_preview", "search_context_size": "low"})
            ),
            builtin(
                BuiltinKind::CodeExecution,
                json!({"type": "code_interpreter", "container": {"type": "auto"}})
            ),
            builtin(
                BuiltinKind::Other("file_search".into()),
                json!({"type": "file_search", "vector_store_ids": ["vs_1"]})
            ),
            Tool::Function(FunctionTool {
                name: "mcp__github__get_me".into(),
                description: None,
                parameters: json!({"type": "object"}),
                strict: None,
                cache_control: None,
            }),
        ]
    );
}

#[test]
fn decode_tools_function_without_parameters_and_duplicate_names() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "additional_tools", "tools": [
                {"type": "function", "name": "first", "description": "late duplicate"},
                {"type": "function", "name": "late"}
            ]},
            {"role": "user", "content": "x"}
        ],
        "tools": [{"type": "function", "name": "first", "description": "top level"}]
    }));
    let names: Vec<_> = request.tools.iter().filter_map(Tool::name).collect();
    assert_eq!(names, vec!["first", "late"]);
    match &request.tools[0] {
        Tool::Function(f) => {
            assert_eq!(f.description.as_deref(), Some("top level"));
            assert_eq!(f.parameters, Value::Null);
        }
        other => panic!("unexpected tool {other:?}"),
    }
    // The declaration item is not conversation content.
    assert_eq!(request.messages, vec![Message::user_text("x")]);
}

#[test]
fn decode_tool_choice_string_variants() {
    let choice = |value: Value| {
        decode(json!({"model": "m", "input": "x", "tool_choice": value})).tool_choice
    };
    assert_eq!(choice(json!("auto")), Some(ToolChoice::Auto));
    assert_eq!(choice(json!("none")), Some(ToolChoice::None));
    assert_eq!(choice(json!("required")), Some(ToolChoice::Required));
    assert_eq!(choice(json!(null)), None);
    assert_eq!(choice(json!("sometimes")), None);
}

#[test]
fn decode_tool_choice_object_variants() {
    let full = |value: Value| decode(json!({"model": "m", "input": "x", "tool_choice": value}));
    let named = |name: &str| Some(ToolChoice::Tool { name: name.into() });
    assert_eq!(
        full(json!({"type": "function", "name": "f"})).tool_choice,
        named("f")
    );
    assert_eq!(
        full(json!({"type": "function", "function": {"name": "f"}})).tool_choice,
        named("f")
    );
    assert_eq!(
        full(json!({"type": "custom", "name": "sql"})).tool_choice,
        named("sql")
    );
    assert_eq!(
        full(json!({"type": "custom", "custom": {"name": "sql"}})).tool_choice,
        named("sql")
    );
    assert_eq!(
        full(json!({"type": "function", "name": "get_me", "namespace": "mcp__github"})).tool_choice,
        named("mcp__github__get_me")
    );

    // Not expressible in the IR: the mode is the choice, the tool list is
    // narrowed to the allowed tools (other protocols have no way to say
    // "only these"), and the value is kept verbatim for a Responses upstream.
    let allowed = json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "f"}]});
    let request = decode(json!({
        "model": "m", "input": "x", "tool_choice": allowed,
        "tools": [{"type": "function", "name": "f"}, {"type": "function", "name": "g"},
                  {"type": "web_search"}]
    }));
    assert_eq!(request.tool_choice, Some(ToolChoice::Required));
    assert_eq!(request.extra.get("tool_choice"), Some(&allowed));
    let names: Vec<Option<&str>> = request.tools.iter().map(Tool::name).collect();
    assert_eq!(names, vec![Some("f")]);
    // Nothing that is allowed is declared: nothing may be called.
    let request = full(allowed.clone());
    assert_eq!(request.tool_choice, Some(ToolChoice::None));
    assert_eq!(request.extra.get("tool_choice"), Some(&allowed));

    let hosted = json!({"type": "web_search_preview"});
    let request = full(hosted.clone());
    assert_eq!(request.tool_choice, Some(ToolChoice::Auto));
    assert_eq!(request.extra.get("tool_choice"), Some(&hosted));

    // Simple choices leave nothing behind.
    assert!(
        full(json!({"type": "function", "name": "f"}))
            .extra
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Multi-turn tool conversation
// ---------------------------------------------------------------------------

#[test]
fn decode_multi_turn_tool_call_conversation() {
    let request = decode(json!({
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather in Paris and Rome?"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Two lookups."}], "encrypted_content": "gAAAAABenc"},
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [
                {"type": "output_text", "text": "Checking.", "annotations": []}
            ]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_a", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"},
            {"type": "function_call", "id": "fc_2", "call_id": "call_b", "name": "get_weather", "arguments": "{\"city\":\"Rome\"}", "status": "completed"},
            {"type": "function_call_output", "call_id": "call_a", "output": "18C"},
            {"type": "function_call_output", "call_id": "call_b", "output": "24C"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Paris 18C, Rome 24C."}]},
            {"type": "message", "role": "user", "content": "Thanks"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("Weather in Paris and Rome?"),
            assistant(vec![
                Part::Reasoning(Reasoning {
                    id: Some("rs_1".into()),
                    text: "Two lookups.".into(),
                    signature: Some(Signature::new(P, "gAAAAABenc")),
                    redacted: false,
                }),
                Part::text("Checking."),
                Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
                Part::tool_call("call_b", "get_weather", "{\"city\":\"Rome\"}"),
            ]),
            user(vec![
                Part::tool_result_text("call_a", "18C"),
                Part::tool_result_text("call_b", "24C"),
            ]),
            Message::assistant_text("Paris 18C, Rome 24C."),
            Message::user_text("Thanks"),
        ]
    );
    assert!(request.has_tool_traffic());
}

#[test]
fn decode_tool_output_content_forms() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "function_call", "call_id": "c1", "name": "shot", "arguments": {"full": true}},
            {"type": "function_call", "call_id": "c2", "name": "data", "arguments": ""},
            {"type": "function_call", "call_id": "c3", "name": "data", "arguments": "{}"},
            {"type": "function_call", "call_id": "c4", "name": "data", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_text", "text": "screenshot"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBOR"}
            ]},
            {"type": "function_call_output", "call_id": "c2", "output": {"rows": 3}},
            {"type": "function_call_output", "call_id": "c3", "output": [{"a": 1}, {"b": 2}]},
            {"type": "function_call_output", "call_id": "c4", "output": ""}
        ]
    }));
    assert_eq!(request.messages.len(), 2);
    // Object arguments (lenient clients) are re-serialised as the JSON string
    // the protocol actually specifies.
    assert_eq!(
        request.messages[0].parts[0],
        Part::tool_call("c1", "shot", "{\"full\":true}")
    );
    let results: Vec<&ToolResult> = request.messages[1].tool_results().collect();
    assert_eq!(
        results[0].content,
        vec![
            Part::text("screenshot"),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
        ]
    );
    assert_eq!(results[1].content, vec![Part::text("{\"rows\":3}")]);
    assert_eq!(
        results[2].content,
        vec![Part::text("[{\"a\":1},{\"b\":2}]")]
    );
    assert_eq!(results[3].content, Vec::<Part>::new());
}

#[test]
fn decode_custom_tool_call_and_output() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "custom_tool_call", "call_id": "call_c", "name": "exec", "namespace": "functions", "input": "ls -la"},
            {"type": "custom_tool_call_output", "call_id": "call_c", "output": "total 0"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            assistant(vec![Part::ToolCall(ToolCall {
                id: "call_c".into(),
                name: "functions__exec".into(),
                arguments: "ls -la".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            })]),
            user(vec![Part::tool_result_text("call_c", "total 0")]),
        ]
    );
}

#[test]
fn decode_call_id_spellings_and_outputs_without_ids() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "function_call", "id": "only_item_id", "name": "a", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "b", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_c", "name": "c", "arguments": "{}"},
            // An explicit id wins; `fco_…` is an item id, never a call id.
            {"type": "function_call_output", "tool_call_id": "call_b", "output": "B"},
            {"type": "function_call_output", "id": "fco_9", "name": "c", "output": "C"},
            {"type": "function_call_output", "output": "A"}
        ]
    }));
    let calls: Vec<&str> = request.messages[0]
        .tool_calls()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(calls, vec!["only_item_id", "call_b", "call_c"]);
    let results: Vec<(&str, String)> = request.messages[1]
        .tool_results()
        .map(|r| (r.call_id.as_str(), r.text()))
        .collect();
    assert_eq!(
        results,
        vec![
            ("call_b", "B".to_string()),
            ("call_c", "C".to_string()),
            ("only_item_id", "A".to_string()),
        ]
    );
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

#[test]
fn decode_reasoning_settings_in_every_spelling() {
    let read = |reasoning: Value| {
        decode(json!({"model": "m", "input": "x", "reasoning": reasoning})).reasoning
    };
    let depth = |d: Depth| {
        Some(ReasoningConfig {
            depth: Some(d),
            summary: None,
        })
    };
    assert_eq!(read(json!({"effort": "none"})), depth(Depth::Off));
    assert_eq!(
        read(json!({"effort": "minimal"})),
        depth(Depth::Level(Effort::Minimal))
    );
    assert_eq!(
        read(json!({"effort": "low"})),
        depth(Depth::Level(Effort::Low))
    );
    assert_eq!(
        read(json!({"effort": "medium"})),
        depth(Depth::Level(Effort::Medium))
    );
    assert_eq!(
        read(json!({"effort": "high"})),
        depth(Depth::Level(Effort::High))
    );
    assert_eq!(
        read(json!({"effort": "xhigh"})),
        depth(Depth::Level(Effort::Xhigh))
    );
    assert_eq!(
        read(json!({"effort": "max"})),
        depth(Depth::Level(Effort::Max))
    );
    assert_eq!(read(json!({"effort": "auto"})), depth(Depth::Auto));
    assert_eq!(
        read(json!({"effort": " High "})),
        depth(Depth::Level(Effort::High))
    );

    let summary = |s: Summary| {
        Some(ReasoningConfig {
            depth: None,
            summary: Some(s),
        })
    };
    assert_eq!(read(json!({"summary": "auto"})), summary(Summary::Auto));
    assert_eq!(
        read(json!({"summary": "concise"})),
        summary(Summary::Concise)
    );
    assert_eq!(
        read(json!({"summary": "detailed"})),
        summary(Summary::Detailed)
    );
    assert_eq!(read(json!({"summary": "none"})), summary(Summary::Off));
    assert_eq!(read(json!({"summary": null})), summary(Summary::Off));
    assert_eq!(
        read(json!({"generate_summary": "concise"})),
        summary(Summary::Concise)
    );

    assert_eq!(
        read(json!({"effort": "low", "summary": "detailed"})),
        Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Detailed),
        })
    );

    // Nothing usable means "the client said nothing".
    assert_eq!(read(json!({})), None);
    assert_eq!(read(json!({"effort": "ludicrous"})), None);
    assert_eq!(read(json!(null)), None);
    assert_eq!(decode(json!({"model": "m", "input": "x"})).reasoning, None);
}

#[test]
fn decode_reasoning_configuration_update_item_overrides_effort() {
    let request = decode(json!({
        "model": "m",
        "reasoning": {"effort": "low"},
        "input": [
            {"role": "user", "content": "x"},
            {"type": "configuration_update", "reasoning": {"effort": "medium"}},
            {"type": "configuration_update", "reasoning": {"effort": "high"}}
        ]
    }));
    assert_eq!(
        request.reasoning,
        Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)))
    );
    assert_eq!(request.messages, vec![Message::user_text("x")]);
}

#[test]
fn decode_reasoning_items_run_blobs_through_the_signature_codec() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "reasoning", "id": "rs_a", "summary": [], "encrypted_content": "gAAAAnative"},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "from claude"}], "encrypted_content": "sy1.a.EqQBsig"},
            {"type": "reasoning", "summary": [], "encrypted_content": "sy1.a.redacted:EuYBdata"},
            {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "raw thoughts"}]},
            {"type": "reasoning", "id": "rs_stored", "summary": []},
            {"type": "message", "role": "assistant", "content": "done"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![assistant(vec![
            Part::Reasoning(Reasoning {
                id: Some("rs_a".into()),
                text: String::new(),
                signature: Some(Signature::new(P, "gAAAAnative")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: "from claude".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "EqQBsig")),
                redacted: false,
            }),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Anthropic, "EuYBdata")),
                redacted: true,
            }),
            // Unsigned reasoning text is still conversation content for
            // protocols that replay it as text.
            Part::reasoning("raw thoughts"),
            // The id-only item (state stored at the vendor) is dropped.
            Part::text("done"),
        ])]
    );
}

// ---------------------------------------------------------------------------
// Structured output
// ---------------------------------------------------------------------------

#[test]
fn decode_structured_output_json_schema_flat_form() {
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"], "additionalProperties": false});
    let request = decode(json!({
        "model": "m",
        "input": "x",
        "text": {
            "format": {"type": "json_schema", "name": "answer", "description": "The answer", "schema": schema, "strict": true},
            "verbosity": "low"
        }
    }));
    assert_eq!(
        request.response_format,
        Some(ResponseFormat::JsonSchema {
            name: Some("answer".into()),
            description: Some("The answer".into()),
            schema,
            strict: Some(true),
        })
    );
    // Verbosity has no IR slot; it rides along for same-family upstreams.
    assert_eq!(
        request.extra.get("text"),
        Some(&json!({"verbosity": "low"}))
    );
}

#[test]
fn decode_structured_output_json_mode_text_and_chat_style() {
    let format =
        |text: Value| decode(json!({"model": "m", "input": "x", "text": text})).response_format;
    assert_eq!(
        format(json!({"format": {"type": "json_object"}})),
        Some(ResponseFormat::JsonObject)
    );
    assert_eq!(
        format(json!({"format": {"type": "text"}})),
        Some(ResponseFormat::Text)
    );
    assert_eq!(format(json!({})), None);

    // The Chat Completions nesting is tolerated.
    let request = decode(json!({
        "model": "m",
        "input": "x",
        "response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": {"type": "object"}}}
    }));
    assert_eq!(
        request.response_format,
        Some(ResponseFormat::JsonSchema {
            name: Some("n".into()),
            description: None,
            schema: json!({"type": "object"}),
            strict: None,
        })
    );
    assert!(request.extra.is_empty());
}

// ---------------------------------------------------------------------------
// Sampling and bookkeeping fields
// ---------------------------------------------------------------------------

#[test]
fn decode_sampling_params_and_bookkeeping_fields() {
    let request = decode(json!({
        "model": "gpt-5",
        "input": "x",
        "stream": true,
        "temperature": 0.2,
        "top_p": 1,
        "max_output_tokens": 256.0,
        "parallel_tool_calls": false,
        "store": false,
        "previous_response_id": "resp_prev",
        "include": ["reasoning.encrypted_content"],
        "metadata": {"trace": "abc"},
        "user": "user-1",
        "prompt_cache_key": "pck",
        "service_tier": "flex",
        "truncation": "auto",
        "top_logprobs": 3
    }));
    assert!(request.stream);
    assert_eq!(request.temperature, Some(0.2));
    assert_eq!(request.top_p, Some(1.0));
    assert_eq!(request.max_output_tokens, Some(256));
    assert_eq!(request.parallel_tool_calls, Some(false));
    assert_eq!(request.store, Some(false));
    assert_eq!(request.previous_response_id.as_deref(), Some("resp_prev"));
    assert_eq!(
        request.metadata,
        json!({"trace": "abc"}).as_object().cloned()
    );
    assert_eq!(request.user.as_deref(), Some("user-1"));
    assert_eq!(request.prompt_cache_key.as_deref(), Some("pck"));
    assert_eq!(request.service_tier.as_deref(), Some("flex"));
    assert_eq!(
        Value::Object(request.extra.clone()),
        json!({
            "include": ["reasoning.encrypted_content"],
            "truncation": "auto",
            "top_logprobs": 3
        })
    );
    // Responses has no slot for these; they must stay unset.
    assert_eq!(request.top_k, None);
    assert_eq!(request.seed, None);
    assert!(request.stop.is_empty());
}

#[test]
fn decode_safety_identifier_stands_in_for_user() {
    let request = decode(json!({"model": "m", "input": "x", "safety_identifier": "sid-1"}));
    assert_eq!(request.user.as_deref(), Some("sid-1"));
    assert_eq!(
        request.extra.get("safety_identifier"),
        Some(&json!("sid-1"))
    );
}

#[test]
fn decode_websocket_envelope_fields_are_not_request_content() {
    let request = decode(json!({
        "type": "response.create",
        "generate": false,
        "stream_id": "main",
        "model": "m",
        "input": []
    }));
    assert!(request.extra.is_empty());
    assert!(request.messages.is_empty());
}

// ---------------------------------------------------------------------------
// Odd but legal inputs
// ---------------------------------------------------------------------------

#[test]
fn decode_odd_inputs_string_vs_array_content_empty_content_null_fields() {
    let request = decode(json!({
        "model": "m",
        "instructions": null,
        "tools": null,
        "tool_choice": null,
        "metadata": null,
        "max_output_tokens": null,
        "temperature": null,
        "reasoning": null,
        "text": null,
        "store": null,
        "input": [
            {"role": "user", "content": ""},
            {"role": "user", "content": []},
            {"role": "user", "content": null},
            {"role": "user"},
            {"role": "user", "content": [{"type": "input_text", "text": ""}, {"type": "input_text", "text": null}]},
            {"role": "user", "content": {"type": "input_text", "text": "single part object"}},
            {"role": "USER", "content": [{"text": "part without type"}]},
            "bare string",
            {"type": "input_text", "text": "bare part"},
            {"type": "item_reference", "id": "msg_stored"},
            17
        ]
    }));
    let mut expected = Request::new("m", P);
    expected.messages = vec![
        Message::user_text("single part object"),
        Message::user_text("part without type"),
        user(vec![Part::text("bare string"), Part::text("bare part")]),
    ];
    assert_eq!(request, expected);
}

#[test]
fn decode_assistant_content_forms_and_refusal() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"role": "assistant", "content": "plain string"},
            {"role": "assistant", "content": [
                {"type": "output_text", "text": "cited", "annotations": [
                    {"type": "url_citation", "url": "https://example.com", "title": "Example", "start_index": 0, "end_index": 5}
                ]},
                {"type": "refusal", "refusal": "I can't help with that."}
            ]}
        ]
    }));
    assert_eq!(request.messages.len(), 1);
    let parts = &request.messages[0].parts;
    assert_eq!(parts[0], Part::text("plain string"));
    match &parts[1] {
        Part::Text(t) => {
            assert_eq!(t.text, "cited");
            assert_eq!(t.citations.len(), 1);
            assert_eq!(t.citations[0].url.as_deref(), Some("https://example.com"));
            assert_eq!(t.citations[0].title.as_deref(), Some("Example"));
            assert_eq!(
                (t.citations[0].start, t.citations[0].end),
                (Some(0), Some(5))
            );
        }
        other => panic!("unexpected part {other:?}"),
    }
    assert_eq!(
        parts[2],
        Part::Refusal(RefusalPart {
            text: "I can't help with that.".into()
        })
    );
}

#[test]
fn decode_unknown_items_become_opaque_parts() {
    let search = json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "rust"}});
    let screenshot = json!({"type": "computer_call_output", "call_id": "cu_1", "output": {"type": "computer_screenshot", "image_url": "data:image/png;base64,AA"}});
    let request = decode(json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "search"},
            search,
            {"role": "assistant", "content": "found"},
            screenshot
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("search"),
            assistant(vec![
                Part::Opaque(OpaquePart {
                    origin: P,
                    raw: search.clone()
                }),
                Part::text("found"),
            ]),
            user(vec![Part::Opaque(OpaquePart {
                origin: P,
                raw: screenshot.clone()
            })]),
        ]
    );
}

#[test]
fn decode_input_may_be_absent_only_when_state_is_referenced() {
    let request = decode(json!({"model": "m", "previous_response_id": "resp_1"}));
    assert!(request.messages.is_empty());
    assert_eq!(request.previous_response_id.as_deref(), Some("resp_1"));

    let single = decode(json!({"model": "m", "input": {"role": "user", "content": "lone item"}}));
    assert_eq!(single.messages, vec![Message::user_text("lone item")]);
}

// ---------------------------------------------------------------------------
// Requests that cannot be understood
// ---------------------------------------------------------------------------

#[test]
fn decode_rejects_requests_that_cannot_be_understood() {
    let param = |result: Result<Request, CodecError>| match result {
        Err(CodecError::InvalidRequest { param, .. }) => param,
        other => panic!("expected an invalid-request error, got {other:?}"),
    };
    assert_eq!(
        param(try_decode(json!({"model": "m"}))).as_deref(),
        Some("input")
    );
    assert_eq!(
        param(try_decode(json!({"model": "m", "input": 42}))).as_deref(),
        Some("input")
    );
    assert_eq!(
        param(try_decode(json!({"input": "x"}))).as_deref(),
        Some("model")
    );
    assert_eq!(
        param(try_decode(json!({"model": 7, "input": "x"}))).as_deref(),
        Some("model")
    );
    assert_eq!(
        param(try_decode(
            json!({"model": "m", "input": "x", "tools": {"type": "function"}})
        ))
        .as_deref(),
        Some("tools")
    );
    assert_eq!(param(try_decode(json!(["not", "an", "object"]))), None);
}

#[test]
fn decode_chat_style_tool_role_message_as_a_tool_result() {
    let request = decode(json!({
        "model": "m",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"role": "tool", "tool_call_id": "call_1", "content": "42"}
        ]
    }));
    assert_eq!(
        request.messages,
        vec![
            assistant(vec![Part::tool_call("call_1", "f", "{}")]),
            user(vec![Part::tool_result_text("call_1", "42")]),
        ]
    );
}
