//! Regression tests (review finding SCHED-1): session affinity versus alias
//! target order.
//!
//! `AliasConfig` (crates/core/src/config.rs): "Requests for `name` are served
//! by `targets`, tried in order: the next target is used only when no
//! credential can serve the previous one."
//!
//! `RoutingConfig::session_affinity`: "Keep a conversation on the credential
//! that served it last, so provider prompt caches stay effective."
//!
//! A binding is to a credential *serving a model*: prompt caches are per
//! model. When the bound credential rests for the model the conversation was
//! on (a per-model cooldown started by some other conversation) but is still
//! fine for a later target's model, staying on the credential would mean the
//! fallback model with a cold cache — while another credential is ready for
//! the preferred model. The binding must not win there; the regular target
//! order applies.

mod common;

use common::{error, fixture};
use pretty_assertions::assert_eq;
use switchyard_core::FailureClass;

/// One provider, two keys, an alias that prefers opus and falls back to
/// sonnet. Fill-first so "who gets picked" needs no rotation bookkeeping.
const SAME_PROVIDER_ALIAS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "claude-sonnet-4-6"]
"#;

#[test]
fn bound_session_is_not_downgraded_to_the_fallback_target_while_the_preferred_one_is_servable() {
    let f = fixture(SAME_PROVIDER_ALIAS);

    // Conversation s1 is served by key a with the preferred model.
    let first = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert!(first.credential.api_key.contains("-a-"));
    assert_eq!(first.client_model, "claude-opus-4-6");
    f.succeed(&first, 100);

    // Some other conversation hits a rate limit for opus on key a. That
    // rests (a, opus) only; s1's binding to a is untouched.
    let other = f.pick("smart").unwrap();
    assert!(other.credential.api_key.contains("-a-"));
    assert_eq!(other.upstream_model, "claude-opus-4-6");
    f.fail(&other, &error(FailureClass::RateLimit, Some(60_000)));

    // A fresh conversation is routed correctly: key b, preferred model.
    let fresh = f.pick_with("smart", &[], Some("s2")).unwrap();
    assert_eq!(fresh.client_model, "claude-opus-4-6");
    assert!(fresh.credential.api_key.contains("-b-"));

    // s1 must get the same: key b can serve the preferred target, so the
    // fallback target must not be used.
    let next = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(
        next.client_model, "claude-opus-4-6",
        "the next alias target is only for when no credential can serve the previous one; \
         got {} on {}",
        next.upstream_model, next.credential.label
    );
    assert!(next.credential.api_key.contains("-b-"));
}

/// Same defect under the default strategy and with two bound conversations.
#[test]
fn bound_sessions_follow_target_order_under_round_robin_too() {
    let f = fixture(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "claude-sonnet-4-6"]
"#,
    );
    let s1 = f.pick_with("smart", &[], Some("s1")).unwrap(); // key a
    let s2 = f.pick_with("smart", &[], Some("s2")).unwrap(); // key b
    let s3 = f.pick_with("smart", &[], Some("s3")).unwrap(); // key a again
    assert_eq!(s1.credential.id, s3.credential.id);
    assert_ne!(s1.credential.id, s2.credential.id);

    // s3's request is rate limited: (a, opus) rests for a minute.
    f.fail(&s3, &error(FailureClass::RateLimit, Some(60_000)));

    // s1 is still bound to a. Key b is ready for opus.
    let next = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(next.client_model, "claude-opus-4-6");
    assert_eq!(next.credential.id, s2.credential.id);
}

#[test]
fn the_conversation_stays_where_it_moved_to() {
    let f = fixture(SAME_PROVIDER_ALIAS);
    let first = f.pick_with("smart", &[], Some("s1")).unwrap();
    f.succeed(&first, 100);
    let other = f.pick("smart").unwrap();
    f.fail(&other, &error(FailureClass::RateLimit, Some(60_000)));

    // Moved to key b, same model.
    let moved = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert!(moved.credential.api_key.contains("-b-"));
    f.succeed(&moved, 100);

    // Key a is back (and fill-first would choose it), but the conversation's
    // cache now lives on b.
    f.advance(60);
    assert!(f.pick("smart").unwrap().credential.api_key.contains("-a-"));
    let later = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert!(later.credential.api_key.contains("-b-"));
    assert_eq!(later.client_model, "claude-opus-4-6");
}

const ONE_KEY_ALIAS: &str = r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-key-a-0000000000000"]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "claude-sonnet-4-6"]
"#;

#[test]
fn a_model_scoped_failure_releases_only_a_binding_to_that_model() {
    let f = fixture(ONE_KEY_ALIAS);
    // s1 starts on (a, opus); the attempt is still in flight.
    let in_flight = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(in_flight.client_model, "claude-opus-4-6");

    // Opus gets rate limited on the key by someone else, so s1's next
    // request is served by sonnet and the conversation is bound there.
    let other = f.pick("smart").unwrap();
    f.fail(&other, &error(FailureClass::RateLimit, Some(30_000)));
    let on_sonnet = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(on_sonnet.client_model, "claude-sonnet-4-6");
    f.succeed(&on_sonnet, 100);

    // The in-flight opus attempt now fails with a 5xx. That says nothing
    // about (a, sonnet), where the conversation lives.
    f.fail_class(&in_flight, FailureClass::Server);
    f.advance(60);
    let still = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(still.client_model, "claude-sonnet-4-6");
    // A conversation without a binding gets the preferred target again.
    assert_eq!(
        f.pick_with("smart", &[], Some("s2")).unwrap().client_model,
        "claude-opus-4-6"
    );
}

#[test]
fn a_credential_wide_failure_releases_the_binding_whatever_the_model() {
    let f = fixture(ONE_KEY_ALIAS);
    let in_flight = f.pick_with("smart", &[], Some("s1")).unwrap();
    let other = f.pick("smart").unwrap();
    f.fail(&other, &error(FailureClass::RateLimit, Some(30_000)));
    let on_sonnet = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(on_sonnet.client_model, "claude-sonnet-4-6");
    assert_eq!(f.scheduler.session_bindings(), 1);

    // The key itself is rejected: the binding to it is void.
    f.fail_class(&in_flight, FailureClass::Auth);
    assert_eq!(f.scheduler.session_bindings(), 0);
    f.advance(1800);
    assert_eq!(
        f.pick_with("smart", &[], Some("s1")).unwrap().client_model,
        "claude-opus-4-6"
    );
}

#[test]
fn a_late_success_on_another_model_does_not_keep_the_binding_alive() {
    let f = fixture(&format!(
        "[routing]\nsession_affinity_ttl_secs = 60\n{ONE_KEY_ALIAS}"
    ));
    let in_flight = f.pick_with("smart", &[], Some("s1")).unwrap();
    let other = f.pick("smart").unwrap();
    f.fail(&other, &error(FailureClass::RateLimit, Some(10_000)));
    let on_sonnet = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(on_sonnet.client_model, "claude-sonnet-4-6");

    // 50 s later the long opus response completes. It is not a use of the
    // (a, sonnet) binding, which therefore runs out at 60 s.
    f.advance(50);
    f.succeed(&in_flight, 50_000);
    f.advance(10);
    assert_eq!(
        f.pick_with("smart", &[], Some("s1")).unwrap().client_model,
        "claude-opus-4-6"
    );
}
