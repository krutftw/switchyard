//! Regression test (QA finding SU-1): payload rules never reached the
//! built-in mock provider, although the rule editor offers it as a target —
//! on a first run, where the mock is the only provider, a rule could not be
//! tried out at all.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use serde_json::{Value, json};
use support::Harness;
use switchyard_core::Protocol;

const CONFIG: &str = r#"
[logging]
request_log = "all"

[[providers]]
name = "mock"
kind = "mock"

[[payload.override]]
models = ["mock-echo"]
[payload.override.set]
temperature = 0.33
"metadata.qa" = "from-rule"

[[payload.filter]]
models = ["mock-*"]
remove = ["user"]

[[payload.default]]
models = ["mock-echo"]
[payload.default.set]
top_p = 0.5
max_tokens = 77
"#;

/// The upstream request captured for a request: for the mock, the
/// canonical request it answered.
async fn upstream_request(harness: &Harness, request_id: &str) -> Value {
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(request_id)
        .expect("captured");
    serde_json::from_str(&bodies.upstream_request.expect("an upstream request")).unwrap()
}

#[tokio::test]
async fn payload_rules_patch_what_the_mock_provider_is_sent() {
    let harness = Harness::start(CONFIG).await;

    let chat = json!({
        "model": "mock-echo",
        "temperature": 0.9,
        "max_tokens": 5,
        "user": "u-123",
        "metadata": {"keep": "yes"},
        "messages": [{"role": "user", "content": "hello there"}],
    });
    let output = harness
        .ask_with(Protocol::OpenaiChat, chat.clone(), "mock-echo", false)
        .await;
    assert_eq!(output.status, 200, "{}", output.wire_text());
    let sent = upstream_request(&harness, &output.request_id).await;
    assert_eq!(sent["temperature"], 0.33, "{sent}");
    assert_eq!(sent["metadata"], json!({"keep": "yes", "qa": "from-rule"}));
    assert!(sent.get("user").is_none_or(Value::is_null), "{sent}");
    // `default` fills what the client left out, and only that.
    assert_eq!(sent["top_p"], 0.5, "{sent}");
    assert_eq!(sent["max_output_tokens"], 5, "{sent}");
    // The mock answers the patched request: it still echoes the user text.
    let answer = output.json()["choices"][0]["message"]["content"].clone();
    assert!(answer.as_str().unwrap().contains("hello"), "{answer}");

    // Streaming takes the same path.
    let mut streamed = chat.clone();
    streamed["stream"] = json!(true);
    let output = harness
        .ask_with(Protocol::OpenaiChat, streamed, "mock-echo", true)
        .await;
    assert_eq!(output.status, 200);
    let sent = upstream_request(&harness, &output.request_id).await;
    assert_eq!(sent["temperature"], 0.33, "{sent}");

    // Messages: rules are written in the layout of the protocol the mock
    // stands in for, the client's.
    let messages = json!({
        "model": "mock-lorem",
        "max_tokens": 64,
        "temperature": 0.9,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let output = harness
        .ask_with(Protocol::Anthropic, messages, "mock-lorem", false)
        .await;
    assert_eq!(output.status, 200);
    let sent = upstream_request(&harness, &output.request_id).await;
    // Only the filter matches `mock-lorem`; the override is for `mock-echo`.
    assert_eq!(sent["temperature"], 0.9, "{sent}");

    // Without matching rules the request is untouched.
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            json!({
                "model": "mock-think",
                "temperature": 0.9,
                "messages": [{"role": "user", "content": "hi"}],
            }),
            "mock-think",
            false,
        )
        .await;
    assert_eq!(output.status, 200);
    let sent = upstream_request(&harness, &output.request_id).await;
    assert_eq!(sent["temperature"], 0.9, "{sent}");
}
