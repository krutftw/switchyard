//! Regression tests (review finding GW-F1): a complete Responses body with
//! `status: "failed"` is a failed attempt whether or not its `output` is
//! empty.
//!
//! A non-stream Responses body with status `"failed"` is classified by the
//! code inside `response.error` (`rate_limit_exceeded` -> rate limit with
//! failover, `server_error` -> server failure with failover, ...). That
//! used to be decided only after `decode_response` had *refused* the body,
//! and the codec refuses a failed body only when it decodes to no part at
//! all.
//!
//! Reasoning models produce a `reasoning` output item before anything else,
//! and every request the gateway translates for a Responses upstream asks
//! for `reasoning.encrypted_content` — so the item of a generation that
//! failed after it started thinking decodes to a part, the body counted as
//! a success, and
//!
//! * the client was answered `200` with an empty message (a Chat client
//!   was told `finish_reason: "length"`),
//! * the scheduler was told the credential succeeded, so a rate-limited key
//!   did not rest,
//! * no other credential was tried, although nothing had been sent to the
//!   client yet.
//!
//! The same happened when the failed body carried the text produced before
//! the failure: the truncated text was delivered as a successful answer.
//! The status is now read before the body is decoded
//! (`target::declared_failure`).

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{Behaviour, Harness};
use switchyard_core::{FailureClass, Protocol};
use switchyard_scheduler::CredentialSnapshot;

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

/// A complete Responses body of a generation that failed (HTTP `200`,
/// `status: "failed"`) after producing `output`.
fn failed_after(output: Value, code: &str, message: &str) -> Behaviour {
    Behaviour::Json {
        status: 200,
        body: json!({
            "id": "resp_failed1", "object": "response", "created_at": 1_700_000_000,
            "status": "failed", "model": "up-responses",
            "output": output,
            "error": {"code": code, "message": message},
            "incomplete_details": null,
            "usage": null
        }),
    }
}

/// The reasoning item a reasoning model has produced by the time it fails:
/// no summary, and the encrypted reasoning the gateway asked for.
fn reasoning_only() -> Value {
    json!([{
        "type": "reasoning", "id": "rs_0123456789abcdef", "summary": [],
        "encrypted_content": "gAAAAABo-encrypted-reasoning-0123456789abcdefghijklmnopqrstuvwxyz"
    }])
}

fn credentials(harness: &Harness) -> Vec<CredentialSnapshot> {
    harness
        .gateway
        .scheduler()
        .snapshot()
        .into_iter()
        .flat_map(|provider| provider.credentials)
        .collect()
}

/// Translated clients: a rate-limited generation that got as far as its
/// reasoning item is still a rate-limited generation.
#[tokio::test]
async fn a_rate_limited_response_that_had_started_reasoning_fails_over() {
    for client in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [failed_after(
                reasoning_only(),
                "rate_limit_exceeded",
                "Rate limit reached for up-responses. Please try again in 20s.",
            )],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(
            harness.fake.keys(),
            vec!["key-a", "key-b"],
            "{client}: the next credential must be tried; the client got {}: {}",
            output.status,
            String::from_utf8_lossy(&output.body)
        );
        assert_eq!(output.status, 200, "{client}: {:?}", output.body);
        assert_eq!(
            output.response(client).text(),
            "Hello from the fake upstream",
            "{client}"
        );

        let creds = credentials(&harness);
        assert_eq!(creds[0].failures, 1, "{client}: {:?}", creds[0]);
        assert_eq!(creds[0].successes, 0, "{client}: {:?}", creds[0]);
        assert_eq!(
            creds[0].model_cooldowns.first().map(|rest| rest.reason),
            Some(FailureClass::RateLimit),
            "{client}: {:?}",
            creds[0]
        );
        let record = harness.record(&output.request_id);
        assert_eq!(record.attempts.len(), 2, "{client}");
        assert_eq!(record.attempts[0].status, 429, "{client}");
    }
}

/// With nothing else to try, the client is told about the failure — not
/// handed an empty `200`.
#[tokio::test]
async fn a_failed_response_that_only_reasoned_is_not_answered_as_a_success() {
    for (code, status, class) in [
        ("server_error", 502, FailureClass::Server),
        ("rate_limit_exceeded", 429, FailureClass::RateLimit),
    ] {
        for client in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
            let label = format!("{code}, {client}");
            let harness = Harness::start(TWO_KEYS).await;
            for key in ["key-a", "key-b"] {
                harness.fake.always(
                    key,
                    failed_after(reasoning_only(), code, "The generation did not finish."),
                );
            }
            let output = harness.ask(client, "m", false).await;
            assert_eq!(
                output.status,
                status,
                "{label}: {}",
                String::from_utf8_lossy(&output.body)
            );
            assert_eq!(
                output.error_message(client),
                "The generation did not finish.",
                "{label}"
            );
            assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{label}");
            let record = harness.record(&output.request_id);
            assert!(!record.ok, "{label}: {record:?}");
            for credential in credentials(&harness) {
                assert_eq!(credential.successes, 0, "{label}: {credential:?}");
                assert_eq!(
                    credential.model_cooldowns.first().map(|rest| rest.reason),
                    Some(class),
                    "{label}: {credential:?}"
                );
            }
        }
    }
}

/// A fault of the request reported on a body that carries a reasoning item
/// is still a `400` in the client's protocol.
#[tokio::test]
async fn a_request_fault_on_a_response_that_only_reasoned_is_a_400() {
    for client in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [failed_after(
                reasoning_only(),
                "context_length_exceeded",
                "Your input exceeds the context window of this model.",
            )],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(
            output.status,
            400,
            "{client}: {}",
            String::from_utf8_lossy(&output.body)
        );
        assert_eq!(harness.fake.keys(), vec!["key-a"], "{client}");
        for credential in credentials(&harness) {
            assert!(credential.model_cooldowns.is_empty(), "{client}");
        }
    }
}

/// The text a generation produced before it failed is not a complete
/// answer: without streaming nothing has reached the client yet, so the
/// attempt fails like any other and the next credential answers in full.
#[tokio::test]
async fn a_failed_response_with_partial_text_is_not_delivered_as_the_answer() {
    for client in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [failed_after(
                json!([{
                    "type": "message", "id": "msg_1", "role": "assistant", "status": "incomplete",
                    "content": [{"type": "output_text", "text": "The answer is", "annotations": []}]
                }]),
                "server_error",
                "The model failed to generate a response.",
            )],
        );
        let output = harness.ask(client, "m", false).await;
        assert_eq!(output.status, 200, "{client}: {:?}", output.body);
        assert_eq!(
            output.response(client).text(),
            "Hello from the fake upstream",
            "{client}: the truncated text of the failed generation was delivered"
        );
        assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{client}");
        let creds = credentials(&harness);
        assert_eq!(
            (creds[0].successes, creds[0].failures),
            (0, 1),
            "{client}: {:?}",
            creds[0]
        );
    }
}

/// Passthrough: the Responses client can read `status: "failed"` itself,
/// but the credential that answered `rate_limit_exceeded` is rate limited
/// all the same — it must rest and the next one be tried, as it is when
/// the body carries no output (and as `response.failed` at the head of a
/// stream is handled).
#[tokio::test]
async fn a_rate_limited_passthrough_response_that_had_started_reasoning_fails_over() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [failed_after(
            reasoning_only(),
            "rate_limit_exceeded",
            "Rate limit reached for up-responses. Please try again in 20s.",
        )],
    );
    let output = harness.ask(Protocol::OpenaiResponses, "m", false).await;
    assert_eq!(
        harness.fake.keys(),
        vec!["key-a", "key-b"],
        "the client got {}: {}",
        output.status,
        String::from_utf8_lossy(&output.body)
    );
    assert_eq!(output.status, 200);
    assert_eq!(output.json()["status"], "completed");
    let creds = credentials(&harness);
    assert_eq!(
        creds[0].model_cooldowns.first().map(|rest| rest.reason),
        Some(FailureClass::RateLimit),
        "{:?}",
        creds[0]
    );
    assert_eq!((creds[0].successes, creds[0].failures), (0, 1));
}
