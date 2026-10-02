//! Regression tests of the integration pass: what the dashboard's engineers
//! found against the running gateway.
//!
//! * Captured bodies can be read the moment `request.finished` announces a
//!   record with `has_bodies`.
//! * A vertex credential whose service-account file is missing or invalid
//!   is unusable, with a reason, from the moment the configuration is
//!   applied — and usable again once the file is fine.
//! * A configuration change re-runs model discovery only for the providers
//!   it concerns, and every provider has a discovery state.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use serde_json::json;
use std::time::Duration;
use support::{Behaviour, FOUR_PROVIDERS, Harness, Kind, PREAMBLE};
use switchyard_core::Protocol;
use switchyard_gateway::{DiscoveryState, DiscoveryStatus};
use switchyard_scheduler::CredentialStatus;
use switchyard_telemetry::Event;

// ---------------------------------------------------------------------------
// Captured bodies
// ---------------------------------------------------------------------------

/// `GET /admin/api/requests/{id}` straight after the `request.finished`
/// frame used to answer `has_bodies: true` with no bodies for some 60 ms:
/// the record was published while the bodies were still being written. A
/// listener that reads the bodies the instant the event arrives finds them
/// — for complete and streamed answers, successes and failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captured_bodies_are_readable_when_the_request_is_announced() {
    let harness = Harness::start(&format!(
        "[logging]\nrequest_log = \"all\"\n{FOUR_PROVIDERS}"
    ))
    .await;
    let telemetry = harness.gateway.telemetry().clone();
    let mut events = telemetry.subscribe();
    // What the admin API does on `GET /requests/{id}`, done the moment the
    // event is received.
    let listener = tokio::spawn(async move {
        let mut seen = Vec::new();
        while seen.len() < 12 {
            let Ok(event) = events.recv().await else {
                break;
            };
            if let Event::RequestFinished(record) = event {
                let found = telemetry.usage().find(&record.id).is_some();
                let bodies = telemetry.bodies().read(&record.id);
                seen.push((record, found, bodies));
            }
        }
        seen
    });

    // A request fault: it fails the request and rests nothing.
    harness.fake.script(
        "key-chat-1",
        [Behaviour::error(400, "the upstream refused")],
    );
    for round in 0..12 {
        // Translated and passthrough, streamed and not; the first fails.
        let protocol = if round % 2 == 0 {
            Protocol::Anthropic
        } else {
            Protocol::OpenaiChat
        };
        harness.ask(protocol, "m-chat", round % 3 == 0).await;
    }

    let seen = tokio::time::timeout(Duration::from_secs(10), listener)
        .await
        .expect("every request is announced")
        .unwrap();
    assert_eq!(seen.len(), 12);
    assert!(!seen[0].0.ok, "the scripted failure comes first");
    for (record, found, bodies) in seen {
        assert!(record.has_bodies, "{record:?}");
        assert!(found, "the record of {} can be looked up", record.id);
        let bodies = bodies
            .unwrap_or_else(|| panic!("{}: has_bodies is true but nothing can be read", record.id));
        assert!(bodies.client_request.is_some(), "{record:?}");
        assert!(bodies.upstream_request.is_some(), "{record:?}");
        assert!(bodies.client_response.is_some(), "{record:?}");
    }
}

// ---------------------------------------------------------------------------
// Service-account files
// ---------------------------------------------------------------------------

/// A throwaway RSA key generated for tests only.
const TEST_KEY_PEM: &str = include_str!("fixtures/test_rsa_pkcs8.pem");

/// A marker that must never show up in a reason: it stands in for what a
/// key file holds.
const FILE_CONTENT: &str = "content-of-the-key-file-0123456789";

const VERTEX: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "vx"
kind = "vertex"
base_url = "{base}"
location = "us-central1"
[[providers.credentials]]
service_account_file = "keys/first.json"
label = "first"
[[providers.credentials]]
service_account_file = "second.json"
label = "second"
[[providers.models]]
id = "gemini-vx"
"#;

fn key_file(harness: &Harness) -> String {
    json!({
        "type": "service_account",
        "project_id": "demo-project",
        "private_key_id": "0123456789abcdef",
        "private_key": TEST_KEY_PEM,
        "client_email": "gateway@demo-project.iam.gserviceaccount.com",
        "client_id": "100000000000000000001",
        "token_uri": format!("{}/token", harness.fake.base()),
    })
    .to_string()
}

/// (status, usable, reason) of each credential of the first provider.
fn credentials(harness: &Harness) -> Vec<(CredentialStatus, bool, String)> {
    harness.gateway.scheduler().snapshot()[0]
        .credentials
        .iter()
        .map(|credential| {
            (
                credential.status,
                credential.usable,
                credential.unusable_reason.clone().unwrap_or_default(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_missing_or_invalid_service_account_file_makes_the_credential_unusable() {
    // Neither file exists when the gateway starts.
    let harness = Harness::start(VERTEX).await;
    let dir = harness.dir.path().to_path_buf();
    let state = credentials(&harness);
    for (index, file) in ["first.json", "second.json"].into_iter().enumerate() {
        let (status, usable, reason) = &state[index];
        assert_eq!(*status, CredentialStatus::Unusable, "{state:?}");
        assert!(!usable);
        assert!(reason.contains(file), "the reason names the file: {reason}");
        assert!(
            reason.contains("cannot read"),
            "the reason names the problem: {reason}"
        );
        // Not where the file is looked for on this machine.
        assert!(!reason.contains(&dir.display().to_string()), "{reason}");
    }
    let warnings = harness.gateway.scheduler().warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].contains("provider `vx`") && warnings[0].contains("first.json"),
        "{warnings:?}"
    );
    // Nothing can serve the model, and nothing was sent anywhere.
    let output = harness.ask(Protocol::Gemini, "gemini-vx", false).await;
    assert_eq!(output.status, 503, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 0);

    // One file appears, the other is not a key file: at the next applied
    // configuration the first credential is back and the second says what
    // is wrong with its file — without a word of what is in it.
    std::fs::create_dir(dir.join("keys")).unwrap();
    std::fs::write(dir.join("keys/first.json"), key_file(&harness)).unwrap();
    std::fs::write(
        dir.join("second.json"),
        json!({
            "type": "service_account",
            "project_id": "demo-project",
            "client_email": "gateway@demo-project.iam.gserviceaccount.com",
            "private_key": FILE_CONTENT,
        })
        .to_string(),
    )
    .unwrap();
    harness.reload().await;
    let state = credentials(&harness);
    assert_eq!(
        (state[0].0, state[0].1, state[0].2.as_str()),
        (CredentialStatus::Ready, true, "")
    );
    let (status, usable, reason) = &state[1];
    assert_eq!((*status, *usable), (CredentialStatus::Unusable, false));
    assert!(reason.contains("second.json"), "{reason}");
    assert!(reason.contains("private_key"), "{reason}");
    assert!(!reason.contains(FILE_CONTENT), "{reason}");
    assert_eq!(harness.gateway.scheduler().warnings().len(), 1);

    let output = harness.ask(Protocol::Gemini, "gemini-vx", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(
        harness
            .record(&output.request_id)
            .credential_label
            .as_deref(),
        Some("first")
    );

    // A file that is not even JSON; then one that is fine.
    std::fs::write(dir.join("second.json"), FILE_CONTENT).unwrap();
    harness.reload().await;
    let reason = credentials(&harness)[1].2.clone();
    assert!(
        reason.contains("second.json") && reason.contains("not valid JSON"),
        "{reason}"
    );
    assert!(!reason.contains(FILE_CONTENT), "{reason}");

    std::fs::write(dir.join("second.json"), key_file(&harness)).unwrap();
    harness.reload().await;
    for (status, usable, reason) in credentials(&harness) {
        assert_eq!(
            (status, usable, reason.as_str()),
            (CredentialStatus::Ready, true, "")
        );
    }
    assert!(harness.gateway.scheduler().warnings().is_empty());

    // And the other way round: a file that goes missing is noticed when a
    // configuration is applied, any configuration.
    std::fs::remove_file(dir.join("keys/first.json")).unwrap();
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            VERTEX.replace("fill-first", "round-robin")
        ))
        .await;
    let state = credentials(&harness);
    assert_eq!(state[0].0, CredentialStatus::Unusable, "{state:?}");
    assert_eq!(state[1].0, CredentialStatus::Ready, "{state:?}");
}

/// The operator repairs the file and presses "Test": the test looks at the
/// file again instead of answering that the provider has no usable
/// credential until the configuration is reloaded.
#[tokio::test]
async fn a_provider_test_notices_a_repaired_service_account_file() {
    let harness = Harness::start(VERTEX).await;
    let test = harness.gateway.test_provider("vx", None).await;
    assert!(!test.ok, "{test:?}");
    assert_eq!(credentials(&harness)[0].0, CredentialStatus::Unusable);

    std::fs::create_dir(harness.dir.path().join("keys")).unwrap();
    std::fs::write(
        harness.dir.path().join("keys/first.json"),
        key_file(&harness),
    )
    .unwrap();
    let test = harness.gateway.test_provider("vx", None).await;
    assert!(test.ok, "{test:?}");
    assert_eq!(test.credential.as_deref(), Some("first"));
    let state = credentials(&harness);
    assert_eq!(state[0].0, CredentialStatus::Ready, "{state:?}");
    assert_eq!(state[1].0, CredentialStatus::Unusable, "{state:?}");
}

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

const TWO_DISCOVERING: &str = r#"
[[providers]]
name = "auto-a"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a-1"]

[[providers]]
name = "auto-b"
kind = "openai-compat"
base_url = "{base}/v1"
prefix = "b"
api_keys = ["key-b-1"]

[[providers]]
name = "fixed"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-fixed-1"]
[[providers.models]]
id = "claude-fixed"

[[providers]]
name = "mock"
kind = "mock"
"#;

/// The keys the fake's model listing was asked with, in order.
fn listings(harness: &Harness) -> Vec<String> {
    harness
        .fake
        .requests()
        .into_iter()
        .filter(|request| request.kind == Kind::Models)
        .map(|request| request.key)
        .collect()
}

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

#[tokio::test]
async fn a_configuration_change_asks_only_the_providers_it_concerns() {
    let harness = Harness::start(TWO_DISCOVERING).await;
    // At start: everyone who wants discovery, nobody else.
    assert_eq!(settled(&harness, "auto-a").await.state, DiscoveryStatus::Ok);
    assert_eq!(settled(&harness, "auto-b").await.state, DiscoveryStatus::Ok);
    let mut asked = listings(&harness);
    asked.sort();
    assert_eq!(asked, ["key-a-1", "key-b-1"]);
    harness.fake.clear();

    // Edits that have nothing to do with where or how a model list is
    // fetched: an alias, a routing setting, the provider's own priority,
    // prefix and exclusions, another provider's models.
    let unrelated = format!(
        "{PREAMBLE}\n[routing]\nstrategy = \"fill-first\"\n{}\n[[aliases]]\nname = \"fast\"\ntargets = [\"disc-alpha\"]\n",
        TWO_DISCOVERING
            .replace(
                "api_keys = [\"key-a-1\"]",
                "api_keys = [\"key-a-1\"]\npriority = 5\nexclude = [\"*-beta\"]"
            )
            .replace("prefix = \"b\"", "prefix = \"bee\"")
            .replace("id = \"claude-fixed\"", "id = \"claude-fixed-2\"")
    );
    harness.reconfigure(&unrelated).await;
    for provider in ["auto-a", "auto-b"] {
        assert_eq!(
            state(&harness, provider).state,
            DiscoveryStatus::Ok,
            "nothing was started for `{provider}`"
        );
    }
    // The lists are still in use.
    assert!(harness.gateway.scheduler().resolve("disc-alpha").is_ok());
    assert!(
        harness
            .gateway
            .scheduler()
            .resolve("bee/disc-alpha")
            .is_ok()
    );

    // One provider gets another key: it is asked, with the new key; the
    // other one is not.
    harness
        .reconfigure(&unrelated.replace("key-b-1", "key-b-2"))
        .await;
    assert_eq!(settled(&harness, "auto-b").await.state, DiscoveryStatus::Ok);
    assert_eq!(
        listings(&harness),
        ["key-b-2"],
        "the unrelated edits and the other provider caused no listing"
    );
    harness.fake.clear();

    // A new provider is asked; a header on an existing one is a reason too.
    let with_header = unrelated.replace("key-b-1", "key-b-2").replace(
        "exclude = [\"*-beta\"]",
        "exclude = [\"*-beta\"]\n[providers.headers]\nX-Team = \"platform\"",
    );
    let grown = format!(
        "{with_header}\n[[providers]]\nname = \"auto-c\"\nkind = \"openai-compat\"\nbase_url = \"{{base}}/v1\"\nprefix = \"c\"\napi_keys = [\"key-c-1\"]\n"
    );
    harness.reconfigure(&grown).await;
    assert_eq!(settled(&harness, "auto-a").await.state, DiscoveryStatus::Ok);
    assert_eq!(settled(&harness, "auto-c").await.state, DiscoveryStatus::Ok);
    let mut asked = listings(&harness);
    asked.sort();
    assert_eq!(asked, ["key-a-1", "key-c-1"]);
    harness.fake.clear();

    // Switching discovery off and on again: off says so, on asks.
    let off = grown.replace("prefix = \"c\"", "prefix = \"c\"\ndiscover = false");
    harness.reconfigure(&off).await;
    assert_eq!(
        state(&harness, "auto-c"),
        DiscoveryState {
            state: DiscoveryStatus::Off,
            at: None,
            error: None,
            models: 0,
        }
    );
    harness.reconfigure(&grown).await;
    // A listing that is planned shows as `pending` the moment the
    // configuration is announced: the others were not planned.
    assert_eq!(state(&harness, "auto-c").state, DiscoveryStatus::Pending);
    assert_eq!(state(&harness, "auto-a").state, DiscoveryStatus::Ok);
    assert_eq!(settled(&harness, "auto-c").await.state, DiscoveryStatus::Ok);
    assert_eq!(listings(&harness), ["key-c-1"]);
    harness.fake.clear();

    // A provider that is removed is forgotten, and nobody else is asked.
    harness.reconfigure(&with_header).await;
    let states = harness.gateway.discovery_states();
    assert!(!states.contains_key("auto-c"));
    assert_eq!(states["auto-a"].state, DiscoveryStatus::Ok);
    assert_eq!(states["auto-b"].state, DiscoveryStatus::Ok);

    // A reload of the unchanged file is the operator asking again: every
    // provider that wants discovery is asked.
    harness.reload().await;
    assert_eq!(state(&harness, "auto-a").state, DiscoveryStatus::Pending);
    assert_eq!(settled(&harness, "auto-a").await.state, DiscoveryStatus::Ok);
    assert_eq!(settled(&harness, "auto-b").await.state, DiscoveryStatus::Ok);
    let mut asked = listings(&harness);
    asked.sort();
    assert_eq!(
        asked,
        ["key-a-1", "key-b-2"],
        "exactly the reload's two listings since the last check"
    );
}

#[tokio::test]
async fn every_provider_has_a_discovery_state() {
    let config = format!(
        "{TWO_DISCOVERING}\n[[providers]]\nname = \"parked\"\nkind = \"openai-compat\"\nenabled = false\nbase_url = \"{{base}}/v1\"\napi_keys = [\"key-parked\"]\n\n[[providers]]\nname = \"keyless\"\nkind = \"openai\"\nbase_url = \"{{base}}/v1\"\napi_keys = [\"env:SWITCHYARD_GATEWAY_TEST_VARIABLE_THAT_IS_NOT_SET\"]\n"
    );
    let harness = Harness::start_raw(&format!("{PREAMBLE}\n{config}")).await;
    // The first listing of `auto-a` takes a while: until it answers, the
    // state says a listing is under way.
    let before = switchyard_core::util::now_unix_ms();
    let ok = settled(&harness, "auto-a").await;
    assert_eq!(ok.state, DiscoveryStatus::Ok);
    assert_eq!(ok.models, 2, "the fake lists two models");
    assert_eq!(ok.error, None);
    assert!(ok.at.is_some_and(|at| at >= before), "{ok:?}");

    let states = harness.gateway.discovery_states();
    let mut names: Vec<&str> = states.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["auto-a", "auto-b", "fixed", "keyless", "mock", "parked"]
    );
    // Not asked: models are configured, a mock, a disabled provider.
    for provider in ["fixed", "mock", "parked"] {
        assert_eq!(
            states[provider],
            DiscoveryState {
                state: DiscoveryStatus::Off,
                at: None,
                error: None,
                models: 0,
            },
            "{provider}"
        );
    }
    // Wanted, but there is nothing to ask with.
    let keyless = settled(&harness, "keyless").await;
    assert_eq!(keyless.state, DiscoveryStatus::Failed);
    assert!(
        keyless
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no usable credential")),
        "{keyless:?}"
    );
    assert_eq!(keyless.models, 0);

    // The state serialises with every field, as the admin API shows it.
    assert_eq!(
        serde_json::to_value(&states["fixed"]).unwrap(),
        json!({"state": "off", "at": null, "error": null, "models": 0})
    );
    let shown = serde_json::to_value(&ok).unwrap();
    assert_eq!(shown["state"], "ok");
    assert_eq!(shown["models"], 2);
}

#[tokio::test]
async fn a_failed_discovery_keeps_the_previous_list_and_says_why() {
    let harness = Harness::start(TWO_DISCOVERING).await;
    assert_eq!(settled(&harness, "auto-a").await.state, DiscoveryStatus::Ok);
    assert!(harness.gateway.scheduler().resolve("disc-alpha").is_ok());

    // The key changes and the upstream refuses the new one, quoting it and
    // rambling over several lines.
    harness.fake.always(
        "key-a-secret-0123456789abcdef",
        Behaviour::error(
            401,
            "Incorrect API key provided:\n  key-a-secret-0123456789abcdef\nSee the docs.",
        ),
    );
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            TWO_DISCOVERING.replace("key-a-1", "key-a-secret-0123456789abcdef")
        ))
        .await;
    let failed = settled(&harness, "auto-a").await;
    assert_eq!(failed.state, DiscoveryStatus::Failed);
    assert_eq!(failed.models, 2, "the list of the last success is kept");
    let error = failed.error.as_deref().unwrap_or_default();
    assert!(error.contains("Incorrect API key"), "{error}");
    assert!(!error.contains("key-a-secret-0123456789abcdef"), "{error}");
    assert!(!error.contains('\n'), "one line: {error:?}");
    assert!(failed.at.is_some());
    // The models are still routable.
    assert!(harness.gateway.scheduler().resolve("disc-alpha").is_ok());
    // The other provider was left alone.
    assert_eq!(state(&harness, "auto-b").state, DiscoveryStatus::Ok);

    // Asking by hand records its outcome too: a failure, then a success
    // with another list.
    let error = harness.gateway.discover("auto-a").await.unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(state(&harness, "auto-a").state, DiscoveryStatus::Failed);

    harness
        .fake
        .always("key-a-secret-0123456789abcdef", Behaviour::text("fine"));
    harness.fake.set_models(&["disc-gamma"]);
    harness.gateway.discover("auto-a").await.unwrap();
    let ok = state(&harness, "auto-a");
    assert_eq!(
        (ok.state, ok.models, ok.error),
        (DiscoveryStatus::Ok, 1, None)
    );

    // Asking a provider that does not want discovery leaves it `off`.
    harness.gateway.discover("mock").await.unwrap();
    assert_eq!(state(&harness, "mock").state, DiscoveryStatus::Off);
}

#[tokio::test]
async fn a_listing_under_way_is_pending() {
    let fake_first = Harness::start(FOUR_PROVIDERS).await;
    // A provider is added whose listing takes a moment.
    fake_first.fake.script(
        "key-slow-1",
        [Behaviour::Slow {
            delay: Duration::from_millis(300),
            then: Box::new(Behaviour::text("unused")),
        }],
    );
    let before = switchyard_core::util::now_unix_ms();
    fake_first
        .reconfigure(&format!(
            "{PREAMBLE}\n{FOUR_PROVIDERS}\n[[providers]]\nname = \"slow\"\nkind = \"openai-compat\"\nbase_url = \"{{base}}/v1\"\nprefix = \"slow\"\napi_keys = [\"key-slow-1\"]\n"
        ))
        .await;
    let pending = state(&fake_first, "slow");
    assert_eq!(pending.state, DiscoveryStatus::Pending);
    assert!(pending.at.is_some_and(|at| at >= before), "{pending:?}");
    assert_eq!((pending.models, pending.error), (0, None));
    let ok = settled(&fake_first, "slow").await;
    assert_eq!((ok.state, ok.models), (DiscoveryStatus::Ok, 2));
    assert!(ok.at >= pending.at);
}
