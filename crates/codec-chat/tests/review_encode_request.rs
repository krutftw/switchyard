//! Review findings for `encode_request` (IR -> Chat Completions upstream body).
//!
//! Every test here asserts the behaviour the reference notes require. They
//! failed against the reviewed implementation and are kept as regression tests.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use switchyard_core::ir::{
    FunctionTool, MediaPart, Message, Part, Request, Role, Tool, ToolResult,
};
use switchyard_core::protocol::Protocol;

fn tool(name: &str, parameters: Value) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: Some("d".into()),
        parameters,
        strict: None,
        cache_control: None,
    })
}

fn encoded_parameters(source: Protocol, parameters: Value) -> Value {
    let mut request = Request::new("gpt-x", source);
    request.messages.push(Message::user_text("hi"));
    request.tools.push(tool("f", parameters));
    encode_request(&request)["tools"][0]["function"]["parameters"].clone()
}

// ---------------------------------------------------------------------------
// Finding: tool parameter schemas from other protocols are forwarded verbatim.
//
// Notes 06 section 5.6 (Claude -> Chat `tools`), "Normalization (recursive over
// subschemas only ...)":
//   * `type:"object"` without `properties` -> add `"properties":{}` (at every depth);
//   * boolean subschema `true` -> `{}`; `false` and `additionalProperties`
//     booleans are preserved;
//   * `pattern` strings containing `\p{`, `\P{` or `\0` are removed.
// Section 10 pins them: "object schema without properties gets `properties:{}`
// at root, nested and in array items", "`\p{..}` patterns removed", "boolean
// subschemas ... -> `{}`".
//
// OpenAI answers 400 `invalid_function_parameters` ("object schema missing
// properties") for the first case, which is what MCP servers emit for every
// tool without arguments (`{"type":"object"}`).
// ---------------------------------------------------------------------------

#[test]
fn review_object_schema_without_properties_gets_empty_properties_at_every_depth() {
    // Root: the classic no-argument MCP tool.
    assert_eq!(
        encoded_parameters(Protocol::Anthropic, json!({"type": "object"})),
        json!({"type": "object", "properties": {}})
    );
    // Nested property and array items.
    assert_eq!(
        encoded_parameters(
            Protocol::Anthropic,
            json!({
                "type": "object",
                "properties": {
                    "options": {"type": "object"},
                    "rows": {"type": "array", "items": {"type": "object"}}
                }
            })
        ),
        json!({
            "type": "object",
            "properties": {
                "options": {"type": "object", "properties": {}},
                "rows": {"type": "array", "items": {"type": "object", "properties": {}}}
            }
        })
    );
}

#[test]
fn review_boolean_subschemas_become_empty_objects() {
    assert_eq!(
        encoded_parameters(
            Protocol::Anthropic,
            json!({
                "type": "object",
                "properties": {
                    "anything": true,
                    "list": {"type": "array", "items": true},
                    "closed": {"type": "object", "properties": {}, "additionalProperties": false}
                },
                "additionalProperties": true
            })
        ),
        json!({
            "type": "object",
            "properties": {
                "anything": {},
                "list": {"type": "array", "items": {}},
                // `additionalProperties` booleans are needed by strict mode and stay.
                "closed": {"type": "object", "properties": {}, "additionalProperties": false}
            },
            "additionalProperties": true
        })
    );
}

#[test]
fn review_unicode_property_patterns_are_removed_from_tool_schemas() {
    assert_eq!(
        encoded_parameters(
            Protocol::Gemini,
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "pattern": "^[\\p{L}\\p{N}_]+$"},
                    "plain": {"type": "string", "pattern": "^[a-z]+$"},
                    // `pattern` as *data* (a default value) is not a keyword.
                    "cfg": {"type": "object", "properties": {}, "default": {"pattern": "\\p{L}"}}
                }
            })
        ),
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "plain": {"type": "string", "pattern": "^[a-z]+$"},
                "cfg": {"type": "object", "properties": {}, "default": {"pattern": "\\p{L}"}}
            }
        })
    );
}

// ---------------------------------------------------------------------------
// Finding: orphan tool results become `tool` messages.
//
// Notes 08 section 5.2 (Responses -> Chat): a tool output whose call id is
// not awaiting an answer ("empty id, id never issued, id already answered")
// becomes `{"role":"user","content":...}`, "Rationale: strict Chat upstreams
// reject a `tool` message without a matching `tool_calls` entry" (OpenAI:
// 400 "messages with role 'tool' must be a response to a preceeding message
// with 'tool_calls'"). The implementation applies this to the empty id only.
// ---------------------------------------------------------------------------

fn result(call_id: &str, text: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: None,
        content: vec![Part::text(text)],
        is_error: false,
        cache_control: None,
    })
}

#[test]
fn review_tool_result_for_a_call_that_was_never_issued_is_shown_as_user_text() {
    let mut request = Request::new("gpt-x", Protocol::OpenaiResponses);
    request.messages = vec![
        Message::user_text("what is the weather?"),
        // The assistant turn that made `call_gone` is not part of the input
        // (trimmed history / `previous_response_id` style follow-up).
        Message::new(Role::User, vec![result("call_gone", "22 degrees")]),
    ];
    let messages = encoded_messages(&request);
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert!(
        !roles.contains(&"tool"),
        "a tool message answering no tool_calls entry is rejected upstream: {messages:#}"
    );
    assert_eq!(
        messages,
        json!([
            {"role": "user", "content": "what is the weather?"},
            {"role": "user", "content": "22 degrees"}
        ])
    );
}

#[test]
fn review_second_result_for_an_already_answered_call_is_shown_as_user_text() {
    let mut request = Request::new("gpt-x", Protocol::OpenaiResponses);
    request.messages = vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("call_1", "f", "{}")]),
        Message::new(
            Role::User,
            vec![result("call_1", "first"), result("call_1", "second")],
        ),
    ];
    assert_eq!(
        encoded_messages(&request),
        json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "first"},
            {"role": "user", "content": "second"}
        ])
    );
}

// ---------------------------------------------------------------------------
// Finding: image `detail` values that Chat does not have are forwarded.
//
// Chat's `image_url.detail` is `auto | low | high` (ir.rs documents the IR
// field the same way). The Responses decoder hands over whatever the client
// wrote; notes 08 section 10 (Responses -> Chat request): "Image `detail`:
// `high`, `original`->`high`, `medium` omitted, non-string omitted".
// ---------------------------------------------------------------------------

fn image_with_detail(detail: &str) -> Value {
    let mut media = MediaPart::url("https://example.com/cat.png");
    media.detail = Some(detail.into());
    let mut request = Request::new("gpt-x", Protocol::OpenaiResponses);
    request.messages.push(Message::new(
        Role::User,
        vec![Part::text("look"), Part::Image(media)],
    ));
    encoded_messages(&request)[0]["content"][1].clone()
}

#[test]
fn review_image_detail_is_limited_to_values_chat_accepts() {
    assert_eq!(
        image_with_detail("high"),
        json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.png", "detail": "high"}})
    );
    assert_eq!(
        image_with_detail("original"),
        json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.png", "detail": "high"}}),
        "`original` (Responses) must be mapped to `high`"
    );
    assert_eq!(
        image_with_detail("medium"),
        json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}),
        "a detail level Chat does not know must be omitted"
    );
}

// ---------------------------------------------------------------------------
// Finding: video media has no Chat representation in either direction.
//
// Notes 07 section 5.4 (Gemini -> Chat): mime `video/` ->
// `{"type":"video_url","video_url":{"url":"data:<mime>;base64,<data>"}}`.
// The Gemini decoder files video under `Part::Document` (the IR has no video
// part), and this encoder turns every inline document into a `file` part
// named "document", which no Chat server reads as a video.
// ---------------------------------------------------------------------------

#[test]
fn review_inline_video_is_sent_as_a_video_url_part() {
    let mut request = Request::new("some-vision-model", Protocol::Gemini);
    request.messages.push(Message::new(
        Role::User,
        vec![
            Part::text("what happens in this clip?"),
            Part::Document(MediaPart::base64("video/mp4", "AAAAIGZ0eXA=")),
        ],
    ));
    assert_eq!(
        encoded_messages(&request),
        json!([{"role": "user", "content": [
            {"type": "text", "text": "what happens in this clip?"},
            {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAAIGZ0eXA="}}
        ]}])
    );
}
