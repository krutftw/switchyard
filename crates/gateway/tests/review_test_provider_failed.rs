//! Regression tests (review finding GW-F2): `Gateway::test_provider` does
//! not take a Responses body that reports a failed generation (HTTP `200`,
//! `status: "failed"`) for a successful test.
//!
//! Such a body is classified by the code in its `error`
//! (`rate_limit_exceeded` -> rate limit, `insufficient_quota` -> quota,
//! ...). `generate` did; the provider test did not look at the body at all.
//! The dashboard therefore showed a green test for a key that was out of
//! quota, and — because the outcome was reported to the scheduler as a
//! success — the test took a model that a real request had just put to rest
//! straight back into rotation. The test now reads the answer (see also
//! `unusable_answers.rs`).

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use support::{Behaviour, Harness};
use switchyard_core::{FailureClass, Protocol};
use switchyard_scheduler::CredentialStatus;

const ONE_KEY: &str = r#"
[[providers]]
name = "oai"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#;

#[tokio::test]
async fn a_failed_response_body_fails_the_provider_test() {
    let harness = Harness::start(ONE_KEY).await;
    harness.fake.always(
        "key-a",
        Behaviour::failed_response(
            "insufficient_quota",
            "You exceeded your current quota, please check your plan and billing details.",
        ),
    );
    let test = harness.gateway.test_provider("oai", None).await;
    assert!(
        !test.ok,
        "a generation the upstream reports as failed is not a passed test: {}",
        serde_json::to_string(&test).unwrap()
    );
    let said = test.error.unwrap_or_default();
    assert!(said.contains("exceeded your current quota"), "{said}");

    // Reported like the same failure of a request would be.
    let providers = harness.gateway.scheduler().snapshot();
    let credential = &providers[0].credentials[0];
    assert_eq!(credential.successes, 0, "{credential:?}");
    assert_eq!(credential.failures, 1, "{credential:?}");
    assert_eq!(
        credential.status,
        CredentialStatus::Cooling,
        "{credential:?}"
    );
    assert_eq!(credential.cooldown_reason, Some(FailureClass::Quota));
}

/// A request finds the model rate limited and rests it; a provider test
/// that is answered the very same way must not put it back into rotation.
#[tokio::test]
async fn a_failed_test_does_not_end_the_rest_a_request_started() {
    let harness = Harness::start(ONE_KEY).await;
    harness.fake.always(
        "key-a",
        Behaviour::failed_response(
            "rate_limit_exceeded",
            "Rate limit reached for up-responses. Please try again in 20s.",
        ),
    );
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 429, "{:?}", output.body);
    let resting = |harness: &Harness| {
        harness.gateway.scheduler().snapshot()[0].credentials[0]
            .model_cooldowns
            .len()
    };
    assert_eq!(resting(&harness), 1, "the request rested the model");

    let test = harness.gateway.test_provider("oai", Some("m")).await;
    assert_eq!(
        resting(&harness),
        1,
        "a test answered with the same failure ended the rest: {}",
        serde_json::to_string(&test).unwrap()
    );
    assert!(!test.ok, "{}", serde_json::to_string(&test).unwrap());
}
