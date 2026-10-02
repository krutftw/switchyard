//! Upstream side: rules the Messages API enforces on a request body and
//! that canonical requests coming from other protocols routinely break.
//! Each of them is answered with a 400, which the gateway treats as a
//! request fault (no failover) and which repeats for as long as the
//! offending content stays in the client's history.

mod common;

use common::{decode_request, encode_request, encode_request_with};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::collections::HashSet;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, MediaPart, Message, Part, Reasoning,
    Request, Role, Signature, TextPart, Tool, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ModelThinking, ReasoningConfig, ThinkingSupport};
use switchyard_core::{Protocol, UpstreamCtx};

fn request(messages: Vec<Message>) -> Request {
    let mut request = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    request.messages = messages;
    request
}

fn function_tool(name: &str) -> Tool {
    Tool::Function(FunctionTool {
        name: name.into(),
        description: None,
        parameters: Value::Null,
        strict: None,
        cache_control: None,
    })
}

fn signed(text: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature: Some(Signature::new(Protocol::Anthropic, "EqQBCkYIBBgCKkD")),
        redacted: false,
    })
}

/// Claude Sonnet 4.5 as the catalog describes it: budgets only.
fn manual_only() -> ThinkingSupport {
    ThinkingSupport {
        min: 1024,
        max: 128_000,
        zero_allowed: true,
        dynamic_allowed: false,
        levels: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Text and media without content
// ---------------------------------------------------------------------------

#[test]
fn whitespace_only_text_is_left_out_everywhere() {
    let mut req = request(vec![
        Message::new(
            Role::User,
            vec![
                Part::text(" "),
                Part::Text(TextPart {
                    text: "\n".into(),
                    cache_control: Some(json!({"type": "ephemeral"})),
                    ..TextPart::default()
                }),
                Part::text("hi"),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![Part::text("\t"), Part::tool_call("toolu_1", "f", "{}")],
        ),
        Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: "toolu_1".into(),
                name: None,
                content: vec![Part::text("  "), Part::text("42"), Part::text("\n")],
                is_error: false,
                cache_control: None,
            })],
        ),
        Message::assistant_text("\u{a0}\u{2003}"),
        Message::user_text("\r\n"),
    ]);
    req.system = vec![Part::text("\n\n"), Part::text("Be brief.")];
    let body = encode_request(&req);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "Be brief."}])
    );
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}
            ]},
            // The whitespace-only assistant and user turns that followed are
            // gone, and so is the whitespace inside the tool result.
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"}
            ]}
        ])
    );
}

#[test]
fn a_tool_result_of_only_whitespace_is_an_empty_result() {
    let body = encode_request(&request(vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("toolu_1", "f", "{}")]),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_1", " \n")]),
    ]));
    assert_eq!(
        body["messages"][2]["content"],
        json!([{"type": "tool_result", "tool_use_id": "toolu_1"}])
    );
}

#[test]
fn a_system_prompt_of_only_whitespace_is_no_system_prompt() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.system = vec![Part::text("   "), Part::text("")];
    assert!(encode_request(&req).get("system").is_none());
}

#[test]
fn media_without_content_is_left_out() {
    let body = encode_request(&request(vec![Message::new(
        Role::User,
        vec![
            Part::Image(MediaPart::base64("image/png", "")),
            Part::Document(MediaPart::base64("application/pdf", "")),
            // "   " as base64 text: a plain-text document without text.
            Part::Document(MediaPart::base64("text/plain", "ICAg")),
            Part::text("see above"),
        ],
    )]));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "see above"}]}])
    );
}

// ---------------------------------------------------------------------------
// System instructions
// ---------------------------------------------------------------------------

/// The IR keeps instructions that precede the conversation in
/// `Request::system`. A request that has them as leading `Role::System`
/// messages means the same thing and gets the same body.
#[test]
fn leading_system_messages_join_the_top_level_system() {
    let mut req = request(vec![
        Message::new(
            Role::System,
            vec![Part::Text(TextPart {
                text: "You answer in French.".into(),
                cache_control: Some(json!({"type": "ephemeral"})),
                ..TextPart::default()
            })],
        ),
        Message::new(Role::System, vec![Part::text("Be brief."), Part::text(" ")]),
        Message::user_text("hi"),
        Message::new(Role::System, vec![Part::text("Now answer in German.")]),
    ]);
    req.system = vec![Part::text("Top-level instructions.")];
    let body = encode_request(&req);
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "Top-level instructions."},
            {"type": "text", "text": "You answer in French.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "Be brief."}
        ])
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "text", "text": "<system>\nNow answer in German.\n</system>"}
        ]}])
    );
}

#[test]
fn a_conversation_of_only_system_messages_still_opens_with_a_user_turn() {
    let body = encode_request(&request(vec![Message::new(
        Role::System,
        vec![Part::text("You are terse.")],
    )]));
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "You are terse."}])
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "(continued)"}]}])
    );
}

/// What an OpenAI-habit client sends to `/v1/messages`, through this codec
/// in both directions: the instructions end up where the API wants them.
#[test]
fn leading_system_role_messages_of_a_messages_client_become_system_blocks() {
    let request = decode_request(&json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64,
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "You answer in French.", "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "developer", "content": "Be brief."},
            {"role": "user", "content": "hi"}
        ]
    }));
    assert_eq!(
        request.system,
        vec![
            Part::Text(TextPart {
                text: "You answer in French.".into(),
                cache_control: Some(json!({"type": "ephemeral"})),
                ..TextPart::default()
            }),
            Part::text("Be brief."),
        ]
    );
    assert_eq!(request.messages, vec![Message::user_text("hi")]);
    let body = encode_request(&request);
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "You answer in French.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "Be brief."}
        ])
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );
}

// ---------------------------------------------------------------------------
// Uniqueness
// ---------------------------------------------------------------------------

fn tool_use_ids(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["id"].as_str().expect("id").to_string())
        .collect()
}

/// Vendors that number their calls from zero in every response reuse ids
/// within a conversation, valid ones included.
#[test]
fn a_reused_call_id_is_unique_per_call_and_pairs_with_its_own_result() {
    let turn = |city: &str, weather: &str| {
        vec![
            Message::user_text(format!("Weather in {city}?")),
            Message::new(
                Role::Assistant,
                vec![
                    Part::tool_call("call_0", "get_weather", format!("{{\"city\":\"{city}\"}}")),
                    Part::tool_call("call_1", "get_time", "{}"),
                ],
            ),
            Message::new(
                Role::User,
                vec![
                    // Answered in the opposite order.
                    Part::tool_result_text("call_1", "noon"),
                    Part::tool_result_text("call_0", weather),
                ],
            ),
            Message::assistant_text("Done."),
        ]
    };
    let mut messages = turn("Paris", "sunny");
    messages.extend(turn("Rome", "rainy"));
    messages.extend(turn("Oslo", "snow"));
    let body = encode_request(&request(messages));

    let ids = tool_use_ids(&body);
    assert_eq!(ids.len(), 6);
    assert_eq!(ids[..2], ["call_0", "call_1"], "first use keeps the id");
    let distinct: HashSet<&String> = ids.iter().collect();
    assert_eq!(distinct.len(), 6, "{ids:?}");
    assert!(ids.iter().all(|id| {
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }));

    let messages = body["messages"].as_array().unwrap();
    for (round, weather) in ["sunny", "rainy", "snow"].into_iter().enumerate() {
        let results = &messages[round * 4 + 2]["content"];
        assert_eq!(
            results,
            &json!([
                {"type": "tool_result", "tool_use_id": ids[round * 2 + 1], "content": "noon"},
                {"type": "tool_result", "tool_use_id": ids[round * 2], "content": weather}
            ])
        );
    }
    // Appending to the conversation never changes the ids of earlier turns.
    let mut shorter = turn("Paris", "sunny");
    shorter.extend(turn("Rome", "rainy"));
    assert_eq!(tool_use_ids(&encode_request(&request(shorter))), ids[..4]);
}

/// Two assistant messages in a row are one turn on the wire; a call id that
/// both of them use must still be told apart, and each result must find the
/// call it belongs to.
#[test]
fn a_call_id_repeated_inside_one_merged_turn_stays_paired() {
    let body = encode_request(&request(vec![
        Message::user_text("go"),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_0", "f", "{\"n\":1}")],
        ),
        Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_0", "f", "{\"n\":2}")],
        ),
        Message::new(
            Role::User,
            vec![
                Part::tool_result_text("call_0", "one"),
                Part::tool_result_text("call_0", "two"),
            ],
        ),
    ]));
    let ids = tool_use_ids(&body);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        body["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": ids[0], "content": "one"},
            {"type": "tool_result", "tool_use_id": ids[1], "content": "two"}
        ])
    );
}

/// A result may only answer a call of the assistant turn right before it. A
/// result that names an older call (same id or not) is plain user content.
#[test]
fn a_result_for_a_call_of_an_older_turn_is_not_paired_with_it() {
    let body = encode_request(&request(vec![
        Message::user_text("go"),
        Message::new(Role::Assistant, vec![Part::tool_call("call_0", "f", "{}")]),
        Message::new(Role::User, vec![Part::tool_result_text("call_0", "first")]),
        Message::assistant_text("ok"),
        Message::new(Role::User, vec![Part::tool_result_text("call_0", "late")]),
    ]));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_0", "name": "f", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_0", "content": "first"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [{"type": "text", "text": "late"}]}
        ])
    );
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_string())
        .collect()
}

/// "Tool names must be unique."
#[test]
fn tool_names_are_unique() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.tools = vec![
        // Needs sanitising and would then take the name of the next tool.
        function_tool("get.weather"),
        function_tool("get_weather"),
        // Declared twice by the client.
        function_tool("lookup"),
        Tool::Function(FunctionTool {
            name: "lookup".into(),
            description: Some("a second declaration".into()),
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }),
        // Two names that only differ in characters the API does not allow.
        function_tool("mcp.server:time"),
        Tool::Custom(CustomTool {
            name: "mcp:server.time".into(),
            description: None,
            format: None,
        }),
        // A provider tool of another vendor next to a function of its name.
        function_tool("web_search"),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::OpenaiResponses,
            raw: json!({"type": "web_search"}),
        }),
    ];
    req.tool_choice = Some(ToolChoice::Auto);
    let body = encode_request(&req);
    assert_eq!(
        tool_names(&body),
        ["get_weather", "lookup", "mcp_server_time", "web_search"]
    );
    // The tool that keeps a name is the one that was declared under it.
    assert!(body["tools"][1].get("description").is_none());
    assert!(body["tools"][3].get("type").is_none());
}

#[test]
fn native_tools_keep_their_place_when_names_are_unique() {
    let mut req = request(vec![Message::user_text("hi")]);
    req.source = Protocol::Anthropic;
    req.tools = vec![
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Anthropic,
            raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
        }),
        function_tool("get_weather"),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::Other("mcp_toolset".into()),
            origin: Protocol::Anthropic,
            // No name of its own: never a duplicate.
            raw: json!({"type": "mcp_toolset", "mcp_server_name": "a"}),
        }),
        Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::Other("mcp_toolset".into()),
            origin: Protocol::Anthropic,
            raw: json!({"type": "mcp_toolset", "mcp_server_name": "b"}),
        }),
    ];
    let body = encode_request(&req);
    assert_eq!(
        body["tools"],
        json!([
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
            {"name": "get_weather", "input_schema": {"type": "object", "properties": {}}},
            {"type": "mcp_toolset", "mcp_server_name": "a"},
            {"type": "mcp_toolset", "mcp_server_name": "b"}
        ])
    );
}

// ---------------------------------------------------------------------------
// Manual thinking
// ---------------------------------------------------------------------------

/// `budget_tokens` must be at least 1024 and below `max_tokens`. With a
/// client limit that cannot hold the smallest budget and nothing known
/// about the model, the request is sent without extended thinking.
#[test]
fn a_budget_that_cannot_fit_under_the_limit_is_not_sent() {
    for max_tokens in [1_u64, 256, 1000, 1024] {
        let mut req = Request::new("some-claude-compatible-model", Protocol::Gemini);
        req.messages = vec![Message::user_text("hi")];
        req.max_output_tokens = Some(max_tokens);
        req.temperature = Some(0.3);
        req.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
        assert_eq!(
            encode_request(&req),
            json!({
                "model": "some-claude-compatible-model",
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                // Nothing is thinking, so sampling is as the client asked.
                "temperature": 0.3
            }),
            "max_tokens {max_tokens}"
        );
    }
    // The smallest limit that holds the smallest budget.
    let mut req = Request::new("some-claude-compatible-model", Protocol::Gemini);
    req.messages = vec![Message::user_text("hi")];
    req.max_output_tokens = Some(1025);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
    let body = encode_request(&req);
    assert_eq!(body["max_tokens"], json!(1025));
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );
}

fn tool_loop_request(first_turn: Vec<Part>, later_steps: usize) -> Request {
    let mut req = Request::new("claude-sonnet-4-5", Protocol::OpenaiChat);
    req.tools = vec![function_tool("lookup")];
    req.max_output_tokens = Some(16_000);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(8192)));
    req.messages = vec![
        Message::user_text("An earlier question."),
        Message::assistant_text("An earlier answer, from a turn without thinking."),
        Message::user_text("What is the answer?"),
        Message::new(Role::Assistant, first_turn),
        Message::new(Role::User, vec![Part::tool_result_text("toolu_0", "42")]),
    ];
    for step in 1..=later_steps {
        let id = format!("toolu_{step}");
        req.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call(id.clone(), "lookup", "{}")],
        ));
        req.messages.push(Message::new(
            Role::User,
            vec![Part::tool_result_text(id, "43")],
        ));
    }
    req
}

/// Notes 15 section 5.6: with `enabled`, "the final assistant turn must
/// begin with a thinking block". The turn is the whole tool loop: only its
/// first assistant message has to open with thinking, later steps carry no
/// thinking of their own unless thinking is interleaved.
#[test]
fn manual_thinking_follows_the_turn_in_progress() {
    let caps = manual_only();
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let manual = json!({"type": "enabled", "budget_tokens": 8192});

    for later_steps in [0, 1, 3] {
        // The loop began with signed thinking: thinking stays on.
        let req = tool_loop_request(
            vec![
                signed("Look it up."),
                Part::tool_call("toolu_0", "lookup", "{}"),
            ],
            later_steps,
        );
        let body = encode_request_with(&req, &ctx);
        assert_eq!(body["thinking"], manual, "{later_steps} later steps");

        // The loop began without it (a client with no signature slot,
        // another vendor's reasoning): no manual thinking for this request.
        let req = tool_loop_request(
            vec![
                Part::reasoning("Unsigned reasoning is not replayable."),
                Part::text("Let me look."),
                Part::tool_call("toolu_0", "lookup", "{}"),
            ],
            later_steps,
        );
        let body = encode_request_with(&req, &ctx);
        assert!(body.get("thinking").is_none(), "{later_steps} later steps");
        assert_eq!(body["max_tokens"], json!(16_000));
    }

    // Once the loop is over and the user speaks again, thinking is back.
    let mut req = tool_loop_request(vec![Part::tool_call("toolu_0", "lookup", "{}")], 1);
    req.messages.push(Message::assistant_text("It is 43."));
    req.messages.push(Message::user_text("Thanks. And now?"));
    assert_eq!(encode_request_with(&req, &ctx)["thinking"], manual);

    // Sampling parameters survive when thinking does not.
    let mut req = tool_loop_request(vec![Part::tool_call("toolu_0", "lookup", "{}")], 0);
    req.temperature = Some(0.2);
    let body = encode_request_with(&req, &ctx);
    assert!(body.get("thinking").is_none());
    assert_eq!(body["temperature"], json!(0.2));
}

/// "Adaptive mode drops that rule."
#[test]
fn adaptive_thinking_is_sent_for_any_tool_loop() {
    let caps = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut req = tool_loop_request(vec![Part::tool_call("toolu_0", "lookup", "{}")], 2);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)));
    let body = encode_request_with(&req, &ctx);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "high"}));
}

/// The rule belongs to Anthropic's validator. Other vendors' models served
/// through a Messages-compatible endpoint take manual thinking for any
/// history, so nothing is taken away from them.
#[test]
fn manual_thinking_is_kept_for_models_that_are_not_claude() {
    let mut req = tool_loop_request(vec![Part::tool_call("toolu_0", "lookup", "{}")], 1);
    req.model = "glm-4.6".into();
    let body = encode_request(&req);
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192})
    );
}

/// A prefill is an assistant turn in progress too.
#[test]
fn manual_thinking_is_not_sent_with_a_prefill() {
    let caps = manual_only();
    let ctx = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(64_000),
        ..UpstreamCtx::default()
    };
    let mut req = request(vec![
        Message::user_text("List three colours."),
        Message::assistant_text("1."),
    ]);
    req.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(4096)));
    let body = encode_request_with(&req, &ctx);
    assert!(body.get("thinking").is_none());
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": [{"type": "text", "text": "1."}]})
    );
}
