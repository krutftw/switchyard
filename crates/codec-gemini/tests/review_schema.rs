//! Review evidence for `sanitize_schema`.

use pretty_assertions::assert_eq;
use serde_json::{Map, json};
use switchyard_codec_gemini::{GeminiCodec, sanitize_schema};
use switchyard_core::ir::{FunctionTool, Message, Request, Tool};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

/// A small schema must not turn into a huge one.
///
/// `sanitize_schema` inlines local `$ref`s (the reference replaces them by a
/// `See: <name>` stub, notes 07 section 2.2 rule 6). The only limit is the
/// *number* of expansions (512), not the amount of data copied, so one
/// definition referenced many times is cloned once per reference: an 80 KiB
/// tool schema becomes more than 30 MiB, i.e. a ~400x amplification of
/// client-controlled input. With the default 64 MiB body limit one request
/// can make the gateway allocate tens of gigabytes (and then try to send
/// them upstream).
#[test]
fn inlining_refs_does_not_amplify_the_schema_by_orders_of_magnitude() {
    let big_description = "x".repeat(64 * 1024);
    let mut properties = Map::new();
    for i in 0..600 {
        properties.insert(format!("p{i}"), json!({"$ref": "#/$defs/Shared"}));
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "$defs": {"Shared": {"type": "string", "description": big_description}}
    });
    let input_bytes = schema.to_string().len();
    let output_bytes = sanitize_schema(&schema).to_string().len();
    assert!(
        output_bytes <= input_bytes * 20,
        "a {input_bytes} byte schema was expanded to {output_bytes} bytes ({}x)",
        output_bytes / input_bytes
    );
}

/// The same through the public request path: the upstream body built from a
/// request must stay within a sane multiple of what the client sent.
#[test]
fn encode_request_does_not_amplify_a_tool_schema() {
    let big_description = "y".repeat(32 * 1024);
    let mut properties = Map::new();
    for i in 0..500 {
        properties.insert(format!("p{i}"), json!({"$ref": "#/definitions/Shared"}));
    }
    let parameters = json!({
        "type": "object",
        "properties": properties,
        "definitions": {"Shared": {"type": "string", "description": big_description}}
    });
    let input_bytes = parameters.to_string().len();
    let mut request = Request::new("gemini-2.5-pro", Protocol::OpenaiChat);
    request.messages = vec![Message::user_text("hi")];
    request.tools = vec![Tool::Function(FunctionTool {
        name: "f".into(),
        description: None,
        parameters,
        strict: None,
        cache_control: None,
    })];
    let body = GeminiCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("request encodes");
    let output_bytes = body.to_string().len();
    assert!(
        output_bytes <= input_bytes * 20,
        "a {input_bytes} byte tool schema produced a {output_bytes} byte upstream body"
    );
}

/// Rule 8 of the notes (07 section 2.2): every `enum` is stringified and the
/// node's `type` is forced to `"string"`. When the enum sits next to a union
/// the type of the chosen branch wins instead, which leaves an
/// `integer`-typed node with string enum values after one pass, and a second
/// pass changes the result — so the documented idempotence
/// (`sanitize_schema(sanitize_schema(x)) == sanitize_schema(x)`) does not
/// hold either.
#[test]
fn enum_next_to_a_union_is_a_string_enum_after_one_pass() {
    let schema = json!({
        "type": "object",
        "properties": {
            "v": {"enum": [1, "a"], "anyOf": [{"type": "integer"}, {"type": "string"}]}
        }
    });
    let once = sanitize_schema(&schema);
    assert_eq!(once["properties"]["v"]["enum"], json!(["1", "a"]));
    assert_eq!(
        once["properties"]["v"]["type"],
        json!("string"),
        "an enum node must be typed as string: {once}"
    );
    let twice = sanitize_schema(&once);
    assert_eq!(once, twice, "sanitize_schema must be idempotent");
}
