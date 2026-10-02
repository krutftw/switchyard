//! `encode_request` / `encode_count_request`: canonical requests -> exact
//! Gemini `generateContent` bodies.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::{GeminiCodec, SKIP_SIGNATURE, adapt_for_vertex};
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, MediaSource, Message,
    OpaquePart, Part, Reasoning, RefusalPart, Request, ResponseFormat, Role, Signature, TextPart,
    Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{
    Depth, Effort, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

fn request(source: Protocol) -> Request {
    Request::new("gemini-2.5-pro", source)
}

fn chat(messages: Vec<Message>) -> Request {
    let mut req = request(Protocol::OpenaiChat);
    req.messages = messages;
    req
}

fn encode(req: &Request) -> Value {
    GeminiCodec
        .encode_request(req, &UpstreamCtx::default())
        .expect("request encodes")
}

fn assistant(parts: Vec<Part>) -> Message {
    Message::new(Role::Assistant, parts)
}

fn user(parts: Vec<Part>) -> Message {
    Message::new(Role::User, parts)
}

fn function(name: &str, parameters: Value) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: Some(format!("{name} tool")),
        parameters,
        strict: None,
        cache_control: None,
    })
}

fn signed_call(id: &str, name: &str, args: &str, signature: Signature) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
        kind: ToolCallKind::Function,
        signature: Some(signature),
        cache_control: None,
    })
}

fn result(call_id: &str, name: Option<&str>, text: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: name.map(str::to_owned),
        content: vec![Part::text(text)],
        is_error: false,
        cache_control: None,
    })
}

// ---------------------------------------------------------------------------
// Plain text and system prompt
// ---------------------------------------------------------------------------

#[test]
fn plain_text_has_no_model_and_no_stream_field() {
    let mut req = chat(vec![Message::user_text("Hello")]);
    req.stream = true;
    assert_eq!(
        encode(&req),
        json!({"contents": [{"role": "user", "parts": [{"text": "Hello"}]}]})
    );
}

#[test]
fn system_prompt_becomes_system_instruction_parts() {
    let mut req = chat(vec![Message::user_text("Hi")]);
    req.system = vec![
        Part::Text(TextPart {
            text: "You are terse.".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
            ..TextPart::default()
        }),
        Part::text(""),
        Part::Image(MediaPart::base64("image/png", "AAAA")),
        Part::text("Answer in English."),
    ];
    assert_eq!(
        encode(&req),
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Hi"}]}],
            "systemInstruction": {"parts": [{"text": "You are terse."}, {"text": "Answer in English."}]}
        })
    );
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

#[test]
fn images_by_base64_url_and_data_uri() {
    let mut detailed = MediaPart::url("https://example.com/cat.png");
    detailed.detail = Some("high".into());
    let req = chat(vec![user(vec![
        Part::text("Describe these."),
        Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
        Part::Image(detailed),
        Part::Image(MediaPart::url("https://example.com/photo")),
        Part::Image(MediaPart::url("data:image/webp;base64,UklGRg==")),
        Part::Image(MediaPart {
            source: MediaSource::Base64 {
                data: "R0lGOD".into(),
            },
            media_type: None,
            filename: Some("anim.gif".into()),
            detail: None,
            cache_control: Some(json!({"type": "ephemeral"})),
        }),
    ])]);
    assert_eq!(
        encode(&req),
        json!({"contents": [{"role": "user", "parts": [
            {"text": "Describe these."},
            {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
            {"fileData": {"mimeType": "image/png", "fileUri": "https://example.com/cat.png"}},
            {"fileData": {"mimeType": "image/jpeg", "fileUri": "https://example.com/photo"}},
            {"inlineData": {"mimeType": "image/webp", "data": "UklGRg=="}},
            {"inlineData": {"mimeType": "image/gif", "data": "R0lGOD"}}
        ]}]})
    );
}

#[test]
fn documents_audio_and_file_references() {
    let file_ref = |id: &str| MediaPart {
        source: MediaSource::FileRef { id: id.into() },
        media_type: Some("application/pdf".into()),
        filename: None,
        detail: None,
        cache_control: None,
    };
    let parts = vec![
        Part::Document(MediaPart::base64("application/pdf", "JVBERi0=")),
        Part::Document(MediaPart::url("https://example.com/paper.pdf")),
        Part::Audio(MediaPart::base64("audio/wav", "UklGRg==")),
        Part::Document(file_ref("file-abc123")),
        Part::text("Summarise."),
    ];
    // A file id issued by another vendor means nothing to Gemini.
    let foreign = chat(vec![user(parts.clone())]);
    assert_eq!(
        encode(&foreign),
        json!({"contents": [{"role": "user", "parts": [
            {"inlineData": {"mimeType": "application/pdf", "data": "JVBERi0="}},
            {"fileData": {"mimeType": "application/pdf", "fileUri": "https://example.com/paper.pdf"}},
            {"inlineData": {"mimeType": "audio/wav", "data": "UklGRg=="}},
            {"text": "Summarise."}
        ]}]})
    );
    // From a Gemini client the handle is Google's own.
    let mut native = request(Protocol::Gemini);
    native.messages = vec![user(vec![
        Part::Document(file_ref("gs://bucket/doc.pdf")),
        Part::text("Summarise."),
    ])];
    assert_eq!(
        encode(&native),
        json!({"contents": [{"role": "user", "parts": [
            {"fileData": {"mimeType": "application/pdf", "fileUri": "gs://bucket/doc.pdf"}},
            {"text": "Summarise."}
        ]}]})
    );
}

// ---------------------------------------------------------------------------
// Tools and tool choice
// ---------------------------------------------------------------------------

#[test]
fn function_tools_are_declared_with_sanitised_json_schema() {
    let mut req = chat(vec![Message::user_text("go")]);
    req.tools = vec![
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Current weather".into()),
            parameters: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": "Args",
                "type": "object",
                "properties": {
                    "city": {"type": "string", "pattern": "^[A-Z]"},
                    "unit": {"type": ["string", "null"], "enum": ["c", "f"]}
                },
                "required": ["city", "unit", "stale"],
                "additionalProperties": false
            }),
            strict: Some(true),
            cache_control: Some(json!({"type": "ephemeral"})),
        }),
        Tool::Function(FunctionTool {
            name: "ping".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }),
        function("untyped", json!({})),
    ];
    assert_eq!(
        encode(&req)["tools"],
        json!([{"functionDeclarations": [
            {
                "name": "get_weather",
                "description": "Current weather",
                "parametersJsonSchema": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "pattern": "^[A-Z]"},
                        "unit": {"type": "string", "enum": ["c", "f"], "description": "Allowed: c, f (nullable)"}
                    },
                    "required": ["city"],
                    "additionalProperties": false
                }
            },
            {"name": "ping", "parametersJsonSchema": {"type": "object", "properties": {}}},
            {"name": "untyped", "description": "untyped tool", "parametersJsonSchema": {"type": "object"}}
        ]}])
    );
    assert!(encode(&req).get("toolConfig").is_none());
}

#[test]
fn tool_names_are_sanitised_consistently() {
    let mut req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("call_1", "mcp server/get data", "{}")]),
        user(vec![result("call_1", None, "ok")]),
        Message::user_text("and now?"),
    ]);
    req.tools = vec![
        function("mcp server/get data", json!({"type": "object"})),
        function("123go", json!({"type": "object"})),
        // Sanitises to the same name as the first tool: only one declaration.
        function("mcp server/get_data", json!({"type": "object"})),
    ];
    req.tool_choice = Some(ToolChoice::Tool {
        name: "mcp server/get data".into(),
    });
    let body = encode(&req);
    let declared: Vec<&str> = body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(declared, ["mcp_server_get_data", "_123go"]);
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["mcp_server_get_data"]}})
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["name"],
        "mcp_server_get_data"
    );
    assert_eq!(
        body["contents"][2]["parts"][1]["functionResponse"]["name"],
        "mcp_server_get_data"
    );
}

#[test]
fn tool_choice_variants() {
    let config = |choice: Option<ToolChoice>, with_tools: bool| {
        let mut req = chat(vec![Message::user_text("go")]);
        if with_tools {
            req.tools = vec![function("lookup", json!({"type": "object"}))];
        }
        req.tool_choice = choice;
        encode(&req).get("toolConfig").cloned()
    };
    assert_eq!(config(None, true), None);
    assert_eq!(
        config(Some(ToolChoice::Auto), true),
        Some(json!({"functionCallingConfig": {"mode": "AUTO"}}))
    );
    assert_eq!(
        config(Some(ToolChoice::None), true),
        Some(json!({"functionCallingConfig": {"mode": "NONE"}}))
    );
    assert_eq!(
        config(Some(ToolChoice::Required), true),
        Some(json!({"functionCallingConfig": {"mode": "ANY"}}))
    );
    assert_eq!(
        config(
            Some(ToolChoice::Tool {
                name: "lookup".into()
            }),
            true
        ),
        Some(json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lookup"]}}))
    );
    // Forcing a tool that was never declared must not let the model pick
    // another one.
    assert_eq!(
        config(
            Some(ToolChoice::Tool {
                name: "other".into()
            }),
            true
        ),
        Some(json!({"functionCallingConfig": {"mode": "NONE"}}))
    );
    // A calling mode without declarations would be rejected.
    assert_eq!(config(Some(ToolChoice::Required), false), None);
}

#[test]
fn parallel_tool_calls_flag_does_not_disable_tools() {
    let mut req = chat(vec![Message::user_text("go")]);
    req.tools = vec![function("lookup", json!({"type": "object"}))];
    req.parallel_tool_calls = Some(false);
    let body = encode(&req);
    assert!(body.get("toolConfig").is_none());
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
}

#[test]
fn custom_tools_become_functions_with_one_string_argument() {
    let mut req = request(Protocol::OpenaiResponses);
    req.tools = vec![Tool::Custom(CustomTool {
        name: "apply_patch".into(),
        description: Some("Apply a patch".into()),
        format: Some(json!({"type": "grammar", "syntax": "lark", "definition": "start: /.+/"})),
    })];
    req.messages = vec![
        Message::user_text("patch it"),
        assistant(vec![Part::ToolCall(ToolCall {
            id: "call_c1".into(),
            name: "apply_patch".into(),
            arguments: "*** Begin Patch\n{\"not\": \"json args\"}".into(),
            kind: ToolCallKind::Custom,
            signature: None,
            cache_control: None,
        })]),
        user(vec![result("call_c1", None, "Done")]),
    ];
    let body = encode(&req);
    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations": [{
            "name": "apply_patch",
            "description": "Apply a patch",
            "parametersJsonSchema": {
                "type": "object",
                "properties": {"input": {"type": "string", "description": "The raw input for the tool."}},
                "required": ["input"]
            }
        }]}])
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"],
        json!({"name": "apply_patch", "args": {"input": "*** Begin Patch\n{\"not\": \"json args\"}"}})
    );
}

#[test]
fn builtin_tools_of_another_family_map_to_gemini_tools() {
    let foreign = |kind: BuiltinKind, origin: Protocol, raw: Value| {
        Tool::Builtin(BuiltinTool { kind, origin, raw })
    };
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![Message::user_text("search")];
    req.tools = vec![
        foreign(
            BuiltinKind::WebSearch,
            Protocol::Anthropic,
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        ),
        foreign(
            BuiltinKind::WebSearch,
            Protocol::OpenaiResponses,
            json!({"type": "web_search"}),
        ),
        foreign(
            BuiltinKind::WebFetch,
            Protocol::Anthropic,
            json!({"type": "web_fetch_20250910", "name": "web_fetch"}),
        ),
        foreign(
            BuiltinKind::CodeExecution,
            Protocol::OpenaiResponses,
            json!({"type": "code_interpreter"}),
        ),
        foreign(
            BuiltinKind::Other("computer_20250124".into()),
            Protocol::Anthropic,
            json!({"type": "computer_20250124"}),
        ),
    ];
    assert_eq!(
        encode(&req)["tools"],
        json!([{"googleSearch": {}}, {"urlContext": {}}, {"codeExecution": {}}])
    );

    // Gemini rejects built-in tools next to function declarations on most
    // models: the functions win.
    req.tools
        .push(function("lookup", json!({"type": "object"})));
    assert_eq!(
        encode(&req)["tools"],
        json!([{"functionDeclarations": [{
            "name": "lookup", "description": "lookup tool", "parametersJsonSchema": {"type": "object"}
        }]}])
    );
}

#[test]
fn gemini_builtin_tools_are_forwarded_verbatim() {
    let mut req = request(Protocol::Gemini);
    req.messages = vec![Message::user_text("search")];
    req.tools = vec![
        function("lookup", json!({"type": "object"})),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Gemini,
            raw: json!({"google_search": {"timeRangeFilter": {"startTime": "2025-01-01T00:00:00Z"}}}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::Other("googleMaps".into()),
            origin: Protocol::Gemini,
            raw: json!({"googleMaps": {}}),
        }),
    ];
    assert_eq!(
        encode(&req)["tools"],
        json!([
            {"functionDeclarations": [{
                "name": "lookup", "description": "lookup tool", "parametersJsonSchema": {"type": "object"}
            }]},
            {"google_search": {"timeRangeFilter": {"startTime": "2025-01-01T00:00:00Z"}}},
            {"googleMaps": {}}
        ])
    );
}

// ---------------------------------------------------------------------------
// Tool conversations
// ---------------------------------------------------------------------------

#[test]
fn multi_turn_tool_conversation_from_a_chat_client() {
    let req = chat(vec![
        Message::user_text("Weather in Paris and Rome?"),
        assistant(vec![
            Part::text("Let me check."),
            Part::tool_call("call_a", "get_weather", r#"{"city":"Paris"}"#),
            Part::tool_call("call_b", "get_weather", r#"{"city":"Rome"}"#),
        ]),
        // Chat clients send one tool message per result, in any order.
        user(vec![result("call_b", None, r#"{"temp":24,"unit":"C"}"#)]),
        user(vec![result("call_a", None, "18C and sunny")]),
        assistant(vec![Part::text("Paris 18C, Rome 24C.")]),
        Message::user_text("Thanks!"),
    ]);
    assert_eq!(
        encode(&req),
        json!({"contents": [
            {"role": "user", "parts": [{"text": "Weather in Paris and Rome?"}]},
            {"role": "model", "parts": [
                {"text": "Let me check."},
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}, "thoughtSignature": SKIP_SIGNATURE},
                {"functionCall": {"name": "get_weather", "args": {"city": "Rome"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "get_weather", "response": {"result": "18C and sunny"}}},
                {"functionResponse": {"name": "get_weather", "response": {"temp": 24, "unit": "C"}}}
            ]},
            {"role": "model", "parts": [{"text": "Paris 18C, Rome 24C."}]},
            {"role": "user", "parts": [{"text": "Thanks!"}]}
        ]})
    );
}

#[test]
fn tool_result_payload_forms() {
    let error = Part::ToolResult(ToolResult {
        call_id: "c3".into(),
        name: Some("f".into()),
        content: vec![Part::text("file not found")],
        is_error: true,
        cache_control: None,
    });
    let empty = Part::ToolResult(ToolResult {
        call_id: "c7".into(),
        name: Some("f".into()),
        content: vec![],
        is_error: false,
        cache_control: None,
    });
    let calls = (1..=7)
        .map(|i| Part::tool_call(format!("c{i}"), "f", "{}"))
        .collect();
    let req = chat(vec![
        Message::user_text("go"),
        assistant(calls),
        user(vec![
            result("c1", Some("f"), "plain text"),
            result("c2", Some("f"), r#"{"rows": [1, 2], "ok": true}"#),
            error,
            result("c4", Some("f"), "[1, 2, 3]"),
            // `$ref` inside a function response is read by Gemini as a media
            // reference, so such a result travels as text.
            result("c5", Some("f"), r##"{"schema": {"$ref": "#/defs/x"}}"##),
            // Would be mistaken for the wrapper itself: wrapped once more.
            result("c6", Some("f"), r#"{"result": "inner"}"#),
            empty,
        ]),
    ]);
    let body = encode(&req);
    let responses: Vec<Value> = body["contents"][2]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["functionResponse"]["response"].clone())
        .collect();
    assert_eq!(
        responses,
        vec![
            json!({"result": "plain text"}),
            json!({"rows": [1, 2], "ok": true}),
            json!({"error": "file not found"}),
            json!({"result": "[1, 2, 3]"}),
            json!({"result": r##"{"schema": {"$ref": "#/defs/x"}}"##}),
            json!({"result": r#"{"result": "inner"}"#}),
            json!({"result": ""}),
        ]
    );
}

#[test]
fn tool_results_carrying_images_put_them_after_the_function_response() {
    let screenshot = Part::ToolResult(ToolResult {
        call_id: "toolu_1".into(),
        name: None,
        content: vec![
            Part::text("Screenshot taken."),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
            Part::Document(MediaPart::base64("application/pdf", "JVBERi0=")),
        ],
        is_error: false,
        cache_control: Some(json!({"type": "ephemeral"})),
    });
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![
        Message::user_text("Take a screenshot"),
        assistant(vec![Part::tool_call("toolu_1", "screenshot", "{}")]),
        user(vec![screenshot]),
    ];
    assert_eq!(
        encode(&req)["contents"][2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"name": "screenshot", "response": {"result": "Screenshot taken."}}},
            {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
            {"inlineData": {"mimeType": "application/pdf", "data": "JVBERi0="}}
        ]})
    );
}

#[test]
fn tool_results_without_names_resolve_through_the_call() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call(
            "call_1",
            "read_file",
            r#"{"path":"a.txt"}"#,
        )]),
        user(vec![result("call_1", None, "contents")]),
    ]);
    assert_eq!(
        encode(&req)["contents"][2]["parts"],
        json!([{"functionResponse": {"name": "read_file", "response": {"result": "contents"}}}])
    );

    // Nothing to resolve against (a Gemini client's own orphan): a
    // placeholder name keeps the part valid.
    let mut native = request(Protocol::Gemini);
    native.messages = vec![user(vec![result(
        "call_0123456789abcdef01234567",
        None,
        "orphan",
    )])];
    assert_eq!(
        encode(&native)["contents"][0]["parts"],
        json!([{"functionResponse": {"name": "unknown_function", "response": {"result": "orphan"}}}])
    );
}

#[test]
fn text_moves_in_front_of_function_responses() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("c1", "f", "{}")]),
        user(vec![
            Part::text("By the way, be quick."),
            result("c1", None, "done"),
        ]),
    ]);
    // `normalize_turns` puts results first; Vertex rejects text after a
    // function response in the same turn, so the text is hoisted.
    assert_eq!(
        encode(&req)["contents"][2],
        json!({"role": "user", "parts": [
            {"text": "By the way, be quick."},
            {"functionResponse": {"name": "f", "response": {"result": "done"}}}
        ]})
    );
}

#[test]
fn unanswered_calls_and_orphan_results_are_repaired_for_foreign_clients() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            Part::tool_call("c1", "first", "{}"),
            Part::tool_call("c2", "second", "{}"),
        ]),
        user(vec![
            result("c2", None, "second done"),
            result("ghost", None, "nobody asked"),
        ]),
        assistant(vec![Part::text("ok")]),
        Message::user_text("next"),
    ]);
    assert_eq!(
        encode(&req)["contents"][2],
        json!({"role": "user", "parts": [
            {"text": "nobody asked"},
            {"functionResponse": {"name": "first", "response": {"result": "call interrupted, no output"}}},
            {"functionResponse": {"name": "second", "response": {"result": "second done"}}}
        ]})
    );
}

#[test]
fn a_user_turn_that_ignores_pending_calls_gets_synthetic_responses() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("c1", "slow", "{}")]),
        Message::user_text("never mind, do something else"),
    ]);
    assert_eq!(
        encode(&req)["contents"][2],
        json!({"role": "user", "parts": [
            {"text": "never mind, do something else"},
            {"functionResponse": {"name": "slow", "response": {"result": "call interrupted, no output"}}}
        ]})
    );
}

#[test]
fn invalid_or_empty_arguments_still_produce_an_object() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            Part::tool_call("c1", "f", ""),
            Part::tool_call("c2", "f", "not json"),
            Part::tool_call("c3", "f", "[1,2]"),
            Part::tool_call("c4", "", "{}"),
        ]),
        user(vec![
            result("c1", None, "a"),
            result("c2", None, "b"),
            result("c3", None, "c"),
        ]),
    ]);
    let body = encode(&req);
    let args: Vec<Value> = body["contents"][1]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["functionCall"]["args"].clone())
        .collect();
    // The nameless call is not a valid function call and is dropped.
    assert_eq!(
        args,
        vec![
            json!({}),
            json!({"input": "not json"}),
            json!({"input": [1, 2]})
        ]
    );
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

#[test]
fn gemini_signatures_are_replayed_and_only_the_first_call_gets_the_bypass() {
    let gemini = Signature::new(Protocol::Gemini, "Q2lRQlNpZw==");
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            signed_call("c1", "a", "{}", gemini.clone()),
            Part::tool_call("c2", "b", "{}"),
        ]),
        user(vec![result("c1", None, "1"), result("c2", None, "2")]),
        assistant(vec![
            Part::tool_call("c3", "a", "{}"),
            signed_call("c4", "b", "{}", gemini.clone()),
            Part::tool_call("c5", "c", "{}"),
        ]),
        user(vec![
            result("c3", None, "3"),
            result("c4", None, "4"),
            result("c5", None, "5"),
        ]),
    ]);
    let body = encode(&req);
    assert_eq!(
        body["contents"][1]["parts"],
        json!([
            {"functionCall": {"name": "a", "args": {}}, "thoughtSignature": "Q2lRQlNpZw=="},
            {"functionCall": {"name": "b", "args": {}}}
        ])
    );
    assert_eq!(
        body["contents"][3]["parts"],
        json!([
            {"functionCall": {"name": "a", "args": {}}, "thoughtSignature": SKIP_SIGNATURE},
            {"functionCall": {"name": "b", "args": {}}, "thoughtSignature": "Q2lRQlNpZw=="},
            {"functionCall": {"name": "c", "args": {}}}
        ])
    );
    assert_eq!(SKIP_SIGNATURE, "skip_thought_signature_validator");
}

#[test]
fn foreign_signatures_on_calls_are_replaced_by_the_bypass_value() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            signed_call(
                "c1",
                "a",
                "{}",
                Signature::new(Protocol::Anthropic, "ErACkgE="),
            ),
            signed_call(
                "c2",
                "b",
                "{}",
                Signature::new(Protocol::OpenaiResponses, "gAAAAAB"),
            ),
        ]),
        user(vec![result("c1", None, "1"), result("c2", None, "2")]),
    ]);
    assert_eq!(
        encode(&req)["contents"][1]["parts"],
        json!([
            {"functionCall": {"name": "a", "args": {}}, "thoughtSignature": SKIP_SIGNATURE},
            {"functionCall": {"name": "b", "args": {}}}
        ])
    );
}

#[test]
fn consecutive_assistant_messages_are_one_model_turn_with_one_bypass() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("c1", "a", "{}")]),
        assistant(vec![Part::tool_call("c2", "b", "{}")]),
        user(vec![result("c1", None, "1")]),
        user(vec![result("c2", None, "2")]),
    ]);
    assert_eq!(
        encode(&req)["contents"],
        json!([
            {"role": "user", "parts": [{"text": "go"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "a", "args": {}}, "thoughtSignature": SKIP_SIGNATURE},
                {"functionCall": {"name": "b", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "a", "response": {"result": "1"}}},
                {"functionResponse": {"name": "b", "response": {"result": "2"}}}
            ]}
        ])
    );
}

#[test]
fn reasoning_parts_by_signature_family() {
    let reasoning = |text: &str, signature: Option<Signature>, redacted: bool| {
        Part::Reasoning(Reasoning {
            id: None,
            text: text.into(),
            signature,
            redacted,
        })
    };
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![
        Message::user_text("think"),
        assistant(vec![
            // Issued by Gemini: replayed with its signature.
            reasoning(
                "gemini thought",
                Some(Signature::new(Protocol::Gemini, "R2VtU2ln")),
                false,
            ),
            // Issued by Anthropic / OpenAI: the whole part is dropped.
            reasoning(
                "claude thought",
                Some(Signature::new(Protocol::Anthropic, "ErACkgE=")),
                false,
            ),
            reasoning(
                "",
                Some(Signature::new(Protocol::Anthropic, "cmVkYWN0ZWQ=")),
                true,
            ),
            reasoning(
                "openai summary",
                Some(Signature::new(Protocol::OpenaiResponses, "gAAAAAB")),
                false,
            ),
            // Unsigned reasoning text is harmless context.
            reasoning("plain reasoning", None, false),
            // Nothing to say and nothing to prove.
            reasoning("", None, false),
            Part::text("Answer."),
            // A bare Gemini signature is replayed as Gemini's carrier part.
            reasoning(
                "",
                Some(Signature::new(Protocol::Gemini, "VHJhaWxpbmc=")),
                false,
            ),
        ]),
        Message::user_text("more"),
    ];
    assert_eq!(
        encode(&req)["contents"][1],
        json!({"role": "model", "parts": [
            {"text": "gemini thought", "thought": true, "thoughtSignature": "R2VtU2ln"},
            {"text": "plain reasoning", "thought": true},
            {"text": "Answer."},
            {"text": "", "thoughtSignature": "VHJhaWxpbmc="}
        ]})
    );
}

#[test]
fn text_signatures_follow_the_same_family_rule() {
    let signed = |text: &str, origin: Protocol| {
        Part::Text(TextPart {
            text: text.into(),
            signature: Some(Signature::new(origin, "U0lH")),
            ..TextPart::default()
        })
    };
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            signed("native", Protocol::Gemini),
            signed("foreign", Protocol::Anthropic),
        ]),
        Message::user_text("more"),
    ]);
    assert_eq!(
        encode(&req)["contents"][1]["parts"],
        json!([{"text": "native", "thoughtSignature": "U0lH"}, {"text": "foreign"}])
    );
}

// ---------------------------------------------------------------------------
// Turn structure
// ---------------------------------------------------------------------------

#[test]
fn cache_control_markers_are_dropped_and_content_kept() {
    let mut req = request(Protocol::Anthropic);
    req.messages = vec![user(vec![
        Part::Text(TextPart {
            text: "Long shared context.".into(),
            cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
            ..TextPart::default()
        }),
        Part::text("Question?"),
    ])];
    assert_eq!(
        encode(&req),
        json!({"contents": [{"role": "user", "parts": [{"text": "Long shared context."}, {"text": "Question?"}]}]})
    );
}

#[test]
fn system_message_in_mid_conversation_becomes_marked_user_text() {
    let req = chat(vec![
        Message::user_text("Hi"),
        assistant(vec![Part::text("Hello!")]),
        Message::new(
            Role::System,
            vec![
                Part::text("The user is now on mobile."),
                Part::text("Keep it short."),
            ],
        ),
        Message::user_text("What's new?"),
    ]);
    assert_eq!(
        encode(&req)["contents"],
        json!([
            {"role": "user", "parts": [{"text": "Hi"}]},
            {"role": "model", "parts": [{"text": "Hello!"}]},
            {"role": "user", "parts": [
                {"text": "<system-reminder>\nThe user is now on mobile.\nKeep it short.\n</system-reminder>"},
                {"text": "What's new?"}
            ]}
        ])
    );
}

#[test]
fn system_message_between_calls_and_results_keeps_the_pairing() {
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("c1", "f", "{}")]),
        Message::new(Role::System, vec![Part::text("Reminder")]),
        user(vec![result("c1", None, "done")]),
    ]);
    let body = encode(&req);
    assert_eq!(body["contents"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["contents"][2],
        json!({"role": "user", "parts": [
            {"text": "<system-reminder>\nReminder\n</system-reminder>"},
            {"functionResponse": {"name": "f", "response": {"result": "done"}}}
        ]})
    );
}

#[test]
fn consecutive_same_role_messages_are_merged() {
    let req = chat(vec![
        Message::user_text("one"),
        Message::user_text("two"),
        assistant(vec![Part::text("three")]),
        assistant(vec![Part::text("four")]),
        user(vec![]),
        Message::user_text("five"),
    ]);
    assert_eq!(
        encode(&req)["contents"],
        json!([
            {"role": "user", "parts": [{"text": "one"}, {"text": "two"}]},
            {"role": "model", "parts": [{"text": "three"}, {"text": "four"}]},
            {"role": "user", "parts": [{"text": "five"}]}
        ])
    );
}

#[test]
fn empty_text_and_empty_turns_are_dropped() {
    let req = chat(vec![
        Message::user_text("question"),
        assistant(vec![Part::text("")]),
        Message::user_text(""),
        Message::user_text("follow-up"),
    ]);
    assert_eq!(
        encode(&req)["contents"],
        json!([{"role": "user", "parts": [{"text": "question"}, {"text": "follow-up"}]}])
    );
}

#[test]
fn the_first_turn_must_be_the_users() {
    let req = chat(vec![
        assistant(vec![Part::text("Welcome!")]),
        Message::user_text("Hi"),
    ]);
    assert_eq!(
        encode(&req)["contents"],
        json!([
            {"role": "user", "parts": [{"text": ""}]},
            {"role": "model", "parts": [{"text": "Welcome!"}]},
            {"role": "user", "parts": [{"text": "Hi"}]}
        ])
    );
}

#[test]
fn trailing_assistant_turns_are_dropped_for_foreign_clients_only() {
    let messages = vec![
        Message::user_text("Write JSON"),
        assistant(vec![Part::text("{")]),
    ];
    assert_eq!(
        encode(&chat(messages.clone()))["contents"],
        json!([{"role": "user", "parts": [{"text": "Write JSON"}]}])
    );
    let unanswered = chat(vec![
        Message::user_text("go"),
        assistant(vec![Part::tool_call("c1", "f", "{}")]),
    ]);
    assert_eq!(
        encode(&unanswered)["contents"],
        json!([{"role": "user", "parts": [{"text": "go"}]}])
    );
    // A Gemini client's own trailing model turn is its business.
    let mut native = request(Protocol::Gemini);
    native.messages = messages;
    assert_eq!(
        encode(&native)["contents"],
        json!([
            {"role": "user", "parts": [{"text": "Write JSON"}]},
            {"role": "model", "parts": [{"text": "{"}]}
        ])
    );
}

#[test]
fn refusals_and_opaque_parts() {
    let gemini_block = json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}});
    let req = chat(vec![
        Message::user_text("go"),
        assistant(vec![
            Part::Refusal(RefusalPart {
                text: "I can't help with that.".into(),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Gemini,
                raw: gemini_block.clone(),
            }),
            Part::Opaque(OpaquePart {
                origin: Protocol::Anthropic,
                raw: json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
            }),
            // Candidate-level metadata does not belong in `parts`.
            Part::Opaque(OpaquePart {
                origin: Protocol::Gemini,
                raw: json!({"groundingMetadata": {"webSearchQueries": ["x"]}}),
            }),
        ]),
        Message::user_text("ok"),
    ]);
    assert_eq!(
        encode(&req)["contents"][1]["parts"],
        json!([{"text": "I can't help with that."}, gemini_block])
    );
}

// ---------------------------------------------------------------------------
// Call ids
// ---------------------------------------------------------------------------

#[test]
fn only_a_gemini_clients_own_call_ids_are_sent() {
    let messages = vec![
        Message::user_text("go"),
        assistant(vec![
            Part::tool_call("fc_native_1", "a", "{}"),
            // Shape of an id minted by the gateway: Gemini never issued it.
            Part::tool_call("call_0123456789abcdef01234567", "b", "{}"),
        ]),
        user(vec![
            result("fc_native_1", Some("a"), "1"),
            result("call_0123456789abcdef01234567", Some("b"), "2"),
        ]),
    ];
    let mut native = request(Protocol::Gemini);
    native.messages = messages.clone();
    let body = encode(&native);
    assert_eq!(
        body["contents"][1]["parts"],
        json!([
            {"functionCall": {"name": "a", "args": {}, "id": "fc_native_1"}, "thoughtSignature": SKIP_SIGNATURE},
            {"functionCall": {"name": "b", "args": {}}}
        ])
    );
    assert_eq!(
        body["contents"][2]["parts"],
        json!([
            {"functionResponse": {"name": "a", "response": {"result": "1"}, "id": "fc_native_1"}},
            {"functionResponse": {"name": "b", "response": {"result": "2"}}}
        ])
    );
    // The same history from an Anthropic client: no ids at all.
    let mut foreign = request(Protocol::Anthropic);
    foreign.messages = messages;
    let body = encode(&foreign);
    assert!(
        body["contents"][1]["parts"][0]["functionCall"]
            .get("id")
            .is_none()
    );
    assert!(
        body["contents"][2]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Generation config
// ---------------------------------------------------------------------------

#[test]
fn sampling_parameters() {
    let mut req = chat(vec![Message::user_text("hi")]);
    req.temperature = Some(0.7);
    req.top_p = Some(0.9);
    req.top_k = Some(40);
    req.max_output_tokens = Some(1024);
    req.stop = vec!["END".into(), "STOP".into()];
    req.seed = Some(42);
    req.presence_penalty = Some(0.5);
    req.frequency_penalty = Some(-0.5);
    req.candidate_count = Some(1);
    req.user = Some("user-123".into());
    req.metadata = Some(json!({"trace": "abc"}).as_object().unwrap().clone());
    req.service_tier = Some("priority".into());
    req.store = Some(true);
    req.prompt_cache_key = Some("k".into());
    assert_eq!(
        encode(&req),
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                "temperature": 0.7,
                "topP": 0.9,
                "topK": 40,
                "maxOutputTokens": 1024,
                "stopSequences": ["END", "STOP"],
                "seed": 42,
                "presencePenalty": 0.5,
                "frequencyPenalty": -0.5
            }
        })
    );
}

#[test]
fn values_gemini_would_reject_are_adjusted() {
    let mut req = chat(vec![Message::user_text("hi")]);
    req.max_output_tokens = Some(200_000);
    req.stop = (1..=7).map(|i| format!("s{i}")).collect();
    req.seed = Some(9_007_199_254_740_993);
    // Explicit zeros are what OpenAI clients send by default; several Gemini
    // models reject the fields altogether.
    req.presence_penalty = Some(0.0);
    req.frequency_penalty = Some(0.0);
    req.candidate_count = Some(3);
    let ctx = UpstreamCtx {
        max_output_tokens: Some(65_536),
        ..UpstreamCtx::default()
    };
    assert_eq!(
        GeminiCodec.encode_request(&req, &ctx).unwrap()["generationConfig"],
        json!({"maxOutputTokens": 65_536, "stopSequences": ["s1", "s2", "s3", "s4", "s5"]})
    );
    // Unknown limit: the client's value goes through.
    assert_eq!(encode(&req)["generationConfig"]["maxOutputTokens"], 200_000);
}

#[test]
fn structured_output() {
    let mut req = chat(vec![Message::user_text("hi")]);
    req.response_format = Some(ResponseFormat::JsonObject);
    assert_eq!(
        encode(&req)["generationConfig"],
        json!({"responseMimeType": "application/json"})
    );

    let schema = json!({
        "type": "object",
        "properties": {"answer": {"anyOf": [{"type": "string"}, {"type": "null"}]}, "ref": {"$ref": "#/$defs/R"}},
        "required": ["answer", "ref"],
        "additionalProperties": false,
        "$defs": {"R": {"type": "integer"}}
    });
    req.response_format = Some(ResponseFormat::JsonSchema {
        name: Some("answer".into()),
        description: Some("The answer".into()),
        schema: schema.clone(),
        strict: Some(true),
    });
    assert_eq!(
        encode(&req)["generationConfig"],
        json!({"responseMimeType": "application/json", "responseJsonSchema": schema})
    );

    req.response_format = Some(ResponseFormat::Text);
    assert!(encode(&req).get("generationConfig").is_none());
}

fn thinking(req: &Request, ctx: &UpstreamCtx<'_>) -> Option<Value> {
    GeminiCodec
        .encode_request(req, ctx)
        .unwrap()
        .get("generationConfig")
        .cloned()
}

#[test]
fn reasoning_depth_is_written_as_given() {
    let with = |config: ReasoningConfig| {
        let mut req = chat(vec![Message::user_text("hi")]);
        req.reasoning = Some(config);
        req
    };
    let unknown = UpstreamCtx::default();
    assert_eq!(
        thinking(
            &with(ReasoningConfig::with_depth(Depth::Budget(8192))),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"thinkingBudget": 8192}}))
    );
    assert_eq!(
        thinking(&with(ReasoningConfig::with_depth(Depth::Auto)), &unknown),
        Some(json!({"thinkingConfig": {"thinkingBudget": -1}}))
    );
    assert_eq!(
        thinking(&with(ReasoningConfig::with_depth(Depth::Off)), &unknown),
        Some(json!({"thinkingConfig": {"thinkingBudget": 0}}))
    );
    assert_eq!(
        thinking(
            &with(ReasoningConfig::with_depth(Depth::Level(Effort::Low))),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"thinkingLevel": "low"}}))
    );
    // Gemini has no level above `high`.
    assert_eq!(
        thinking(
            &with(ReasoningConfig::with_depth(Depth::Level(Effort::Max))),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"thinkingLevel": "high"}}))
    );
    assert_eq!(
        thinking(
            &with(ReasoningConfig {
                depth: Some(Depth::Level(Effort::High)),
                summary: Some(Summary::Detailed)
            }),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}}))
    );
    assert_eq!(
        thinking(
            &with(ReasoningConfig {
                depth: None,
                summary: Some(Summary::Auto)
            }),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"includeThoughts": true}}))
    );
    assert_eq!(
        thinking(
            &with(ReasoningConfig {
                depth: Some(Depth::Budget(512)),
                summary: Some(Summary::Off)
            }),
            &unknown
        ),
        Some(json!({"thinkingConfig": {"thinkingBudget": 512, "includeThoughts": false}}))
    );
    // "No summaries" is the default: on its own it creates no thinking config.
    assert_eq!(
        thinking(
            &with(ReasoningConfig {
                depth: None,
                summary: Some(Summary::Off)
            }),
            &unknown
        ),
        None
    );
    assert_eq!(thinking(&with(ReasoningConfig::default()), &unknown), None);
}

#[test]
fn reasoning_respects_what_is_known_about_the_model() {
    let mut req = chat(vec![Message::user_text("hi")]);
    req.temperature = Some(1.0);
    req.reasoning = Some(ReasoningConfig {
        depth: Some(Depth::Level(Effort::High)),
        summary: Some(Summary::Auto),
    });
    // A model that does not think gets no thinking config at all.
    let unsupported = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    assert_eq!(
        thinking(&req, &unsupported),
        Some(json!({"temperature": 1.0}))
    );
    // A budget-only model never receives a level.
    let caps = ThinkingSupport::budget(128, 32768);
    let budget_only = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        ..UpstreamCtx::default()
    };
    assert_eq!(
        thinking(&req, &budget_only),
        Some(
            json!({"temperature": 1.0, "thinkingConfig": {"thinkingBudget": 24576, "includeThoughts": true}})
        )
    );
}

// ---------------------------------------------------------------------------
// Fields only a Gemini client can have sent
// ---------------------------------------------------------------------------

#[test]
fn native_extras_are_restored_for_gemini_clients_only() {
    let safety = json!([{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH"}]);
    let fill = |req: &mut Request| {
        req.messages = vec![Message::user_text("hi")];
        req.tools = vec![
            function("a", json!({"type": "object"})),
            function("b", json!({"type": "object"})),
        ];
        req.tool_choice = Some(ToolChoice::Required);
        req.temperature = Some(0.2);
        req.candidate_count = Some(2);
        req.presence_penalty = Some(0.0);
        req.metadata = Some(json!({"team": "search"}).as_object().unwrap().clone());
        req.service_tier = Some("flex".into());
        req.store = Some(false);
        req.extra.insert("safetySettings".into(), safety.clone());
        req.extra
            .insert("cachedContent".into(), json!("cachedContents/abc"));
        req.extra.insert(
            "toolConfig".into(),
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["a", "b"]}}),
        );
        req.extra.insert(
            "generationConfig".into(),
            json!({"responseModalities": ["TEXT"], "temperature": 0.9}),
        );
        req.extra.insert("modelArmorConfig".into(), json!({"x": 1}));
    };
    let mut native = request(Protocol::Gemini);
    fill(&mut native);
    let body = encode(&native);
    assert_eq!(body["safetySettings"], safety);
    assert_eq!(body["cachedContent"], "cachedContents/abc");
    assert_eq!(body["labels"], json!({"team": "search"}));
    assert_eq!(body["serviceTier"], "flex");
    assert_eq!(body["store"], false);
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["a", "b"]}})
    );
    assert_eq!(
        body["generationConfig"],
        json!({"temperature": 0.2, "candidateCount": 2, "presencePenalty": 0.0, "responseModalities": ["TEXT"]})
    );
    assert!(body.get("modelArmorConfig").is_none());

    let mut foreign = request(Protocol::OpenaiChat);
    fill(&mut foreign);
    let body = encode(&foreign);
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["contents", "tools", "toolConfig", "generationConfig"]
    );
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY"}})
    );
    assert_eq!(body["generationConfig"], json!({"temperature": 0.2}));
}

// ---------------------------------------------------------------------------
// countTokens
// ---------------------------------------------------------------------------

#[test]
fn count_request_without_system_or_tools_is_just_contents() {
    let mut req = chat(vec![
        Message::user_text("Hello"),
        // Counting must see the whole history, trailing model turn included.
        assistant(vec![Part::text("Hi there")]),
    ]);
    req.temperature = Some(0.5);
    req.max_output_tokens = Some(10);
    assert_eq!(
        GeminiCodec.encode_count_request(&req, &UpstreamCtx::default()),
        Some(json!({"contents": [
            {"role": "user", "parts": [{"text": "Hello"}]},
            {"role": "model", "parts": [{"text": "Hi there"}]}
        ]}))
    );
}

#[test]
fn count_request_with_system_and_tools_uses_the_generate_content_request_form() {
    let mut req = chat(vec![Message::user_text("Hello")]);
    req.system = vec![Part::text("Be brief.")];
    req.tools = vec![function("lookup", json!({"type": "object"}))];
    req.tool_choice = Some(ToolChoice::Required);
    let mut body = GeminiCodec
        .encode_count_request(&req, &UpstreamCtx::default())
        .unwrap();
    assert_eq!(
        body,
        json!({"generateContentRequest": {
            "model": "models/gemini-2.5-pro",
            "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
            "systemInstruction": {"parts": [{"text": "Be brief."}]},
            "tools": [{"functionDeclarations": [{
                "name": "lookup", "description": "lookup tool", "parametersJsonSchema": {"type": "object"}
            }]}]
        }})
    );
    // Vertex AI takes the same fields without the wrapper and without a model.
    adapt_for_vertex(&mut body);
    assert_eq!(
        body,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
            "systemInstruction": {"parts": [{"text": "Be brief."}]},
            "tools": [{"functionDeclarations": [{
                "name": "lookup", "description": "lookup tool", "parametersJsonSchema": {"type": "object"}
            }]}]
        })
    );
}

#[test]
fn count_response_round_trip() {
    assert_eq!(
        GeminiCodec.encode_count_response(1234),
        Some(json!({"totalTokens": 1234}))
    );
    assert_eq!(
        GeminiCodec.decode_count_response(&json!({"totalTokens": 1234})),
        Some(1234)
    );
    assert_eq!(
        GeminiCodec.decode_count_response(&json!({
            "totalTokens": 31.0, "cachedContentTokenCount": 10,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 31}]
        })),
        Some(31)
    );
    assert_eq!(
        GeminiCodec.decode_count_response(&json!({"total_tokens": "7"})),
        Some(7)
    );
    assert_eq!(
        GeminiCodec.decode_count_response(&json!({"error": {"code": 400}})),
        None
    );
}
