//! Regression tests for the defects found in review (the reviewer's own
//! evidence lives in `review_*.rs`) and for their relatives found while
//! fixing them:
//!
//! * the schema cleaner is bounded in size and depth, and its invariants hold
//!   after merges (`sanitize_schema`);
//! * Gemini's protobuf-JSON spellings are normalised on the way in (numeric
//!   constraints as strings, URL-safe base64);
//! * tool results are named after the call of their own turn;
//! * signatures of other vendors are valid `bytes` for a Gemini client, come
//!   back with their origin, and never reach a Gemini upstream;
//! * the wrapped `countTokens` form is read and patched like the plain one;
//! * no MIME type is invented for a Gemini client's `fileData`.

use pretty_assertions::assert_eq;
use serde_json::{Map, Value, json};
use switchyard_codec_gemini::{
    GeminiCodec, SKIP_SIGNATURE, adapt_for_vertex, sanitize_schema, sanitize_schema_legacy,
};
use switchyard_core::ir::{
    MediaPart, Message, Part, Reasoning, Request, Response, ResponseFormat, Role, Signature, Tool,
    ToolCall, ToolCallKind, ToolResult,
};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx};

fn path() -> RequestPath<'static> {
    RequestPath {
        model: Some("gemini-2.5-pro"),
        stream: Some(false),
    }
}

fn decode(body: Value) -> Request {
    GeminiCodec
        .decode_request(&body, &path())
        .expect("request decodes")
}

fn encode(request: &Request) -> Value {
    GeminiCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("request encodes")
}

fn prepare(mut body: Value) -> Value {
    GeminiCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());
    body
}

fn depth_of(value: &Value) -> usize {
    match value {
        Value::Array(list) => 1 + list.iter().map(depth_of).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(depth_of).max().unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Schema cleaner: bounded output
// ---------------------------------------------------------------------------

#[test]
fn references_within_the_allowance_are_inlined_and_the_rest_become_stubs() {
    let big = "d".repeat(64 * 1024);
    let mut properties = Map::new();
    for i in 0..600 {
        properties.insert(format!("p{i:03}"), json!({"$ref": "#/$defs/Shared"}));
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "$defs": {"Shared": {"type": "string", "description": big}}
    });
    let cleaned = sanitize_schema(&schema);
    // The first references are inlined in document order ...
    assert_eq!(
        cleaned["properties"]["p000"],
        json!({"type": "string", "description": big})
    );
    // ... and once the copies add up to a few times the schema's own size,
    // the remaining ones degrade to the stub form.
    assert_eq!(
        cleaned["properties"]["p599"],
        json!({"type": "object", "description": "See: Shared"})
    );
    let size_in = schema.to_string().len();
    let size_out = cleaned.to_string().len();
    assert!(size_out <= size_in * 6, "{size_in} bytes became {size_out}");
    assert_eq!(sanitize_schema(&cleaned), cleaned);
    // The strict dialect shares the limit.
    assert!(sanitize_schema_legacy(&schema).to_string().len() <= size_in * 6);
}

#[test]
fn nested_fan_out_cannot_grow_exponentially() {
    // Every level references the next one twice: 2^24 copies if inlined
    // without a limit, from a schema of a couple of kilobytes.
    let mut defs = Map::new();
    for level in 0..24 {
        let next = format!("#/$defs/L{}", level + 1);
        defs.insert(
            format!("L{level}"),
            json!({"type": "object", "properties": {"a": {"$ref": next}, "b": {"$ref": next}}}),
        );
    }
    defs.insert("L24".into(), json!({"type": "string"}));
    let schema = json!({"$ref": "#/$defs/L0", "$defs": defs});
    let cleaned = sanitize_schema(&schema);
    let size_in = schema.to_string().len();
    let size_out = cleaned.to_string().len();
    // The copies are limited to a few times the input; the stubs that stand
    // in for the references left over are a little longer than those were.
    assert!(
        size_out <= size_in * 12,
        "{size_in} bytes became {size_out}"
    );
    // The top of the tree is still inlined.
    assert_eq!(cleaned["type"], json!("object"));
    assert_eq!(cleaned["properties"]["a"]["type"], json!("object"));
    assert!(cleaned["properties"]["a"]["properties"]["a"].is_object());
}

#[test]
fn chained_references_do_not_multiply_the_nesting_depth() {
    // Thirty definitions, each `wraps` objects deep, each ending in a
    // reference to the next. Inlined naively the result is thousands of
    // levels deep: more than a JSON parser reads and enough to exhaust the
    // stack. With 60 wraps the input itself is as deep as a parser accepts.
    for (wraps, first_stub) in [(30, "See: D1"), (60, "See: D0")] {
        let mut defs = Map::new();
        for index in 0..30 {
            let mut node = json!({"$ref": format!("#/$defs/D{}", index + 1)});
            for _ in 0..wraps {
                node = json!({"type": "object", "properties": {"n": node}});
            }
            defs.insert(format!("D{index}"), node);
        }
        defs.insert("D30".into(), json!({"type": "string"}));
        let schema = json!({
            "type": "object",
            "properties": {"start": {"$ref": "#/$defs/D0"}},
            "$defs": defs
        });
        assert_eq!(depth_of(&schema), 2 * wraps + 3);

        for cleaned in [sanitize_schema(&schema), sanitize_schema_legacy(&schema)] {
            assert!(
                depth_of(&cleaned) <= 64,
                "depth {} after cleaning",
                depth_of(&cleaned)
            );
            // What goes upstream must be readable by a JSON parser.
            let text = cleaned.to_string();
            serde_json::from_str::<Value>(&text).expect("the cleaned schema parses");
            // A definition is inlined where it fits; where it would exceed
            // the depth limit there is a stub.
            assert_eq!(cleaned["properties"]["start"]["type"], json!("object"));
            assert!(text.contains(first_stub), "{text}");
            assert_eq!(text.matches("See: ").count(), 1, "{text}");
        }
    }
}

#[test]
fn a_self_referencing_root_is_inlined_once() {
    let schema = json!({
        "type": "object",
        "properties": {"child": {"$ref": "#"}, "name": {"type": "string"}}
    });
    assert_eq!(
        sanitize_schema(&schema),
        json!({
            "type": "object",
            "properties": {
                "child": {
                    "type": "object",
                    "properties": {
                        "child": {"type": "object", "description": "See: #"},
                        "name": {"type": "string"}
                    }
                },
                "name": {"type": "string"}
            }
        })
    );
}

// ---------------------------------------------------------------------------
// Schema cleaner: invariants after merges
// ---------------------------------------------------------------------------

#[test]
fn enum_merged_in_from_all_of_is_a_string_enum() {
    let schema = json!({"type": "integer", "allOf": [{"enum": [1, 2]}]});
    let cleaned = sanitize_schema(&schema);
    assert_eq!(
        cleaned,
        json!({"type": "string", "enum": ["1", "2"], "description": "Allowed: 1, 2"})
    );
    assert_eq!(sanitize_schema(&cleaned), cleaned);
}

#[test]
fn enum_merged_into_an_existing_property_is_a_string_enum() {
    // Both the parent and the `allOf` member describe property `a`; the two
    // descriptions are merged and the result must still be a string enum.
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "integer"}},
        "allOf": [{"properties": {"a": {"enum": [1, 2]}, "b": {"type": "boolean"}}}]
    });
    let cleaned = sanitize_schema(&schema);
    assert_eq!(
        cleaned,
        json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "enum": ["1", "2"], "description": "Allowed: 1, 2"},
                "b": {"type": "boolean"}
            }
        })
    );
    assert_eq!(sanitize_schema(&cleaned), cleaned);
}

#[test]
fn all_of_never_mixes_data_values() {
    // `default` holds data, not a schema: the parent's value wins as a whole.
    let schema = json!({
        "type": "object",
        "default": {"a": 1},
        "allOf": [{"default": {"b": 2}}]
    });
    assert_eq!(
        sanitize_schema(&schema),
        json!({"type": "object", "default": {"a": 1}})
    );
}

#[test]
fn enum_next_to_a_union_keeps_its_hint_and_is_stable() {
    let schema = json!({
        "enum": [1, "a"],
        "anyOf": [{"type": "integer"}, {"type": "string"}]
    });
    let cleaned = sanitize_schema(&schema);
    assert_eq!(cleaned["type"], json!("string"));
    assert_eq!(cleaned["enum"], json!(["1", "a"]));
    assert_eq!(
        cleaned["description"],
        json!("Allowed: 1, a (Accepts: integer | string)")
    );
    assert_eq!(sanitize_schema(&cleaned), cleaned);
    let legacy = sanitize_schema_legacy(&schema);
    assert_eq!(legacy["type"], json!("string"));
    assert_eq!(sanitize_schema_legacy(&legacy), legacy);
}

#[test]
fn boolean_required_of_a_referenced_property_is_promoted() {
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"$ref": "#/$defs/A"},
            "b": {"type": "string", "required": true}
        },
        "$defs": {"A": {"type": "string", "required": true}}
    });
    let cleaned = sanitize_schema(&schema);
    assert_eq!(
        cleaned,
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            "required": ["a", "b"]
        })
    );
    assert_eq!(sanitize_schema(&cleaned), cleaned);
}

#[test]
fn a_required_that_is_not_a_list_is_removed() {
    // Not a property, so there is no parent to promote the flag to.
    let schema = json!({
        "type": "object",
        "properties": {"list": {"type": "array", "items": {"type": "string", "required": true}}},
        "required": true
    });
    assert_eq!(
        sanitize_schema(&schema),
        json!({
            "type": "object",
            "properties": {"list": {"type": "array", "items": {"type": "string"}}}
        })
    );
}

#[test]
fn stray_keys_of_a_node_that_becomes_an_object_are_dropped() {
    // `b` is declared a string, so its stray object-valued key is not a bare
    // property; the union then turns `b` into an object. The stray key was
    // never cleaned and must not surface as a property on a second pass.
    let schema = json!({
        "type": "object",
        "properties": {"b": {
            "type": "string",
            "anyOf": [{"type": "object", "properties": {"x": {"type": "string"}}}],
            "stray": {"$ref": "#/nope", "anyOf": [{"type": "string"}]}
        }}
    });
    let cleaned = sanitize_schema(&schema);
    assert_eq!(
        cleaned,
        json!({
            "type": "object",
            "properties": {"b": {"type": "object", "properties": {"x": {"type": "string"}}}}
        })
    );
    assert_eq!(sanitize_schema(&cleaned), cleaned);
}

// ---------------------------------------------------------------------------
// Protobuf-JSON spellings of a native Gemini request
// ---------------------------------------------------------------------------

#[test]
fn numeric_constraints_written_as_strings_become_numbers() {
    let request = decode(json!({
        "contents": [{"role": "user", "parts": [{"text": "x"}]}],
        "tools": [{"functionDeclarations": [{"name": "f", "parameters": {
            "type": "OBJECT",
            "properties": {
                "n": {"type": "NUMBER", "minimum": "0.5", "maximum": "10"},
                "i": {"type": "INTEGER", "minimum": 1, "maximum": "-3"},
                // A property that merely has the name of a keyword.
                "minItems": {"type": "STRING", "maxLength": "5"},
                "odd": {"type": "ARRAY", "minItems": "many", "maxItems": "2.5",
                        "items": {"type": "STRING"}}
            }
        }}]}],
        "generationConfig": {"responseMimeType": "application/json", "responseSchema": {
            "type": "ARRAY", "minItems": "2", "items": {"type": "STRING", "nullable": true}
        }}
    }));
    let Tool::Function(function) = &request.tools[0] else {
        panic!("expected a function tool");
    };
    assert_eq!(
        function.parameters,
        json!({
            "type": "object",
            "properties": {
                "n": {"type": "number", "minimum": 0.5, "maximum": 10},
                "i": {"type": "integer", "minimum": 1, "maximum": -3},
                "minItems": {"type": "string", "maxLength": 5},
                // Not numbers: left for the next reader to judge.
                "odd": {"type": "array", "minItems": "many", "maxItems": "2.5",
                        "items": {"type": "string"}}
            }
        })
    );
    let Some(ResponseFormat::JsonSchema { schema, .. }) = &request.response_format else {
        panic!("expected a JSON schema format");
    };
    assert_eq!(
        schema,
        &json!({"type": "array", "minItems": 2, "items": {"type": ["string", "null"]}})
    );
}

#[test]
fn standard_json_schema_declarations_are_not_rewritten() {
    // `parametersJsonSchema` is JSON Schema already; only the dialect field
    // is normalised.
    let request = decode(json!({
        "contents": [{"role": "user", "parts": [{"text": "x"}]}],
        "tools": [{"functionDeclarations": [{"name": "f", "parametersJsonSchema": {
            "type": "object", "properties": {"a": {"type": "array", "minItems": "1"}}
        }}]}]
    }));
    let Tool::Function(function) = &request.tools[0] else {
        panic!("expected a function tool");
    };
    assert_eq!(
        function.parameters["properties"]["a"]["minItems"],
        json!("1")
    );
}

#[test]
fn url_safe_and_unpadded_inline_data_becomes_standard_base64() {
    // Bytes fb ff fe 01: `+//+AQ==` in the standard alphabet, `-__-AQ` as
    // Google's Python SDK sends it. Other vendors only take the former.
    let request = decode(json!({"contents": [{"role": "user", "parts": [
        {"inlineData": {"mimeType": "image/png", "data": "-__-AQ"}},
        {"inlineData": {"mimeType": "image/png", "data": "-__-AQ=="}},
        {"inlineData": {"mimeType": "image/png", "data": "+//+AQ"}},
        {"inlineData": {"mimeType": "image/png", "data": "+//+AQ=="}},
        // Not plain base64 (line breaks): passed on as it is.
        {"inlineData": {"mimeType": "image/png", "data": "-__-\nAQ"}}
    ]}]}));
    let data: Vec<Part> = request.messages[0].parts.clone();
    assert_eq!(
        data,
        vec![
            Part::Image(MediaPart::base64("image/png", "+//+AQ==")),
            Part::Image(MediaPart::base64("image/png", "+//+AQ==")),
            Part::Image(MediaPart::base64("image/png", "+//+AQ==")),
            Part::Image(MediaPart::base64("image/png", "+//+AQ==")),
            Part::Image(MediaPart::base64("image/png", "-__-\nAQ")),
        ]
    );
}

// ---------------------------------------------------------------------------
// Tool results are named after the call of their own turn
// ---------------------------------------------------------------------------

fn named_result(call_id: &str, name: &str, text: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: Some(name.into()),
        content: vec![Part::text(text)],
        is_error: false,
        cache_control: None,
    })
}

fn response_names(body: &Value, content: usize) -> Vec<String> {
    body["contents"][content]["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .filter_map(|part| part["functionResponse"]["name"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn reused_ids_with_parallel_calls_are_resolved_per_turn_and_in_call_order() {
    let mut request = Request::new("gemini-2.5-pro", Protocol::Anthropic);
    request.messages = vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("c1", "first_a", "{}"),
                Part::tool_call("c2", "first_b", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("c2", "b"),
                Part::tool_result_text("c1", "a"),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("c1", "second_a", "{}"),
                Part::tool_call("c2", "second_b", "{}"),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("c1", "a2"),
                Part::tool_result_text("c2", "b2"),
            ],
        ),
    ];
    let body = encode(&request);
    assert_eq!(response_names(&body, 2), ["first_a", "first_b"]);
    assert_eq!(response_names(&body, 4), ["second_a", "second_b"]);
    assert_eq!(
        body["contents"][4]["parts"][0]["functionResponse"]["response"],
        json!({"result": "a2"})
    );
}

#[test]
fn a_matched_result_takes_the_name_of_its_call_even_when_it_brought_one() {
    // A Chat tool message may carry a `name` of its own; the call it answers
    // decides, because Gemini pairs by name.
    let mut request = Request::new("gemini-2.5-pro", Protocol::OpenaiChat);
    request.messages = vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("c1", "get weather", "{}")],
        ),
        Message::new(
            Role::User,
            vec![named_result("c1", "something_else", "sunny")],
        ),
    ];
    let body = encode(&request);
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["name"],
        json!("get_weather")
    );
    // ... in its sanitised spelling, like the call.
    assert_eq!(response_names(&body, 2), ["get_weather"]);
}

#[test]
fn a_gemini_clients_own_response_names_are_left_alone() {
    let body = json!({"contents": [
        {"role": "user", "parts": [{"text": "go"}]},
        {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}, "id": "fc_1"}}]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "g", "id": "fc_1", "response": {"result": "x"}}}]}
    ]});
    let encoded = encode(&decode(body));
    assert_eq!(response_names(&encoded, 2), ["g"]);
}

#[test]
fn an_unmatched_result_still_falls_back_to_the_lookup_by_id() {
    // The result answers a call from two turns back; with a Gemini client's
    // history nothing is rearranged, and the name comes from the lookup.
    let mut request = Request::new("gemini-2.5-pro", Protocol::Gemini);
    request.messages = vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("old", "lookup", "{}")],
        ),
        Message::user_text("never mind"),
        Message::assistant_text("ok"),
        Message::new(Role::User, vec![Part::tool_result_text("old", "late")]),
    ];
    let body = encode(&request);
    assert_eq!(response_names(&body, 4), ["lookup"]);
}

#[test]
fn count_requests_name_results_after_their_own_call_too() {
    let mut request = Request::new("gemini-2.5-pro", Protocol::OpenaiChat);
    request.messages = vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "tool_a", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "a")]),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "tool_b", "{}")],
        ),
        Message::new(Role::User, vec![Part::tool_result_text("call_1", "b")]),
    ];
    let body = GeminiCodec
        .encode_count_request(&request, &UpstreamCtx::default())
        .expect("gemini counts tokens");
    assert_eq!(response_names(&body, 2), ["tool_a"]);
    assert_eq!(response_names(&body, 4), ["tool_b"]);
}

// ---------------------------------------------------------------------------
// Signatures of other vendors on the Gemini wire
// ---------------------------------------------------------------------------

/// base64("sy1.a.ErACkgE="): an Anthropic signature as a Gemini client sees it.
const ARMOURED_ANTHROPIC: &str = "c3kxLmEuRXJBQ2tnRT0=";

fn anthropic_sig() -> Signature {
    Signature::new(Protocol::Anthropic, "ErACkgE=")
}

fn reasoning_signature(request: &Request, message: usize, part: usize) -> Option<Signature> {
    match &request.messages[message].parts[part] {
        Part::Reasoning(reasoning) => reasoning.signature.clone(),
        other => panic!("expected reasoning, got {other:?}"),
    }
}

fn model_turn_with(signature: &str) -> Value {
    json!({"contents": [
        {"role": "user", "parts": [{"text": "hi"}]},
        {"role": "model", "parts": [
            {"text": "thinking", "thought": true, "thoughtSignature": signature},
            {"text": "answer"}]},
        {"role": "user", "parts": [{"text": "more"}]}
    ]})
}

#[test]
fn armoured_signatures_are_recognised_however_the_client_re_encodes_them() {
    for wire in [
        ARMOURED_ANTHROPIC,
        // Unpadded, as SDKs that strip padding send `bytes`.
        "c3kxLmEuRXJBQ2tnRT0",
    ] {
        let request = decode(model_turn_with(wire));
        assert_eq!(
            reasoning_signature(&request, 1, 0),
            Some(anthropic_sig()),
            "{wire}"
        );
    }
    // A blob whose armour differs between the two base64 alphabets:
    // "sy1.a.??>>~~" is `c3kxLmEuPz8+Pn5+` / `c3kxLmEuPz8-Pn5-`.
    for wire in ["c3kxLmEuPz8+Pn5+", "c3kxLmEuPz8-Pn5-"] {
        let request = decode(model_turn_with(wire));
        assert_eq!(
            reasoning_signature(&request, 1, 0),
            Some(Signature::new(Protocol::Anthropic, "??>>~~")),
            "{wire}"
        );
    }
}

#[test]
fn signatures_that_only_look_armoured_stay_geminis_own() {
    for wire in [
        // base64("sy1-signature"): shares the first characters, is not tagged.
        "c3kxLXNpZ25hdHVyZQ==",
        // base64("sy1.x.abc"): unknown origin tag.
        "c3kxLnguYWJj",
        // Not base64 at all.
        "c3kxL!!",
    ] {
        let request = decode(model_turn_with(wire));
        assert_eq!(
            reasoning_signature(&request, 1, 0),
            Some(Signature::new(Protocol::Gemini, wire)),
            "{wire}"
        );
    }
}

#[test]
fn a_tagged_gemini_signature_comes_back_as_geminis() {
    // base64("sy1.g.R1NJRw==")
    let request = decode(model_turn_with("c3kxLmcuUjFOSlJ3PT0="));
    assert_eq!(
        reasoning_signature(&request, 1, 0),
        Some(Signature::new(Protocol::Gemini, "R1NJRw=="))
    );
}

#[test]
fn native_signatures_are_delivered_untouched() {
    let mut response = Response::new("r1", "gemini-2.5-pro");
    response.parts = vec![Part::ToolCall(ToolCall {
        id: "fc_1".into(),
        name: "f".into(),
        arguments: "{}".into(),
        kind: ToolCallKind::Function,
        signature: Some(Signature::new(Protocol::Gemini, "CiQBjz1rX+real/sig==")),
        cache_control: None,
    })];
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("m"))
        .expect("response encodes");
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        json!("CiQBjz1rX+real/sig==")
    );
}

#[test]
fn a_foreign_signature_round_trips_through_a_gemini_client_and_is_not_sent_to_gemini() {
    // An Anthropic model answered a Gemini client: thinking plus a tool call.
    let mut response = Response::new("r1", "claude-sonnet-4-5");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "plan".into(),
            signature: Some(anthropic_sig()),
            redacted: false,
        }),
        Part::tool_call("toolu_1", "lookup", r#"{"q":"x"}"#),
    ];
    let body = GeminiCodec
        .encode_response(&response, &ClientCtx::new("m"))
        .expect("response encodes");
    let model_parts = body["candidates"][0]["content"]["parts"].clone();
    assert_eq!(
        model_parts[0]["thoughtSignature"],
        json!(ARMOURED_ANTHROPIC)
    );

    // The stream encoder writes the same value.
    let mut encoder = GeminiCodec.stream_encoder(&ClientCtx::new("m"));
    let mut streamed = Vec::new();
    for event in response_to_events(&response) {
        streamed.extend(encoder.encode(&event));
    }
    streamed.extend(encoder.finish());
    assert!(
        streamed
            .iter()
            .any(|event| event.data.contains(ARMOURED_ANTHROPIC)),
        "{streamed:?}"
    );
    assert!(streamed.iter().all(|event| !event.data.contains("sy1.")));

    // The client replays the turn. Decoding restores the origin ...
    let replay = json!({"contents": [
        {"role": "user", "parts": [{"text": "hi"}]},
        {"role": "model", "parts": model_parts},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "lookup", "id": "toolu_1", "response": {"result": "42"}}}]}
    ]});
    let request = decode(replay.clone());
    assert_eq!(reasoning_signature(&request, 1, 0), Some(anthropic_sig()));

    // ... so a Gemini upstream gets neither the blob nor the reasoning bound
    // to it, and the call is signed with the documented bypass value.
    let upstream = encode(&request);
    assert_eq!(
        upstream["contents"][1],
        json!({"role": "model", "parts": [
            {"functionCall": {"name": "lookup", "args": {"q": "x"}, "id": "toolu_1"},
             "thoughtSignature": SKIP_SIGNATURE}
        ]})
    );
    assert!(!upstream.to_string().contains("c3kxL"));

    // The verbatim path reaches the same contents.
    let forwarded = prepare(replay);
    assert_eq!(forwarded["contents"][1], upstream["contents"][1]);
}

#[test]
fn prepare_passthrough_takes_foreign_signatures_out() {
    let body = json!({"contents": [
        {"role": "user", "parts": [{"text": "hi"}]},
        // A turn that is nothing but another vendor's reasoning disappears.
        {"role": "model", "parts": [
            {"text": "claude thought", "thought": true, "thoughtSignature": ARMOURED_ANTHROPIC},
            {"text": "", "thought_signature": ARMOURED_ANTHROPIC}]},
        {"role": "model", "parts": [
            // Gemini's own signature: untouched.
            {"text": "gemini thought", "thought": true, "thoughtSignature": "R1NJRw=="},
            // Text keeps its text.
            {"text": "visible", "thoughtSignature": ARMOURED_ANTHROPIC},
            // A call loses the blob wherever the client put it and gets the
            // bypass value, being the first call of the turn.
            {"functionCall": {"name": "f", "args": {}, "thoughtSignature": ARMOURED_ANTHROPIC}},
            // A later call just loses it. Plainly tagged blobs count too.
            {"functionCall": {"name": "g", "args": {}},
             "extra_content": {"google": {"thought_signature": "sy1.r.gAAAAAB"}}},
            // A tagged Gemini blob is restored.
            {"functionCall": {"name": "h", "args": {}}, "thoughtSignature": "c3kxLmcuUjFOSlJ3PT0="}
        ]},
        {"role": "user", "parts": [
            {"functionResponse": {"name": "f", "response": {"result": 1}}},
            {"functionResponse": {"name": "g", "response": {"result": 2}}},
            {"functionResponse": {"name": "h", "response": {"result": 3}}}]}
    ]});
    assert_eq!(
        prepare(body),
        json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"text": "gemini thought", "thought": true, "thoughtSignature": "R1NJRw=="},
                {"text": "visible"},
                {"functionCall": {"name": "f", "args": {}}, "thoughtSignature": SKIP_SIGNATURE},
                {"functionCall": {"name": "g", "args": {}}},
                {"functionCall": {"name": "h", "args": {}}, "thoughtSignature": "R1NJRw=="}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "f", "response": {"result": 1}}},
                {"functionResponse": {"name": "g", "response": {"result": 2}}},
                {"functionResponse": {"name": "h", "response": {"result": 3}}}]}
        ]})
    );
}

// ---------------------------------------------------------------------------
// The wrapped countTokens form
// ---------------------------------------------------------------------------

#[test]
fn the_wrapper_is_read_in_snake_case_and_wins_over_top_level_contents() {
    let body = json!({
        // Ignored by the API when the wrapper is present.
        "contents": [{"role": "user", "parts": [{"text": "ignored"}]}],
        "generate_content_request": {
            "model": "models/gemini-2.5-flash",
            "contents": [{"role": "user", "parts": [{"text": "counted"}]}],
            "system_instruction": {"parts": [{"text": "sys"}]},
            "generation_config": {"temperature": 0.5, "thinking_config": {"thinking_budget": 512}}
        }
    });
    // Without a URL model the wrapper names it.
    let meta = GeminiCodec
        .request_meta(&body, &RequestPath::default())
        .expect("meta");
    assert_eq!(meta.model, "gemini-2.5-flash");
    assert!(!meta.stream);

    let request = GeminiCodec
        .decode_request(&body, &RequestPath::default())
        .expect("request decodes");
    assert_eq!(request.model, "gemini-2.5-flash");
    assert_eq!(request.messages, vec![Message::user_text("counted")]);
    assert_eq!(request.system, vec![Part::text("sys")]);
    assert_eq!(request.temperature, Some(0.5));
    assert!(request.reasoning.is_some());
    assert!(request.extra.is_empty(), "{:?}", request.extra);
}

#[test]
fn a_decoded_count_request_encodes_back_into_the_wrapped_form() {
    let body = json!({"generateContentRequest": {
        "model": "models/gemini-2.5-pro",
        "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
        "systemInstruction": {"parts": [{"text": "be brief"}]}
    }});
    let request = decode(body.clone());
    assert_eq!(
        GeminiCodec.encode_count_request(&request, &UpstreamCtx::default()),
        Some(body)
    );
}

#[test]
fn set_request_model_follows_every_model_field_and_invents_none() {
    let mut both = json!({
        "model": "alias",
        "generateContentRequest": {"model": "models/alias", "contents": []}
    });
    GeminiCodec.set_request_model(&mut both, "models/gemini-2.5-flash");
    assert_eq!(
        both,
        json!({
            "model": "gemini-2.5-flash",
            "generateContentRequest": {"model": "models/gemini-2.5-flash", "contents": []}
        })
    );

    let mut snake = json!({"generate_content_request": {"model": "alias", "contents": []}});
    GeminiCodec.set_request_model(&mut snake, "gemini-2.5-flash");
    assert_eq!(
        snake,
        json!({"generate_content_request": {"model": "gemini-2.5-flash", "contents": []}})
    );

    let mut bare = json!({"generateContentRequest": {"contents": []}});
    GeminiCodec.set_request_model(&mut bare, "gemini-2.5-flash");
    assert_eq!(bare, json!({"generateContentRequest": {"contents": []}}));
}

#[test]
fn prepare_passthrough_reaches_inside_the_wrapper() {
    let body = json!({"generateContentRequest": {
        "model": "models/gemini-2.5-pro",
        "contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}}}]},
            {"role": "function", "parts": [{"functionResponse": {"name": "", "response": {}}}]}
        ]
    }});
    assert_eq!(
        prepare(body),
        json!({"generateContentRequest": {
            "model": "models/gemini-2.5-pro",
            "contents": [
                {"role": "user", "parts": [{"text": ""}]},
                {"role": "model", "parts": [
                    {"functionCall": {"name": "f", "args": {}}, "thoughtSignature": SKIP_SIGNATURE}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": {}}}]}
            ]
        }})
    );
}

#[test]
fn adapt_for_vertex_unwraps_the_snake_case_wrapper_too() {
    let mut body = json!({"generate_content_request": {
        "model": "models/gemini-2.5-pro",
        "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
        "tools": [{"googleSearch": {}}]
    }});
    adapt_for_vertex(&mut body);
    assert_eq!(
        body,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
            "tools": [{"googleSearch": {}}]
        })
    );
}

// ---------------------------------------------------------------------------
// fileData without a MIME type
// ---------------------------------------------------------------------------

#[test]
fn another_vendors_untyped_url_still_gets_the_type_of_its_kind() {
    // An Anthropic `document` with a URL source is a PDF by definition and an
    // OpenAI `image_url` is an image: here the kind is real information.
    let mut request = Request::new("gemini-2.5-pro", Protocol::Anthropic);
    request.messages = vec![Message::new(
        Role::User,
        vec![
            Part::Document(MediaPart::url("https://example.com/download?id=7")),
            Part::Image(MediaPart::url("https://example.com/photo")),
        ],
    )];
    assert_eq!(
        encode(&request)["contents"][0]["parts"],
        json!([
            {"fileData": {"mimeType": "application/pdf", "fileUri": "https://example.com/download?id=7"}},
            {"fileData": {"mimeType": "image/jpeg", "fileUri": "https://example.com/photo"}}
        ])
    );
}

#[test]
fn a_gemini_clients_typed_or_recognisable_url_keeps_its_type() {
    let body = json!({"contents": [{"role": "user", "parts": [
        {"fileData": {"mimeType": "video/mp4", "fileUri": "https://example.com/v"}},
        {"fileData": {"fileUri": "https://example.com/clip.mp4"}},
        {"fileData": {"fileUri": "https://example.com/watch?v=1"}}
    ]}]});
    assert_eq!(
        encode(&decode(body))["contents"][0]["parts"],
        json!([
            {"fileData": {"mimeType": "video/mp4", "fileUri": "https://example.com/v"}},
            {"fileData": {"mimeType": "video/mp4", "fileUri": "https://example.com/clip.mp4"}},
            {"fileData": {"fileUri": "https://example.com/watch?v=1"}}
        ])
    );
}
