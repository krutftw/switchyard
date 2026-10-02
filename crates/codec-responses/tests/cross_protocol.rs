//! Regression tests for defects found by the cross-protocol matrix
//! (`crates/codecs/tests`): what a Responses upstream is sent for a request
//! written in another protocol, and what a Responses client is shown for an
//! answer another protocol's upstream gave.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use switchyard_codec_responses::ResponsesCodec;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, FinishReason, FunctionTool, Message, Part, Reasoning, Request,
    Response, ResponseFormat, Role, Signature, Tool, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ModelThinking, ReasoningConfig, Summary};
use switchyard_core::stream::response_to_events;
use switchyard_core::{ClientCtx, Codec, Protocol, RequestPath, UpstreamCtx};

const DOTTED: &str = "mcp.files:read-file";

fn function(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: Some("Reads a file.".into()),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        strict: None,
        cache_control: None,
    })
}

fn encode(request: &Request) -> Value {
    ResponsesCodec
        .encode_request(request, &UpstreamCtx::default())
        .expect("encodes")
}

fn decode(body: &Value) -> Request {
    ResponsesCodec
        .decode_request(body, &RequestPath::default())
        .expect("decodes")
}

/// A tool loop over one tool named `name`, as a client of `source` wrote it.
fn tool_loop(source: Protocol, name: &str) -> Request {
    let mut request = Request::new("gpt-5.5", source);
    request.tools.push(function(name));
    request.messages = vec![
        Message::user_text("Read the notes."),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", name, r#"{"path":"/tmp/notes"}"#)],
        ),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "call_1".into(),
                name: None,
                content: vec![Part::text("remember the milk")],
                is_error: false,
                cache_control: None,
            })],
        ),
    ];
    request
}

fn items_of<'a>(body: &'a Value, key: &str, kind: &str) -> Vec<&'a Value> {
    body[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == kind)
        .collect()
}

// ---------------------------------------------------------------------------
// Tool names
//
// OpenAI: "Invalid 'tools[0].name': string does not match pattern. Expected a
// string that matches the pattern '^[a-zA-Z0-9_-]+$'", and at most 64
// characters. Gemini allows dots and colons, Anthropic 128 characters.
// ---------------------------------------------------------------------------

#[test]
fn foreign_tool_names_are_made_valid_wherever_they_appear() {
    let mut request = tool_loop(Protocol::Gemini, DOTTED);
    request.tool_choice = Some(ToolChoice::Tool {
        name: DOTTED.into(),
    });
    let body = encode(&request);
    let wire = "mcp_files_read-file";
    assert_eq!(body["tools"][0]["name"], json!(wire));
    assert_eq!(
        items_of(&body, "input", "function_call")[0]["name"],
        json!(wire)
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "name": wire})
    );

    let long = "read_".repeat(20);
    let body = encode(&tool_loop(Protocol::Anthropic, &long));
    assert_eq!(body["tools"][0]["name"], json!(&long[..64]));
    assert_eq!(
        items_of(&body, "input", "function_call")[0]["name"],
        json!(&long[..64])
    );
}

#[test]
fn a_responses_clients_own_names_are_replayed() {
    let body = encode(&tool_loop(Protocol::OpenaiResponses, DOTTED));
    assert_eq!(body["tools"][0]["name"], json!(DOTTED));
    assert_eq!(
        items_of(&body, "input", "function_call")[0]["name"],
        json!(DOTTED)
    );
}

#[test]
fn tool_calls_come_back_under_the_names_the_client_declared() {
    let long = "read_".repeat(20);
    let ctx = ClientCtx::new("gpt-5.5").with_request(Arc::new(json!({
        "model": "gpt-5.5",
        "input": "hi",
        "tools": [
            {"type": "function", "name": DOTTED, "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": long, "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": "a.b", "parameters": {"type": "object", "properties": {}}},
            {"type": "function", "name": "a:b", "parameters": {"type": "object", "properties": {}}}
        ]
    })));
    for (upstream_name, declared) in [
        // What an Anthropic upstream is given for the dotted name ...
        ("mcp_files_read-file", DOTTED),
        // ... what a Gemini upstream (64 characters) is given for the long one ...
        (&long[..64], long.as_str()),
        // ... and a name two declarations fit, which is not guessed at.
        ("a_b", "a_b"),
        ("unknown_tool", "unknown_tool"),
    ] {
        let mut response = Response::new("msg_1", "upstream-model");
        response.parts = vec![Part::tool_call("call_1", upstream_name, "{}")];
        response.finish = FinishReason::ToolCalls;
        let body = ResponsesCodec
            .encode_response(&response, &ctx)
            .expect("encodes");
        assert_eq!(
            items_of(&body, "output", "function_call")[0]["name"],
            json!(declared)
        );

        let mut encoder = ResponsesCodec.stream_encoder(&ctx);
        let mut wire = Vec::new();
        for event in response_to_events(&response) {
            wire.extend(encoder.encode(&event));
        }
        wire.extend(encoder.finish());
        let names: Vec<Value> = wire
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .filter(|payload| payload["type"] == "response.output_item.done")
            .map(|payload| payload["item"]["name"].clone())
            .collect();
        assert_eq!(names, [json!(declared)], "stream, {upstream_name}");
    }
}

// ---------------------------------------------------------------------------
// Blobs of the other OpenAI protocol
// ---------------------------------------------------------------------------

/// A Chat Completions upstream's reasoning signature (an OpenRouter relay's,
/// say) shown to a Responses client bare came back as "encrypted reasoning a
/// Responses upstream issued": it was sent to OpenAI as `encrypted_content`,
/// which is a 400, and never again to the Chat upstream that can read it.
#[test]
fn a_chat_blob_keeps_its_origin_through_a_responses_client() {
    let mut response = Response::new("chatcmpl-1", "some-model");
    response.parts = vec![
        Part::Reasoning(Reasoning {
            id: None,
            text: "Thinking.".into(),
            signature: Some(Signature::new(Protocol::OpenaiChat, "ChatSigErUB")),
            redacted: false,
        }),
        Part::Reasoning(Reasoning {
            id: None,
            text: String::new(),
            signature: Some(Signature::new(Protocol::OpenaiChat, "ChatRedactedPayload")),
            redacted: true,
        }),
        Part::text("Answer."),
    ];
    response.finish = FinishReason::Stop;
    let body = ResponsesCodec
        .encode_response(&response, &ClientCtx::new("gpt-5.5"))
        .expect("encodes");
    let reasoning = items_of(&body, "output", "reasoning");
    assert_eq!(
        reasoning[0]["encrypted_content"],
        json!("sy1.c.ChatSigErUB")
    );
    assert_eq!(
        reasoning[1]["encrypted_content"],
        json!("sy1.c.redacted:ChatRedactedPayload")
    );

    // The client replays the output items; the blobs are Chat blobs again.
    let mut input = vec![json!({"type": "message", "role": "user", "content": "Question?"})];
    input.extend(body["output"].as_array().expect("output").iter().cloned());
    input.push(json!({"type": "message", "role": "user", "content": "And then?"}));
    let request = decode(&json!({"model": "gpt-5.5", "input": input}));
    let blobs: Vec<(Signature, bool)> = request
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            Part::Reasoning(reasoning) => reasoning
                .signature
                .clone()
                .map(|signature| (signature, reasoning.redacted)),
            _ => None,
        })
        .collect();
    assert_eq!(
        blobs,
        [
            (Signature::new(Protocol::OpenaiChat, "ChatSigErUB"), false),
            (
                Signature::new(Protocol::OpenaiChat, "ChatRedactedPayload"),
                true
            ),
        ]
    );

    // And a Responses upstream is not sent them.
    let upstream = encode(&request);
    assert!(items_of(&upstream, "input", "reasoning").is_empty());
    let wire = upstream.to_string();
    assert!(
        !wire.contains("ChatSigErUB") && !wire.contains("ChatRedactedPayload"),
        "{wire}"
    );
}

#[test]
fn a_responses_upstreams_own_blob_still_travels_bare_and_is_replayed() {
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
    let body = ResponsesCodec
        .encode_response(&response, &ClientCtx::new("gpt-5.5"))
        .expect("encodes");
    assert_eq!(
        items_of(&body, "output", "reasoning")[0]["encrypted_content"],
        json!("gAAAAAencrypted")
    );

    let mut input = vec![json!({"type": "message", "role": "user", "content": "Question?"})];
    input.extend(body["output"].as_array().expect("output").iter().cloned());
    input.push(json!({"type": "message", "role": "user", "content": "And then?"}));
    let request = decode(&json!({"model": "gpt-5.5", "input": input}));
    let upstream = encode(&request);
    assert_eq!(
        items_of(&upstream, "input", "reasoning")[0]["encrypted_content"],
        json!("gAAAAAencrypted")
    );
}

// ---------------------------------------------------------------------------
// tool_choice
// ---------------------------------------------------------------------------

/// A Messages client may force a server tool (`code_execution`) by name. The
/// choice used to name a function the request does not declare, which the
/// vendor refuses.
#[test]
fn a_forced_tool_the_upstream_is_not_given_becomes_none() {
    let mut request = Request::new("gpt-5.5", Protocol::Anthropic);
    request.messages.push(Message::user_text("hi"));
    request.tools.push(function("read"));
    request.tool_choice = Some(ToolChoice::Tool {
        name: "code_execution".into(),
    });
    assert_eq!(encode(&request)["tool_choice"], json!("none"));

    request.tool_choice = Some(ToolChoice::Tool {
        name: "read".into(),
    });
    assert_eq!(
        encode(&request)["tool_choice"],
        json!({"type": "function", "name": "read"})
    );
}

/// A Messages client forces its web-search server tool by the tool's name.
/// The tool is mapped to the hosted `web_search`; the choice used to become
/// `"none"`, forbidding the one tool the client insisted on (notes 08 §8.3:
/// `tool` -> `{"type":"web_search"}` if the name is the declared web search
/// tool).
#[test]
fn a_forced_provider_tool_that_was_mapped_forces_the_hosted_tool() {
    let mut request = Request::new("gpt-5.5", Protocol::Anthropic);
    request.messages.push(Message::user_text("What changed?"));
    request.tools = vec![
        function("read"),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::CodeExecution,
            origin: Protocol::Anthropic,
            raw: json!({"type": "code_execution_20250825", "name": "code_execution"}),
        }),
        // No hosted equivalent: dropped, and so forcing it means "none".
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebFetch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_fetch_20250910", "name": "web_fetch"}),
        }),
    ];
    let mut forced = |name: &str| {
        request.tool_choice = Some(ToolChoice::Tool { name: name.into() });
        encode(&request)["tool_choice"].clone()
    };
    assert_eq!(forced("web_search"), json!({"type": "web_search"}));
    assert_eq!(
        forced("code_execution"),
        json!({"type": "code_interpreter"})
    );
    assert_eq!(forced("web_fetch"), json!("none"));
    assert_eq!(forced("read"), json!({"type": "function", "name": "read"}));
}

/// `allowed_tools` restricts the model to some of the declared tools. No
/// other protocol can say that, so the decoder narrows the tool list itself:
/// passing the mode on with every tool declared would let the model call
/// what the client excluded.
#[test]
fn allowed_tools_narrows_the_tool_list() {
    let tools = json!([
        {"type": "function", "name": "confirm_order", "parameters": {"type": "object", "properties": {}}},
        {"type": "custom", "name": "cancel_order"},
        {"type": "function", "name": "delete_account", "parameters": {"type": "object", "properties": {}}},
        {"type": "namespace", "name": "crm", "tools": [
            {"type": "function", "name": "lookup"},
            {"type": "function", "name": "purge"}
        ]},
        {"type": "web_search_preview"},
        {"type": "mcp", "server_label": "wiki", "server_url": "https://wiki.example/mcp"},
        {"type": "mcp", "server_label": "billing", "server_url": "https://billing.example/mcp"},
        {"type": "image_generation"}
    ]);
    let request_with = |choice: Value| {
        decode(&json!({"model": "gpt-5.5", "input": "Go.", "tools": tools, "tool_choice": choice}))
    };
    let kept = |request: &Request| -> Vec<String> {
        request
            .tools
            .iter()
            .map(|tool| match tool {
                Tool::Builtin(builtin) => builtin
                    .raw
                    .get("server_label")
                    .or_else(|| builtin.raw.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                named => named.name().unwrap_or("?").to_string(),
            })
            .collect()
    };

    let request = request_with(
        json!({"type": "allowed_tools", "mode": "required", "tools": [
            {"type": "function", "name": "confirm_order"},
            {"type": "custom", "name": "cancel_order"},
            {"type": "function", "name": "lookup", "namespace": "crm"},
            {"type": "web_search"},
            {"type": "mcp", "server_label": "wiki"}
        ]}),
    );
    assert_eq!(request.tool_choice, Some(ToolChoice::Required));
    assert_eq!(
        kept(&request),
        [
            "confirm_order",
            "cancel_order",
            "crm__lookup",
            "web_search_preview",
            "wiki"
        ]
    );

    // A namespaced tool named by its local name alone, and the nested
    // spelling Chat Completions uses for the same choice.
    let request = request_with(
        json!({"type": "allowed_tools", "allowed_tools": {"mode": "auto", "tools": [
            {"type": "function", "function": {"name": "purge"}}
        ]}}),
    );
    assert_eq!(request.tool_choice, Some(ToolChoice::Auto));
    assert_eq!(kept(&request), ["crm__purge"]);

    // Nothing allowed: nothing may be called.
    let request = request_with(json!({"type": "allowed_tools", "mode": "required", "tools": []}));
    assert_eq!(request.tool_choice, Some(ToolChoice::None));
    assert!(request.tools.is_empty());

    // Every other choice leaves the list alone.
    assert_eq!(request_with(json!("required")).tools.len(), 9);
}

// ---------------------------------------------------------------------------
// Reasoning summaries for Chat clients
// ---------------------------------------------------------------------------

/// A Chat Completions client cannot ask for reasoning text other than by
/// turning reasoning on; this API returns summaries only on request (notes 12
/// §8.1 "openai (implicit)", §8.3 "openai-response").
#[test]
fn a_chat_clients_effort_asks_for_summaries() {
    let reasoning = |source: Protocol, config: ReasoningConfig| {
        let mut request = Request::new("gpt-5.5", source);
        request.messages.push(Message::user_text("hi"));
        request.reasoning = Some(config);
        encode(&request).get("reasoning").cloned()
    };
    let level = ReasoningConfig::with_depth(Depth::Level(Effort::High));
    assert_eq!(
        reasoning(Protocol::OpenaiChat, level.clone()),
        Some(json!({"effort": "high", "summary": "auto"}))
    );
    // "Provider decides" still turns reasoning on.
    assert_eq!(
        reasoning(
            Protocol::OpenaiChat,
            ReasoningConfig::with_depth(Depth::Auto)
        ),
        Some(json!({"summary": "auto"}))
    );
    // Off, and an explicit "no summaries", ask for nothing.
    assert_eq!(
        reasoning(
            Protocol::OpenaiChat,
            ReasoningConfig {
                depth: Some(Depth::Off),
                summary: Some(Summary::Off)
            }
        ),
        Some(json!({"effort": "none"}))
    );
    assert_eq!(
        reasoning(
            Protocol::OpenaiChat,
            ReasoningConfig {
                depth: Some(Depth::Level(Effort::Low)),
                summary: Some(Summary::Off)
            }
        ),
        Some(json!({"effort": "low"}))
    );
    // An explicit detail wins.
    assert_eq!(
        reasoning(
            Protocol::OpenaiChat,
            ReasoningConfig {
                depth: Some(Depth::Level(Effort::Low)),
                summary: Some(Summary::Detailed)
            }
        ),
        Some(json!({"effort": "low", "summary": "detailed"}))
    );
    // Clients that have a field for it are taken at their word.
    for source in [
        Protocol::OpenaiResponses,
        Protocol::Anthropic,
        Protocol::Gemini,
    ] {
        assert_eq!(
            reasoning(source, level.clone()),
            Some(json!({"effort": "high"})),
            "{source}"
        );
    }
    // A model known not to reason gets no reasoning settings at all.
    let mut request = Request::new("gpt-4.1", Protocol::OpenaiChat);
    request.messages.push(Message::user_text("hi"));
    request.reasoning = Some(level);
    let unsupported = UpstreamCtx {
        thinking: ModelThinking::Unsupported,
        ..UpstreamCtx::default()
    };
    let body = ResponsesCodec
        .encode_request(&request, &unsupported)
        .expect("encodes");
    assert!(body.get("reasoning").is_none(), "{body}");
}

// ---------------------------------------------------------------------------
// Output format
// ---------------------------------------------------------------------------

fn with_format(source: Protocol, format: ResponseFormat) -> Request {
    let mut request = Request::new("gpt-5.5", source);
    request.system = vec![Part::text("You are terse.")];
    request
        .messages
        .push(Message::user_text("List two recipes."));
    request.response_format = Some(format);
    request
}

fn schema_format(schema: Value) -> ResponseFormat {
    ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema,
        strict: None,
    }
}

/// Gemini's `responseSchema` takes any root; this API's `json_schema` format
/// takes an object schema only and answers anything else with a 400. Such a
/// schema is described in the instructions instead.
#[test]
fn a_foreign_schema_without_an_object_root_becomes_an_instruction() {
    let list = json!({"type": "array", "items": {"type": "object",
                      "properties": {"recipeName": {"type": "string"}}}});
    let body = encode(&with_format(Protocol::Gemini, schema_format(list.clone())));
    assert!(body.get("text").is_none(), "{body}");
    assert_eq!(
        body["instructions"],
        json!(format!(
            "You are terse.\n\nRespond with a single valid JSON value that conforms to the JSON \
             Schema below and nothing else: no explanations and no markdown code fences.\n\
             JSON Schema:\n{list}"
        ))
    );

    // An object root stays a native format; one that only implies it is an
    // object is given its type.
    let object = json!({"type": "object", "properties": {"a": {"type": "string"}}});
    let body = encode(&with_format(
        Protocol::Gemini,
        schema_format(json!({"properties": {"a": {"type": "string"}}})),
    ));
    assert_eq!(
        body["text"]["format"],
        json!({"type": "json_schema", "name": "response", "schema": object})
    );
    assert_eq!(body["instructions"], json!("You are terse."));

    // An OpenAI client wrote its schema for OpenAI: it is not second-guessed.
    for source in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
        let body = encode(&with_format(source, schema_format(list.clone())));
        assert_eq!(body["text"]["format"]["schema"], list, "{source}");
    }
}

/// "Response input messages must contain the word 'json' in some form to use
/// 'text.format' of type 'json_object'." Gemini's JSON mode has no such rule.
#[test]
fn json_mode_of_another_vendors_client_always_mentions_json() {
    let mut request = with_format(Protocol::Gemini, ResponseFormat::JsonObject);
    let body = encode(&request);
    assert_eq!(body["text"]["format"], json!({"type": "json_object"}));
    // The rule speaks of the input messages, so that is where it is said.
    assert_eq!(body["instructions"], json!("You are terse."));
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "system", "content": [{"type": "input_text", "text":
                "Respond with a single valid JSON object and nothing else: no explanations and \
                 no markdown code fences."}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "List two recipes."}]}
        ])
    );
    // The conversation says it already: nothing is added.
    request
        .messages
        .push(Message::user_text("Answer in JSON, please."));
    let body = encode(&request);
    let roles: Vec<&str> = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter_map(|item| item["role"].as_str())
        .collect();
    assert_eq!(roles, ["user", "user"]);
    // A Chat Completions client may have satisfied its API's rule in a
    // system message, which travels as `instructions`: the same care.
    let chat = with_format(Protocol::OpenaiChat, ResponseFormat::JsonObject);
    assert_eq!(encode(&chat)["input"][0]["role"], json!("system"));
    // A Responses client's own request is replayed as it was written.
    let own = with_format(Protocol::OpenaiResponses, ResponseFormat::JsonObject);
    assert_eq!(encode(&own)["input"].as_array().map(Vec::len), Some(1));
    // A schema format without a schema can only mean "some JSON".
    request.response_format = Some(schema_format(Value::Null));
    assert_eq!(
        encode(&request)["text"]["format"],
        json!({"type": "json_object"})
    );
}

// ---------------------------------------------------------------------------
// Strict tools
// ---------------------------------------------------------------------------

/// Anthropic's strict tools allow optional properties; OpenAI's strict mode
/// wants every property listed in `required` and answers anything else with
/// a 400. `strict` of another vendor's client is therefore not taken over
/// (and has to be spelled out as `false`, because this API defaults to
/// `true`).
#[test]
fn strict_of_another_vendors_tool_is_not_taken_over() {
    let strict_tool = |source: Protocol, strict: Option<bool>| {
        let mut request = Request::new("gpt-5.5", source);
        request.messages.push(Message::user_text("hi"));
        request.tools.push(Tool::Function(FunctionTool {
            name: "read".into(),
            description: None,
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}},
                               "additionalProperties": false}),
            strict,
            cache_control: None,
        }));
        encode(&request)["tools"][0].get("strict").cloned()
    };
    assert_eq!(
        strict_tool(Protocol::Anthropic, Some(true)),
        Some(json!(false))
    );
    assert_eq!(strict_tool(Protocol::Anthropic, None), Some(json!(false)));
    assert_eq!(strict_tool(Protocol::Gemini, None), Some(json!(false)));
    // OpenAI clients wrote it for the same rules.
    assert_eq!(
        strict_tool(Protocol::OpenaiChat, Some(true)),
        Some(json!(true))
    );
    assert_eq!(strict_tool(Protocol::OpenaiChat, None), Some(json!(false)));
    assert_eq!(
        strict_tool(Protocol::OpenaiResponses, Some(true)),
        Some(json!(true))
    );
    // A Responses client's silence meant this API's default.
    assert_eq!(strict_tool(Protocol::OpenaiResponses, None), None);
}
