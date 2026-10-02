//! Regression tests for defects found by the cross-protocol matrix
//! (`crates/codecs/tests`): what a Messages client is shown for an answer
//! another protocol's upstream gave, and what an Anthropic upstream is sent
//! for a tool choice it cannot honour.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    FinishReason, FunctionTool, Message, Part, Request, Response, Tool, ToolChoice,
};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx};

fn tool_call_response(name: &str, finish: FinishReason) -> Response {
    let mut response = Response::new("chatcmpl-1", "upstream-model");
    response.parts = vec![Part::tool_call("call_1", name, r#"{"path":"/tmp/notes"}"#)];
    response.finish = finish;
    response
}

/// The events a Messages client receives for `response`, as JSON.
fn stream_events(response: &Response, ctx: &ClientCtx) -> Vec<Value> {
    let mut encoder = AnthropicCodec.stream_encoder(ctx);
    let mut wire = Vec::new();
    for event in response_to_events(response) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    wire.iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
        .collect()
}

fn tool_use_names(body: &Value) -> Vec<Value> {
    body["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["name"].clone())
        .collect()
}

/// Anthropic takes tool names of up to 128 characters, OpenAI and Gemini of
/// 64: an MCP tool named `mcp__server__a_long_description…` is declared to
/// those upstreams cut short, and they call it by the short name. Claude
/// Code has no tool of that name, so the turn was lost. The same happened to
/// a name written with a character the upstream had to replace.
#[test]
fn tool_calls_come_back_under_the_names_the_client_declared() {
    let long = format!("mcp__workspace__{}", "read_the_file_".repeat(6));
    assert!(long.len() > 64 && long.len() <= 128);
    let ctx = ClientCtx::new("claude-sonnet-4-5").with_request(Arc::new(json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"name": long, "input_schema": {"type": "object"}},
            {"name": "mcp.files:read-file", "input_schema": {"type": "object"}},
            {"name": "9lives", "input_schema": {"type": "object"}},
            {"name": "a.b", "input_schema": {"type": "object"}},
            {"name": "a:b", "input_schema": {"type": "object"}}
        ]
    })));
    for (upstream_name, declared) in [
        // Cut to 64 by an OpenAI upstream.
        (&long[..64], long.as_str()),
        // Punctuation replaced by an OpenAI upstream.
        ("mcp_files_read-file", "mcp.files:read-file"),
        // A leading underscore added by a Gemini upstream (already handled).
        ("_9lives", "9lives"),
        // Two declarations fit: not guessed at.
        ("a_b", "a_b"),
        ("unknown_tool", "unknown_tool"),
    ] {
        let response = tool_call_response(upstream_name, FinishReason::ToolCalls);
        let body = AnthropicCodec
            .encode_response(&response, &ctx)
            .expect("encodes");
        assert_eq!(tool_use_names(&body), [json!(declared)]);

        let streamed: Vec<Value> = stream_events(&response, &ctx)
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .filter(|event| event["content_block"]["type"] == "tool_use")
            .map(|event| event["content_block"]["name"].clone())
            .collect();
        assert_eq!(streamed, [json!(declared)], "stream, {upstream_name}");
    }
}

/// A short returned name is never taken for the beginning of a longer one:
/// only a name at the length limit of some upstream can have been cut.
#[test]
fn a_short_name_is_not_mistaken_for_a_truncated_one() {
    let ctx = ClientCtx::new("claude-sonnet-4-5").with_request(Arc::new(json!({
        "tools": [{"name": "read_file_from_workspace", "input_schema": {"type": "object"}}]
    })));
    let response = tool_call_response("read_file", FinishReason::ToolCalls);
    let body = AnthropicCodec
        .encode_response(&response, &ctx)
        .expect("encodes");
    assert_eq!(tool_use_names(&body), [json!("read_file")]);
}

/// Messages clients run their tool loop on `stop_reason == "tool_use"`. An
/// upstream that ended a tool turn with a finish reason of its own was
/// reported as `end_turn`, and the `tool_use` blocks were never executed.
#[test]
fn a_tool_turn_with_an_unknown_finish_reason_stops_for_tool_use() {
    let ctx = ClientCtx::new("claude-sonnet-4-5");
    let response = tool_call_response("read", FinishReason::Other("WEIRD".into()));
    let body = AnthropicCodec
        .encode_response(&response, &ctx)
        .expect("encodes");
    assert_eq!(body["stop_reason"], json!("tool_use"));
    let streamed: Vec<Value> = stream_events(&response, &ctx)
        .iter()
        .filter(|event| event["type"] == "message_delta")
        .map(|event| event["delta"]["stop_reason"].clone())
        .collect();
    assert_eq!(streamed, [json!("tool_use")]);

    // Without a call the turn simply ended.
    let mut response = Response::new("r", "m");
    response.parts = vec![Part::text("Hello.")];
    response.finish = FinishReason::Other("WEIRD".into());
    let body = AnthropicCodec
        .encode_response(&response, &ctx)
        .expect("encodes");
    assert_eq!(body["stop_reason"], json!("end_turn"));
}

// ---------------------------------------------------------------------------
// A forced tool the body does not offer
//
// "tool_choice.name: Tool 'web_search' not found in provided tools" is a 400
// no failover repairs.
// ---------------------------------------------------------------------------

/// The token-counting endpoint takes no provider-executed tools, so the
/// count request leaves them out. A choice that forces such a tool used to
/// stay behind and name a tool the body no longer offers.
#[test]
fn a_count_request_does_not_force_a_server_tool_it_left_out() {
    let request = AnthropicCodec
        .decode_request(
            &json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "What changed in Rust 1.90?"}],
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
                    {"name": "get_weather", "input_schema": {"type": "object", "properties": {}}}
                ],
                "tool_choice": {"type": "tool", "name": "web_search"}
            }),
            &RequestPath::default(),
        )
        .expect("decodes");
    // The generation request replays the client's own choice.
    let body = AnthropicCodec
        .encode_request(&request, &UpstreamCtx::default())
        .expect("encodes");
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": "web_search"})
    );
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));

    let count = AnthropicCodec
        .encode_count_request(&request, &UpstreamCtx::default())
        .expect("a count request");
    assert_eq!(
        count["tools"],
        json!([{"name": "get_weather", "input_schema": {"type": "object", "properties": {}}}])
    );
    assert_eq!(count["tool_choice"], json!({"type": "auto"}));

    // A forced function that is still offered stays forced.
    let mut forced = request.clone();
    forced.tool_choice = Some(ToolChoice::Tool {
        name: "get_weather".into(),
    });
    let count = AnthropicCodec
        .encode_count_request(&forced, &UpstreamCtx::default())
        .expect("a count request");
    assert_eq!(
        count["tool_choice"],
        json!({"type": "tool", "name": "get_weather"})
    );
}

/// A client of another protocol forces a tool this body does not offer (a
/// name it never declared, or a tool that has no counterpart here). The
/// other three encoders answer that with "no tool"; this one named the
/// missing tool.
#[test]
fn a_foreign_forced_tool_the_body_does_not_offer_becomes_none() {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.messages.push(Message::user_text("hi"));
    request.tools.push(Tool::Function(FunctionTool {
        name: "read".into(),
        description: None,
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
        cache_control: None,
    }));
    let mut forced = |name: &str| {
        request.tool_choice = Some(ToolChoice::Tool { name: name.into() });
        AnthropicCodec
            .encode_request(&request, &UpstreamCtx::default())
            .expect("encodes")["tool_choice"]
            .clone()
    };
    assert_eq!(forced("file_search"), json!({"type": "none"}));
    assert_eq!(forced("read"), json!({"type": "tool", "name": "read"}));
}

// ---------------------------------------------------------------------------
// Rules of the newer Claude generations (notes 15 §5.2)
// ---------------------------------------------------------------------------

fn two_tool_request(source: Protocol, model: &str) -> Request {
    let tool = |name: &str| {
        Tool::Function(FunctionTool {
            name: name.into(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        })
    };
    let mut request = Request::new(model, source);
    request.messages.push(Message::user_text("hi"));
    request.tools = vec![tool("read"), tool("write")];
    request
}

fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect()
}

/// Claude Opus 5.5, Sonnet 5.5, Fable 5.1 and Mythos 5.1 answer forced tool
/// use with 400 `tool_choice: type "tool" and "any" are not supported for
/// this model`. Chat clients send `tool_choice: "required"` as a matter of
/// course; translated for such a model it is `auto`, and a forced tool is
/// `auto` over a tool list narrowed to that tool.
#[test]
fn forced_tool_use_is_relaxed_for_models_that_refuse_it() {
    let encode = |request: &Request| {
        AnthropicCodec
            .encode_request(request, &UpstreamCtx::default())
            .expect("encodes")
    };
    for model in [
        "claude-opus-5-5",
        "claude-sonnet-5-5-20260601",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ] {
        let mut request = two_tool_request(Protocol::OpenaiChat, model);
        request.tool_choice = Some(ToolChoice::Required);
        request.parallel_tool_calls = Some(false);
        let body = encode(&request);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true}),
            "{model}"
        );
        assert_eq!(tool_names(&body), ["read", "write"], "{model}");

        request.tool_choice = Some(ToolChoice::Tool {
            name: "write".into(),
        });
        let body = encode(&request);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true}),
            "{model}"
        );
        assert_eq!(tool_names(&body), ["write"], "{model}");

        // The count request is refused for the same reason.
        let count = AnthropicCodec
            .encode_count_request(&request, &UpstreamCtx::default())
            .expect("a count request");
        assert_eq!(count["tool_choice"]["type"], json!("auto"), "{model}");

        // `auto` and `none` are accepted as they are.
        request.tool_choice = Some(ToolChoice::None);
        assert_eq!(encode(&request)["tool_choice"], json!({"type": "none"}));
    }

    // Models that take forced tool use are sent it.
    for model in ["claude-sonnet-4-5", "claude-opus-4-6", "claude-haiku-5-5"] {
        let mut request = two_tool_request(Protocol::OpenaiChat, model);
        request.tool_choice = Some(ToolChoice::Required);
        assert_eq!(
            encode(&request)["tool_choice"],
            json!({"type": "any"}),
            "{model}"
        );
        request.tool_choice = Some(ToolChoice::Tool {
            name: "write".into(),
        });
        let body = encode(&request);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "write"}),
            "{model}"
        );
        assert_eq!(tool_names(&body), ["read", "write"], "{model}");
    }

    // A Messages client's own choice is replayed; the API judges it.
    let mut request = two_tool_request(Protocol::Anthropic, "claude-opus-5-5");
    request.tool_choice = Some(ToolChoice::Required);
    assert_eq!(encode(&request)["tool_choice"], json!({"type": "any"}));
}

/// Claude 4.6 and later: "This model does not support assistant message
/// prefill. The conversation must end with a user message." A trailing
/// assistant turn of another protocol's client is left out for them.
#[test]
fn a_prefill_is_left_out_for_models_that_refuse_it() {
    let roles = |source: Protocol, model: &str, messages: Vec<Message>| -> Vec<String> {
        let mut request = Request::new(model, source);
        request.messages = messages;
        AnthropicCodec
            .encode_request(&request, &UpstreamCtx::default())
            .expect("encodes")["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|message| message["role"].as_str().map(str::to_string))
            .collect()
    };
    let prefilled = || {
        vec![
            Message::user_text("Name three colours."),
            Message::assistant_text("Here they are: 1."),
        ]
    };
    for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-opus-5-5"] {
        assert_eq!(
            roles(Protocol::OpenaiChat, model, prefilled()),
            ["user"],
            "{model}"
        );
    }
    for model in ["claude-sonnet-4-5", "claude-opus-4-1-20250805"] {
        assert_eq!(
            roles(Protocol::OpenaiChat, model, prefilled()),
            ["user", "assistant"],
            "{model}"
        );
    }
    // A Messages client's own prefill is replayed.
    assert_eq!(
        roles(Protocol::Anthropic, "claude-opus-4-6", prefilled()),
        ["user", "assistant"]
    );
    // A turn that ends in a tool call is not a prefill: it is answered (by
    // the result the conversation lacks) and the conversation ends with the
    // user either way.
    let pending_call = vec![
        Message::user_text("Read it."),
        Message::new(
            switchyard_core::ir::Role::Assistant,
            vec![Part::tool_call("call_1", "read", "{}")],
        ),
    ];
    assert_eq!(
        roles(Protocol::OpenaiChat, "claude-opus-4-6", pending_call),
        ["user", "assistant", "user"]
    );
    // A conversation that was nothing but a prefill still opens with a user
    // turn.
    assert_eq!(
        roles(
            Protocol::OpenaiChat,
            "claude-opus-4-6",
            vec![Message::assistant_text("Once upon a time")]
        ),
        ["user"]
    );
}
