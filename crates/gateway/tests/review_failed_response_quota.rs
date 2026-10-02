//! Review finding: a failed Responses body whose code says "this account is
//! out of money" is only treated as exhausted quota when the code happens to
//! be `insufficient_quota`.
//!
//! `target::failed_response` classifies a `status: "failed"` body with
//! `stream_failure`, which calls a failure `Quota` only when the stream
//! decoder gave it status `429` *and* the code contains `quota` or
//! `billing`. The transport's classifier for the same failure arriving as an
//! HTTP error (`switchyard_upstream::classify`, `QUOTA_CODES`) knows a dozen
//! such codes. In a complete body (and at the head of a stream) most of
//! them fall through:
//!
//! * `billing_hard_limit_reached`, `billing_not_active`, … — no status for
//!   the code, so `502` / `Server`: the model rests briefly on the key, the
//!   key is tried again a moment later, and a client is told `502`;
//! * `usage_limit_reached`, `credit_balance_exhausted` — `429`, but the code
//!   contains neither word, so `RateLimit`: one model rests, the key stays
//!   in rotation for every other model.
//!
//! Expected (API.md: "classified by the code … `insufficient_quota` rests
//! the key", and "classified like the same failure" as an HTTP error): the
//! codes the transport treats as exhausted quota rest the whole credential
//! here too.

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use support::{Behaviour, Harness};
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
async fn the_quota_codes_of_the_transport_rest_the_key_in_a_failed_body_too() {
    for (code, message) in [
        // The control: this one works.
        (
            "insufficient_quota",
            "You exceeded your current quota, please check your plan and billing details.",
        ),
        (
            "billing_hard_limit_reached",
            "Billing hard limit has been reached.",
        ),
        ("billing_not_active", "Your account is not active."),
        ("usage_limit_reached", "The usage limit has been reached."),
        (
            "credit_balance_exhausted",
            "Your credit balance is exhausted.",
        ),
    ] {
        let harness = Harness::start(TWO_KEYS).await;
        harness
            .fake
            .script("key-a", [Behaviour::failed_response(code, message)]);
        let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
        assert_eq!(output.status, 200, "{code}: {:?}", output.body);
        assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{code}");

        let creds = credentials(&harness);
        assert_eq!(
            (creds[0].status, creds[0].cooldown_reason),
            (CredentialStatus::Cooling, Some(FailureClass::Quota)),
            "{code}: the key is out of money, whatever model is asked for: {:?}",
            creds[0]
        );
        let record = harness.record(&output.request_id);
        assert_eq!(record.attempts[0].status, 429, "{code}");
    }
}
