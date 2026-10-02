//! Review finding: a refusal that belongs to the *previous* configuration is
//! remembered against the *new* one.
//!
//! The per-provider memory of refused reasoning summaries is "cleared when
//! the config changes" (`Inner::apply` → `summary_refusals.clear()`). But
//! `summary_refused_or_failed` inserts the provider name unconditionally,
//! whatever configuration the request was started under. A request that is
//! in flight across a configuration change — sent with the old key, refused
//! by the old organisation after the new configuration was applied — puts
//! the provider right back into the freshly cleared set. From then on the
//! new key (another, verified organisation) is never asked for a summary,
//! until the configuration changes again.
//!
//! Expected: what a request learns about a provider is only remembered for
//! the configuration the request ran under (compare `job.config` with the
//! store's current configuration, or key the memory by a configuration
//! generation). The in-flight request itself may of course still be healed.

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use support::{Answer, Behaviour, Harness, PREAMBLE, body, eventually};
use switchyard_core::Protocol;

fn provider(key: &str) -> String {
    format!(
        r#"
[[providers]]
name = "openai"
kind = "openai"
wire_api = "responses"
base_url = "{{base}}/v1"
api_keys = ["{key}"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#
    )
}

fn chat_with_reasoning() -> serde_json::Value {
    let mut request = body(Protocol::OpenaiChat, "m", false, false);
    request["reasoning_effort"] = json!("high");
    request
}

#[tokio::test]
async fn a_refusal_from_before_a_configuration_change_is_not_remembered_after_it() {
    const OLD: &str = "key-old-unverified";
    const NEW: &str = "key-new-verified";
    let harness = Harness::start(&provider(OLD)).await;
    // The old organisation is not verified, and slow to say so.
    harness.fake.script(
        OLD,
        [Behaviour::Slow {
            delay: Duration::from_millis(1000),
            then: Box::new(Behaviour::UnverifiedOrg(Answer::text("unused"))),
        }],
    );
    harness.fake.always(
        OLD,
        Behaviour::UnverifiedOrg(Answer::text("Thought through.")),
    );
    // The new one is verified.
    harness.fake.always(
        NEW,
        Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789")),
    );

    let in_flight = harness.ask_with(Protocol::OpenaiChat, chat_with_reasoning(), "m", false);
    let swap_the_key = async {
        eventually("the first call is under way", || harness.fake.count() == 1).await;
        // The operator replaces the key with one of a verified organisation
        // while that call is still waiting for its answer.
        harness
            .reconfigure(&format!("{PREAMBLE}\n{}", provider(NEW)))
            .await;
        assert_eq!(
            harness.fake.requests_with(OLD).len(),
            1,
            "the refusal must arrive after the new configuration was applied for this test to mean anything"
        );
    };
    let (output, ()) = tokio::join!(in_flight, swap_the_key);
    // The request that was in flight is healed with the credential it had.
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.requests_with(OLD).len(), 2);

    // The next request runs under the new configuration: nothing is known
    // against the new key, so it is asked for the summary and gives it.
    harness.fake.clear();
    let output = harness
        .ask_with(Protocol::OpenaiChat, chat_with_reasoning(), "m", false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key, NEW);
    assert_eq!(
        calls[0].body["reasoning"]["summary"],
        json!("auto"),
        "the old organisation's refusal was remembered against the new key: {}",
        calls[0].body
    );
    assert_eq!(
        output.response(Protocol::OpenaiChat).reasoning_text(),
        "Because."
    );
}
