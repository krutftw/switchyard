//! Regression tests for what the dashboard's engineers found against the
//! real admin API:
//!
//! * the credentials of a switched-off provider read `ready` and were
//!   counted as ready;
//! * a credential only the gateway can tell is unusable (a Vertex service
//!   account whose key file is missing) read `ready` too, was selected, and
//!   was mentioned nowhere;
//! * handing a provider the model list it already has re-derived the whole
//!   model table.

mod common;

use common::{config, fixture, resolver};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use switchyard_core::ModelInfo;
use switchyard_scheduler::{CredentialStatus, DisabledBy, PickError};

const TWO_PROVIDERS: &str = r#"
[[providers]]
name = "live"
kind = "openai"
api_keys = ["sk-live-aaaaaaaaaaaaaaaaaaaa"]

[[providers.credentials]]
api_key = "sk-live-off-aaaaaaaaaaaaaaaa"
label = "switched off"
disabled = true

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "parked"
kind = "openai"
enabled = false
api_keys = ["sk-parked-aaaaaaaaaaaaaaaaaa", "env:UNSET_PARKED_KEY"]

[[providers.credentials]]
api_key = "sk-parked-off-aaaaaaaaaaaaaa"
label = "also off"
disabled = true

[[providers.models]]
id = "gpt-5.5"
"#;

// ---------------------------------------------------------------------------
// Credentials of a disabled provider
// ---------------------------------------------------------------------------

#[test]
fn credentials_of_a_disabled_provider_are_disabled_not_ready() {
    let f = fixture(TWO_PROVIDERS);
    let snapshot = f.scheduler.snapshot();
    assert!(snapshot[0].enabled && !snapshot[1].enabled);

    let live = &snapshot[0].credentials;
    assert_eq!(live[0].status, CredentialStatus::Ready);
    assert_eq!(live[0].disabled_by, None);
    assert_eq!(live[1].status, CredentialStatus::Disabled);
    assert_eq!(live[1].disabled_by, Some(DisabledBy::Credential));

    // Every credential of the parked provider is out of rotation because of
    // the provider, whatever else is true of it; `disabled` and `usable`
    // still describe the credential itself.
    let parked = &snapshot[1].credentials;
    assert_eq!(parked.len(), 3);
    for credential in parked {
        assert_eq!(credential.status, CredentialStatus::Disabled);
        assert_eq!(credential.disabled_by, Some(DisabledBy::Provider));
        assert!(credential.provider_disabled());
    }
    assert!(!parked[0].disabled && parked[0].usable);
    assert!(!parked[1].disabled && !parked[1].usable);
    assert!(parked[2].disabled && parked[2].usable);

    // The single-credential view agrees with the list.
    assert_eq!(
        f.scheduler.credential_snapshot(&parked[0].id).as_ref(),
        Some(&parked[0])
    );

    // Counts derived from snapshots can leave the parked ones out.
    let all: Vec<_> = snapshot.iter().flat_map(|p| &p.credentials).collect();
    assert_eq!(all.len(), 5);
    let in_rotation: Vec<_> = all.iter().filter(|c| !c.provider_disabled()).collect();
    assert_eq!(in_rotation.len(), 2);
    let ready = all
        .iter()
        .filter(|c| c.status == CredentialStatus::Ready)
        .count();
    assert_eq!(ready, 1);
    // Nothing of a parked provider is worth a warning.
    assert!(f.scheduler.warnings().is_empty());
}

#[test]
fn disabled_by_is_serialised_only_when_disabled() {
    let f = fixture(TWO_PROVIDERS);
    let live = f.scheduler.snapshot()[0].credentials[0].id.clone();
    f.scheduler.set_runtime_disabled(&live, true);
    let value = serde_json::to_value(f.scheduler.snapshot()).unwrap();
    assert_eq!(value[0]["credentials"][0]["status"], "disabled");
    assert_eq!(value[0]["credentials"][0]["disabled_by"], "runtime");
    assert_eq!(value[0]["credentials"][1]["disabled_by"], "credential");
    assert_eq!(value[1]["credentials"][0]["status"], "disabled");
    assert_eq!(value[1]["credentials"][0]["disabled_by"], "provider");
    assert_eq!(value[1]["credentials"][0]["disabled"], false);

    f.scheduler.set_runtime_disabled(&live, false);
    let value = serde_json::to_value(f.scheduler.snapshot()).unwrap();
    assert_eq!(value[0]["credentials"][0]["status"], "ready");
    assert!(value[0]["credentials"][0].get("disabled_by").is_none());
    assert_eq!(DisabledBy::Provider.as_str(), "provider");
}

#[test]
fn enabling_the_provider_puts_its_credentials_back() {
    let f = fixture(TWO_PROVIDERS);
    f.rebuild(&TWO_PROVIDERS.replace("enabled = false\n", ""));
    let parked = &f.scheduler.snapshot()[1].credentials;
    assert_eq!(parked[0].status, CredentialStatus::Ready);
    assert_eq!(parked[0].disabled_by, None);
    assert_eq!(parked[1].status, CredentialStatus::Unusable);
    assert_eq!(parked[2].disabled_by, Some(DisabledBy::Credential));
}

// ---------------------------------------------------------------------------
// Runtime usability
// ---------------------------------------------------------------------------

const VERTEX: &str = r#"
[[providers]]
name = "vertex-eu"
kind = "vertex"
project = "demo"
location = "europe-west4"

[[providers.credentials]]
service_account_file = "keys/first.json"
label = "first"

[[providers.credentials]]
service_account_file = "keys/second.json"
label = "second"

[[providers.models]]
id = "gemini-2.5-pro"
"#;

#[test]
fn a_credential_marked_unusable_is_never_selected_and_says_why() {
    let f = fixture(VERTEX);
    let ids: Vec<String> = f.all_credentials().into_iter().map(|c| c.id).collect();
    assert!(f.scheduler.warnings().is_empty());

    let reason = "service account file keys/first.json does not exist";
    assert!(f.scheduler.set_unusable(&ids[0], Some(reason.to_string())));
    assert!(
        !f.scheduler
            .set_unusable("vertex-eu:000000000000", Some("x".into()))
    );
    assert!(!f.scheduler.set_unusable("vertex-eu:000000000000", None));

    // Never selected, in any rotation position.
    for _ in 0..6 {
        assert_eq!(f.pick("gemini-2.5-pro").unwrap().credential.label, "second");
    }
    let usable: Vec<String> = f
        .scheduler
        .credentials("vertex-eu")
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert_eq!(usable, vec!["second"]);

    // Shown as unusable, with the reason.
    let first = f.credential_by_label("first");
    assert_eq!(first.status, CredentialStatus::Unusable);
    assert!(!first.usable);
    assert_eq!(first.unusable_reason.as_deref(), Some(reason));
    assert_eq!(first.disabled_by, None);
    let value = serde_json::to_value(&first).unwrap();
    assert_eq!(value["status"], "unusable");
    assert_eq!(value["usable"], false);
    assert_eq!(value["unusable_reason"], reason);
    assert_eq!(
        f.credential_by_label("second").status,
        CredentialStatus::Ready
    );

    // Listed among the warnings, like an unset environment variable is.
    assert_eq!(
        f.scheduler.warnings(),
        vec![format!(
            "provider `vertex-eu`: credential 1 (first) is unusable: {reason}"
        )]
    );
    // And not available in the model table.
    let table = f.scheduler.models();
    assert_eq!(table[0].routes[0].credentials_total, 2);
    assert_eq!(table[0].routes[0].credentials_available, 1);

    // Both marked: nothing can serve the model.
    assert!(f.scheduler.set_unusable(&ids[1], Some(String::new())));
    assert!(matches!(
        f.pick("gemini-2.5-pro"),
        Err(PickError::NoCredentials { .. })
    ));
    assert_eq!(
        f.credential_by_label("second").unusable_reason.as_deref(),
        Some("marked as unusable")
    );
    assert_eq!(f.scheduler.warnings().len(), 2);

    // Cleared again: back in rotation, no warning left.
    assert!(f.scheduler.set_unusable(&ids[0], None));
    assert!(f.scheduler.set_unusable(&ids[1], None));
    let labels: Vec<String> = (0..2)
        .map(|_| f.pick("gemini-2.5-pro").unwrap().credential.label)
        .collect();
    assert!(labels.contains(&"first".to_string()) && labels.contains(&"second".to_string()));
    assert!(f.all_credentials().iter().all(|c| c.usable));
    assert!(f.scheduler.warnings().is_empty());
}

#[test]
fn the_unusable_mark_survives_a_rebuild_until_it_is_cleared_or_the_credential_changes() {
    let f = fixture(VERTEX);
    let first = f.credential_by_label("first").id;
    f.scheduler
        .set_unusable(&first, Some("the key file is not valid JSON".to_string()));

    // An unrelated edit: the id is unchanged, so is the mark.
    f.rebuild(&format!("{VERTEX}\n[routing]\nmax_attempts = 5\n"));
    let after = f.credential_by_label("first");
    assert_eq!(after.id, first);
    assert_eq!(after.status, CredentialStatus::Unusable);
    assert_eq!(
        after.unusable_reason.as_deref(),
        Some("the key file is not valid JSON")
    );
    assert_eq!(f.scheduler.warnings().len(), 1);
    assert_eq!(f.pick("gemini-2.5-pro").unwrap().credential.label, "second");

    // Re-set with another reason: the new one is shown.
    f.scheduler
        .set_unusable(&first, Some("the key file\n  is empty".to_string()));
    assert_eq!(
        f.credential_by_label("first").unusable_reason.as_deref(),
        Some("the key file is empty")
    );
    // A reason is shown to operators: anything in it shaped like a key is
    // masked, and it is kept to one short line.
    f.scheduler.set_unusable(
        &first,
        Some(format!(
            "rejected: sk-proj-abcdefghijklmnopqrstuvwxyz {}",
            "x".repeat(500)
        )),
    );
    let shown = f
        .credential_by_label("first")
        .unusable_reason
        .unwrap_or_default();
    assert!(shown.starts_with("rejected: sk-pro…wxyz x"), "{shown}");
    assert!(shown.chars().count() <= 201, "{shown}");
    assert!(!f.scheduler.warnings().join("\n").contains("abcdefghij"));

    // Another file is another credential: it starts without the mark.
    f.rebuild(&VERTEX.replace("keys/first.json", "keys/replaced.json"));
    let replaced = f.credential_by_label("first");
    assert_ne!(replaced.id, first);
    assert_eq!(replaced.status, CredentialStatus::Ready);
    assert!(f.scheduler.warnings().is_empty());
}

#[test]
fn switches_and_the_configurations_own_reason_come_before_the_mark() {
    let toml = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:UNSET_OPENAI_KEY", "sk-plain-aaaaaaaaaaaaaaaaaa"]
"#;
    let f = fixture(toml);
    let all = f.all_credentials();
    // The configuration already says why the first cannot be used.
    f.scheduler
        .set_unusable(&all[0].id, Some("something else".to_string()));
    assert_eq!(
        f.credential_by_label("env:UNSET_OPENAI_KEY")
            .unusable_reason
            .as_deref(),
        Some("environment variable UNSET_OPENAI_KEY is not set")
    );
    assert_eq!(f.scheduler.warnings().len(), 1);

    // A credential that is switched off reads `disabled`, and nobody is
    // warned about it; the reason is still there to be read.
    f.scheduler
        .set_unusable(&all[1].id, Some("checked and found wanting".to_string()));
    assert_eq!(f.scheduler.warnings().len(), 2);
    f.scheduler.set_runtime_disabled(&all[1].id, true);
    let second = f.scheduler.credential_snapshot(&all[1].id).unwrap();
    assert_eq!(second.status, CredentialStatus::Disabled);
    assert_eq!(second.disabled_by, Some(DisabledBy::Runtime));
    assert!(!second.usable);
    assert_eq!(f.scheduler.warnings().len(), 1);
    // Switched on again while still marked: unusable, and not selected.
    f.scheduler.set_runtime_disabled(&all[1].id, false);
    assert_eq!(
        f.scheduler.credential_snapshot(&all[1].id).unwrap().status,
        CredentialStatus::Unusable
    );
    assert!(matches!(
        f.pick("gpt-5.5"),
        Err(PickError::NoCredentials { .. })
    ));
}

// ---------------------------------------------------------------------------
// Model lists
// ---------------------------------------------------------------------------

const MOCKS: &str = r#"
[[providers]]
name = "one"
kind = "mock"

[[providers]]
name = "two"
kind = "mock"
prefix = "b"
"#;

#[test]
fn several_model_lists_are_taken_in_one_go() {
    let f = fixture(MOCKS);
    let lists = HashMap::from([
        ("one".to_string(), vec![ModelInfo::bare("mock-echo")]),
        ("two".to_string(), vec![ModelInfo::bare("mock-think")]),
        ("ghost".to_string(), vec![ModelInfo::bare("unused")]),
    ]);
    // Two of the three names are configured providers.
    assert_eq!(f.scheduler.set_discovered_many(lists.clone()), 2);
    assert_eq!(f.pick("mock-echo").unwrap().credential.provider, "one");
    assert_eq!(f.pick("b/mock-think").unwrap().credential.provider, "two");
    assert!(f.scheduler.resolve("unused").is_err());

    // The same lists again change nothing (and the answer is the same).
    assert_eq!(f.scheduler.set_discovered_many(lists), 2);
    assert!(
        f.scheduler
            .set_discovered("one", vec![ModelInfo::bare("mock-echo")])
    );
    assert!(
        !f.scheduler
            .set_discovered("ghost", vec![ModelInfo::bare("x")])
    );
    assert_eq!(f.scheduler.models_routable(), 3);

    // An empty list forgets the remembered one; one that was never there
    // is nothing to forget.
    let lists = HashMap::from([
        ("one".to_string(), Vec::new()),
        ("two".to_string(), vec![ModelInfo::bare("mock-lorem")]),
    ]);
    assert_eq!(f.scheduler.set_discovered_many(lists), 2);
    assert!(f.scheduler.resolve("mock-echo").is_err());
    assert!(f.scheduler.resolve("mock-think").is_err());
    assert!(f.scheduler.resolve("mock-lorem").is_ok());
    assert!(f.scheduler.set_discovered("one", Vec::new()));
    assert_eq!(f.scheduler.set_discovered_many(HashMap::new()), 0);

    // Remembered across a rebuild, as before.
    f.scheduler
        .rebuild(&config(MOCKS), &resolver, HashMap::new());
    assert!(f.scheduler.resolve("b/mock-lorem").is_ok());
    assert_eq!(
        serde_json::to_value(f.scheduler.models()).unwrap()[0]["routes"][0]["provider"],
        json!("two")
    );
}

const DISCOVERING: &str = r#"
[[providers]]
name = "listed"
kind = "openai-compat"
base_url = "https://one.example/v1"
api_keys = ["sk-listed-aaaaaaaaaaaaaaaaaa"]
"#;

/// The size of the list in use is what a discovery state reports as
/// `models`: it has to follow what a rebuild keeps and what it drops.
#[test]
fn the_size_of_the_remembered_list_can_be_asked() {
    let f = fixture(DISCOVERING);
    assert_eq!(f.scheduler.discovered_models("listed"), 0);
    assert_eq!(f.scheduler.discovered_models("ghost"), 0);
    f.scheduler.set_discovered(
        "listed",
        vec![ModelInfo::bare("alpha"), ModelInfo::bare("beta")],
    );
    assert_eq!(f.scheduler.discovered_models("listed"), 2);

    // Kept while the provider is switched off and on again …
    let off = DISCOVERING.replace(
        "kind = \"openai-compat\"",
        "kind = \"openai-compat\"\nenabled = false",
    );
    f.scheduler
        .rebuild(&config(&off), &resolver, HashMap::new());
    assert_eq!(f.scheduler.discovered_models("listed"), 2);
    f.scheduler
        .rebuild(&config(DISCOVERING), &resolver, HashMap::new());
    assert_eq!(f.scheduler.discovered_models("listed"), 2);
    assert!(f.scheduler.resolve("alpha").is_ok());

    // … dropped with the endpoint it was listed from, and when forgotten.
    let moved = DISCOVERING.replace("one.example", "two.example");
    f.scheduler
        .rebuild(&config(&moved), &resolver, HashMap::new());
    assert_eq!(f.scheduler.discovered_models("listed"), 0);
    f.scheduler
        .set_discovered("listed", vec![ModelInfo::bare("gamma")]);
    assert_eq!(f.scheduler.discovered_models("listed"), 1);
    f.scheduler.set_discovered("listed", Vec::new());
    assert_eq!(f.scheduler.discovered_models("listed"), 0);
}
