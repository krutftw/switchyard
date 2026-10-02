//! Tool names, tool definitions and tool arguments keep their meaning on the
//! way through the gateway.

mod common;

use common::{decode_request, encode_request, wire};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{FinishReason, Message, Part, Request, Response, ToolCallKind};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::{ClientCtx, Codec, Protocol};

/// The Messages request the client originally sent.
fn client_request() -> Arc<Value> {
    Arc::new(json!({
        "model": "sonnet", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"name": "Read", "input_schema": {"type": "object"}},
            {"name": "9lives", "input_schema": {"type": "object"}},
            {"name": "Bash", "input_schema": {"type": "object"}}
        ]
    }))
}

fn tool_response() -> Response {
    let mut response = Response::new("msg_01", "upstream");
    response.parts = vec![
        // An upstream of another protocol had to respell these.
        Part::tool_call("call_1", "read", "{}"),
        Part::tool_call("call_2", "_9lives", "{}"),
        Part::tool_call("call_3", "Bash", "{}"),
        Part::tool_call("call_4", "not_declared", "{}"),
    ];
    response.finish = FinishReason::ToolCalls;
    response
}

#[test]
fn encode_response_hands_tool_calls_back_under_the_clients_names() {
    let ctx = ClientCtx::new("sonnet").with_request(client_request());
    let body = AnthropicCodec
        .encode_response(&tool_response(), &ctx)
        .unwrap();
    let names: Vec<&str> = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Read", "9lives", "Bash", "not_declared"]);
    // Without the original request the names pass through untouched.
    let body = AnthropicCodec
        .encode_response(&tool_response(), &ClientCtx::new("sonnet"))
        .unwrap();
    assert_eq!(body["content"][0]["name"], json!("read"));
}

#[test]
fn stream_encoder_hands_tool_calls_back_under_the_clients_names() {
    let ctx = ClientCtx::new("sonnet").with_request(client_request());
    let mut encoder = AnthropicCodec.stream_encoder(&ctx);
    let mut out = Vec::new();
    for event in response_to_events(&tool_response()) {
        out.extend(encoder.encode(&event));
    }
    out.extend(encoder.finish());
    let names: Vec<String> = wire(&out)
        .into_iter()
        .filter(|(name, _)| name == "content_block_start")
        .map(|(_, data)| data["content_block"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["Read", "9lives", "Bash", "not_declared"]);
}

#[test]
fn stream_encoder_null_arguments_mean_no_arguments() {
    let events = vec![
        StreamEvent::Start {
            id: "msg_1".into(),
            model: "m".into(),
            created: 0,
        },
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::ToolCall {
                id: "call_1".into(),
                name: "now".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        },
        StreamEvent::ToolArgsDelta {
            index: 0,
            fragment: "null".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::Finish {
            reason: FinishReason::ToolCalls,
            stop_sequence: None,
        },
    ];
    let out = wire(&common::encode_stream(&events, "m"));
    assert_eq!(
        out[2].1["delta"],
        json!({"type": "input_json_delta", "partial_json": "{}"})
    );
    assert_eq!(out[4].1["delta"]["stop_reason"], json!("tool_use"));
}

#[test]
fn decode_request_remembers_tool_fields_the_ir_cannot_hold() {
    let request = decode_request(&json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "tools": [
            {"name": "search_docs", "description": "Search", "input_schema": {"type": "object"},
             "defer_loading": true, "input_examples": [{"q": "x"}], "eager_input_streaming": true},
            {"name": "plain", "input_schema": {"type": "object"}}
        ],
        // A client cannot plant the gateway's private key.
        "x-switchyard-anthropic-tool-extras": {"plain": {"defer_loading": true}}
    }));
    assert_eq!(
        request.extra.get("x-switchyard-anthropic-tool-extras"),
        Some(&json!({"search_docs": {
            "defer_loading": true, "input_examples": [{"q": "x"}], "eager_input_streaming": true
        }}))
    );
    // Re-encoded for an Anthropic upstream the definition is whole again…
    assert_eq!(
        encode_request(&request)["tools"],
        json!([
            {"name": "search_docs", "description": "Search", "input_schema": {"type": "object"},
             "defer_loading": true, "input_examples": [{"q": "x"}], "eager_input_streaming": true},
            {"name": "plain", "input_schema": {"type": "object"}}
        ])
    );
    // …and the private key itself never reaches the wire.
    assert!(
        encode_request(&request)
            .get("x-switchyard-anthropic-tool-extras")
            .is_none()
    );
}

#[test]
fn encode_request_ignores_tool_extras_of_other_sources() {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.messages = vec![Message::user_text("hi")];
    request.tools = vec![switchyard_core::ir::Tool::Function(
        switchyard_core::ir::FunctionTool {
            name: "f".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        },
    )];
    request.extra.insert(
        "x-switchyard-anthropic-tool-extras".into(),
        json!({"f": {"defer_loading": true}}),
    );
    assert_eq!(
        encode_request(&request)["tools"],
        json!([{"name": "f", "input_schema": {"type": "object", "properties": {}}}])
    );
}

#[test]
fn encode_request_leaves_out_an_oversized_user_id() {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.messages = vec![Message::user_text("hi")];
    request.user = Some("u".repeat(256));
    assert_eq!(
        encode_request(&request)["metadata"]["user_id"],
        json!("u".repeat(256))
    );
    request.user = Some("u".repeat(257));
    assert!(encode_request(&request).get("metadata").is_none());
}
