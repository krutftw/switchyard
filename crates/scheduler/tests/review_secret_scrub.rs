//! Regression tests (review finding SCHED-5): no key material survives in
//! `last_error`.
//!
//! Every key-shaped run in an upstream message is masked, however many sit
//! in one whitespace-delimited word and whatever precedes them. Messages
//! without spaces are normal: codecs fall back to the compact JSON of the
//! error payload when it has no `message`
//! (e.g. `codec-chat/src/error.rs`: `truncate_chars(&payload.to_string(), …)`).
//! The credential's own key is removed even when it is too short to be
//! recognised by shape.
//!
//! The stored message is shown in the dashboard (`CredentialSnapshot`) and is
//! quoted to API clients in the "cooling down" error, so DESIGN's rule
//! "Never log or return API keys" applies to it.

mod common;

use common::{error, fixture};
use switchyard_core::{ApiError, FailureClass};

const ONE: &str = r#"
[[providers]]
name = "compat"
kind = "openai-compat"
base_url = "http://upstream.test/v1"
api_keys = ["sk-own-key-000000000000000000"]

[[providers.models]]
id = "m"
"#;

/// Keys that are *not* the credential's own (another tenant's, or a
/// differently encoded copy), so only the shape-based masking can catch them.
const FOREIGN_A: &str = "sk-proj-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const FOREIGN_B: &str = "sk-proj-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";

fn stored_and_client_visible(message: String) -> (String, String) {
    let f = fixture(ONE);
    let lease = f.pick("m").unwrap();
    let mut failure = error(FailureClass::Auth, None);
    failure.info.message = message;
    f.fail(&lease, &failure);
    let stored = f.all_credentials()[0].last_error.clone().unwrap().message;
    let api: ApiError = f.pick("m").unwrap_err().into();
    (stored, api.message)
}

#[test]
fn second_key_in_a_compact_json_message_is_masked() {
    let (stored, client) = stored_and_client_visible(format!(
        r#"{{"error":{{"type":"invalid_api_key","received":"{FOREIGN_A}","expected_one_of":["{FOREIGN_B}"]}}}}"#
    ));
    assert!(!stored.contains(FOREIGN_A), "{stored}");
    assert!(
        !stored.contains(FOREIGN_B),
        "snapshot leaks a key: {stored}"
    );
    assert!(
        !client.contains(FOREIGN_B),
        "client error leaks a key: {client}"
    );
}

/// Short keys are normal for self-hosted OpenAI-compatible servers
/// (LiteLLM's documented default master key is `sk-1234`), and such servers
/// do echo the offending key. A key under 8 characters is removed where it
/// stands as a token of its own, which leaves ordinary words that merely
/// contain it alone.
#[test]
fn a_short_own_key_echoed_by_the_upstream_is_removed() {
    let f = fixture(
        r#"
[[providers]]
name = "litellm"
kind = "openai-compat"
base_url = "http://litellm.test/v1"
api_keys = ["sk-9x7Q"]

[[providers.models]]
id = "m"
"#,
    );
    let lease = f.pick("m").unwrap();
    assert_eq!(lease.credential.api_key, "sk-9x7Q");
    let mut failure = error(FailureClass::Auth, None);
    failure.info.message =
        "Authentication Error, Invalid proxy server token passed. Received API Key = sk-9x7Q"
            .into();
    f.fail(&lease, &failure);
    let stored = f.all_credentials()[0].last_error.clone().unwrap().message;
    assert!(
        !stored.contains("sk-9x7Q"),
        "snapshot leaks the key: {stored}"
    );
    let api: ApiError = f.pick("m").unwrap_err().into();
    assert!(
        !api.message.contains("sk-9x7Q"),
        "client error leaks the key: {}",
        api.message
    );
}

#[test]
fn key_after_a_short_key_like_token_in_the_same_word_is_masked() {
    // The first `sk-` candidate ("sk-invalid") is too short to be a key; the
    // real one follows without whitespace.
    let (stored, client) =
        stored_and_client_visible(format!(r#"{{"code":"sk-invalid","key":"{FOREIGN_A}"}}"#));
    assert!(
        !stored.contains(FOREIGN_A),
        "snapshot leaks a key: {stored}"
    );
    assert!(
        !client.contains(FOREIGN_A),
        "client error leaks a key: {client}"
    );
}
