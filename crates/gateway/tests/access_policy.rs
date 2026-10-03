//! Bounded regression checks of the gateway's current authorization policy.
mod support;

use serde_json::json;
use support::{FOUR_PROVIDERS, Harness};
use switchyard_core::Protocol;

#[tokio::test]
async fn issued_identity_uses_reloaded_restrictions_and_revocation() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let issued = harness.identity();
    let mut config = format!("{}\n{FOUR_PROVIDERS}", support::PREAMBLE);
    config = config.replace(
        "name = \"tester\"",
        "name = \"tester\"\nmodels = [\"m-chat\"]\nrate_limit_rpm = 1",
    );
    harness.reconfigure(&config).await;
    assert!(harness.gateway.validate_session_identity(&issued).is_err());
    let denied = harness.request_by(&issued, Protocol::OpenaiChat, "m-anthropic", false);
    assert_eq!(harness.gateway.generate(denied).await.status(), 403);
    let allowed = harness.request_by(&issued, Protocol::OpenaiChat, "m-chat", false);
    assert_eq!(harness.gateway.generate(allowed).await.status(), 200);
    let limited = harness.request_by(&issued, Protocol::OpenaiChat, "m-chat", false);
    assert_eq!(harness.gateway.generate(limited).await.status(), 429);
    harness
        .reconfigure(&config.replace("name = \"tester\"", "name = \"tester\"\nenabled = false"))
        .await;
    let revoked = harness.request_by(&issued, Protocol::OpenaiChat, "m-chat", false);
    assert_eq!(harness.gateway.generate(revoked).await.status(), 401);
    assert!(harness.gateway.issue_ws_ticket(&issued).is_err());
}

#[tokio::test]
async fn fuzzy_model_spelling_is_authorized_as_the_registered_name() {
    let config = format!("{}\n{FOUR_PROVIDERS}", support::PREAMBLE).replace(
        "name = \"tester\"",
        "name = \"tester\"\nmodels = [\"m-chat-*\"]",
    );
    let harness = Harness::start_raw(&config).await;
    assert_eq!(
        harness
            .gateway
            .scheduler()
            .resolve("m-chat-latest")
            .unwrap()
            .base,
        "m-chat"
    );
    let request = harness.request(Protocol::OpenaiChat, "m-chat-latest", false);
    assert_eq!(harness.gateway.generate(request).await.status(), 403);
    assert_eq!(harness.fake.count(), 0);
}

#[tokio::test]
async fn literal_parenthesized_model_is_authorized_as_its_full_name() {
    let config = format!("{}\n{FOUR_PROVIDERS}", support::PREAMBLE)
        .replace(
            "name = \"tester\"",
            "name = \"tester\"\nmodels = [\"special\"]",
        )
        .replace("alias = \"m-chat\"", "alias = \"special(model)\"");
    let harness = Harness::start_raw(&config).await;
    assert_eq!(
        harness
            .gateway
            .scheduler()
            .resolve("special(model)")
            .unwrap()
            .base,
        "special(model)"
    );
    let request = harness.request(Protocol::OpenaiChat, "special(model)", false);
    assert_eq!(harness.gateway.generate(request).await.status(), 403);
    assert_eq!(harness.fake.count(), 0);
}

#[tokio::test]
async fn restricted_keys_reject_alternative_upstream_model_selectors() {
    let config = format!("{}\n{FOUR_PROVIDERS}", support::PREAMBLE)
        .replace("name = \"tester\"", "name = \"tester\"\nmodels = [\"m-*\"]");
    let harness = Harness::start_raw(&config).await;
    for (field, value) in [
        ("models", json!(["m-other"])),
        ("route", json!("fallback")),
        ("fallbacks", json!("default")),
        ("context_window_fallbacks", json!(["m-other"])),
        ("preset", json!("alternate")),
    ] {
        let mut body = support::body(Protocol::OpenaiChat, "m-chat", false, false);
        body[field] = value;
        let request = harness.request_with(Protocol::OpenaiChat, body, "m-chat", false);
        assert_eq!(
            harness.gateway.generate(request).await.status(),
            403,
            "{field}"
        );
    }
    let mut body = support::body(Protocol::Anthropic, "m-anthropic", false, false);
    body["fallbacks"] = json!("default");
    let request = harness.request_with(Protocol::Anthropic, body.clone(), "m-anthropic", false);
    assert_eq!(harness.gateway.generate(request).await.status(), 403);
    let request = harness.request_with(Protocol::Anthropic, body, "m-anthropic", false);
    assert_eq!(harness.gateway.count_tokens(request).await.status(), 403);
    assert_eq!(harness.fake.count(), 0);
}
