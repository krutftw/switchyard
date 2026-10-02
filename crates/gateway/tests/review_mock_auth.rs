//! Review finding GW-11: trying the mock provider's `mock-error-401` model
//! does not switch the whole mock provider off.
//!
//! DESIGN.md section 5: the mock provider exists "so the dashboard playground
//! and the test-suite work without keys", and its model names select the
//! behaviour: `mock-error-429` / `mock-error-500` / `mock-error-401` fail on
//! purpose, so that an operator can see what such a failure looks like end to
//! end.
//!
//! The gateway reports a mock model's scripted failure to the scheduler like
//! a real upstream's. For `mock-error-429` and `mock-error-500` that rests
//! *that model* — harmless. `mock-error-401`, however, is an `Auth` failure,
//! and an `Auth` failure rests the **whole credential** for
//! `routing.cooldown.auth_secs` (30 minutes by default). A mock provider has
//! exactly one credential: one playground request for `mock-error-401` and
//! every other mock model (`mock-echo`, `mock-lorem`, `mock-think`,
//! `mock-tools`) answers `429 … cooling down; retry in 1800s` to everybody
//! for half an hour.

// The shared support module re-exports more than one review file uses.
#[allow(unused_imports)]
mod support;

use support::{Harness, Output};
use switchyard_core::Protocol;

const MOCK: &str = r#"
[[providers]]
name = "demo"
kind = "mock"
"#;

#[tokio::test]
async fn the_failing_mock_models_do_not_take_the_other_mock_models_down() {
    let harness = Harness::start(MOCK).await;
    let healthy = harness.ask(Protocol::OpenaiChat, "mock-echo", false).await;
    assert_eq!(healthy.status, 200);

    // Somebody looks at each scripted failure once, as the model names
    // invite them to — the admin playground runs with the dashboard
    // identity.
    for model in ["mock-error-429", "mock-error-500", "mock-error-401"] {
        let mut request = harness.request(Protocol::OpenaiChat, model, false);
        request.identity = harness.gateway.dashboard_identity();
        let failed = Output::read(harness.gateway.generate(request).await).await;
        assert!(failed.status >= 400, "{model}: {}", failed.status);
    }

    // The models that work keep working, for every client.
    for (client, model) in [
        (Protocol::OpenaiChat, "mock-echo"),
        (Protocol::Anthropic, "mock-lorem"),
        (Protocol::Gemini, "mock-think"),
        (Protocol::OpenaiResponses, "mock-tools"),
    ] {
        let output = harness.ask(client, model, false).await;
        assert_eq!(
            output.status,
            200,
            "{model} after a look at mock-error-401: {}",
            String::from_utf8_lossy(&output.body)
        );
    }
}
