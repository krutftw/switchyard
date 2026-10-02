//! `encode_request`: IR → body for a Responses upstream, asserted as exact
//! JSON.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, Citation, CustomTool, FunctionTool, MediaPart, MediaSource, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, TextPart,
    Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{
    Depth, Effort, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

const P: Protocol = Protocol::OpenaiResponses;

fn encode(request: &Request) -> Value {
    ResponsesCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("request encodes")
}

fn encode_for(request: &Request, thinking: ModelThinking<'_>) -> Value {
    let ctx = UpstreamCtx {
        thinking,
        ..UpstreamCtx::default()
    };
    ResponsesCodec
        .encode_request(request, &ctx)
        .expect("request encodes")
}

fn base(source: Protocol) -> Request {
    let mut request = Request::new("gpt-5", source);
    request.messages.push(Message::user_text("Hello"));
    request
}

fn hello_item() -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]})
}

fn function_tool(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: Some("A tool".into()),
        parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        strict: None,
        cache_control: None,
    })
}

fn signed(id: Option<&str>, text: &str, origin: Protocol, blob: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: id.map(str::to_string),
        text: text.into(),
        signature: Some(Signature::new(origin, blob)),
        redacted: false,
    })
}

// ---------------------------------------------------------------------------
// Plain text and system prompts
// ---------------------------------------------------------------------------

#[test]
fn encode_plain_text() {
    assert_eq!(
        encode(&base(P)),
        json!({"model": "gpt-5", "input": [hello_item()], "stream": false})
    );
    let mut streaming = base(Protocol::Anthropic);
    streaming.stream = true;
    assert_eq!(
        encode(&streaming),
        json!({"model": "gpt-5", "input": [hello_item()], "store": false, "stream": true})
    );
}

#[test]
fn encode_system_prompt_forms() {
    let mut request = base(Protocol::Anthropic);
    request.system = vec![
        Part::Text(TextPart {
            text: "You are terse.".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
            ..TextPart::default()
        }),
        Part::text("Answer in French."),
    ];
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "instructions": "You are terse.\n\nAnswer in French.",
            "input": [hello_item()],
            "store": false,
            "stream": false
        })
    );
}

#[test]
fn encode_system_role_messages_mid_conversation() {
    let mut request = base(Protocol::OpenaiChat);
    request.messages.push(Message::new(
        Role::System,
        vec![Part::text("From now on, be formal.")],
    ));
    request.messages.push(Message::user_text("Continue"));
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [
                hello_item(),
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "From now on, be formal."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Continue"}]}
            ],
            "store": false,
            "stream": false
        })
    );
}

#[test]
fn encode_consecutive_same_role_messages_stay_separate_items() {
    let mut request = Request::new("gpt-5", Protocol::Gemini);
    request.messages = vec![
        Message::user_text("one"),
        Message::user_text("two"),
        Message::assistant_text("three"),
        Message::assistant_text("four"),
    ];
    assert_eq!(
        encode(&request)["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "one"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "two"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "three"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "four"}]}
        ])
    );
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

#[test]
fn encode_multimodal_images_files_and_audio() {
    let mut by_url = MediaPart::url("https://example.com/a.png");
    by_url.detail = Some("low".into());
    let mut named_pdf = MediaPart::base64("application/pdf", "JVBERi0x");
    named_pdf.filename = Some("report.pdf".into());
    let file_ref = |id: &str| MediaPart {
        source: MediaSource::FileRef { id: id.into() },
        media_type: None,
        filename: None,
        detail: None,
        cache_control: None,
    };
    let mut request = Request::new("gpt-5", P);
    request.messages.push(Message::new(
        Role::User,
        vec![
            Part::text("Look"),
            Part::Image(by_url),
            Part::Image(MediaPart::base64("image/png", "iVBOR")),
            Part::Image(file_ref("file-img")),
            Part::Document(named_pdf),
            Part::Document(MediaPart::base64("application/pdf", "AAAA")),
            Part::Document(MediaPart::url("https://example.com/doc.pdf")),
            Part::Document(file_ref("file-doc")),
            Part::Audio(MediaPart::base64("audio/mpeg", "UklGRg==")),
        ],
    ));
    assert_eq!(
        encode(&request)["input"],
        json!([{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "Look"},
            {"type": "input_image", "image_url": "https://example.com/a.png", "detail": "low"},
            {"type": "input_image", "image_url": "data:image/png;base64,iVBOR"},
            {"type": "input_image", "file_id": "file-img"},
            {"type": "input_file", "filename": "report.pdf", "file_data": "data:application/pdf;base64,JVBERi0x"},
            {"type": "input_file", "filename": "document.pdf", "file_data": "data:application/pdf;base64,AAAA"},
            {"type": "input_file", "file_url": "https://example.com/doc.pdf"},
            {"type": "input_file", "file_id": "file-doc"},
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "mp3"}}
        ]}])
    );
}

#[test]
fn encode_drops_file_handles_issued_by_another_vendor() {
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages.push(Message::new(
        Role::User,
        vec![
            Part::text("See file"),
            Part::Document(MediaPart {
                source: MediaSource::FileRef {
                    id: "file_011CNha8".into(),
                },
                media_type: Some("application/pdf".into()),
                filename: None,
                detail: None,
                cache_control: None,
            }),
        ],
    ));
    assert_eq!(
        encode(&request)["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "See file"}]}])
    );
}

// ---------------------------------------------------------------------------
// Tools and tool_choice
// ---------------------------------------------------------------------------

#[test]
fn encode_tools_function_custom_and_builtin() {
    let mut request = base(P);
    request.tools = vec![
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Weather by city".into()),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            strict: Some(true),
            cache_control: None,
        }),
        Tool::Function(FunctionTool {
            name: "no_params".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }),
        Tool::Custom(CustomTool {
            name: "run_sql".into(),
            description: Some("Runs SQL".into()),
            format: Some(json!({"type": "text"})),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: P,
            raw: json!({"type": "web_search_preview", "search_context_size": "low"}),
        }),
    ];
    assert_eq!(
        encode(&request)["tools"],
        json!([
            {"type": "function", "name": "get_weather", "description": "Weather by city",
             "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}, "strict": true},
            // A Responses client that omitted `strict` meant the Responses default.
            {"type": "function", "name": "no_params", "parameters": {"type": "object", "properties": {}}},
            {"type": "custom", "name": "run_sql", "description": "Runs SQL", "format": {"type": "text"}},
            {"type": "web_search_preview", "search_context_size": "low"}
        ])
    );
}

#[test]
fn encode_tools_from_other_protocols_spell_out_strict_false() {
    let mut request = base(Protocol::Anthropic);
    request.tools = vec![Tool::Function(FunctionTool {
        name: "lookup".into(),
        description: Some("Looks things up".into()),
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
        // Anthropic cache breakpoints have no meaning here.
        cache_control: Some(json!({"type": "ephemeral"})),
    })];
    assert_eq!(
        encode(&request)["tools"],
        json!([{"type": "function", "name": "lookup", "description": "Looks things up",
                "parameters": {"type": "object", "properties": {}}, "strict": false}])
    );
}

#[test]
fn encode_builtin_tools_of_another_family() {
    let foreign = |kind: BuiltinKind, raw: Value| {
        Tool::Builtin(BuiltinTool {
            kind,
            origin: Protocol::Anthropic,
            raw,
        })
    };
    let mut request = base(Protocol::Anthropic);
    request.tools = vec![
        foreign(
            BuiltinKind::WebSearch,
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        ),
        foreign(
            BuiltinKind::CodeExecution,
            json!({"type": "code_execution_20250825", "name": "code_execution"}),
        ),
        foreign(
            BuiltinKind::WebFetch,
            json!({"type": "web_fetch_20250910", "name": "web_fetch"}),
        ),
        foreign(
            BuiltinKind::Other("bash_20250124".into()),
            json!({"type": "bash_20250124", "name": "bash"}),
        ),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Gemini,
            raw: json!({"googleSearch": {}}),
        }),
    ];
    request.tool_choice = Some(ToolChoice::Auto);
    let body = encode(&request);
    // Own default declarations (once each) for kinds that exist here; the
    // rest is dropped.
    assert_eq!(
        body["tools"],
        json!([
            {"type": "web_search"},
            {"type": "code_interpreter", "container": {"type": "auto"}}
        ])
    );
    assert_eq!(body["tool_choice"], json!("auto"));
}

#[test]
fn encode_tool_choice_variants() {
    let choice = |choice: ToolChoice| {
        let mut request = base(Protocol::OpenaiChat);
        request.tools = vec![
            function_tool("f"),
            Tool::Custom(CustomTool {
                name: "sql".into(),
                description: None,
                format: None,
            }),
        ];
        request.tool_choice = Some(choice);
        request.parallel_tool_calls = Some(false);
        let body = encode(&request);
        assert_eq!(body["parallel_tool_calls"], json!(false));
        body["tool_choice"].clone()
    };
    assert_eq!(choice(ToolChoice::Auto), json!("auto"));
    assert_eq!(choice(ToolChoice::None), json!("none"));
    assert_eq!(choice(ToolChoice::Required), json!("required"));
    assert_eq!(
        choice(ToolChoice::Tool { name: "f".into() }),
        json!({"type": "function", "name": "f"})
    );
    assert_eq!(
        choice(ToolChoice::Tool { name: "sql".into() }),
        json!({"type": "custom", "name": "sql"})
    );
}

#[test]
fn encode_tool_settings_are_omitted_without_tools() {
    let mut request = base(Protocol::Anthropic);
    request.tool_choice = Some(ToolChoice::Required);
    request.parallel_tool_calls = Some(true);
    // The only tool has no equivalent, so nothing may refer to tools at all.
    request.tools = vec![Tool::Builtin(BuiltinTool {
        kind: BuiltinKind::Other("computer_20250124".into()),
        origin: Protocol::Anthropic,
        raw: json!({"type": "computer_20250124", "name": "computer"}),
    })];
    assert_eq!(
        encode(&request),
        json!({"model": "gpt-5", "input": [hello_item()], "store": false, "stream": false})
    );
}

#[test]
fn encode_tool_choice_kept_verbatim_for_a_responses_client() {
    let raw = json!({"type": "allowed_tools", "mode": "auto", "tools": [{"type": "function", "name": "f"}]});
    let mut request = base(P);
    request.tools = vec![function_tool("f")];
    request.tool_choice = Some(ToolChoice::Auto);
    request.extra.insert("tool_choice".into(), raw.clone());
    assert_eq!(encode(&request)["tool_choice"], raw);

    // The same leftover from another protocol means nothing here.
    request.source = Protocol::Anthropic;
    assert_eq!(encode(&request)["tool_choice"], json!("auto"));
}

// ---------------------------------------------------------------------------
// Multi-turn tool conversation
// ---------------------------------------------------------------------------

#[test]
fn encode_multi_turn_tool_call_conversation() {
    let mut request = Request::new("gpt-5", P);
    request.tools = vec![function_tool("get_weather")];
    request.messages = vec![
        Message::user_text("Weather in Paris and Rome?"),
        Message::new(
            Role::Assistant,
            vec![
                signed(Some("rs_1"), "Two lookups.", P, "gAAAAABenc"),
                Part::text("Checking."),
                Part::tool_call("call_a", "get_weather", "{\"city\":\"Paris\"}"),
                Part::tool_call("call_b", "get_weather", "{\"city\":\"Rome\"}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call_a", "18C"),
                Part::tool_result_text("call_b", "24C"),
            ],
        ),
        Message::assistant_text("Paris 18C, Rome 24C."),
        Message::user_text("Thanks"),
    ];
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather in Paris and Rome?"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Two lookups."}], "encrypted_content": "gAAAAABenc"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Checking."}]},
                {"type": "function_call", "call_id": "call_a", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call", "call_id": "call_b", "name": "get_weather", "arguments": "{\"city\":\"Rome\"}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "18C"},
                {"type": "function_call_output", "call_id": "call_b", "output": "24C"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Paris 18C, Rome 24C."}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Thanks"}]}
            ],
            "tools": [{"type": "function", "name": "get_weather", "description": "A tool",
                       "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}],
            // Signed reasoning in the history is evidence the model reasons:
            // keep the next turn replayable without server-side state.
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": false
        })
    );
}

#[test]
fn encode_tool_result_and_text_in_one_user_message() {
    // Anthropic-style: the tool result and the follow-up text share a turn.
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages = vec![
        Message::new(Role::Assistant, vec![Part::tool_call("toolu_1", "f", "")]),
        Message::new(
            Role::User,
            vec![
                Part::ToolResult(ToolResult {
                    call_id: "toolu_1".into(),
                    name: None,
                    content: vec![Part::text("line 1\n"), Part::text("line 2")],
                    is_error: true,
                    cache_control: Some(json!({"type": "ephemeral"})),
                }),
                Part::text("What next?"),
            ],
        ),
    ];
    assert_eq!(
        encode(&request)["input"],
        json!([
            // Empty arguments mean "no arguments"; strict upstreams want `{}`.
            {"type": "function_call", "call_id": "toolu_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "toolu_1", "output": "line 1\nline 2"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What next?"}]}
        ])
    );
}

#[test]
fn encode_tool_results_carrying_images() {
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages = vec![
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("c1", "screenshot", "{}")],
        ),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: None,
                content: vec![
                    Part::text("Captured."),
                    Part::Image(MediaPart::base64("image/png", "iVBOR")),
                ],
                is_error: false,
                cache_control: None,
            })],
        ),
    ];
    assert_eq!(
        encode(&request)["input"][1],
        json!({"type": "function_call_output", "call_id": "c1", "output": [
            {"type": "input_text", "text": "Captured."},
            {"type": "input_image", "image_url": "data:image/png;base64,iVBOR"}
        ]})
    );
}

#[test]
fn encode_custom_tool_call_and_its_output() {
    let mut request = Request::new("gpt-5", P);
    request.messages = vec![
        Message::new(
            Role::Assistant,
            vec![Part::ToolCall(ToolCall {
                id: "call_c".into(),
                name: "exec".into(),
                arguments: "ls -la".into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            })],
        ),
        Message::new(
            Role::User,
            vec![Part::tool_result_text("call_c", "total 0")],
        ),
    ];
    assert_eq!(
        encode(&request)["input"],
        json!([
            {"type": "custom_tool_call", "call_id": "call_c", "name": "exec", "input": "ls -la"},
            {"type": "custom_tool_call_output", "call_id": "call_c", "output": "total 0"}
        ])
    );
}

#[test]
fn encode_shortens_call_ids_beyond_the_vendor_limit_consistently() {
    let long = format!("toolu_{}", "x".repeat(90));
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages = vec![
        Message::new(
            Role::Assistant,
            vec![Part::tool_call(long.clone(), "f", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text(long.clone(), "ok")]),
    ];
    let input = encode(&request)["input"].clone();
    let call_id = input[0]["call_id"].as_str().unwrap().to_string();
    assert_eq!(call_id.len(), 64);
    assert!(call_id.starts_with(&long[..47]));
    assert_eq!(input[1]["call_id"], json!(call_id));
}

// ---------------------------------------------------------------------------
// Reasoning parts and signatures
// ---------------------------------------------------------------------------

#[test]
fn encode_reasoning_parts_by_signature_family() {
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages = vec![
        Message::user_text("q"),
        Message::new(
            Role::Assistant,
            vec![
                // Native blob: replayed.
                signed(None, "", P, "gAAAAnative"),
                // Foreign blob: cannot be decrypted here, dropped.
                signed(None, "claude thoughts", Protocol::Anthropic, "EqQBsig"),
                // Unsigned: rejected by the vendor, dropped.
                Part::reasoning("unsigned thoughts"),
                Part::text("answer"),
            ],
        ),
    ];
    let body = encode(&request);
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
            {"type": "reasoning", "summary": [], "encrypted_content": "gAAAAnative"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
        ])
    );
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["store"], json!(false));
}

#[test]
fn encode_only_foreign_reasoning_leaves_no_trace() {
    let mut request = Request::new("gpt-5", Protocol::Anthropic);
    request.messages = vec![
        Message::user_text("q"),
        Message::new(
            Role::Assistant,
            vec![
                signed(None, "gemini thoughts", Protocol::Gemini, "CvkBsig"),
                Part::text("answer"),
            ],
        ),
    ];
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
            ],
            "store": false,
            "stream": false
        })
    );
}

#[test]
fn encode_reasoning_that_nothing_follows_is_dropped() {
    // The vendor rejects a reasoning item without the output it led to.
    let mut request = Request::new("gpt-5", P);
    request.messages = vec![
        Message::user_text("q"),
        Message::new(
            Role::Assistant,
            vec![
                Part::text("partial"),
                signed(Some("rs_tail"), "cut off", P, "gAAAAtail"),
            ],
        ),
        Message::user_text("go on"),
    ];
    let body = encode(&request);
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "partial"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go on"}]}
        ])
    );
    assert_eq!(body.get("include"), None);
}

#[test]
fn encode_drops_signatures_and_parts_of_other_protocols() {
    let mut request = Request::new("gpt-5", Protocol::Gemini);
    request.messages = vec![
        Message::new(
            Role::User,
            vec![Part::Text(TextPart {
                text: "cached question".into(),
                cache_control: Some(json!({"type": "ephemeral"})),
                ..TextPart::default()
            })],
        ),
        Message::new(
            Role::Assistant,
            vec![
                Part::Text(TextPart {
                    text: "signed text".into(),
                    signature: Some(Signature::new(Protocol::Gemini, "CtextSig")),
                    citations: vec![Citation {
                        url: Some("https://example.com".into()),
                        title: Some("Example".into()),
                        cited_text: None,
                        start: Some(0),
                        end: Some(6),
                    }],
                    ..TextPart::default()
                }),
                Part::Refusal(RefusalPart { text: "no".into() }),
                Part::Image(MediaPart::base64("image/png", "generated")),
                Part::Opaque(OpaquePart {
                    origin: Protocol::Anthropic,
                    raw: json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
                }),
                Part::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "f".into(),
                    arguments: "{\"a\":1}".into(),
                    kind: ToolCallKind::Function,
                    signature: Some(Signature::new(Protocol::Gemini, "CcallSig")),
                    cache_control: None,
                }),
            ],
        ),
    ];
    assert_eq!(
        encode(&request)["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "cached question"}]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "signed text", "annotations": [
                    {"type": "url_citation", "start_index": 0, "end_index": 6, "url": "https://example.com", "title": "Example"}
                ]},
                {"type": "refusal", "refusal": "no"}
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{\"a\":1}"}
        ])
    );
}

#[test]
fn encode_opaque_items_only_for_this_protocol() {
    let search = json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "rust"}});
    let mut request = Request::new("gpt-5", P);
    request.messages = vec![
        Message::user_text("search"),
        Message::new(
            Role::Assistant,
            vec![
                Part::Opaque(OpaquePart {
                    origin: P,
                    raw: search.clone(),
                }),
                Part::Opaque(OpaquePart {
                    origin: Protocol::OpenaiChat,
                    raw: json!({"type": "audio", "id": "audio_1"}),
                }),
                Part::text("found"),
            ],
        ),
    ];
    assert_eq!(
        encode(&request)["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "search"}]},
            search,
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "found"}]}
        ])
    );
}

// ---------------------------------------------------------------------------
// Reasoning settings
// ---------------------------------------------------------------------------

#[test]
fn encode_reasoning_settings() {
    let body = |config: ReasoningConfig, thinking: ModelThinking<'_>| {
        let mut request = base(Protocol::Anthropic);
        request.reasoning = Some(config);
        encode_for(&request, thinking)
    };
    let unknown = ModelThinking::Unknown;

    let high = body(
        ReasoningConfig::with_depth(Depth::Level(Effort::High)),
        unknown,
    );
    assert_eq!(
        high,
        json!({
            "model": "gpt-5",
            "input": [hello_item()],
            "reasoning": {"effort": "high"},
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": false
        })
    );

    // Off is spelled `none`, and a model that will not reason has no
    // encrypted reasoning to ask for.
    let off = body(ReasoningConfig::with_depth(Depth::Off), unknown);
    assert_eq!(
        off,
        json!({"model": "gpt-5", "input": [hello_item()], "reasoning": {"effort": "none"}, "store": false, "stream": false})
    );

    // "Let the provider decide" is the absence of an effort.
    let auto = body(ReasoningConfig::with_depth(Depth::Auto), unknown);
    assert_eq!(auto.get("reasoning"), None);
    assert_eq!(auto["include"], json!(["reasoning.encrypted_content"]));

    // Budgets are bucketed into levels.
    let budget = body(ReasoningConfig::with_depth(Depth::Budget(9000)), unknown);
    assert_eq!(budget["reasoning"], json!({"effort": "high"}));

    let summarised = body(
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Detailed),
        },
        unknown,
    );
    assert_eq!(
        summarised["reasoning"],
        json!({"effort": "low", "summary": "detailed"})
    );

    // Summary off is the absence of the field.
    let quiet = body(
        ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Off),
        },
        unknown,
    );
    assert_eq!(quiet["reasoning"], json!({"effort": "low"}));
}

#[test]
fn encode_reasoning_respects_model_capabilities() {
    let mut request = base(Protocol::Anthropic);
    request.reasoning = Some(ReasoningConfig {
        depth: Some(Depth::Level(Effort::Max)),
        summary: Some(Summary::Auto),
    });

    let levels = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let clamped = encode_for(&request, ModelThinking::Supported(&levels));
    assert_eq!(
        clamped["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(clamped["include"], json!(["reasoning.encrypted_content"]));

    // A model known not to reason gets no reasoning settings at all.
    let plain = encode_for(&request, ModelThinking::Unsupported);
    assert_eq!(
        plain,
        json!({"model": "gpt-5", "input": [hello_item()], "store": false, "stream": false})
    );

    // A reasoning model reasons even when the client said nothing.
    let mut silent = base(Protocol::OpenaiChat);
    silent.reasoning = None;
    let body = encode_for(&silent, ModelThinking::Supported(&levels));
    assert_eq!(body.get("reasoning"), None);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["store"], json!(false));
}

#[test]
fn encode_store_and_include_follow_what_the_request_said() {
    let levels = ThinkingSupport::levels(&[Effort::Low, Effort::High]);
    let thinking = ModelThinking::Supported(&levels);

    // The client relies on stored state: leave it alone.
    let mut stored = base(P);
    stored.store = Some(true);
    let body = encode_for(&stored, thinking);
    assert_eq!(body["store"], json!(true));
    assert_eq!(body.get("include"), None);

    let mut chained = base(P);
    chained.previous_response_id = Some("resp_prev".into());
    let body = encode_for(&chained, thinking);
    assert_eq!(body["previous_response_id"], json!("resp_prev"));
    assert_eq!(body.get("store"), None);
    assert_eq!(body.get("include"), None);

    // The client's own include list is kept and extended, not duplicated.
    let mut including = base(P);
    including.extra.insert(
        "include".into(),
        json!([
            "web_search_call.action.sources",
            "reasoning.encrypted_content"
        ]),
    );
    let body = encode_for(&including, thinking);
    assert_eq!(
        body["include"],
        json!([
            "web_search_call.action.sources",
            "reasoning.encrypted_content"
        ])
    );
    assert_eq!(body["store"], json!(false));

    // No reasoning expected: `store` is passed through only when given.
    let mut explicit = base(P);
    explicit.store = Some(false);
    let body = encode(&explicit);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body.get("include"), None);
}

// ---------------------------------------------------------------------------
// Structured output and sampling
// ---------------------------------------------------------------------------

#[test]
fn encode_structured_output_and_json_mode() {
    let text = |format: ResponseFormat| {
        let mut request = base(Protocol::OpenaiChat);
        request.response_format = Some(format);
        encode(&request)["text"].clone()
    };
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
    assert_eq!(
        text(ResponseFormat::JsonSchema {
            name: Some("answer".into()),
            description: Some("The answer".into()),
            schema: schema.clone(),
            strict: Some(true),
        }),
        json!({"format": {"type": "json_schema", "name": "answer", "description": "The answer", "schema": schema, "strict": true}})
    );
    // `name` is mandatory in the flat form.
    assert_eq!(
        text(ResponseFormat::JsonSchema {
            name: None,
            description: None,
            schema: schema.clone(),
            strict: None,
        }),
        json!({"format": {"type": "json_schema", "name": "response", "schema": schema}})
    );
    assert_eq!(
        text(ResponseFormat::JsonObject),
        json!({"format": {"type": "json_object"}})
    );
    assert_eq!(
        text(ResponseFormat::Text),
        json!({"format": {"type": "text"}})
    );
}

#[test]
fn encode_sampling_params_and_drops_what_the_protocol_lacks() {
    let mut request = base(Protocol::Gemini);
    request.temperature = Some(0.3);
    request.top_p = Some(0.9);
    request.max_output_tokens = Some(1024);
    request.top_k = Some(40);
    request.seed = Some(7);
    request.presence_penalty = Some(0.5);
    request.frequency_penalty = Some(0.5);
    request.stop = vec!["END".into()];
    request.candidate_count = Some(2);
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [hello_item()],
            "max_output_tokens": 1024,
            "temperature": 0.3,
            "top_p": 0.9,
            "store": false,
            "stream": false
        })
    );
}

#[test]
fn encode_raises_tiny_output_limits_to_the_vendor_minimum() {
    let mut request = base(Protocol::Anthropic);
    request.max_output_tokens = Some(1);
    assert_eq!(encode(&request)["max_output_tokens"], json!(16));
}

#[test]
fn encode_bookkeeping_fields() {
    let mut request = base(Protocol::Anthropic);
    request.user = Some("user-1".into());
    request.metadata = json!({"trace": "abc", "attempt": 2}).as_object().cloned();
    request.prompt_cache_key = Some("pck".into());
    request.service_tier = Some("standard_only".into());
    // State of another vendor cannot be referenced here.
    request.previous_response_id = None;
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [hello_item()],
            "metadata": {"trace": "abc", "attempt": "2"},
            "user": "user-1",
            "prompt_cache_key": "pck",
            "store": false,
            "stream": false
        })
    );

    request.service_tier = Some("priority".into());
    assert_eq!(encode(&request)["service_tier"], json!("priority"));
}

#[test]
fn encode_responses_client_extras_are_carried_over() {
    let mut request = base(P);
    request.user = Some("sid-1".into());
    request
        .extra
        .insert("safety_identifier".into(), json!("sid-1"));
    request.extra.insert("truncation".into(), json!("auto"));
    request
        .extra
        .insert("text".into(), json!({"verbosity": "low"}));
    request.response_format = Some(ResponseFormat::JsonObject);
    assert_eq!(
        encode(&request),
        json!({
            "model": "gpt-5",
            "input": [hello_item()],
            "text": {"verbosity": "low", "format": {"type": "json_object"}},
            "safety_identifier": "sid-1",
            "truncation": "auto",
            "stream": false
        })
    );

    // Leftovers of another protocol are not Responses fields.
    let mut foreign = base(Protocol::Anthropic);
    foreign.extra.insert("anthropic_beta".into(), json!(["x"]));
    foreign.extra.insert("truncation".into(), json!("auto"));
    assert_eq!(
        encode(&foreign),
        json!({"model": "gpt-5", "input": [hello_item()], "store": false, "stream": false})
    );

    // Chat Completions spells verbosity at the top level.
    let mut chat = base(Protocol::OpenaiChat);
    chat.extra.insert("verbosity".into(), json!("high"));
    chat.extra
        .insert("logit_bias".into(), json!({"50256": -100}));
    assert_eq!(
        encode(&chat),
        json!({"model": "gpt-5", "input": [hello_item()], "text": {"verbosity": "high"}, "store": false, "stream": false})
    );
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

#[test]
fn encode_count_request_keeps_only_what_the_endpoint_takes() {
    let mut request = base(Protocol::Anthropic);
    request.stream = true;
    request.system = vec![Part::text("Be brief.")];
    request.tools = vec![function_tool("f")];
    request.tool_choice = Some(ToolChoice::Auto);
    request.temperature = Some(0.5);
    request.max_output_tokens = Some(100);
    request.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::Low)));
    let body = ResponsesCodec
        .encode_count_request(&request, &UpstreamCtx::default())
        .expect("responses has a counting endpoint");
    assert_eq!(
        body,
        json!({
            "model": "gpt-5",
            "instructions": "Be brief.",
            "input": [hello_item()],
            "tools": [{"type": "function", "name": "f", "description": "A tool",
                       "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}, "strict": false}],
            "tool_choice": "auto",
            "reasoning": {"effort": "low"}
        })
    );
}
