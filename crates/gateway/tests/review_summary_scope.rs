//! Regression test (review finding GW-F3): a reasoning-summary refusal by
//! one provider does not take the summary away from every *other* provider
//! the same request fails over to.
//!
//! When an upstream refuses `reasoning.summary`, the field is removed and
//! the attempt repeated once on the same credential, and the outcome is
//! remembered **per provider name**. A request-wide flag used to be set as
//! well and never reset, so when the repeat failed for an unrelated reason
//! (a rate limit, a 5xx) and the request moved on to a provider of another
//! — verified — organisation, that provider was not asked for a summary
//! either, although nothing was known against it, and the client got its
//! answer without the reasoning text it asked for. What a request has
//! learnt is now kept per provider too (`Job::summary_refused_by`).

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{Answer, Behaviour, Harness, body};
use switchyard_core::Protocol;

/// The same model served by two OpenAI organisations, tried in order.
const TWO_ORGANISATIONS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "org-unverified"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-unverified"]
[[providers.models]]
id = "up-responses"
alias = "m"

[[providers]]
name = "org-verified"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-verified"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#;

#[tokio::test]
async fn a_refusal_by_one_provider_does_not_cost_another_its_summary() {
    let harness = Harness::start(TWO_ORGANISATIONS).await;
    harness.fake.script(
        "key-unverified",
        [
            // Refuses the summary …
            Behaviour::UnverifiedOrg(Answer::text("never seen")),
            // … and is rate limited when asked again without it.
            Behaviour::rate_limited(7),
        ],
    );
    harness.fake.always(
        "key-verified",
        Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789")),
    );

    let mut request = body(Protocol::OpenaiChat, "m", false, false);
    request["reasoning_effort"] = json!("high");
    let output = harness
        .ask_with(Protocol::OpenaiChat, request, "m", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);

    let calls = harness.fake.requests();
    let asked: Vec<(&str, &Value)> = calls
        .iter()
        .map(|call| (call.key.as_str(), &call.body["reasoning"]["summary"]))
        .collect();
    assert_eq!(
        asked,
        vec![
            ("key-unverified", &json!("auto")),
            ("key-unverified", &Value::Null),
            // Nothing is known against this provider: it is asked.
            ("key-verified", &json!("auto")),
        ]
    );
}
