//! `decode_request`: Gemini `generateContent` bodies -> canonical requests.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_codec_gemini::GeminiCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, FunctionTool, MediaPart, MediaSource, Message, OpaquePart, Part,
    Reasoning, Request, ResponseFormat, Role, Signature, TextPart, Tool, ToolCall, ToolChoice,
    ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ReasoningConfig, Summary};
use switchyard_core::{Codec, CodecError, Protocol, RequestPath};

const MODEL: &str = "gemini-2.5-pro";

fn path() -> RequestPath<'static> {
    RequestPath {
        model: Some(MODEL),
        stream: Some(false),
    }
}

fn decode(body: Value) -> Request {
    GeminiCodec
        .decode_request(&body, &path())
        .expect("request decodes")
}

fn user(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

fn is_minted_call_id(id: &str) -> bool {
    id.strip_prefix("call_")
        .is_some_and(|hex| hex.len() == 24 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

fn tool_calls(message: &Message) -> Vec<&ToolCall> {
    message.tool_calls().collect()
}

fn tool_results(message: &Message) -> Vec<&ToolResult> {
    message.tool_results().collect()
}

// ---------------------------------------------------------------------------
// Plain text, model and stream
// ---------------------------------------------------------------------------

#[test]
fn plain_text_request() {
    let request = decode(json!({"contents": [{"role": "user", "parts": [{"text": "Hello"}]}]}));
    let mut expected = Request::new(MODEL, Protocol::Gemini);
    expected.messages.push(Message::user_text("Hello"));
    assert_eq!(request, expected);
}

#[test]
fn model_and_stream_come_from_the_url() {
    let body = json!({"contents": [user("hi")], "model": "models/ignored", "stream": false});
    let request = GeminiCodec
        .decode_request(
            &body,
            &RequestPath {
                model: Some("gemini-3-flash(high)"),
                stream: Some(true),
            },
        )
        .unwrap();
    assert_eq!(request.model, "gemini-3-flash(high)");
    assert!(request.stream);
    assert!(
        request.extra.is_empty(),
        "model/stream are not extras: {:?}",
        request.extra
    );
}

#[test]
fn body_model_is_the_fallback_when_the_url_has_none() {
    for (written, expected) in [
        ("models/gemini-2.5-flash", "gemini-2.5-flash"),
        ("gemini-2.5-flash", "gemini-2.5-flash"),
    ] {
        let body = json!({"model": written, "contents": [user("hi")]});
        let request = GeminiCodec
            .decode_request(&body, &RequestPath::default())
            .unwrap();
        assert_eq!(request.model, expected);
        assert!(!request.stream);
    }
}

#[test]
fn missing_model_is_an_error() {
    let err = GeminiCodec
        .decode_request(&json!({"contents": [user("hi")]}), &RequestPath::default())
        .unwrap_err();
    assert!(
        matches!(err, CodecError::InvalidRequest { param: Some(ref p), .. } if p == "model"),
        "{err:?}"
    );
}

#[test]
fn contents_must_be_present_and_well_typed() {
    for body in [
        json!({}),
        json!({"contents": null}),
        json!({"contents": 7}),
        json!({"contents": true}),
    ] {
        let err = GeminiCodec.decode_request(&body, &path()).unwrap_err();
        assert!(
            matches!(err, CodecError::InvalidRequest { param: Some(ref p), .. } if p == "contents"),
            "{body}: {err:?}"
        );
    }
    let err = GeminiCodec
        .decode_request(&json!([1, 2]), &path())
        .unwrap_err();
    assert!(matches!(err, CodecError::InvalidRequest { .. }));
}

// ---------------------------------------------------------------------------
// System prompt forms
// ---------------------------------------------------------------------------

#[test]
fn system_instruction_camel_case_with_role() {
    let request = decode(json!({
        "systemInstruction": {"role": "user", "parts": [{"text": "Be brief."}, {"text": "Answer in French."}]},
        "contents": [user("hi")]
    }));
    assert_eq!(
        request.system,
        vec![Part::text("Be brief."), Part::text("Answer in French.")]
    );
    assert_eq!(request.system_text(), "Be brief.\n\nAnswer in French.");
    assert_eq!(request.messages.len(), 1);
}

#[test]
fn system_instruction_snake_case_string_and_single_part() {
    let snake =
        decode(json!({"system_instruction": {"parts": [{"text": "S"}]}, "contents": [user("hi")]}));
    assert_eq!(snake.system, vec![Part::text("S")]);
    let string = decode(json!({"systemInstruction": "Plain string", "contents": [user("hi")]}));
    assert_eq!(string.system, vec![Part::text("Plain string")]);
    let single = decode(
        json!({"systemInstruction": {"parts": {"text": "One part"}}, "contents": [user("hi")]}),
    );
    assert_eq!(single.system, vec![Part::text("One part")]);
    let bare =
        decode(json!({"systemInstruction": {"text": "Bare part"}, "contents": [user("hi")]}));
    assert_eq!(bare.system, vec![Part::text("Bare part")]);
}

#[test]
fn system_instruction_skips_thoughts_and_empty_text() {
    let request = decode(json!({
        "systemInstruction": {"parts": [{"text": "hidden", "thought": true}, {"text": ""}, {"text": "Visible"}]},
        "contents": [user("hi")]
    }));
    assert_eq!(request.system, vec![Part::text("Visible")]);
    let null = decode(json!({"systemInstruction": null, "contents": [user("hi")]}));
    assert!(null.system.is_empty());
}

#[test]
fn system_role_content_is_system_prompt_first_and_system_message_later() {
    let request = decode(json!({"contents": [
        {"role": "system", "parts": [{"text": "Leading"}]},
        user("hi"),
        {"role": "system", "parts": [{"text": "Mid-conversation"}]},
        user("again")
    ]}));
    assert_eq!(request.system, vec![Part::text("Leading")]);
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("hi"),
            Message::new(Role::System, vec![Part::text("Mid-conversation")]),
            Message::user_text("again"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

#[test]
fn inline_image_in_both_spellings() {
    let request = decode(json!({"contents": [{"role": "user", "parts": [
        {"text": "What is this?"},
        {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}},
        {"inline_data": {"mime_type": "image/jpeg", "data": "/9j/4AAQ"}}
    ]}]}));
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::text("What is this?"),
            Part::Image(MediaPart::base64("image/png", "iVBORw0KGgo=")),
            Part::Image(MediaPart::base64("image/jpeg", "/9j/4AAQ")),
        ]
    );
}

#[test]
fn inline_pdf_audio_and_video() {
    let request = decode(json!({"contents": [{"role": "user", "parts": [
        {"inlineData": {"mimeType": "application/pdf", "data": "JVBERi0=", "displayName": "report.pdf"}},
        {"inlineData": {"mimeType": "audio/mpeg", "data": "SUQz"}},
        {"inlineData": {"mimeType": "video/mp4", "data": "AAAAIGZ0"}},
        {"inlineData": {"data": "AAAA"}},
        {"inlineData": {"mimeType": "image/png", "data": ""}}
    ]}]}));
    let mut pdf = MediaPart::base64("application/pdf", "JVBERi0=");
    pdf.filename = Some("report.pdf".into());
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::Document(pdf),
            Part::Audio(MediaPart::base64("audio/mpeg", "SUQz")),
            Part::Document(MediaPart::base64("video/mp4", "AAAAIGZ0")),
            Part::Document(MediaPart::base64("application/octet-stream", "AAAA")),
        ]
    );
}

#[test]
fn file_data_by_url_and_by_provider_handle() {
    let request = decode(json!({"contents": [{"role": "user", "parts": [
        {"fileData": {"mimeType": "image/jpeg", "fileUri": "https://example.com/cat.jpg"}},
        {"file_data": {"mime_type": "application/pdf", "file_uri": "https://generativelanguage.googleapis.com/v1beta/files/abc123"}},
        {"fileData": {"fileUri": "gs://bucket/clip.mp4"}},
        {"fileData": {"fileUri": "https://example.com/photo.png"}},
        {"fileData": {"mimeType": "image/png"}}
    ]}]}));
    let media = |source: MediaSource, media_type: Option<&str>| MediaPart {
        source,
        media_type: media_type.map(str::to_owned),
        filename: None,
        detail: None,
        cache_control: None,
    };
    assert_eq!(
        request.messages[0].parts,
        vec![
            Part::Image(media(
                MediaSource::Url {
                    url: "https://example.com/cat.jpg".into()
                },
                Some("image/jpeg")
            )),
            Part::Document(media(
                MediaSource::FileRef {
                    id: "https://generativelanguage.googleapis.com/v1beta/files/abc123".into()
                },
                Some("application/pdf")
            )),
            Part::Document(media(
                MediaSource::FileRef {
                    id: "gs://bucket/clip.mp4".into()
                },
                None
            )),
            // No MIME type: the extension decides what kind of part it is.
            Part::Image(media(
                MediaSource::Url {
                    url: "https://example.com/photo.png".into()
                },
                None
            )),
        ]
    );
}

// ---------------------------------------------------------------------------
// Tools and tool choice
// ---------------------------------------------------------------------------

#[test]
fn function_declarations_in_the_gemini_dialect_become_json_schema() {
    let request = decode(json!({
        "contents": [user("weather?")],
        "tools": [{"functionDeclarations": [{
            "name": "get_weather",
            "description": "Current weather",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "city": {"type": "STRING", "description": "City"},
                    "unit": {"type": "STRING", "enum": ["c", "f"], "nullable": true},
                    "days": {"type": "ARRAY", "items": {"type": "INTEGER"}},
                    "type": {"type": "STRING"}
                },
                "required": ["city"],
                "propertyOrdering": ["city", "unit", "days"]
            }
        }]}]
    }));
    assert_eq!(
        request.tools,
        vec![Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Current weather".into()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City"},
                    "unit": {"type": ["string", "null"], "enum": ["c", "f"]},
                    "days": {"type": "array", "items": {"type": "integer"}},
                    "type": {"type": "string"}
                },
                "required": ["city"]
            }),
            strict: None,
            cache_control: None,
        })]
    );
}

#[test]
fn parameters_json_schema_is_kept_verbatim_and_missing_parameters_are_null() {
    let schema = json!({"type": "object", "properties": {"q": {"type": ["string", "null"]}}, "additionalProperties": false});
    let request = decode(json!({
        "contents": [user("go")],
        "tools": [
            {"function_declarations": [
                {"name": "search", "parametersJsonSchema": schema, "parameters": {"type": "OBJECT"}},
                {"name": "ping"},
                {"description": "nameless declarations are skipped"}
            ]},
            {"functionDeclarations": [{"name": "snake", "parameters_json_schema": {"type": "object"}}]}
        ]
    }));
    let names: Vec<_> = request
        .tools
        .iter()
        .map(|t| t.name().unwrap().to_string())
        .collect();
    assert_eq!(names, ["search", "ping", "snake"]);
    let Tool::Function(search) = &request.tools[0] else {
        panic!()
    };
    assert_eq!(search.parameters, schema);
    let Tool::Function(ping) = &request.tools[1] else {
        panic!()
    };
    assert_eq!(ping.parameters, Value::Null);
    assert_eq!(ping.description, None);
}

#[test]
fn builtin_tools_keep_their_declaration() {
    let request = decode(json!({
        "contents": [user("search")],
        "tools": [
            {"googleSearch": {}},
            {"google_search": {"timeRangeFilter": {"startTime": "2025-01-01T00:00:00Z"}}},
            {"googleSearchRetrieval": {"dynamicRetrievalConfig": {"mode": "MODE_DYNAMIC"}}},
            {"codeExecution": {}},
            {"urlContext": {}},
            {"googleMaps": {"enableWidget": true}}
        ]
    }));
    let builtin = |kind: BuiltinKind, raw: Value| {
        Tool::Builtin(BuiltinTool {
            kind,
            origin: Protocol::Gemini,
            raw,
        })
    };
    assert_eq!(
        request.tools,
        vec![
            builtin(BuiltinKind::WebSearch, json!({"googleSearch": {}})),
            builtin(
                BuiltinKind::WebSearch,
                json!({"google_search": {"timeRangeFilter": {"startTime": "2025-01-01T00:00:00Z"}}})
            ),
            builtin(
                BuiltinKind::WebSearch,
                json!({"googleSearchRetrieval": {"dynamicRetrievalConfig": {"mode": "MODE_DYNAMIC"}}})
            ),
            builtin(BuiltinKind::CodeExecution, json!({"codeExecution": {}})),
            builtin(BuiltinKind::WebFetch, json!({"urlContext": {}})),
            builtin(
                BuiltinKind::Other("googleMaps".into()),
                json!({"googleMaps": {"enableWidget": true}})
            ),
        ]
    );
}

#[test]
fn one_tool_object_may_mix_functions_and_builtins() {
    let request = decode(json!({
        "contents": [user("go")],
        "tools": {"functionDeclarations": [{"name": "f"}], "codeExecution": {}}
    }));
    assert_eq!(request.tools.len(), 2);
    assert_eq!(request.tools[0].name(), Some("f"));
    assert!(matches!(&request.tools[1], Tool::Builtin(b) if b.kind == BuiltinKind::CodeExecution));
}

fn choice_of(tool_config: Value) -> (Option<ToolChoice>, Option<Value>) {
    let request = decode(json!({"contents": [user("go")], "toolConfig": tool_config}));
    (
        request.tool_choice,
        request.extra.get("toolConfig").cloned(),
    )
}

#[test]
fn tool_config_modes() {
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {"mode": "AUTO"}})),
        (Some(ToolChoice::Auto), None)
    );
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {"mode": "NONE"}})),
        (Some(ToolChoice::None), None)
    );
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {"mode": "ANY"}})),
        (Some(ToolChoice::Required), None)
    );
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {"mode": "any"}})),
        (Some(ToolChoice::Required), None)
    );
    assert_eq!(
        choice_of(
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}})
        ),
        (
            Some(ToolChoice::Tool {
                name: "get_weather".into()
            }),
            None
        )
    );
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {}})),
        (None, None)
    );
    assert_eq!(
        choice_of(json!({"functionCallingConfig": {"mode": "MODE_UNSPECIFIED"}})),
        (None, None)
    );
}

#[test]
fn tool_config_that_the_canonical_choice_cannot_express_is_kept_raw() {
    let several =
        json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["a", "b"]}});
    assert_eq!(
        choice_of(several.clone()),
        (Some(ToolChoice::Required), Some(several))
    );
    let validated = json!({"functionCallingConfig": {"mode": "VALIDATED"}});
    assert_eq!(
        choice_of(validated.clone()),
        (Some(ToolChoice::Auto), Some(validated))
    );
    let retrieval = json!({"functionCallingConfig": {"mode": "AUTO"}, "retrievalConfig": {"languageCode": "en"}});
    assert_eq!(
        choice_of(retrieval.clone()),
        (Some(ToolChoice::Auto), Some(retrieval))
    );
}

#[test]
fn tool_config_snake_case() {
    let request = decode(json!({
        "contents": [user("go")],
        "tool_config": {"function_calling_config": {"mode": "ANY", "allowed_function_names": ["f"]}}
    }));
    assert_eq!(
        request.tool_choice,
        Some(ToolChoice::Tool { name: "f".into() })
    );
}

// ---------------------------------------------------------------------------
// Multi-turn tool conversations
// ---------------------------------------------------------------------------

fn tool_conversation() -> Value {
    json!({"contents": [
        {"role": "user", "parts": [{"text": "Weather in Paris and Rome?"}]},
        {"role": "model", "parts": [
            {"text": "Let me check."},
            {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}, "thoughtSignature": "Q2lRQlNpZw=="},
            {"functionCall": {"name": "get_weather", "args": {"city": "Rome"}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "get_weather", "response": {"result": "18C"}}},
            {"functionResponse": {"name": "get_weather", "response": {"temp": 24, "unit": "C"}}}
        ]},
        {"role": "model", "parts": [{"text": "Paris 18C, Rome 24C."}]},
        {"role": "user", "parts": [{"text": "Thanks"}]}
    ]})
}

#[test]
fn multi_turn_tool_conversation() {
    let request = decode(tool_conversation());
    assert_eq!(request.messages.len(), 5);
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );

    let calls = tool_calls(&request.messages[1]);
    assert_eq!(calls.len(), 2);
    assert_eq!(request.messages[1].parts[0], Part::text("Let me check."));
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, r#"{"city":"Paris"}"#);
    assert_eq!(
        calls[0].signature,
        Some(Signature::new(Protocol::Gemini, "Q2lRQlNpZw=="))
    );
    assert_eq!(calls[1].arguments, r#"{"city":"Rome"}"#);
    assert_eq!(calls[1].signature, None);
    assert!(is_minted_call_id(&calls[0].id), "{}", calls[0].id);
    assert!(is_minted_call_id(&calls[1].id));
    assert_ne!(calls[0].id, calls[1].id);

    // Responses are paired with the calls of the same name, in order.
    let results = tool_results(&request.messages[2]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].call_id, calls[0].id);
    assert_eq!(results[1].call_id, calls[1].id);
    assert_eq!(results[0].name.as_deref(), Some("get_weather"));
    assert_eq!(results[0].content, vec![Part::text("18C")]);
    assert!(!results[0].is_error);
    assert_eq!(
        results[1].content,
        vec![Part::text(r#"{"temp":24,"unit":"C"}"#)]
    );
    assert_eq!(
        request.tool_name_for_call(&results[1].call_id),
        Some("get_weather")
    );
    assert!(request.has_tool_traffic());
}

#[test]
fn minted_call_ids_are_stable_across_replays() {
    let first = decode(tool_conversation());
    let second = decode(tool_conversation());
    assert_eq!(first, second);
}

#[test]
fn explicit_call_ids_are_kept_in_every_spelling() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"functionCall": {"id": "fc_1", "name": "a", "args": {}}},
            {"functionCall": {"call_id": "fc_2", "name": "b", "args": {}}},
            {"functionCall": {"callId": "fc_3", "name": "c", "args": {}}},
            {"functionCall": {"id": "fc_4", "name": "a", "args": {}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"id": "fc_4", "name": "a", "response": {"result": "fourth"}}},
            {"functionResponse": {"name": "a", "response": {"result": "first"}}},
            {"functionResponse": {"callId": "fc_3", "name": "c", "response": {"result": "third"}}},
            {"functionResponse": {"name": "b", "response": {"result": "second"}}}
        ]}
    ]}));
    let call_ids: Vec<_> = tool_calls(&request.messages[1])
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(call_ids, ["fc_1", "fc_2", "fc_3", "fc_4"]);
    // An explicit response id is used as is and is not handed out again; an
    // id-less response inherits the id of the pending call with its name.
    let result_ids: Vec<_> = tool_results(&request.messages[2])
        .iter()
        .map(|r| r.call_id.clone())
        .collect();
    assert_eq!(result_ids, ["fc_4", "fc_1", "fc_3", "fc_2"]);
}

#[test]
fn responses_are_matched_per_name_in_call_order() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"functionCall": {"name": "A", "args": {"n": 1}}},
            {"functionCall": {"name": "B", "args": {"n": 1}}},
            {"functionCall": {"name": "A", "args": {"n": 2}}},
            {"functionCall": {"name": "B", "args": {"n": 2}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "B", "response": {"result": "b1"}}},
            {"functionResponse": {"name": "A", "response": {"result": "a1"}}},
            {"functionResponse": {"name": "B", "response": {"result": "b2"}}},
            {"functionResponse": {"name": "A", "response": {"result": "a2"}}}
        ]}
    ]}));
    let calls: Vec<_> = tool_calls(&request.messages[1])
        .iter()
        .map(|c| c.id.clone())
        .collect();
    let results: Vec<_> = tool_results(&request.messages[2])
        .iter()
        .map(|r| r.call_id.clone())
        .collect();
    assert_eq!(
        results,
        [
            calls[1].clone(),
            calls[0].clone(),
            calls[3].clone(),
            calls[2].clone()
        ]
    );
    let distinct: std::collections::HashSet<_> = calls.iter().collect();
    assert_eq!(distinct.len(), 4);
}

#[test]
fn identical_calls_in_one_turn_get_distinct_ids() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"functionCall": {"name": "roll", "args": {}}},
            {"functionCall": {"name": "roll", "args": {}}}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "roll", "response": {"result": "3"}}},
            {"functionResponse": {"name": "roll", "response": {"result": "5"}}},
            {"functionResponse": {"name": "roll", "response": {"result": "extra"}}}
        ]}
    ]}));
    let calls = tool_calls(&request.messages[1]);
    let results = tool_results(&request.messages[2]);
    assert_ne!(calls[0].id, calls[1].id);
    assert_eq!(results[0].call_id, calls[0].id);
    assert_eq!(results[1].call_id, calls[1].id);
    // One response too many: it gets an id of its own, never a reused one.
    assert!(is_minted_call_id(&results[2].call_id));
    assert!(results[2].call_id != calls[0].id && results[2].call_id != calls[1].id);
}

#[test]
fn a_later_round_does_not_inherit_unanswered_calls_of_an_earlier_one() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"functionCall": {"name": "f", "args": {"round": 1}}},
            {"functionCall": {"name": "g", "args": {"round": 1}}}
        ]},
        {"role": "user", "parts": [{"functionResponse": {"name": "g", "response": {"result": "g1"}}}]},
        {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {"round": 2}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": {"result": "f2"}}}]}
    ]}));
    let second_round = tool_calls(&request.messages[3]);
    let answer = tool_results(&request.messages[4]);
    assert_eq!(answer[0].call_id, second_round[0].id);
}

#[test]
fn orphan_and_nameless_function_responses() {
    let orphan = json!({"contents": [
        {"role": "user", "parts": [{"functionResponse": {"name": "lost", "response": {"result": "?"}}}]}
    ]});
    let first = decode(orphan.clone());
    let again = decode(orphan);
    let result = tool_results(&first.messages[0])[0].clone();
    assert!(is_minted_call_id(&result.call_id));
    assert_eq!(first, again, "fallback ids are deterministic");

    // A response without a name answers the oldest pending call.
    let nameless = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "", "response": {"result": "ok"}}}]}
    ]}));
    let call = tool_calls(&nameless.messages[1])[0].clone();
    let result = tool_results(&nameless.messages[2])[0].clone();
    assert_eq!(result.call_id, call.id);
    assert_eq!(result.name.as_deref(), Some("lookup"));
}

#[test]
fn legacy_function_role_and_misplaced_parts() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}}}]},
        {"role": "function", "parts": [{"functionResponse": {"name": "f", "response": {"result": "done"}}}]},
        // A function call in a user turn is not a thing; it is ignored.
        {"role": "user", "parts": [{"functionCall": {"name": "g", "args": {}}}, {"text": "next"}]},
        // A function response inside a model turn is moved to a user turn.
        {"role": "model", "parts": [{"text": "hm"}, {"functionResponse": {"name": "h", "response": {"result": "x"}}}]}
    ]}));
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
    assert_eq!(tool_results(&request.messages[2]).len(), 1);
    assert_eq!(request.messages[3].parts, vec![Part::text("next")]);
    assert_eq!(request.messages[4].parts, vec![Part::text("hm")]);
    assert_eq!(
        tool_results(&request.messages[5])[0].name.as_deref(),
        Some("h")
    );
}

#[test]
fn function_response_payload_forms() {
    let request = decode(json!({"contents": [{"role": "user", "parts": [
        {"functionResponse": {"id": "1", "name": "f", "response": {"result": "plain"}}},
        {"functionResponse": {"id": "2", "name": "f", "response": {"output": {"rows": 3}}}},
        {"functionResponse": {"id": "3", "name": "f", "response": {"error": "boom"}}},
        {"functionResponse": {"id": "4", "name": "f", "response": {"a": 1, "b": [true]}}},
        {"functionResponse": {"id": "5", "name": "f", "response": {}}},
        {"functionResponse": {"id": "6", "name": "f"}},
        {"functionResponse": {"id": "7", "name": "f", "response": {"result": "see image"},
            "parts": [{"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]}},
        {"function_response": {"id": "8", "name": "f", "response": {"content": "kept as json"}}}
    ]}]}));
    let results = tool_results(&request.messages[0]);
    let summary: Vec<(String, bool)> = results.iter().map(|r| (r.text(), r.is_error)).collect();
    assert_eq!(
        summary,
        vec![
            ("plain".to_string(), false),
            (r#"{"rows":3}"#.to_string(), false),
            ("boom".to_string(), true),
            (r#"{"a":1,"b":[true]}"#.to_string(), false),
            ("{}".to_string(), false),
            (String::new(), false),
            ("see image".to_string(), false),
            (r#"{"content":"kept as json"}"#.to_string(), false),
        ]
    );
    assert!(results[5].content.is_empty());
    assert_eq!(
        results[6].content,
        vec![
            Part::text("see image"),
            Part::Image(MediaPart::base64("image/png", "AAAA"))
        ]
    );
}

#[test]
fn function_call_argument_forms() {
    let request = decode(json!({"contents": [{"role": "model", "parts": [
        {"functionCall": {"id": "1", "name": "f", "args": {"a": {"b": [1, 2]}}}},
        {"functionCall": {"id": "2", "name": "f"}},
        {"functionCall": {"id": "3", "name": "f", "args": null}},
        {"function_call": {"id": "4", "name": "f", "args": {}}}
    ]}]}));
    let args: Vec<_> = tool_calls(&request.messages[0])
        .iter()
        .map(|c| c.arguments.clone())
        .collect();
    assert_eq!(args, [r#"{"a":{"b":[1,2]}}"#, "{}", "{}", "{}"]);
}

// ---------------------------------------------------------------------------
// Thoughts and signatures
// ---------------------------------------------------------------------------

#[test]
fn thought_parts_and_signatures() {
    let request = decode(json!({"contents": [
        user("think"),
        {"role": "model", "parts": [
            {"text": "Considering options…", "thought": true, "thoughtSignature": "U0lHLXRob3VnaHQ="},
            {"text": "The answer is 4.", "thought_signature": "U0lHLXRleHQ="},
            {"text": "No signature here."}
        ]}
    ]}));
    assert_eq!(
        request.messages[1].parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "Considering options…".into(),
                signature: Some(Signature::new(Protocol::Gemini, "U0lHLXRob3VnaHQ=")),
                redacted: false,
            }),
            Part::Text(TextPart {
                text: "The answer is 4.".into(),
                signature: Some(Signature::new(Protocol::Gemini, "U0lHLXRleHQ=")),
                ..TextPart::default()
            }),
            Part::text("No signature here."),
        ]
    );
}

#[test]
fn wrapped_foreign_signatures_keep_their_origin() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"text": "claude thought", "thought": true, "thoughtSignature": "sy1.a.RXJBQ2tnRQ=="},
            {"functionCall": {"name": "f", "args": {}}, "thoughtSignature": "sy1.r.gAAAAABencrypted"}
        ]}
    ]}));
    let Part::Reasoning(reasoning) = &request.messages[1].parts[0] else {
        panic!()
    };
    assert_eq!(
        reasoning.signature,
        Some(Signature::new(Protocol::Anthropic, "RXJBQ2tnRQ=="))
    );
    let call = tool_calls(&request.messages[1])[0];
    assert_eq!(
        call.signature,
        Some(Signature::new(
            Protocol::OpenaiResponses,
            "gAAAAABencrypted"
        ))
    );
}

#[test]
fn bypass_literals_are_not_signatures() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"functionCall": {"name": "f", "args": {}}, "thoughtSignature": "skip_thought_signature_validator"},
            {"functionCall": {"name": "g", "args": {}}, "thoughtSignature": "context_engineering_is_the_way_to_go"},
            {"functionCall": {"name": "h", "args": {}, "thoughtSignature": "bmVzdGVk"}},
            {"functionCall": {"name": "i", "args": {}}, "extra_content": {"google": {"thought_signature": "ZXh0cmE="}}}
        ]}
    ]}));
    let signatures: Vec<_> = tool_calls(&request.messages[1])
        .iter()
        .map(|c| c.signature.clone())
        .collect();
    assert_eq!(
        signatures,
        vec![
            None,
            None,
            Some(Signature::new(Protocol::Gemini, "bmVzdGVk")),
            Some(Signature::new(Protocol::Gemini, "ZXh0cmE=")),
        ]
    );
}

#[test]
fn a_part_that_is_only_a_signature_is_text_less_reasoning() {
    let request = decode(json!({"contents": [
        user("go"),
        {"role": "model", "parts": [
            {"text": "thinking", "thought": true},
            {"thoughtSignature": "YXR0YWNoZWQ="},
            {"text": "Final answer."},
            {"text": "", "thoughtSignature": "dHJhaWxpbmc="}
        ]}
    ]}));
    assert_eq!(
        request.messages[1].parts,
        vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "thinking".into(),
                signature: Some(Signature::new(Protocol::Gemini, "YXR0YWNoZWQ=")),
                redacted: false,
            }),
            Part::text("Final answer."),
            Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(Signature::new(Protocol::Gemini, "dHJhaWxpbmc=")),
                redacted: false,
            }),
        ]
    );
}

#[test]
fn thoughts_in_a_user_turn_are_dropped() {
    let request = decode(json!({"contents": [
        {"role": "user", "parts": [{"text": "not yours", "thought": true}, {"text": "hello"}]}
    ]}));
    assert_eq!(request.messages, vec![Message::user_text("hello")]);
}

#[test]
fn unknown_parts_are_kept_opaque() {
    let code = json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}});
    let result = json!({"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1\n"}});
    let request = decode(json!({"contents": [
        user("run"),
        {"role": "model", "parts": [code.clone(), result.clone(), {"text": "It printed 1."}]}
    ]}));
    let opaque = |raw: Value| {
        Part::Opaque(OpaquePart {
            origin: Protocol::Gemini,
            raw,
        })
    };
    assert_eq!(
        request.messages[1].parts,
        vec![opaque(code), opaque(result), Part::text("It printed 1.")]
    );
}

// ---------------------------------------------------------------------------
// Reasoning settings
// ---------------------------------------------------------------------------

fn reasoning_of(thinking_config: Value) -> Option<ReasoningConfig> {
    decode(
        json!({"contents": [user("hi")], "generationConfig": {"thinkingConfig": thinking_config}}),
    )
    .reasoning
}

#[test]
fn thinking_budget_spellings() {
    let depth = |d| Some(ReasoningConfig::with_depth(d));
    assert_eq!(
        reasoning_of(json!({"thinkingBudget": 0})),
        depth(Depth::Off)
    );
    assert_eq!(
        reasoning_of(json!({"thinkingBudget": -1})),
        depth(Depth::Auto)
    );
    assert_eq!(
        reasoning_of(json!({"thinkingBudget": 8192})),
        depth(Depth::Budget(8192))
    );
    assert_eq!(
        reasoning_of(json!({"thinking_budget": 1024})),
        depth(Depth::Budget(1024))
    );
    assert_eq!(
        reasoning_of(json!({"thinkingBudget": 2048.0})),
        depth(Depth::Budget(2048))
    );
    assert_eq!(reasoning_of(json!({"thinkingBudget": -5})), None);
    assert_eq!(reasoning_of(json!({})), None);
}

#[test]
fn thinking_level_spellings() {
    let depth = |d| Some(ReasoningConfig::with_depth(d));
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "low"})),
        depth(Depth::Level(Effort::Low))
    );
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "HIGH"})),
        depth(Depth::Level(Effort::High))
    );
    assert_eq!(
        reasoning_of(json!({"thinking_level": "minimal"})),
        depth(Depth::Level(Effort::Minimal))
    );
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "medium"})),
        depth(Depth::Level(Effort::Medium))
    );
    // A level wins over a budget when both are present.
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "high", "thinkingBudget": 128})),
        depth(Depth::Level(Effort::High))
    );
    // An unknown level falls back to the budget, then to nothing.
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "THINKING_LEVEL_UNSPECIFIED", "thinkingBudget": 512})),
        depth(Depth::Budget(512))
    );
    assert_eq!(
        reasoning_of(json!({"thinkingLevel": "THINKING_LEVEL_UNSPECIFIED"})),
        None
    );
}

#[test]
fn include_thoughts_is_the_summary_intent() {
    assert_eq!(
        reasoning_of(json!({"includeThoughts": true})),
        Some(ReasoningConfig {
            depth: None,
            summary: Some(Summary::Auto)
        })
    );
    assert_eq!(
        reasoning_of(json!({"include_thoughts": false, "thinkingBudget": 1024})),
        Some(ReasoningConfig {
            depth: Some(Depth::Budget(1024)),
            summary: Some(Summary::Off)
        })
    );
    // Only a JSON boolean counts.
    assert_eq!(reasoning_of(json!({"includeThoughts": "true"})), None);
}

#[test]
fn snake_case_generation_config_is_read_too() {
    let request = decode(json!({
        "contents": [user("hi")],
        "generation_config": {"thinking_config": {"thinking_level": "low", "include_thoughts": true}, "max_output_tokens": 100}
    }));
    assert_eq!(
        request.reasoning,
        Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Auto)
        })
    );
    assert_eq!(request.max_output_tokens, Some(100));
}

// ---------------------------------------------------------------------------
// Structured output and sampling
// ---------------------------------------------------------------------------

fn format_of(generation_config: Value) -> (Option<ResponseFormat>, Option<Value>) {
    let request = decode(json!({"contents": [user("hi")], "generationConfig": generation_config}));
    (
        request.response_format,
        request.extra.get("generationConfig").cloned(),
    )
}

#[test]
fn json_mode_and_schemas() {
    assert_eq!(
        format_of(json!({"responseMimeType": "application/json"})),
        (Some(ResponseFormat::JsonObject), None)
    );
    let json_schema =
        json!({"type": "object", "properties": {"n": {"type": ["integer", "null"]}}, "$defs": {}});
    assert_eq!(
        format_of(
            json!({"responseMimeType": "application/json", "responseJsonSchema": json_schema})
        ),
        (
            Some(ResponseFormat::JsonSchema {
                name: None,
                description: None,
                schema: json_schema.clone(),
                strict: None
            }),
            None
        )
    );
    assert_eq!(
        format_of(
            json!({"response_mime_type": "application/json", "response_schema": {
                "type": "ARRAY", "items": {"type": "OBJECT", "properties": {"n": {"type": "NUMBER", "nullable": true}}}
            }})
        ),
        (
            Some(ResponseFormat::JsonSchema {
                name: None,
                description: None,
                schema: json!({
                    "type": "array",
                    "items": {"type": "object", "properties": {"n": {"type": ["number", "null"]}}}
                }),
                strict: None
            }),
            None
        )
    );
    assert_eq!(
        format_of(json!({"responseMimeType": "text/plain"})),
        (None, None)
    );
    assert_eq!(format_of(json!({})), (None, None));
    assert_eq!(
        format_of(json!({"responseMimeType": "text/x.enum"})),
        (None, Some(json!({"responseMimeType": "text/x.enum"})))
    );
}

#[test]
fn sampling_parameters() {
    let request = decode(json!({
        "contents": [user("hi")],
        "generationConfig": {
            "temperature": 0.7,
            "topP": 0.95,
            "topK": 40.0,
            "maxOutputTokens": 2048,
            "stopSequences": ["END", "STOP"],
            "candidateCount": 2,
            "seed": 42,
            "presencePenalty": 0.5,
            "frequencyPenalty": -0.25
        }
    }));
    assert_eq!(request.temperature, Some(0.7));
    assert_eq!(request.top_p, Some(0.95));
    assert_eq!(request.top_k, Some(40));
    assert_eq!(request.max_output_tokens, Some(2048));
    assert_eq!(request.stop, ["END", "STOP"]);
    assert_eq!(request.candidate_count, Some(2));
    assert_eq!(request.seed, Some(42));
    assert_eq!(request.presence_penalty, Some(0.5));
    assert_eq!(request.frequency_penalty, Some(-0.25));
    assert!(request.extra.is_empty());
}

#[test]
fn sampling_parameters_snake_case_and_wrong_types() {
    let request = decode(json!({
        "contents": [user("hi")],
        "generationConfig": {
            "top_p": 0.5,
            "top_k": 3,
            "max_output_tokens": "512",
            "stop_sequences": "single",
            "candidate_count": 1,
            "presence_penalty": 1,
            "frequency_penalty": null,
            "temperature": "hot",
            "seed": 12.0
        }
    }));
    assert_eq!(request.top_p, Some(0.5));
    assert_eq!(request.top_k, Some(3));
    assert_eq!(request.max_output_tokens, Some(512));
    assert_eq!(request.stop, ["single"]);
    assert_eq!(request.candidate_count, Some(1));
    assert_eq!(request.presence_penalty, Some(1.0));
    assert_eq!(request.frequency_penalty, None);
    assert_eq!(request.temperature, None);
    assert_eq!(request.seed, Some(12));
}

#[test]
fn generation_config_without_canonical_slot_is_kept_for_gemini_upstreams() {
    let request = decode(json!({
        "contents": [user("draw")],
        "generationConfig": {
            "temperature": 1,
            "responseModalities": ["TEXT", "IMAGE"],
            "imageConfig": {"aspectRatio": "16:9"},
            "mediaResolution": "MEDIA_RESOLUTION_LOW",
            "thinkingConfig": {"thinkingBudget": 0}
        }
    }));
    assert_eq!(
        request.extra.get("generationConfig"),
        Some(&json!({
            "responseModalities": ["TEXT", "IMAGE"],
            "imageConfig": {"aspectRatio": "16:9"},
            "mediaResolution": "MEDIA_RESOLUTION_LOW"
        }))
    );
}

// ---------------------------------------------------------------------------
// Other top-level fields
// ---------------------------------------------------------------------------

#[test]
fn safety_settings_cached_content_labels_and_unknown_fields() {
    let safety = json!([{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}]);
    let request = decode(json!({
        "contents": [user("hi")],
        "safetySettings": safety,
        "cachedContent": "cachedContents/abc",
        "labels": {"team": "search", "env": "prod"},
        "serviceTier": "flex",
        "store": false,
        "modelArmorConfig": {"promptTemplateName": "t"}
    }));
    assert_eq!(request.extra.get("safetySettings"), Some(&safety));
    assert_eq!(
        request.extra.get("cachedContent"),
        Some(&json!("cachedContents/abc"))
    );
    assert_eq!(
        request.extra.get("modelArmorConfig"),
        Some(&json!({"promptTemplateName": "t"}))
    );
    assert_eq!(
        request.metadata.map(Value::Object),
        Some(json!({"team": "search", "env": "prod"}))
    );
    assert_eq!(request.service_tier.as_deref(), Some("flex"));
    assert_eq!(request.store, Some(false));

    let snake = decode(json!({
        "contents": [user("hi")],
        "safety_settings": [],
        "cached_content": "cachedContents/x",
        "service_tier": "priority"
    }));
    assert_eq!(snake.extra.get("safetySettings"), Some(&json!([])));
    assert_eq!(
        snake.extra.get("cachedContent"),
        Some(&json!("cachedContents/x"))
    );
    assert_eq!(snake.service_tier.as_deref(), Some("priority"));
}

// ---------------------------------------------------------------------------
// Odd but legal inputs
// ---------------------------------------------------------------------------

#[test]
fn missing_role_is_a_user_turn_unless_it_calls_a_function() {
    let request = decode(json!({"contents": [
        {"parts": [{"text": "no role"}]},
        {"parts": [{"functionCall": {"name": "f", "args": {}}}]},
        {"role": "", "parts": [{"functionResponse": {"name": "f", "response": {"result": "r"}}}]},
        {"role": "MODEL", "parts": [{"text": "upper-case role, as Vertex samples send it"}]},
        {"role": "USER", "parts": [{"text": "ok"}]}
    ]}));
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
}

#[test]
fn contents_as_a_single_object_or_string_and_parts_as_a_single_object() {
    let object = decode(json!({"contents": {"role": "user", "parts": {"text": "single"}}}));
    assert_eq!(object.messages, vec![Message::user_text("single")]);
    let string = decode(json!({"contents": "bare string"}));
    assert_eq!(string.messages, vec![Message::user_text("bare string")]);
    let mixed = decode(json!({"contents": ["first", {"role": "model", "parts": ["second"]}]}));
    assert_eq!(
        mixed.messages,
        vec![
            Message::user_text("first"),
            Message::assistant_text("second")
        ]
    );
}

#[test]
fn empty_content_and_null_fields_are_tolerated() {
    let request = decode(json!({
        "contents": [
            {"role": "user", "parts": []},
            {"role": "user"},
            {"role": "user", "parts": [{"text": ""}]},
            {"role": "user", "parts": null},
            {"role": "user", "parts": [null, 7, {}, {"text": null}, {"text": "kept"}]},
            null
        ],
        "tools": null,
        "toolConfig": null,
        "generationConfig": null,
        "systemInstruction": null,
        "safetySettings": null,
        "cachedContent": null
    }));
    assert_eq!(request.messages, vec![Message::user_text("kept")]);
    assert!(request.tools.is_empty());
    assert_eq!(request.tool_choice, None);
    assert!(request.extra.is_empty());

    let empty = decode(json!({"contents": []}));
    assert!(empty.messages.is_empty());
}

#[test]
fn consecutive_same_role_contents_stay_separate_messages() {
    let request = decode(
        json!({"contents": [user("one"), user("two"), {"role": "model", "parts": [{"text": "three"}]}]}),
    );
    assert_eq!(
        request.messages,
        vec![
            Message::user_text("one"),
            Message::user_text("two"),
            Message::assistant_text("three")
        ]
    );
}
