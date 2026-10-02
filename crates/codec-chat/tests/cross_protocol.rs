//! Regression tests for defects found by the cross-protocol matrix
//! (`crates/codecs/tests`): what a Chat upstream is sent for a request
//! written in another protocol, and what a Chat client is shown for an
//! answer another protocol's upstream gave.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_chat::ChatCodec;
use switchyard_core::ir::{
    CustomTool, FinishReason, FunctionTool, Message, Part, Reasoning, RefusalPart, Request,
    Response, ResponseFormat, Role, Signature, Tool, ToolCall, ToolCallKind, ToolChoice,
    ToolResult,
};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx};

const DOTTED: &str = "mcp.files:read-file";

fn function(name: &str, parameters: Value) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: Some("Reads a file.".into()),
        parameters,
        strict: None,
        cache_control: None,
    })
}

fn object_schema() -> Value {
    json!({"type": "object", "properties": {"path": {"type": "string"}}})
}

fn result(call_id: &str, text: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: call_id.into(),
        name: None,
        content: vec![Part::text(text)],
        is_error: false,
        cache_control: None,
    })
}

fn encode(request: &Request) -> Value {
    ChatCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("encodes")
}

/// A tool loop over one tool named `name`, as a client of `source` wrote it.
fn tool_loop(source: Protocol, name: &str, call_id: &str) -> Request {
    let mut request = Request::new("gpt-5.5", source);
    request.tools.push(function(name, object_schema()));
    request.messages = vec![
        Message::user_text("Read the notes."),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call(call_id, name, r#"{"path":"/tmp/notes"}"#)],
        ),
        Message::new(Role::User, vec![result(call_id, "remember the milk")]),
    ];
    request
}

fn client_ctx(request: Value) -> ClientCtx {
    ClientCtx::new("gpt-5.5").with_request(Arc::new(request))
}

fn tool_call_response(name: &str, finish: FinishReason) -> Response {
    let mut response = Response::new("resp_1", "upstream-model");
    response.parts = vec![Part::tool_call("call_1", name, r#"{"path":"/tmp/notes"}"#)];
    response.finish = finish;
    response
}

/// The chunks a Chat client receives for `response`, as JSON.
fn stream_chunks(response: &Response, ctx: &ClientCtx) -> Vec<Value> {
    let mut encoder = ChatCodec.stream_encoder(ctx);
    let mut wire = Vec::new();
    for event in response_to_events(response) {
        wire.extend(encoder.encode(&event));
    }
    wire.extend(encoder.finish());
    wire.iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Tool names
//
// OpenAI: "Invalid 'tools[0].function.name': string does not match pattern.
// Expected a string that matches the pattern '^[a-zA-Z0-9_-]+$'", and at most
// 64 characters. Gemini allows dots and colons, Anthropic 128 characters.
// ---------------------------------------------------------------------------

#[test]
fn foreign_tool_names_are_made_valid_wherever_they_appear() {
    let mut request = tool_loop(Protocol::Gemini, DOTTED, "call_1");
    request.tool_choice = Some(ToolChoice::Tool {
        name: DOTTED.into(),
    });
    let body = encode(&request);
    let wire = "mcp_files_read-file";
    assert_eq!(body["tools"][0]["function"]["name"], json!(wire));
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"],
        json!(wire)
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "function": {"name": wire}})
    );

    let long = "read_".repeat(20);
    let body = encode(&tool_loop(Protocol::Anthropic, &long, "call_1"));
    let declared = body["tools"][0]["function"]["name"]
        .as_str()
        .expect("a name");
    assert_eq!(declared, &long[..64]);
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"],
        json!(declared)
    );
}

#[test]
fn two_foreign_names_that_sanitise_alike_stay_two_tools() {
    let mut request = Request::new("gpt-5.5", Protocol::Gemini);
    request.messages.push(Message::user_text("hi"));
    request.tools = vec![
        function("files.read", object_schema()),
        function("files:read", object_schema()),
        function("files_read", object_schema()),
    ];
    let body = encode(&request);
    let names: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    // The name that was valid all along keeps its spelling.
    assert_eq!(names, ["files_read_2", "files_read_3", "files_read"]);
}

#[test]
fn a_chat_clients_own_names_are_replayed() {
    let body = encode(&tool_loop(Protocol::OpenaiChat, DOTTED, "call_1"));
    assert_eq!(body["tools"][0]["function"]["name"], json!(DOTTED));
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"],
        json!(DOTTED)
    );
}

#[test]
fn tool_calls_come_back_under_the_names_the_client_declared() {
    let long = "read_".repeat(20);
    let ctx = client_ctx(json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "function": {"name": DOTTED, "parameters": {"type": "object", "properties": {}}}},
            {"type": "function", "function": {"name": long, "parameters": {"type": "object", "properties": {}}}}
        ]
    }));
    // What an Anthropic upstream calls the first tool, and what a Gemini
    // upstream (64 characters) calls the second.
    for (upstream_name, declared) in [
        ("mcp_files_read-file", DOTTED),
        (&long[..64], long.as_str()),
    ] {
        let response = tool_call_response(upstream_name, FinishReason::ToolCalls);
        let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            json!(declared)
        );
        let streamed: Vec<Value> = stream_chunks(&response, &ctx)
            .iter()
            .filter_map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"][0]["function"]
                    .get("name")
                    .cloned()
            })
            .collect();
        assert_eq!(streamed, [json!(declared)]);
    }
    // A name the client did declare, or one that matches nothing, is not
    // touched.
    let response = tool_call_response("unknown_tool", FinishReason::ToolCalls);
    let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        json!("unknown_tool")
    );
}

// ---------------------------------------------------------------------------
// Tool-call ids
//
// OpenAI: "Invalid 'messages[1].tool_calls[0].id': string too long. Expected
// a string with maximum length 40". Responses item ids and the ids other
// gateways mint are longer.
// ---------------------------------------------------------------------------

#[test]
fn long_foreign_call_ids_are_shortened_and_still_pair() {
    let long_id = format!("fc_{}", "0123456789abcdef".repeat(4));
    let body = encode(&tool_loop(Protocol::OpenaiResponses, "read", &long_id));
    let call = body["messages"][1]["tool_calls"][0]["id"]
        .as_str()
        .expect("an id");
    assert!(call.len() <= 40, "{call} is longer than 40 characters");
    assert_eq!(body["messages"][2]["role"], json!("tool"));
    assert_eq!(body["messages"][2]["tool_call_id"], json!(call));
    // Deterministic: the same history encodes to the same body next turn.
    let again = encode(&tool_loop(Protocol::OpenaiResponses, "read", &long_id));
    assert_eq!(again["messages"][1]["tool_calls"][0]["id"], json!(call));

    // A Chat client's own ids are its business.
    let body = encode(&tool_loop(Protocol::OpenaiChat, "read", &long_id));
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], json!(long_id));
}

// ---------------------------------------------------------------------------
// Blobs of the other OpenAI protocol
// ---------------------------------------------------------------------------

/// A Responses upstream's encrypted reasoning shown to a Chat client bare
/// came back as "a blob a Chat upstream issued": it was then replayed to
/// Chat upstreams as a `reasoning_details` signature (which the server
/// behind them cannot verify) and its origin was lost for good.
#[test]
fn a_responses_blob_keeps_its_origin_through_a_chat_client() {
    let mut response = Response::new("resp_1", "gpt-5.5");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: Some("rs_1".into()),
            text: "Summary.".into(),
            signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAAAencrypted")),
            redacted: false,
        }),
        Part::text("Answer."),
    ];
    response.finish = FinishReason::Stop;
    let body = ChatCodec
        .encode_response(&response, &ClientCtx::new("gpt-5.5"))
        .expect("encodes");
    let message = body["choices"][0]["message"].clone();
    assert_eq!(
        message["reasoning_details"][0]["signature"],
        json!("sy1.r.gAAAAAencrypted")
    );

    // The client echoes the message; the blob is a Responses blob again.
    let echoed = json!({
        "model": "gpt-5.5",
        "messages": [
            {"role": "user", "content": "Question?"},
            message,
            {"role": "user", "content": "And then?"}
        ]
    });
    let request = ChatCodec
        .decode_request(&echoed, &RequestPath::default())
        .expect("decodes");
    let signature = request.messages[1]
        .parts
        .iter()
        .find_map(|part| match part {
            Part::Reasoning(reasoning) => reasoning.signature.clone(),
            _ => None,
        })
        .expect("the reasoning kept its blob");
    assert_eq!(
        signature,
        Signature::new(Protocol::OpenaiResponses, "gAAAAAencrypted")
    );

    // And a Chat upstream is not sent it.
    let upstream = encode(&request).to_string();
    assert!(!upstream.contains("gAAAAAencrypted"), "{upstream}");
    assert!(!upstream.contains("sy1."), "{upstream}");
}

#[test]
fn a_chat_upstreams_own_blob_still_travels_bare() {
    let mut response = Response::new("chatcmpl-1", "some-model");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "Thinking.".into(),
            signature: Some(Signature::new(Protocol::OpenaiChat, "ChatSig")),
            redacted: false,
        }),
        Part::tool_call("call_1", "read", "{}"),
    ];
    response.finish = FinishReason::ToolCalls;
    let body = ChatCodec
        .encode_response(&response, &ClientCtx::new("m"))
        .expect("encodes");
    assert_eq!(
        body["choices"][0]["message"]["reasoning_details"][0]["signature"],
        json!("ChatSig")
    );
}

// ---------------------------------------------------------------------------
// Finish reason of a tool turn
// ---------------------------------------------------------------------------

/// Clients run their tool loop on `finish_reason == "tool_calls"`. An
/// upstream that ended a tool turn with a reason Chat has no word for
/// (Anthropic `pause_turn`, a vendor-specific one) was reported as `stop`,
/// and the calls in the message were never executed.
#[test]
fn a_tool_turn_is_reported_as_tool_calls_whatever_the_upstream_called_it() {
    let ctx = ClientCtx::new("gpt-5.5");
    for (finish, expected) in [
        (FinishReason::Other("WEIRD".into()), "tool_calls"),
        (FinishReason::PauseTurn, "tool_calls"),
        (FinishReason::Stop, "tool_calls"),
        // An incomplete turn stays incomplete.
        (FinishReason::Length, "length"),
        (FinishReason::ContentFilter, "content_filter"),
    ] {
        let response = tool_call_response("read", finish.clone());
        let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
        assert_eq!(
            body["choices"][0]["finish_reason"],
            json!(expected),
            "{finish:?}"
        );
        let streamed: Vec<Value> = stream_chunks(&response, &ctx)
            .iter()
            .map(|chunk| chunk["choices"][0]["finish_reason"].clone())
            .filter(|reason| !reason.is_null())
            .collect();
        assert_eq!(streamed, [json!(expected)], "{finish:?} (stream)");
    }
    // Without calls nothing changes.
    let mut response = Response::new("r", "m");
    response.parts = vec![Part::text("Hello.")];
    response.finish = FinishReason::Other("WEIRD".into());
    let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));
}

// ---------------------------------------------------------------------------
// Request fields with stricter limits on OpenAI
// ---------------------------------------------------------------------------

/// OpenAI: "Invalid schema for function 'f': schema must be a JSON Schema of
/// 'type: \"object\"', got 'type: \"None\"'". Gemini declarations may leave
/// the root type out.
#[test]
fn a_foreign_parameter_schema_without_a_root_type_becomes_an_object_schema() {
    let mut request = Request::new("gpt-5.5", Protocol::Gemini);
    request.messages.push(Message::user_text("hi"));
    request.tools = vec![
        function(
            "typed",
            json!({"properties": {"q": {"type": "string"}}, "required": ["q"]}),
        ),
        function("empty", json!({})),
    ];
    let body = encode(&request);
    assert_eq!(
        body["tools"][0]["function"]["parameters"],
        json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]})
    );
    assert_eq!(
        body["tools"][1]["function"]["parameters"],
        json!({"type": "object", "properties": {}})
    );
}

/// OpenAI takes four stop sequences ("Invalid 'stop': array too long"),
/// Gemini five and Anthropic more.
#[test]
fn stop_sequences_of_a_foreign_request_are_cut_to_four() {
    let stops = ["one", "two", "three", "four", "five"];
    let mut request = Request::new("gpt-5.5", Protocol::Gemini);
    request.messages.push(Message::user_text("hi"));
    request.stop = stops.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        encode(&request)["stop"],
        json!(["one", "two", "three", "four"])
    );

    request.source = Protocol::OpenaiChat;
    assert_eq!(encode(&request)["stop"], json!(stops));
}

/// A Messages client may force a server tool (`web_search`) by name. Chat has
/// no such tool, so the declaration is dropped; the choice used to stay and
/// name a function the request does not declare, which OpenAI refuses.
#[test]
fn a_forced_tool_the_upstream_is_not_given_becomes_none() {
    let mut request = Request::new("gpt-5.5", Protocol::Anthropic);
    request.messages.push(Message::user_text("hi"));
    request.tools.push(function("read", object_schema()));
    request.tool_choice = Some(ToolChoice::Tool {
        name: "web_search".into(),
    });
    assert_eq!(encode(&request)["tool_choice"], json!("none"));

    request.tool_choice = Some(ToolChoice::Tool {
        name: "read".into(),
    });
    assert_eq!(
        encode(&request)["tool_choice"],
        json!({"type": "function", "function": {"name": "read"}})
    );
}

// ---------------------------------------------------------------------------
// Free-form (custom) tools
//
// Codex declares `apply_patch` as `{"type":"custom","format":{grammar}}` and
// replays its calls as `custom_tool_call` items. A Responses request is only
// translated to Chat when the provider does not speak Responses: a
// compatible server, whose `tools[].type` is `function` and nothing else.
// Notes 08 §1.3 / §5.1 / §5.2.
// ---------------------------------------------------------------------------

const PATCH: &str = "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch";

fn codex_turn(source: Protocol) -> Request {
    let mut request = Request::new("deepseek-chat", source);
    request.tools = vec![
        function("shell", object_schema()),
        Tool::Custom(CustomTool {
            name: "apply_patch".into(),
            description: Some("Edit files with a patch.".into()),
            format: Some(json!({"type": "grammar", "syntax": "lark", "definition": "start: /.+/"})),
        }),
    ];
    request.tool_choice = Some(ToolChoice::Tool {
        name: "apply_patch".into(),
    });
    request.messages = vec![
        Message::user_text("Fix the typo."),
        Message::new(
            Role::Assistant,
            vec![Part::ToolCall(ToolCall {
                id: "call_2".into(),
                name: "apply_patch".into(),
                arguments: PATCH.into(),
                kind: ToolCallKind::Custom,
                signature: None,
                cache_control: None,
            })],
        ),
        Message::new(Role::User, vec![result("call_2", "Done!")]),
    ];
    request
}

#[test]
fn a_foreign_custom_tool_is_a_function_taking_one_string() {
    for source in [
        Protocol::OpenaiResponses,
        Protocol::Anthropic,
        Protocol::Gemini,
    ] {
        let body = encode(&codex_turn(source));
        assert_eq!(
            body["tools"][1],
            json!({"type": "function", "function": {
                "name": "apply_patch",
                "description": "Edit files with a patch.",
                "parameters": {"type": "object", "properties": {"input": {"type": "string"}},
                               "required": ["input"]}
            }}),
            "{source}"
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "apply_patch"}}),
            "{source}"
        );
        let call = &body["messages"][1]["tool_calls"][0];
        assert_eq!(call["type"], json!("function"), "{source}");
        assert_eq!(call["function"]["name"], json!("apply_patch"), "{source}");
        let arguments: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().expect("a string"))
                .expect("arguments are JSON");
        assert_eq!(arguments, json!({"input": PATCH}), "{source}");
        assert_eq!(
            body["messages"][2],
            json!({"role": "tool", "tool_call_id": "call_2", "content": "Done!"}),
            "{source}"
        );
    }
}

#[test]
fn a_chat_clients_own_custom_tool_is_replayed_as_custom() {
    let body = encode(&codex_turn(Protocol::OpenaiChat));
    assert_eq!(body["tools"][1]["type"], json!("custom"));
    assert_eq!(
        body["tool_choice"],
        json!({"type": "custom", "custom": {"name": "apply_patch"}})
    );
    assert_eq!(
        body["messages"][1]["tool_calls"][0],
        json!({"id": "call_2", "type": "custom", "custom": {"name": "apply_patch", "input": PATCH}})
    );
}

// ---------------------------------------------------------------------------
// Output format
// ---------------------------------------------------------------------------

fn with_format(source: Protocol, schema: Value) -> Request {
    let mut request = Request::new("gpt-5.5", source);
    request.system = vec![Part::text("You are terse.")];
    request
        .messages
        .push(Message::user_text("List two recipes."));
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema,
        strict: None,
    });
    request
}

/// Gemini's `responseSchema` takes any root (the example in its guide is an
/// array). OpenAI: "schema must be a JSON Schema of 'type: \"object\"', got
/// 'type: \"array\"'", a 400 no failover repairs. Such a schema is described
/// to the model instead.
#[test]
fn a_foreign_schema_without_an_object_root_becomes_a_system_instruction() {
    let list = json!({"type": "array", "items": {"type": "object",
                      "properties": {"recipeName": {"type": "string"}}}});
    for schema in [
        list.clone(),
        json!({"type": "string", "enum": ["Percussion", "String"]}),
        json!({"anyOf": [{"type": "object"}, {"type": "array"}]}),
    ] {
        let body = encode(&with_format(Protocol::Gemini, schema.clone()));
        assert!(body.get("response_format").is_none(), "{body}");
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": [
                {"type": "text", "text": "You are terse."},
                {"type": "text", "text": format!(
                    "Respond with a single valid JSON value that conforms to the JSON Schema \
                     below and nothing else: no explanations and no markdown code fences.\n\
                     JSON Schema:\n{schema}")}
            ]})
        );
    }

    // An object root stays a native format; one that only implies it is an
    // object (legal in Gemini's dialect) is given its type.
    let object = json!({"type": "object", "properties": {"a": {"type": "string"}}});
    let body = encode(&with_format(Protocol::Gemini, object.clone()));
    assert_eq!(
        body["response_format"],
        json!({"type": "json_schema", "json_schema": {"name": "response", "schema": object}})
    );
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "You are terse."})
    );
    let body = encode(&with_format(
        Protocol::Gemini,
        json!({"properties": {"a": {"type": "string"}}}),
    ));
    assert_eq!(body["response_format"]["json_schema"]["schema"], object);

    // An OpenAI client wrote its schema for OpenAI: it is not second-guessed.
    for source in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
        let body = encode(&with_format(source, list.clone()));
        assert_eq!(
            body["response_format"]["json_schema"]["schema"], list,
            "{source}"
        );
    }
}

/// OpenAI: "'messages' must contain the word 'json' in some form, to use
/// 'response_format' of type 'json_object'". Gemini's JSON mode
/// (`responseMimeType: "application/json"`) has no such rule.
#[test]
fn json_mode_of_another_vendors_client_always_mentions_json() {
    let mut request = Request::new("gpt-5.5", Protocol::Gemini);
    request
        .messages
        .push(Message::user_text("List two recipes."));
    request.response_format = Some(ResponseFormat::JsonObject);
    let body = encode(&request);
    assert_eq!(body["response_format"], json!({"type": "json_object"}));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "Respond with a single valid JSON object and nothing \
                                           else: no explanations and no markdown code fences."},
            {"role": "user", "content": "List two recipes."}
        ])
    );

    // The conversation says it already: nothing is added.
    request.messages = vec![Message::user_text("List two recipes as Json.")];
    assert_eq!(
        encode(&request)["messages"],
        json!([{"role": "user", "content": "List two recipes as Json."}])
    );

    // A schema format without a schema can only mean "some JSON".
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema: Value::Null,
        strict: None,
    });
    assert_eq!(
        encode(&request)["response_format"],
        json!({"type": "json_object"})
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A Responses upstream reports a refusal as `status: "completed"` with a
/// refusal part, which decodes to `FinishReason::Refusal`. For a Chat client
/// that is `finish_reason: "stop"` next to `message.refusal`, exactly what a
/// Chat upstream sends for the same answer; `content_filter` makes SDK
/// helpers raise instead of returning the refusal.
#[test]
fn a_written_out_refusal_ends_with_stop_in_body_and_stream() {
    let mut response = Response::new("resp_1", "upstream-model");
    response.parts = vec![Part::Refusal(RefusalPart {
        text: "I can't help with that.".into(),
    })];
    response.finish = FinishReason::Refusal;
    let ctx = client_ctx(json!({"model": "gpt-5.5", "messages": []}));

    let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(
        body["choices"][0]["message"]["refusal"],
        json!("I can't help with that.")
    );

    let finish = |response: &Response| -> Value {
        stream_chunks(response, &ctx)
            .iter()
            .filter_map(|chunk| chunk["choices"][0].get("finish_reason"))
            .find(|reason| !reason.is_null())
            .cloned()
            .expect("a finish chunk")
    };
    assert_eq!(finish(&response), json!("stop"));

    // Withheld without a refusal text: not a completed answer.
    response.parts = vec![Part::text("I can")];
    let body = ChatCodec.encode_response(&response, &ctx).expect("encodes");
    assert_eq!(body["choices"][0]["finish_reason"], json!("content_filter"));
    assert_eq!(finish(&response), json!("content_filter"));
}

// ---------------------------------------------------------------------------
// Strict tools
// ---------------------------------------------------------------------------

/// Anthropic's strict tools allow optional properties; OpenAI's strict mode
/// wants every property listed in `required` and answers anything else with
/// a 400. `strict` of another vendor's client is therefore not forwarded.
#[test]
fn strict_of_another_vendors_tool_is_not_forwarded() {
    let strict_tool = |source: Protocol| {
        let mut request = Request::new("gpt-5.5", source);
        request.messages.push(Message::user_text("hi"));
        request.tools.push(Tool::Function(FunctionTool {
            name: "read".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}},
                               "additionalProperties": false}),
            strict: Some(true),
            cache_control: None,
        }));
        encode(&request)["tools"][0]["function"]
            .get("strict")
            .cloned()
    };
    assert_eq!(strict_tool(Protocol::Anthropic), None);
    assert_eq!(strict_tool(Protocol::Gemini), None);
    // OpenAI clients wrote it for the same rules.
    assert_eq!(strict_tool(Protocol::OpenaiResponses), Some(json!(true)));
    assert_eq!(strict_tool(Protocol::OpenaiChat), Some(json!(true)));
}
