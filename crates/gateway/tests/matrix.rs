//! Every client protocol against every upstream protocol: complete
//! responses and streams, plain text and a tool call. The client's output
//! is read with the client protocol's own decoder; the upstream request is
//! read with the upstream protocol's own decoder.

mod support;

use pretty_assertions::assert_eq;
use serde_json::json;
use support::{
    Answer, Behaviour, FOUR_PROVIDERS, Harness, Kind, PROTOCOLS, arguments, body, decode_upstream,
    route, wire,
};
use switchyard_core::Protocol;
use switchyard_core::ir::{FinishReason, Part, Role};
use switchyard_telemetry::{Mode, Transport};

/// Runs one cell of the matrix and checks everything about it.
async fn cell(harness: &Harness, client: Protocol, upstream: Protocol, stream: bool, tool: bool) {
    let label = format!("{client} -> {upstream}, stream={stream}, tool={tool}");
    let (model, upstream_model, key) = route(upstream);
    let answer = if tool {
        Answer::tool("get_weather", json!({"city": "Paris"}))
    } else {
        Answer::text("Hello from the upstream, in two halves.")
    };
    harness.fake.clear();
    harness.fake.script(key, [Behaviour::Reply(answer)]);

    let request = harness.request_with(client, body(client, model, stream, tool), model, stream);
    let output = support::Output::read(harness.gateway.generate(request).await).await;

    // --- what the client received ---------------------------------------
    assert_eq!(output.streamed, stream, "{label}: reply kind");
    let response = output.response(client);
    assert_eq!(
        response.model, model,
        "{label}: the client sees the name it asked for"
    );
    if tool {
        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1, "{label}: {response:?}");
        assert_eq!(calls[0].name, "get_weather", "{label}");
        assert_eq!(
            arguments(&calls[0].arguments),
            json!({"city": "Paris"}),
            "{label}"
        );
        assert!(!calls[0].id.is_empty(), "{label}: tool calls carry an id");
        assert_eq!(response.finish, FinishReason::ToolCalls, "{label}");
    } else {
        assert_eq!(
            response.text(),
            "Hello from the upstream, in two halves.",
            "{label}"
        );
        assert_eq!(response.finish, FinishReason::Stop, "{label}");
        assert_eq!(response.tool_calls().count(), 0, "{label}");
    }
    assert_eq!(
        response.usage.input_tokens, 11,
        "{label}: usage reaches the client"
    );
    assert_eq!(response.usage.output_tokens, 7, "{label}");
    assert_eq!(
        output.header("x-request-id"),
        Some(output.request_id.as_str())
    );
    assert_eq!(output.header("x-switchyard-model"), Some(upstream_model));

    // --- what the upstream received -------------------------------------
    assert_eq!(
        harness.fake.count(),
        1,
        "{label}: exactly one upstream call"
    );
    let recorded = harness.fake.last();
    assert_eq!(
        recorded.wire,
        Some(wire(upstream)),
        "{label}: upstream endpoint"
    );
    assert_eq!(
        recorded.kind,
        Kind::Generate { stream },
        "{label}: upstream stream flag"
    );
    assert_eq!(recorded.key, key, "{label}: upstream credential");
    assert_eq!(recorded.model, upstream_model, "{label}: upstream model id");
    let sent = decode_upstream(&recorded, upstream);
    assert_eq!(sent.model, upstream_model, "{label}");
    assert_eq!(sent.stream, stream, "{label}");
    let user: Vec<_> = sent
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .collect();
    assert_eq!(user.len(), 1, "{label}: {sent:?}");
    assert_eq!(user[0].text(), "hi", "{label}");
    assert!(
        !user[0]
            .parts
            .iter()
            .any(|p| matches!(p, Part::ToolResult(_))),
        "{label}"
    );
    if tool {
        assert_eq!(sent.tools.len(), 1, "{label}: {sent:?}");
        assert_eq!(sent.tools[0].name(), Some("get_weather"), "{label}");
    } else {
        assert!(sent.tools.is_empty(), "{label}");
    }
    // The client's gateway key never travels upstream.
    assert!(
        !String::from_utf8_lossy(&recorded.raw).contains(support::CLIENT_KEY),
        "{label}"
    );
    for (name, value) in &recorded.headers {
        assert!(
            !value.to_str().unwrap_or("").contains(support::CLIENT_KEY),
            "{label}: header {name} leaks the client key"
        );
    }

    // --- what was recorded -----------------------------------------------
    let record = harness.record(&output.request_id);
    assert!(record.ok, "{label}: {record:?}");
    assert_eq!(record.status, 200, "{label}");
    assert_eq!(record.client_protocol, client, "{label}");
    assert_eq!(record.upstream_protocol, Some(upstream), "{label}");
    assert_eq!(
        record.mode,
        Some(if client == upstream {
            Mode::Passthrough
        } else {
            Mode::Translated
        }),
        "{label}"
    );
    assert_eq!(record.requested_model, model, "{label}");
    assert_eq!(record.client_model.as_deref(), Some(model), "{label}");
    assert_eq!(
        record.upstream_model.as_deref(),
        Some(upstream_model),
        "{label}"
    );
    assert_eq!(record.stream, stream, "{label}");
    assert_eq!(
        record.transport,
        if stream {
            Transport::Sse
        } else {
            Transport::Http
        },
        "{label}"
    );
    assert_eq!(record.usage.input_tokens, 11, "{label}: {:?}", record.usage);
    assert_eq!(record.usage.output_tokens, 7, "{label}");
    assert_eq!(record.attempts.len(), 1, "{label}");
    assert!(record.attempts[0].ok, "{label}");
    assert!(record.ttfb_ms.is_some(), "{label}");
    assert_eq!(record.client.key_name.as_deref(), Some("tester"), "{label}");
}

async fn matrix(stream: bool, tool: bool) {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for client in PROTOCOLS {
        for upstream in PROTOCOLS {
            cell(&harness, client, upstream, stream, tool).await;
        }
    }
    // Nothing is left counted as in flight or streaming.
    support::eventually("gauges return to zero", || {
        let gauges = harness.gateway.telemetry().gauges();
        gauges.in_flight() == 0 && gauges.active_streams() == 0
    })
    .await;
    let totals = harness.gateway.telemetry().gauges().totals();
    assert_eq!(totals.requests, 16);
}

#[tokio::test]
async fn text_complete_all_sixteen_pairs() {
    matrix(false, false).await;
}

#[tokio::test]
async fn text_streamed_all_sixteen_pairs() {
    matrix(true, false).await;
}

#[tokio::test]
async fn tool_call_complete_all_sixteen_pairs() {
    matrix(false, true).await;
}

#[tokio::test]
async fn tool_call_streamed_all_sixteen_pairs() {
    matrix(true, true).await;
}

/// A tool result travelling back: the second turn of a tool conversation
/// in every client protocol reaches every upstream as a well-formed tool
/// result.
#[tokio::test]
async fn tool_results_reach_every_upstream() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for client in PROTOCOLS {
        for upstream in PROTOCOLS {
            let (model, _, _) = route(upstream);
            let turn_two = match client {
                Protocol::OpenaiChat => json!({
                    "model": model,
                    "messages": [
                        {"role": "user", "content": "weather in Paris?"},
                        {"role": "assistant", "content": null, "tool_calls": [{
                            "id": "call_1", "type": "function",
                            "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                        }]},
                        {"role": "tool", "tool_call_id": "call_1", "content": "18 degrees"}
                    ],
                    "tools": support::tool_declaration(client, "get_weather")
                }),
                Protocol::OpenaiResponses => json!({
                    "model": model,
                    "input": [
                        {"role": "user", "content": [{"type": "input_text", "text": "weather in Paris?"}]},
                        {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                         "arguments": "{\"city\":\"Paris\"}"},
                        {"type": "function_call_output", "call_id": "call_1", "output": "18 degrees"}
                    ],
                    "tools": support::tool_declaration(client, "get_weather")
                }),
                Protocol::Anthropic => json!({
                    "model": model, "max_tokens": 256,
                    "messages": [
                        {"role": "user", "content": "weather in Paris?"},
                        {"role": "assistant", "content": [{
                            "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                            "input": {"city": "Paris"}
                        }]},
                        {"role": "user", "content": [{
                            "type": "tool_result", "tool_use_id": "toolu_1", "content": "18 degrees"
                        }]}
                    ],
                    "tools": support::tool_declaration(client, "get_weather")
                }),
                Protocol::Gemini => json!({
                    "contents": [
                        {"role": "user", "parts": [{"text": "weather in Paris?"}]},
                        {"role": "model", "parts": [{"functionCall": {
                            "name": "get_weather", "args": {"city": "Paris"}
                        }}]},
                        {"role": "user", "parts": [{"functionResponse": {
                            "name": "get_weather", "response": {"result": "18 degrees"}
                        }}]}
                    ],
                    "tools": support::tool_declaration(client, "get_weather")
                }),
            };
            harness.fake.clear();
            let output = harness.ask_with(client, turn_two, model, false).await;
            let label = format!("{client} -> {upstream}");
            assert_eq!(output.status, 200, "{label}: {:?}", output.body);

            let sent = decode_upstream(&harness.fake.last(), upstream);
            let calls: Vec<_> = sent.messages.iter().flat_map(|m| m.tool_calls()).collect();
            assert_eq!(calls.len(), 1, "{label}: {sent:?}");
            assert_eq!(calls[0].name, "get_weather", "{label}");
            assert_eq!(
                arguments(&calls[0].arguments),
                json!({"city": "Paris"}),
                "{label}"
            );
            let results: Vec<_> = sent
                .messages
                .iter()
                .flat_map(|m| m.tool_results())
                .collect();
            assert_eq!(results.len(), 1, "{label}: {sent:?}");
            assert!(
                results[0].text().contains("18 degrees"),
                "{label}: {:?}",
                results[0]
            );
            // The result answers the call.
            assert_eq!(results[0].call_id, calls[0].id, "{label}");
        }
    }
}
