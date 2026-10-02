//! What the pipeline does to a request on its way upstream and to the
//! answer on its way back: passthrough fidelity, model naming (aliases,
//! prefixes, excludes), reasoning suffixes, payload rules, tool-name
//! mapping and the reasoning store.

mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::fake::{complete, frames};
use support::{
    Answer, Behaviour, FOUR_PROVIDERS, Harness, Wire, arguments, body, codec, tool_declaration,
};
use switchyard_core::Protocol;
use switchyard_core::ir::FinishReason;
use switchyard_telemetry::Mode;

// ---------------------------------------------------------------------------
// Passthrough fidelity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn passthrough_forwards_the_clients_json_with_only_the_model_replaced() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Unknown fields, nested values, and an order no encoder would produce.
    let text = r#"{"zeta":1,"my_vendor_extension":{"keep":["me",2,{"x":true}]},"model":"m-chat","temperature":0.25,"messages":[{"role":"user","content":"hi","x-extra":null}],"alpha":"last"}"#;
    let client: Value = serde_json::from_str(text).unwrap();
    let output = harness
        .ask_with(Protocol::OpenaiChat, client, "m-chat", false)
        .await;
    assert_eq!(output.status, 200);

    let recorded = harness.fake.last();
    // Byte for byte what the client sent, apart from the model name: same
    // keys, same order, same values.
    assert_eq!(
        String::from_utf8_lossy(&recorded.raw),
        text.replace("\"m-chat\"", "\"up-chat\"")
    );
    assert_eq!(
        harness.record(&output.request_id).mode,
        Some(Mode::Passthrough)
    );
}

#[tokio::test]
async fn passthrough_forwards_the_response_with_only_the_model_renamed() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let answer = Answer::text("verbatim, please").with_usage(123, 45);
    for protocol in support::PROTOCOLS {
        let (model, upstream_model, key) = support::route(protocol);
        harness.fake.script(key, [Behaviour::Reply(answer.clone())]);
        let output = harness.ask(protocol, model, false).await;
        let upstream_body = complete(support::wire(protocol), upstream_model, &answer).to_string();
        assert_eq!(
            String::from_utf8_lossy(&output.body),
            upstream_body.replace(&format!("\"{upstream_model}\""), &format!("\"{model}\"")),
            "{protocol}"
        );
        // Usage is extracted on the side.
        let record = harness.record(&output.request_id);
        assert_eq!(
            (record.usage.input_tokens, record.usage.output_tokens),
            (123, 45),
            "{protocol}"
        );
    }
}

#[tokio::test]
async fn passthrough_streams_forward_the_upstreams_events() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let answer = Answer::text("streamed verbatim");
    for protocol in support::PROTOCOLS {
        let (model, upstream_model, key) = support::route(protocol);
        harness.fake.script(key, [Behaviour::Reply(answer.clone())]);
        let output = harness.ask(protocol, model, true).await;
        assert!(output.streamed, "{protocol}");
        // The gateway asks Chat upstreams for usage, so the fake sends it.
        let expected: String = frames(support::wire(protocol), upstream_model, &answer, true)
            .concat()
            .replace(&format!("\"{upstream_model}\""), &format!("\"{model}\""));
        assert_eq!(output.wire_text(), expected, "{protocol}");
        let record = harness.record(&output.request_id);
        assert_eq!(record.mode, Some(Mode::Passthrough), "{protocol}");
        assert_eq!(record.usage.output_tokens, 7, "{protocol}");
    }
}

#[tokio::test]
async fn a_body_with_a_foreign_signature_is_translated_not_forwarded() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // An Anthropic client replaying thinking that a Gemini upstream issued:
    // the signature is wrapped (`sy1.g.…`) and must never reach Anthropic.
    let client = json!({
        "model": "m-anthropic", "max_tokens": 256,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "from gemini", "signature": "sy1.g.Zm9yZWlnbg=="},
                {"type": "text", "text": "hello"}
            ]},
            {"role": "user", "content": "again"}
        ]
    });
    let output = harness
        .ask_with(Protocol::Anthropic, client, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200);
    let recorded = harness.fake.last();
    assert!(
        !String::from_utf8_lossy(&recorded.raw).contains("sy1."),
        "a wrapped signature reached the upstream: {}",
        recorded.body
    );
    assert!(!String::from_utf8_lossy(&recorded.raw).contains("Zm9yZWlnbg"));
    assert_eq!(
        harness.record(&output.request_id).mode,
        Some(Mode::Translated)
    );
}

// ---------------------------------------------------------------------------
// Model names
// ---------------------------------------------------------------------------

const NAMING: &str = r#"
[[providers]]
name = "one"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-one"]
prefix = "team"
exclude = ["hidden-*"]
[[providers.models]]
id = "real-model"
alias = "nice"
[[providers.models]]
id = "hidden-model"
[[providers.models]]
id = "plain"

[[providers]]
name = "two"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-two"]
[[providers.models]]
id = "claude-backup"

[[aliases]]
name = "smart"
targets = ["nice", "claude-backup"]

[[aliases]]
name = "pinned"
targets = ["plain(low)"]
"#;

#[tokio::test]
async fn provider_aliases_and_prefixes_route_to_the_upstream_id() {
    let harness = Harness::start(NAMING).await;
    for name in ["nice", "team/nice", "NICE"] {
        harness.fake.clear();
        let output = harness.ask(Protocol::OpenaiChat, name, false).await;
        assert_eq!(output.status, 200, "{name}: {:?}", output.body);
        let recorded = harness.fake.last();
        assert_eq!(recorded.model, "real-model", "{name}");
        assert_eq!(recorded.key, "key-one", "{name}");
        // The client is answered under the name it used.
        assert_eq!(output.json()["model"], name, "{name}");
    }
    // The upstream id itself is not a client-facing name when an alias is
    // configured.
    let output = harness.ask(Protocol::OpenaiChat, "real-model", false).await;
    assert_eq!(output.status, 404);
}

#[tokio::test]
async fn excluded_and_unknown_models_are_404_in_the_clients_envelope() {
    let harness = Harness::start(NAMING).await;
    for model in ["hidden-model", "team/hidden-model", "no-such-model"] {
        let output = harness.ask(Protocol::OpenaiChat, model, false).await;
        assert_eq!(output.status, 404, "{model}");
        let error = &output.json()["error"];
        assert_eq!(error["code"], "model_not_found", "{model}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains(model.rsplit('/').next().unwrap()),
            "{error}"
        );
    }
    assert_eq!(harness.fake.count(), 0, "nothing was sent upstream");

    // The same error in every protocol's own envelope.
    let anthropic = harness
        .ask(Protocol::Anthropic, "no-such-model", false)
        .await;
    assert_eq!(anthropic.status, 404);
    assert_eq!(anthropic.json()["type"], "error");
    assert_eq!(anthropic.json()["error"]["type"], "not_found_error");
    let gemini = harness.ask(Protocol::Gemini, "no-such-model", true).await;
    assert!(
        !gemini.streamed,
        "a failed stream request is a plain error reply"
    );
    assert_eq!(gemini.status, 404);
    assert_eq!(gemini.json()["error"]["status"], "NOT_FOUND");
    let responses = harness
        .ask(Protocol::OpenaiResponses, "no-such-model", false)
        .await;
    assert_eq!(responses.json()["error"]["code"], "model_not_found");

    let record = harness.record(&gemini.request_id);
    assert_eq!(record.status, 404);
    assert!(!record.ok);
    assert!(record.attempts.is_empty());
    assert_eq!(record.error.as_ref().unwrap().kind, "not_found");
}

#[tokio::test]
async fn an_alias_serves_its_first_target_and_falls_back_to_the_next() {
    let harness = Harness::start(NAMING).await;
    let output = harness.ask(Protocol::OpenaiChat, "smart", false).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.last().key, "key-one");
    assert_eq!(harness.fake.last().model, "real-model");
    assert_eq!(output.json()["model"], "smart");

    // The first target's only credential fails: the request moves on to
    // the second target, which speaks another protocol.
    harness.fake.clear();
    harness
        .fake
        .script("key-one", [Behaviour::error(500, "boom")]);
    let output = harness.ask(Protocol::OpenaiChat, "smart", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-one", "key-two"]);
    let fallback = harness.fake.last();
    assert_eq!(fallback.wire, Some(Wire::Anthropic));
    assert_eq!(fallback.model, "claude-backup");
    assert_eq!(output.json()["model"], "smart");
    assert_eq!(
        output.response(Protocol::OpenaiChat).text(),
        "Hello from the fake upstream"
    );

    let record = harness.record(&output.request_id);
    assert_eq!(record.client_model.as_deref(), Some("smart"));
    assert_eq!(record.provider.as_deref(), Some("two"));
    assert_eq!(record.upstream_model.as_deref(), Some("claude-backup"));
    assert_eq!(record.mode, Some(Mode::Translated));
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].provider, "one");
    assert_eq!(record.attempts[0].status, 500);
    assert!(!record.attempts[0].ok);
    assert!(record.attempts[1].ok);
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reasoning_suffix_is_written_into_a_passthrough_body() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let mut client = body(Protocol::OpenaiChat, "m-chat(high)", false, false);
    client["reasoning_effort"] = json!("low");
    let output = harness
        .ask_with(Protocol::OpenaiChat, client, "m-chat(high)", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let recorded = harness.fake.last();
    assert_eq!(recorded.body["model"], "up-chat", "the suffix is not sent");
    assert_eq!(
        recorded.body["reasoning_effort"], "high",
        "the suffix beats the body"
    );
    // The client is answered under the exact name it wrote.
    assert_eq!(output.json()["model"], "m-chat(high)");
    let record = harness.record(&output.request_id);
    assert_eq!(record.reasoning.as_deref(), Some("high"));
    assert_eq!(record.requested_model, "m-chat(high)");
    assert_eq!(record.client_model.as_deref(), Some("m-chat"));
    assert_eq!(record.mode, Some(Mode::Passthrough));
}

#[tokio::test]
async fn without_a_suffix_a_passthrough_body_keeps_its_own_reasoning() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let mut client = body(Protocol::OpenaiChat, "m-chat", false, false);
    client["reasoning_effort"] = json!("minimal");
    let output = harness
        .ask_with(Protocol::OpenaiChat, client, "m-chat", false)
        .await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.last().body["reasoning_effort"], "minimal");
    assert_eq!(
        harness.record(&output.request_id).reasoning.as_deref(),
        Some("minimal")
    );

    // Nothing requested: nothing invented.
    let output = harness.ask(Protocol::OpenaiChat, "m-chat", false).await;
    assert!(harness.fake.last().body.get("reasoning_effort").is_none());
    assert_eq!(harness.record(&output.request_id).reasoning, None);
}

#[tokio::test]
async fn a_reasoning_suffix_is_fitted_in_translation() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for (upstream, model) in [
        (Protocol::Anthropic, "m-anthropic(high)"),
        (Protocol::Gemini, "m-gemini(high)"),
        (Protocol::OpenaiResponses, "m-responses(high)"),
    ] {
        let output = harness.ask(Protocol::OpenaiChat, model, false).await;
        assert_eq!(output.status, 200, "{upstream}: {:?}", output.body);
        let recorded = harness.fake.last();
        let depth = codec(upstream).read_reasoning(&recorded.body).depth;
        assert_eq!(
            depth.map(|d| d.label()),
            Some("high"),
            "{upstream}: {}",
            recorded.body
        );
        assert_eq!(
            harness.record(&output.request_id).reasoning.as_deref(),
            Some("high"),
            "{upstream}"
        );
        assert_eq!(output.response(Protocol::OpenaiChat).model, model);
    }
}

#[tokio::test]
async fn body_reasoning_is_translated_and_a_suffix_overrides_it() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let client = |model: &str| {
        json!({
            "model": model,
            "input": "hi",
            "reasoning": {"effort": "low"}
        })
    };
    // Body only.
    let output = harness
        .ask_with(
            Protocol::OpenaiResponses,
            client("m-gemini"),
            "m-gemini",
            false,
        )
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let sent = codec(Protocol::Gemini).read_reasoning(&harness.fake.last().body);
    assert_eq!(sent.depth.map(|d| d.label()), Some("low"));

    // Suffix on top of the body.
    let output = harness
        .ask_with(
            Protocol::OpenaiResponses,
            client("m-gemini(none)"),
            "m-gemini(none)",
            false,
        )
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let sent = codec(Protocol::Gemini).read_reasoning(&harness.fake.last().body);
    assert_eq!(sent.depth.map(|d| d.label()), Some("none"));
    assert_eq!(
        harness.record(&output.request_id).reasoning.as_deref(),
        Some("none")
    );
}

#[tokio::test]
async fn a_depth_pinned_by_an_alias_beats_the_clients_suffix() {
    let harness = Harness::start(NAMING).await;
    let output = harness
        .ask(Protocol::OpenaiChat, "pinned(high)", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let recorded = harness.fake.last();
    assert_eq!(recorded.model, "plain");
    assert_eq!(recorded.body["reasoning_effort"], "low");
}

// ---------------------------------------------------------------------------
// Payload rules
// ---------------------------------------------------------------------------

const PAYLOAD: &str = r#"
[[payload.default]]
models = ["up-chat"]
set = { "temperature" = 0.5, "top_p" = 0.9 }

[[payload.override]]
models = ["m-chat"]
protocol = "openai-chat"
set = { "user" = "forced", "extra.nested" = true }

[[payload.filter]]
models = ["*"]
remove = ["metadata", "messages.0.name"]
"#;

#[tokio::test]
async fn payload_rules_default_override_and_filter_a_passthrough_body() {
    let harness = Harness::start(&format!("{FOUR_PROVIDERS}\n{PAYLOAD}")).await;
    let client = json!({
        "model": "m-chat",
        "messages": [{"role": "user", "content": "hi", "name": "bob"}],
        "temperature": 0.1,
        "user": "me",
        "metadata": {"trace": "abc"}
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, client, "m-chat", false)
        .await;
    assert_eq!(output.status, 200);
    let sent = harness.fake.last().body;
    assert_eq!(sent["temperature"], 0.1, "the client's own value is kept");
    assert_eq!(sent["top_p"], 0.9, "an absent field is defaulted");
    assert_eq!(sent["user"], "forced", "override always wins");
    assert_eq!(sent["extra"]["nested"], true);
    assert!(sent.get("metadata").is_none(), "filtered");
    assert!(
        sent["messages"][0].get("name").is_none(),
        "filtered by index"
    );
    assert_eq!(sent["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn payload_rules_apply_to_translated_bodies_in_the_upstreams_layout() {
    let harness = Harness::start(&format!("{FOUR_PROVIDERS}\n{PAYLOAD}")).await;
    // An Anthropic client on the Chat upstream: rules address Chat fields.
    let client = json!({
        "model": "m-chat", "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.3,
        "metadata": {"user_id": "u-1"}
    });
    let output = harness
        .ask_with(Protocol::Anthropic, client, "m-chat", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let sent = harness.fake.last().body;
    assert_eq!(sent["temperature"], 0.3);
    assert_eq!(sent["top_p"], 0.9);
    assert_eq!(sent["user"], "forced");
    assert!(sent.get("metadata").is_none());

    // The override is limited to Chat upstreams and the default to one
    // model: an Anthropic upstream sees neither. The filter matches all.
    let client = json!({
        "model": "m-anthropic",
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"trace": "abc"}
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, client, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let sent = harness.fake.last().body;
    assert!(sent.get("user").is_none(), "{sent}");
    assert!(sent.get("extra").is_none(), "{sent}");
    assert!(sent.get("top_p").is_none(), "{sent}");
    assert!(sent.get("metadata").is_none(), "{sent}");
}

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

/// A name OpenAI accepts and Gemini refuses, and the reverse.
const OPENAI_ONLY: &str = "1st-lookup";
const GEMINI_ONLY: &str = "mcp.server:get-data";

#[tokio::test]
async fn openai_tool_names_are_made_valid_for_gemini_and_restored() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for stream in [false, true] {
        let mut client = body(Protocol::OpenaiChat, "m-gemini", stream, false);
        client["tools"] = tool_declaration(Protocol::OpenaiChat, OPENAI_ONLY);
        client["tool_choice"] = json!({"type": "function", "function": {"name": OPENAI_ONLY}});
        harness.fake.script(
            "key-gemini-1",
            [Behaviour::Reply(Answer::tool(
                "_1st-lookup",
                json!({"city": "Oslo"}),
            ))],
        );
        let output = harness
            .ask_with(Protocol::OpenaiChat, client, "m-gemini", stream)
            .await;

        // Upstream: a name Gemini accepts, everywhere the name appears.
        let sent = harness.fake.last().body;
        let declared = &sent["tools"][0]["functionDeclarations"][0]["name"];
        assert_eq!(declared, "_1st-lookup", "stream={stream}: {sent}");
        assert!(
            !sent.to_string().contains("\"1st-lookup\""),
            "the original name leaked upstream: {sent}"
        );

        // Client: its own name again.
        let response = output.response(Protocol::OpenaiChat);
        let call = response.tool_calls().next().expect("a tool call");
        assert_eq!(call.name, OPENAI_ONLY, "stream={stream}");
        assert_eq!(arguments(&call.arguments), json!({"city": "Oslo"}));
        assert_eq!(response.finish, FinishReason::ToolCalls);
    }
}

#[tokio::test]
async fn gemini_tool_names_are_made_valid_for_openai_and_restored() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for (upstream, key) in [
        (Protocol::OpenaiChat, "key-chat-1"),
        (Protocol::OpenaiResponses, "key-responses-1"),
        (Protocol::Anthropic, "key-anthropic-1"),
    ] {
        let (model, _, _) = support::route(upstream);
        for stream in [false, true] {
            let label = format!("{upstream}, stream={stream}");
            // Second turn of a conversation: the name is in the
            // declaration, in the history's call and in its result.
            let client = json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "data please"}]},
                    {"role": "model", "parts": [{"functionCall": {
                        "name": GEMINI_ONLY, "args": {"city": "Rome"}
                    }}]},
                    {"role": "user", "parts": [{"functionResponse": {
                        "name": GEMINI_ONLY, "response": {"result": "sunny"}
                    }}]}
                ],
                "tools": tool_declaration(Protocol::Gemini, GEMINI_ONLY)
            });
            harness.fake.script(
                key,
                [Behaviour::Reply(Answer::tool(
                    "mcp_server_get-data",
                    json!({"city": "Oslo"}),
                ))],
            );
            let output = harness
                .ask_with(Protocol::Gemini, client, model, stream)
                .await;

            let sent = support::decode_upstream(&harness.fake.last(), upstream);
            assert_eq!(sent.tools[0].name(), Some("mcp_server_get-data"), "{label}");
            let history: Vec<_> = sent.messages.iter().flat_map(|m| m.tool_calls()).collect();
            assert_eq!(history[0].name, "mcp_server_get-data", "{label}");
            assert!(
                !harness.fake.last().body.to_string().contains(GEMINI_ONLY),
                "{label}: the original name leaked upstream"
            );

            let response = output.response(Protocol::Gemini);
            let call = response.tool_calls().next().expect("a tool call");
            assert_eq!(call.name, GEMINI_ONLY, "{label}");
            assert_eq!(
                arguments(&call.arguments),
                json!({"city": "Oslo"}),
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn names_valid_for_the_upstream_are_not_touched() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Valid for Gemini as it is: passthrough to Gemini and translation to
    // Gemini both leave it alone.
    for client in [Protocol::Gemini, Protocol::Anthropic] {
        let mut request = body(client, "m-gemini", false, false);
        request["tools"] = tool_declaration(client, "plain_tool");
        harness.ask_with(client, request, "m-gemini", false).await;
        let sent = harness.fake.last().body;
        assert_eq!(
            sent["tools"][0]["functionDeclarations"][0]["name"], "plain_tool",
            "{client}"
        );
    }
}

// ---------------------------------------------------------------------------
// Reasoning store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signed_thinking_is_restored_for_a_chat_client_on_an_anthropic_upstream() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let tools = tool_declaration(Protocol::OpenaiChat, "get_weather");

    // Turn 1: the upstream thinks (signed) and calls a tool.
    harness.fake.script(
        "key-anthropic-1",
        [Behaviour::Reply(
            Answer::tool("get_weather", json!({"city": "Paris"}))
                .with_reasoning("I should look the weather up.", "sig-anthropic-0123456789"),
        )],
    );
    let turn_one = json!({
        "model": "m-anthropic",
        "messages": [{"role": "user", "content": "weather in Paris?"}],
        "tools": tools,
        "reasoning_effort": "high"
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, turn_one, "m-anthropic", false)
        .await;
    let response = output.response(Protocol::OpenaiChat);
    let call = response.tool_calls().next().expect("a tool call").clone();
    // Chat Completions proper has no slot for the signature. The gateway
    // offers it in the `reasoning_details` extension, wrapped so it can
    // never be replayed to another vendor — but ordinary Chat clients drop
    // that field, which is the case the reasoning store exists for.
    assert!(String::from_utf8_lossy(&output.body).contains("sy1.a.sig-anthropic-0123456789"));

    // Turn 2: the client replays the call (without any thinking) and adds
    // the tool result.
    let turn_two = json!({
        "model": "m-anthropic",
        "messages": [
            {"role": "user", "content": "weather in Paris?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": call.id, "type": "function",
                "function": {"name": "get_weather", "arguments": call.arguments}
            }]},
            {"role": "tool", "tool_call_id": call.id, "content": "18 degrees"}
        ],
        "tools": tools,
        "reasoning_effort": "high"
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, turn_two.clone(), "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);

    let sent = harness.fake.last().body;
    let assistant = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap_or_else(|| panic!("no assistant turn upstream: {sent}"));
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking", "{assistant}");
    assert_eq!(blocks[0]["thinking"], "I should look the weather up.");
    assert_eq!(blocks[0]["signature"], "sig-anthropic-0123456789");
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[1]["id"], call.id.as_str());

    // Another client replaying the same ids gets nothing restored: hidden
    // reasoning never crosses client keys.
    let other = Harness::start(FOUR_PROVIDERS).await;
    let output = other
        .ask_with(Protocol::OpenaiChat, turn_two, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200);
    assert!(!other.fake.last().body.to_string().contains("sig-anthropic"));
}

#[tokio::test]
async fn streamed_thinking_is_remembered_too() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let tools = tool_declaration(Protocol::OpenaiChat, "get_weather");
    harness.fake.script(
        "key-anthropic-1",
        [Behaviour::Reply(
            Answer::tool("get_weather", json!({"city": "Paris"}))
                .with_reasoning("Streaming thoughts.", "sig-streamed-0123456789"),
        )],
    );
    let mut turn_one = json!({
        "model": "m-anthropic",
        "messages": [{"role": "user", "content": "weather in Paris?"}],
        "tools": tools,
        "reasoning_effort": "high",
        "stream": true
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, turn_one.clone(), "m-anthropic", true)
        .await;
    let call = output
        .response(Protocol::OpenaiChat)
        .tool_calls()
        .next()
        .expect("a tool call")
        .clone();

    turn_one["stream"] = json!(false);
    turn_one["messages"] = json!([
        {"role": "user", "content": "weather in Paris?"},
        {"role": "assistant", "content": null, "tool_calls": [{
            "id": call.id, "type": "function",
            "function": {"name": "get_weather", "arguments": call.arguments}
        }]},
        {"role": "tool", "tool_call_id": call.id, "content": "18 degrees"}
    ]);
    let output = harness
        .ask_with(Protocol::OpenaiChat, turn_one, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert!(
        harness
            .fake
            .last()
            .body
            .to_string()
            .contains("sig-streamed-0123456789"),
        "{}",
        harness.fake.last().body
    );
}

/// A Gemini client carries signatures itself (`thoughtSignature`), wrapped
/// and base64-armoured by the gateway while another vendor serves it. A
/// real Anthropic signature is long — several hundred characters — and has
/// to survive the round trip through the client unharmed: turn two must
/// reach Anthropic with the signed thinking block in front of the tool use.
#[tokio::test]
async fn a_gemini_client_replays_a_long_anthropic_signature() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let signature = format!("sig-{}", "A1b2".repeat(250));
    harness.fake.script(
        "key-anthropic-1",
        [Behaviour::Reply(
            Answer::tool("get_weather", json!({"city": "Paris"}))
                .with_reasoning("Paris, then.", &signature),
        )],
    );
    let mut request = body(Protocol::Gemini, "m-anthropic", false, true);
    request["generationConfig"] =
        json!({"thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}});
    let output = harness
        .ask_with(Protocol::Gemini, request.clone(), "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let parts = output.json()["candidates"][0]["content"]["parts"].clone();
    assert!(
        !parts.to_string().contains(&signature),
        "the client is handed the wrapped form, not Anthropic's own bytes"
    );

    // The client replays the model turn exactly as it received it.
    let mut contents = request["contents"].as_array().cloned().unwrap();
    contents.push(json!({"role": "model", "parts": parts}));
    contents.push(json!({"role": "user", "parts": [{"functionResponse": {
        "name": "get_weather", "response": {"temperature": "18 degrees"}
    }}]}));
    request["contents"] = json!(contents);
    let output = harness
        .ask_with(Protocol::Gemini, request, "m-anthropic", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);

    let upstream = harness.fake.last().body;
    let blocks = upstream["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "Paris, then.");
    assert_eq!(blocks[0]["signature"], signature.as_str());
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[1]["name"], "get_weather");
}
