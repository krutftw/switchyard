//! Review of the integration pass: the per-provider discovery state.
//!
//! * The `error` of a failed discovery is shown by `GET /providers`. It is
//!   only cleaned of the credential's *own* key: any other key-shaped text
//!   the upstream puts into its error body is shown as it came, while the
//!   same text is masked in the log line of the same failure and in a
//!   credential's `last_error`.
//! * The number by which an overtaken background listing is recognised
//!   starts again at 1 when a provider is removed and created again under
//!   the same name, so the listing that was still under way for the removed
//!   provider is taken for the new provider's and decides its state.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use std::time::Duration;
use support::{Behaviour, FOUR_PROVIDERS, Harness, PREAMBLE};
use switchyard_gateway::{DiscoveryState, DiscoveryStatus};

fn state(harness: &Harness, provider: &str) -> DiscoveryState {
    harness
        .gateway
        .discovery_states()
        .remove(provider)
        .unwrap_or_else(|| panic!("no discovery state for `{provider}`"))
}

async fn settled(harness: &Harness, provider: &str) -> DiscoveryState {
    support::eventually(&format!("the discovery of `{provider}` ends"), || {
        state(harness, provider).state != DiscoveryStatus::Pending
    })
    .await;
    state(harness, provider)
}

const DISCOVERING: &str = r#"
[[providers]]
name = "auto"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-auto-1"]
"#;

/// Another credential's key, as an upstream (a relay in front of several
/// accounts, say) may quote it in an error.
const OTHER_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD";

#[tokio::test]
async fn a_discovery_error_shows_no_key_the_upstream_quoted() {
    let harness = Harness::start(DISCOVERING).await;
    assert_eq!(settled(&harness, "auto").await.state, DiscoveryStatus::Ok);

    harness.fake.always(
        "key-auto-2",
        Behaviour::error(
            401,
            &format!("this account is suspended; use {OTHER_KEY} instead"),
        ),
    );
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            DISCOVERING.replace("key-auto-1", "key-auto-2")
        ))
        .await;
    let failed = settled(&harness, "auto").await;
    assert_eq!(failed.state, DiscoveryStatus::Failed);
    let error = failed.error.unwrap_or_default();
    assert!(error.contains("suspended"), "{error}");
    assert!(
        !error.contains(OTHER_KEY),
        "the discovery state shows a key in full: {error}"
    );
}

/// `models` is documented as the size of "the upstream's list that is in
/// use", kept when a later listing fails. A provider that was switched off
/// and on again keeps its list in the scheduler (same endpoint), but its
/// state starts again at 0: when the listing that follows fails, the state
/// says `failed` with no models while the old list is still routed.
#[tokio::test]
async fn the_model_count_is_that_of_the_list_in_use() {
    let harness = Harness::start(DISCOVERING).await;
    let ok = settled(&harness, "auto").await;
    assert_eq!((ok.state, ok.models), (DiscoveryStatus::Ok, 2));

    // Switched off, then on again while the upstream is failing.
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            DISCOVERING.replace(
                "kind = \"openai-compat\"",
                "kind = \"openai-compat\"\nenabled = false"
            )
        ))
        .await;
    assert_eq!(state(&harness, "auto").state, DiscoveryStatus::Off);
    harness
        .fake
        .always("key-auto-1", Behaviour::error(500, "down for maintenance"));
    harness
        .reconfigure(&format!("{PREAMBLE}\n{DISCOVERING}"))
        .await;
    let failed = settled(&harness, "auto").await;
    assert_eq!(failed.state, DiscoveryStatus::Failed, "{failed:?}");

    // The list of the earlier success is in use: its models are routable.
    let routable = harness.gateway.scheduler().resolve("disc-alpha").is_ok();
    assert!(routable, "the earlier list was expected to be kept");
    assert_eq!(
        failed.models, 2,
        "two discovered models are routed, the state counts {}: {failed:?}",
        failed.models
    );
}

#[tokio::test]
async fn a_listing_of_a_removed_provider_does_not_decide_its_successors_state() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let with = |key: &str| {
        format!(
            "{PREAMBLE}\n{FOUR_PROVIDERS}\n[[providers]]\nname = \"again\"\nkind = \"openai-compat\"\nbase_url = \"{{base}}/v1\"\nprefix = \"again\"\napi_keys = [\"{key}\"]\n"
        )
    };

    // A provider is added whose endpoint hangs and then fails (a mistyped
    // address, say) …
    harness.fake.script(
        "key-old",
        [Behaviour::Slow {
            delay: Duration::from_millis(400),
            then: Box::new(Behaviour::error(500, "the old endpoint is broken")),
        }],
    );
    harness.reconfigure(&with("key-old")).await;
    assert_eq!(state(&harness, "again").state, DiscoveryStatus::Pending);

    // … so the operator deletes it and creates it again, correctly this
    // time, while that first listing is still under way.
    harness
        .reconfigure(&format!("{PREAMBLE}\n{FOUR_PROVIDERS}"))
        .await;
    assert!(!harness.gateway.discovery_states().contains_key("again"));
    harness.reconfigure(&with("key-new")).await;
    let ok = settled(&harness, "again").await;
    assert_eq!((ok.state, ok.models), (DiscoveryStatus::Ok, 2), "{ok:?}");

    // The first listing ends. It was for a provider that no longer exists.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let later = state(&harness, "again");
    assert_eq!(
        (later.state, later.error.clone()),
        (DiscoveryStatus::Ok, None),
        "the listing of the removed provider became the state of the new one: {later:?}"
    );
}

/// The same when the removed provider's listing succeeds in the end: its
/// list must not become the list of the provider created after it.
#[tokio::test]
async fn a_list_of_a_removed_provider_does_not_reach_its_successor() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let with = |key: &str| {
        format!(
            "{PREAMBLE}\n{FOUR_PROVIDERS}\n[[providers]]\nname = \"again\"\nkind = \"openai-compat\"\nbase_url = \"{{base}}/v1\"\nprefix = \"again\"\napi_keys = [\"{key}\"]\n"
        )
    };
    harness.fake.script(
        "key-old",
        [Behaviour::Slow {
            delay: Duration::from_millis(400),
            then: Box::new(Behaviour::Json {
                status: 200,
                body: serde_json::json!({
                    "object": "list",
                    "data": [{"id": "only-at-the-old-endpoint", "object": "model"}]
                }),
            }),
        }],
    );
    harness.reconfigure(&with("key-old")).await;
    assert_eq!(state(&harness, "again").state, DiscoveryStatus::Pending);
    harness
        .reconfigure(&format!("{PREAMBLE}\n{FOUR_PROVIDERS}"))
        .await;
    harness.reconfigure(&with("key-new")).await;
    let ok = settled(&harness, "again").await;
    assert_eq!((ok.state, ok.models), (DiscoveryStatus::Ok, 2), "{ok:?}");

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(state(&harness, "again"), ok);
    let scheduler = harness.gateway.scheduler();
    assert!(scheduler.resolve("again/disc-alpha").is_ok());
    assert!(
        scheduler.resolve("again/only-at-the-old-endpoint").is_err(),
        "the list of the removed provider is in use for the new one"
    );
}

/// Whatever an upstream quotes in its refusal — another account's key, a
/// token, a password — is masked in the state like in the log.
#[tokio::test]
async fn a_discovery_error_masks_every_kind_of_secret_the_log_masks() {
    let harness = Harness::start(DISCOVERING).await;
    assert_eq!(settled(&harness, "auto").await.state, DiscoveryStatus::Ok);

    harness.fake.always(
        "key-auto-2",
        Behaviour::error(
            401,
            "Incorrect API key. Also seen: sk-ant-api03-OTHERSECRETOTHERSECRET0123456789 and \
             password=hunter2hunter2 and Bearer abcdefghijklmnop0123456789",
        ),
    );
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            DISCOVERING.replace("key-auto-1", "key-auto-2")
        ))
        .await;
    let failed = settled(&harness, "auto").await;
    assert_eq!(failed.state, DiscoveryStatus::Failed);
    let error = failed.error.unwrap_or_default();
    assert!(error.contains("Incorrect API key"), "{error}");
    for secret in [
        "OTHERSECRETOTHERSECRET",
        "hunter2hunter2",
        "abcdefghijklmnop0123456789",
    ] {
        assert!(!error.contains(secret), "`{secret}` is shown: {error}");
    }
    // A listing the operator asks for: neither its answer nor the state it
    // leaves shows them, and a provider test that is refused the same way
    // does not either.
    let refused = harness
        .gateway
        .discover("auto")
        .await
        .expect_err("the listing is refused");
    let again = state(&harness, "auto").error.unwrap_or_default();
    let test = harness
        .gateway
        .test_provider("auto", Some("disc-alpha"))
        .await;
    let tested = test.error.unwrap_or_default();
    for (what, text) in [
        ("the answer of discover", &refused.message),
        ("the state after discover", &again),
        ("the provider test", &tested),
    ] {
        assert!(text.contains("Incorrect API key"), "{what}: {text}");
        for secret in [
            "OTHERSECRETOTHERSECRET",
            "hunter2hunter2",
            "abcdefghijklmnop0123456789",
        ] {
            assert!(!text.contains(secret), "{what} shows `{secret}`: {text}");
        }
    }
}
