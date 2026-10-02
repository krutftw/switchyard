//! Review of the integration pass: the mark a missing or invalid
//! service-account file puts on a credential (`Scheduler::set_unusable`)
//! must go away when the credential stops naming that file.
//!
//! `Inner::check_service_accounts` only looks at credentials that name a
//! file (`if file.is_empty() { continue; }`, and an early return when no
//! credential of the configuration names one). The scheduler keeps the mark
//! across rebuilds for an unchanged credential id — and the id of a
//! credential that has an `api_key` is derived from that key, not from the
//! file. So a credential with a key *and* a file keeps its id when the file
//! reference is removed, and nothing ever takes the mark off: it stays
//! `unusable`, quoting a file it no longer refers to, until the process is
//! restarted.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use support::{Harness, PREAMBLE};
use switchyard_scheduler::CredentialStatus;

/// One vertex credential that has an API key and (wrongly) names a key file
/// that does not exist, next to one that only has a file.
const BOTH: &str = r#"
[[providers]]
name = "vx"
kind = "vertex"
base_url = "{base}"
project = "demo-project"
location = "us-central1"
[[providers.credentials]]
api_key = "vx-api-key-0123456789abcdef"
service_account_file = "missing.json"
label = "both"
[[providers.models]]
id = "gemini-vx"
"#;

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
async fn a_credential_that_no_longer_names_a_key_file_is_usable_again() {
    let harness = Harness::start(BOTH).await;
    let before = credentials(&harness);
    assert_eq!(before[0].0, CredentialStatus::Unusable, "{before:?}");
    assert!(before[0].2.contains("missing.json"), "{before:?}");
    let id = harness.gateway.scheduler().snapshot()[0].credentials[0]
        .id
        .clone();

    // The operator takes the file reference out; the API key stays.
    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            BOTH.replace("service_account_file = \"missing.json\"\n", "")
        ))
        .await;
    let snapshot = harness.gateway.scheduler().snapshot();
    assert_eq!(
        snapshot[0].credentials[0].id, id,
        "the credential is the same one: its id comes from the key"
    );

    let after = credentials(&harness);
    assert_eq!(
        (after[0].0, after[0].1, after[0].2.as_str()),
        (CredentialStatus::Ready, true, ""),
        "the credential names no key file any more, yet it is still marked for one"
    );
    assert!(
        harness.gateway.scheduler().warnings().is_empty(),
        "{:?}",
        harness.gateway.scheduler().warnings()
    );
}

/// The mark follows the configuration in both directions, any number of
/// times, and the credential serves requests while it names no file.
#[tokio::test]
async fn the_mark_comes_and_goes_with_the_file_reference() {
    let harness = Harness::start(BOTH).await;
    let with_file = format!("{PREAMBLE}\n{BOTH}");
    let without_file = with_file.replace("service_account_file = \"missing.json\"\n", "");
    assert_ne!(with_file, without_file);

    for round in 0..3 {
        harness.reconfigure(&without_file).await;
        let now = credentials(&harness);
        assert_eq!(now[0].0, CredentialStatus::Ready, "round {round}: {now:?}");
        assert_eq!(
            harness.gateway.scheduler().credentials("vx").len(),
            1,
            "round {round}: the credential can be picked"
        );
        // A reload and a provider test check the files again: neither may
        // bring the mark back.
        harness.reload().await;
        let _ = harness.gateway.test_provider("vx", None).await;
        let now = credentials(&harness);
        assert_eq!(now[0].0, CredentialStatus::Ready, "round {round}: {now:?}");

        harness.reconfigure(&with_file).await;
        let now = credentials(&harness);
        assert_eq!(
            now[0].0,
            CredentialStatus::Unusable,
            "round {round}: {now:?}"
        );
        assert!(now[0].2.contains("missing.json"), "round {round}: {now:?}");
        assert!(harness.gateway.scheduler().credentials("vx").is_empty());
    }
}

/// The same with another credential that still names a (missing) file, so
/// that the check does run over the configuration.
#[tokio::test]
async fn the_mark_goes_when_another_credential_still_names_a_file() {
    let two = format!(
        "{BOTH}[[providers.credentials]]\nservice_account_file = \"other.json\"\nlabel = \"file only\"\n"
    )
    // Credentials come before the models table in the fixture.
    .replace(
        "[[providers.models]]\nid = \"gemini-vx\"\n[[providers.credentials]]\nservice_account_file = \"other.json\"\nlabel = \"file only\"\n",
        "[[providers.credentials]]\nservice_account_file = \"other.json\"\nlabel = \"file only\"\n[[providers.models]]\nid = \"gemini-vx\"\n",
    );
    let harness = Harness::start(&two).await;
    let before = credentials(&harness);
    assert_eq!(before.len(), 2, "{before:?}");
    assert_eq!(before[0].0, CredentialStatus::Unusable, "{before:?}");
    assert_eq!(before[1].0, CredentialStatus::Unusable, "{before:?}");

    harness
        .reconfigure(&format!(
            "{PREAMBLE}\n{}",
            two.replace("service_account_file = \"missing.json\"\n", "")
        ))
        .await;
    let after = credentials(&harness);
    assert_eq!(
        (after[0].0, after[0].1, after[0].2.as_str()),
        (CredentialStatus::Ready, true, ""),
        "{after:?}"
    );
    // The other one is still what it was.
    assert_eq!(after[1].0, CredentialStatus::Unusable, "{after:?}");
    assert!(after[1].2.contains("other.json"), "{after:?}");
}
