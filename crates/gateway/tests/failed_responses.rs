//! A complete Responses body that reports a failed generation.
//!
//! Without streaming, the Responses API reports a generation that failed
//! with HTTP `200` and a response object whose `status` is `"failed"`; the
//! `error` inside says why. That reason decides what the gateway does, just
//! as an HTTP error or a `response.failed` stream event of the same meaning
//! would: a rate limit rests the model on the credential and the next
//! credential is tried, exhausted quota rests the key, a server error fails
//! over, and a fault of the request ends it with a `400` in the client's
//! protocol without resting anything. That holds whatever the failed
//! generation had produced before it failed: nothing of it has reached the
//! client.

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{Behaviour, Harness, PROTOCOLS};
use switchyard_core::{FailureClass, Protocol};
use switchyard_scheduler::{CredentialSnapshot, CredentialStatus};

/// One Responses provider with two keys, tried in order.
const TWO_KEYS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "oai"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#;

fn credentials(harness: &Harness) -> Vec<CredentialSnapshot> {
    harness
        .gateway
        .scheduler()
        .snapshot()
        .into_iter()
        .flat_map(|provider| provider.credentials)
        .collect()
}

#[tokio::test]
async fn a_rate_limited_response_fails_over_and_rests_the_model() {
    // Translated (Chat, Anthropic, Gemini clients) and passthrough alike.
    for client in PROTOCOLS {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [Behaviour::failed_response(
                "rate_limit_exceeded",
                "Rate limit reached for up-responses. Please try again in 20s.",
            )],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(output.status, 200, "{client}: {:?}", output.body);
        assert_eq!(
            output.response(client).text(),
            "Hello from the fake upstream",
            "{client}"
        );
        assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{client}");

        let creds = credentials(&harness);
        assert_eq!(
            creds[0].model_cooldowns.len(),
            1,
            "{client}: {:?}",
            creds[0]
        );
        assert_eq!(
            creds[0].model_cooldowns[0].reason,
            FailureClass::RateLimit,
            "{client}"
        );
        assert_eq!(creds[0].failures, 1, "{client}");
        assert_eq!(creds[0].last_error.as_ref().map(|e| e.status), Some(429));
        assert_eq!(creds[1].successes, 1, "{client}");

        let record = harness.record(&output.request_id);
        assert!(record.ok, "{client}: {record:?}");
        assert_eq!(record.attempts.len(), 2, "{client}");
        assert_eq!(record.attempts[0].status, 429, "{client}");
        assert!(
            record.attempts[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Rate limit reached"),
            "{client}: {:?}",
            record.attempts[0]
        );

        // While the model rests on the first key, requests go to the second.
        harness.fake.clear();
        assert_eq!(harness.ask(client, "m", false).await.status, 200);
        assert_eq!(harness.fake.keys(), vec!["key-b"], "{client}");
    }
}

#[tokio::test]
async fn exhausted_quota_rests_the_whole_credential() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::failed_response(
            "insufficient_quota",
            "You exceeded your current quota, please check your plan and billing details.",
        )],
    );
    let output = harness.ask(Protocol::Anthropic, "m", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);

    let creds = credentials(&harness);
    assert_eq!(creds[0].status, CredentialStatus::Cooling, "{:?}", creds[0]);
    assert_eq!(creds[0].cooldown_reason, Some(FailureClass::Quota));
    assert_eq!(creds[1].status, CredentialStatus::Ready);
}

#[tokio::test]
async fn a_server_error_fails_over() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::failed_response(
            "server_error",
            "The model failed to generate a response.",
        )],
    );
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    let creds = credentials(&harness);
    assert_eq!(creds[0].failures, 1);
    assert_eq!(
        creds[0].model_cooldowns.first().map(|rest| rest.reason),
        Some(FailureClass::Server),
        "{:?}",
        creds[0]
    );
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts[0].status, 502);
}

#[tokio::test]
async fn when_every_credential_is_rate_limited_the_client_is_told_429() {
    for client in PROTOCOLS {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.always(
            "key-a",
            Behaviour::failed_response("rate_limit_exceeded", "Slow down, please."),
        );
        harness.fake.always(
            "key-b",
            Behaviour::failed_response("rate_limit_exceeded", "Slow down, please."),
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(output.status, 429, "{client}: {:?}", output.body);
        assert_eq!(
            output.error_message(client),
            "Slow down, please.",
            "{client}"
        );
        assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{client}");
        let record = harness.record(&output.request_id);
        assert!(!record.ok, "{client}");
        assert_eq!(
            record.error.as_ref().map(|error| error.kind.as_str()),
            Some("rate_limit"),
            "{client}: {:?}",
            record.error
        );
    }
}

#[tokio::test]
async fn a_request_fault_is_answered_with_400_and_rests_nothing() {
    for code in [
        "invalid_prompt",
        "context_length_exceeded",
        "string_above_max_length",
        "invalid_image",
    ] {
        for client in PROTOCOLS {
            let label = format!("{code}, {client}");
            let harness = Harness::start(TWO_KEYS).await;
            harness.fake.script(
                "key-a",
                [Behaviour::failed_response(
                    code,
                    "Your input was rejected for a reason only you can fix.",
                )],
            );
            let output = harness.ask(client, "m", false).await;
            assert_eq!(output.status, 400, "{label}: {:?}", output.body);
            assert_eq!(
                output.error_message(client),
                "Your input was rejected for a reason only you can fix.",
                "{label}"
            );
            // The body is an error in the client's own protocol, not the
            // upstream's response object.
            assert!(output.json().get("error").is_some(), "{label}");
            assert!(output.json().get("output").is_none(), "{label}");
            // No other credential would fare better, and none is tried.
            assert_eq!(harness.fake.keys(), vec!["key-a"], "{label}");
            for credential in credentials(&harness) {
                assert_eq!(credential.status, CredentialStatus::Ready, "{label}");
                assert!(credential.model_cooldowns.is_empty(), "{label}");
                assert_eq!(credential.failures, 0, "{label}");
            }
            let record = harness.record(&output.request_id);
            assert_eq!(record.attempts.len(), 1, "{label}");
            assert_eq!(record.attempts[0].status, 400, "{label}");
            assert_eq!(
                record.error.as_ref().map(|error| error.kind.as_str()),
                Some("invalid_request"),
                "{label}: {:?}",
                record.error
            );

            // The model is still in rotation on the same credential.
            harness.fake.clear();
            assert_eq!(harness.ask(client, "m", false).await.status, 200, "{label}");
            assert_eq!(harness.fake.keys(), vec!["key-a"], "{label}");
        }
    }
}

#[tokio::test]
async fn the_vendor_code_reaches_openai_clients() {
    let harness = Harness::start(TWO_KEYS).await;
    for client in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
        harness.fake.script(
            "key-a",
            [Behaviour::failed_response("invalid_prompt", "Flagged.")],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(output.status, 400, "{client}");
        assert_eq!(output.json()["error"]["code"], "invalid_prompt", "{client}");
        assert_eq!(
            output.json()["error"]["type"],
            "invalid_request_error",
            "{client}"
        );
    }
}

// ---------------------------------------------------------------------------
// A generation that failed after it had produced something
// ---------------------------------------------------------------------------

/// A complete Responses body of a generation that failed (HTTP `200`,
/// `status: "failed"`) after producing `output`.
fn failed_after(output: Value, code: &str, message: &str) -> Behaviour {
    Behaviour::Json {
        status: 200,
        body: json!({
            "id": "resp_failed2", "object": "response", "created_at": 1_700_000_000,
            "status": "failed", "model": "up-responses",
            "output": output,
            "error": {"code": code, "message": message},
            "incomplete_details": null,
            "usage": null
        }),
    }
}

/// What a generation may have produced by the time it fails: reasoning, a
/// hosted tool call, the first words of the answer, a tool call.
fn partial_outputs() -> Vec<(&'static str, Value)> {
    vec![
        (
            "reasoning",
            json!([{"type": "reasoning", "id": "rs_0123456789abcdef", "summary": [],
                    "encrypted_content": "gAAAAABo-encrypted-reasoning-0123456789abcdefghij"}]),
        ),
        (
            "hosted tool",
            json!([{"type": "file_search_call", "id": "fs_1", "status": "completed",
                    "queries": ["weather"], "results": null}]),
        ),
        (
            "partial text",
            json!([{"type": "message", "id": "msg_1", "role": "assistant", "status": "incomplete",
                    "content": [{"type": "output_text", "text": "The answer is",
                                 "annotations": []}]}]),
        ),
        (
            "tool call",
            json!([{"type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "get_weather", "arguments": "{\"city\":", "status": "incomplete"}]),
        ),
    ]
}

/// Without streaming nothing has reached the client when the upstream says
/// the generation failed, so whatever the body still carries the attempt
/// failed: the model rests as the error code demands, the next credential
/// answers in full, and nothing of the failed generation is delivered.
#[tokio::test]
async fn a_generation_that_failed_after_producing_output_is_a_failed_attempt() {
    for (what, output) in partial_outputs() {
        for client in PROTOCOLS {
            let label = format!("{what}, {client}");
            let harness = Harness::start(TWO_KEYS).await;
            harness.fake.script(
                "key-a",
                [failed_after(
                    output.clone(),
                    "server_error",
                    "The model failed to generate a response.",
                )],
            );
            let output = harness.ask(client, "m", false).await;
            assert_eq!(output.status, 200, "{label}: {:?}", output.body);
            let response = output.response(client);
            assert_eq!(response.text(), "Hello from the fake upstream", "{label}");
            assert_eq!(response.tool_calls().count(), 0, "{label}");
            assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{label}");

            let creds = credentials(&harness);
            assert_eq!(
                (creds[0].successes, creds[0].failures),
                (0, 1),
                "{label}: {:?}",
                creds[0]
            );
            assert_eq!(
                creds[0].model_cooldowns.first().map(|rest| rest.reason),
                Some(FailureClass::Server),
                "{label}"
            );
            let record = harness.record(&output.request_id);
            assert!(record.ok, "{label}");
            assert_eq!(record.attempts.len(), 2, "{label}");
            assert_eq!(
                (record.attempts[0].ok, record.attempts[0].status),
                (false, 502),
                "{label}"
            );
        }
    }
}

/// With nobody left to try, the client is told about the failure in its own
/// protocol's error envelope — a Responses client too, which is not handed
/// the upstream's `200`.
#[tokio::test]
async fn a_failed_generation_with_output_is_never_answered_200() {
    for (code, status) in [
        ("rate_limit_exceeded", 429),
        ("server_error", 502),
        ("context_length_exceeded", 400),
    ] {
        for (what, output) in partial_outputs() {
            for client in PROTOCOLS {
                let label = format!("{code}, {what}, {client}");
                let harness = Harness::start(TWO_KEYS).await;
                for key in ["key-a", "key-b"] {
                    harness.fake.always(
                        key,
                        failed_after(output.clone(), code, "The generation did not finish."),
                    );
                }
                let output = harness.ask(client, "m", false).await;
                assert_eq!(output.status, status, "{label}: {:?}", output.body);
                assert_eq!(
                    output.error_message(client),
                    "The generation did not finish.",
                    "{label}"
                );
                assert!(output.json().get("error").is_some(), "{label}");
                assert!(output.json().get("output").is_none(), "{label}");
                assert!(
                    !String::from_utf8_lossy(&output.body).contains("The answer is"),
                    "{label}"
                );
                // A request fault is nobody's fault but the request's: one
                // call, nothing rests. Everything else is tried everywhere.
                let expected: Vec<&str> = if status == 400 {
                    vec!["key-a"]
                } else {
                    vec!["key-a", "key-b"]
                };
                assert_eq!(harness.fake.keys(), expected, "{label}");
                let record = harness.record(&output.request_id);
                assert!(!record.ok, "{label}");
                for credential in credentials(&harness) {
                    assert_eq!(credential.successes, 0, "{label}");
                    assert_eq!(
                        credential.model_cooldowns.is_empty(),
                        status == 400,
                        "{label}: {credential:?}"
                    );
                }
            }
        }
    }
}

/// The terminal stream event as the body of a non-streaming call, which
/// some upstreams answer with, is read the same way.
#[tokio::test]
async fn a_failed_terminal_event_as_the_body_is_a_failed_attempt_too() {
    for client in PROTOCOLS {
        let harness = Harness::start(TWO_KEYS).await;
        let (_, reasoning) = partial_outputs().swap_remove(0);
        harness.fake.script(
            "key-a",
            [Behaviour::Json {
                status: 200,
                body: json!({"type": "response.failed", "sequence_number": 7, "response": {
                    "id": "resp_failed3", "object": "response", "status": "failed",
                    "model": "up-responses",
                    "output": reasoning,
                    "error": {"code": "rate_limit_exceeded", "message": "Slow down, please."}
                }}),
            }],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(output.status, 200, "{client}: {:?}", output.body);
        assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{client}");
        assert_eq!(
            credentials(&harness)[0]
                .model_cooldowns
                .first()
                .map(|rest| rest.reason),
            Some(FailureClass::RateLimit),
            "{client}"
        );
    }
}

/// Only `failed` is a failure. A response that stopped early (`incomplete`)
/// or that carries an `error` member next to a finished answer is delivered
/// as the answer it is, by the credential that produced it.
#[tokio::test]
async fn responses_that_did_not_fail_are_still_delivered() {
    let message = |status: &str, text: &str| {
        json!([{"type": "message", "id": "msg_1", "role": "assistant", "status": status,
                "content": [{"type": "output_text", "text": text, "annotations": []}]}])
    };
    let usage = json!({"input_tokens": 5, "output_tokens": 3, "total_tokens": 8});
    let bodies = [
        json!({"id": "resp_1", "object": "response", "status": "incomplete",
               "model": "up-responses", "output": message("incomplete", "Cut sho"),
               "incomplete_details": {"reason": "max_output_tokens"}, "error": null,
               "usage": usage}),
        json!({"id": "resp_2", "object": "response", "status": "completed",
               "model": "up-responses", "output": message("completed", "All of it."),
               "error": {"code": "server_error", "message": "a note nobody should mind"},
               "usage": usage}),
    ];
    for (body, text) in bodies.into_iter().zip(["Cut sho", "All of it."]) {
        for client in PROTOCOLS {
            let harness = Harness::start(TWO_KEYS).await;
            harness.fake.script(
                "key-a",
                [Behaviour::Json {
                    status: 200,
                    body: body.clone(),
                }],
            );
            let output = harness.ask(client, "m", false).await;
            assert_eq!(output.status, 200, "{client}: {:?}", output.body);
            assert_eq!(output.response(client).text(), text, "{client}");
            assert_eq!(harness.fake.keys(), vec!["key-a"], "{client}");
            let creds = credentials(&harness);
            assert_eq!((creds[0].successes, creds[0].failures), (1, 0), "{client}");
            assert!(harness.record(&output.request_id).ok, "{client}");
        }
    }
}
