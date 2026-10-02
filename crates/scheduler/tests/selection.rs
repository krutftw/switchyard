//! Selection: strategies, priority tiers, the tried list, alias target
//! order and session affinity.

mod common;

use common::{Fixture, error, fixture};
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use switchyard_core::{ApiError, FailureClass};
use switchyard_scheduler::{Lease, PickError};

/// Keys are `sk-key-<letter>-…`; tests talk about credentials by letter.
fn letter(lease: &Lease) -> char {
    lease
        .credential
        .api_key
        .strip_prefix("sk-key-")
        .and_then(|rest| rest.chars().next())
        .expect("test key")
}

fn letters(f: &Fixture, model: &str, n: usize) -> String {
    (0..n).map(|_| letter(&f.pick(model).unwrap())).collect()
}

fn session_letter(f: &Fixture, model: &str, session: &str) -> char {
    letter(&f.pick_with(model, &[], Some(session)).unwrap())
}

fn three_keys(routing: &str) -> String {
    format!(
        r#"
[routing]
{routing}

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000", "sk-key-c-0000000000000"]
"#
    )
}

fn weighted(a: u32, b: u32, c: Option<u32>) -> String {
    let c = c.map(|w| format!("weight = {w}")).unwrap_or_default();
    format!(
        r#"
[routing]
strategy = "weighted"

[[providers]]
name = "openai"
kind = "openai"

[[providers.credentials]]
api_key = "sk-key-a-0000000000000"
weight = {a}

[[providers.credentials]]
api_key = "sk-key-b-0000000000000"
weight = {b}

[[providers.credentials]]
api_key = "sk-key-c-0000000000000"
{c}
"#
    )
}

const TIERS: &str = r#"
[[providers]]
name = "primary"
kind = "openai"
priority = 10
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "backup"
kind = "openai-compat"
base_url = "http://backup.test/v1"
api_keys = ["sk-key-c-0000000000000"]

[[providers.models]]
id = "gpt-5.5-backup"
alias = "gpt-5.5"
"#;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

#[test]
fn round_robin_rotates_in_config_order() {
    let f = fixture(&three_keys(""));
    assert_eq!(letters(&f, "gpt-5.5", 9), "abcabcabc");
}

#[test]
fn round_robin_distribution_is_even_over_many_picks() {
    let f = fixture(&three_keys("strategy = \"round-robin\""));
    let mut counts: HashMap<char, usize> = HashMap::new();
    for _ in 0..300 {
        *counts
            .entry(letter(&f.pick("gpt-5.5").unwrap()))
            .or_default() += 1;
    }
    assert_eq!(counts, HashMap::from([('a', 100), ('b', 100), ('c', 100)]));
}

#[test]
fn round_robin_keeps_a_cursor_per_model() {
    let f = fixture(&three_keys(""));
    assert_eq!(letters(&f, "gpt-5.5", 2), "ab");
    // A different model starts its own rotation.
    assert_eq!(letters(&f, "gpt-6-sol", 2), "ab");
    assert_eq!(letters(&f, "gpt-5.5", 2), "ca");
    // Spelling does not split the cursor.
    assert_eq!(letters(&f, "GPT-5.5", 1), "b");
}

#[test]
fn round_robin_stays_fair_when_a_credential_rests() {
    let f = fixture(&three_keys(""));
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    assert_eq!((letter(&a), letter(&b)), ('a', 'b'));
    f.fail_class(&b, FailureClass::Server);
    // b rests for 60 s: the rotation continues c, a, c, a.
    assert_eq!(letters(&f, "gpt-5.5", 4), "caca");
    f.advance(60);
    // Last pick was a; b is back in its place.
    assert_eq!(letters(&f, "gpt-5.5", 3), "bca");
}

#[test]
fn fill_first_sticks_to_the_first_available_credential() {
    let f = fixture(&three_keys("strategy = \"fill-first\""));
    assert_eq!(letters(&f, "gpt-5.5", 5), "aaaaa");
    let a = f.pick("gpt-5.5").unwrap();
    f.fail(&a, &error(FailureClass::RateLimit, Some(30_000)));
    assert_eq!(letters(&f, "gpt-5.5", 3), "bbb");
    // Another model on the first credential is unaffected.
    assert_eq!(letters(&f, "gpt-6-sol", 2), "aa");
    f.advance(30);
    assert_eq!(letters(&f, "gpt-5.5", 2), "aa");
}

#[test]
fn weighted_is_smooth_and_exactly_proportional() {
    let f = fixture(&weighted(5, 1, None));
    // The classic smooth sequence for weights 5/1/1.
    assert_eq!(letters(&f, "gpt-5.5", 7), "aabacaa");
    let mut counts: HashMap<char, usize> = HashMap::new();
    for _ in 0..700 {
        *counts
            .entry(letter(&f.pick("gpt-5.5").unwrap()))
            .or_default() += 1;
    }
    assert_eq!(counts, HashMap::from([('a', 500), ('b', 100), ('c', 100)]));
}

#[test]
fn weighted_proportions_for_other_weights() {
    let f = fixture(&weighted(3, 2, Some(1)));
    let mut counts: HashMap<char, usize> = HashMap::new();
    for _ in 0..600 {
        *counts
            .entry(letter(&f.pick("gpt-5.5").unwrap()))
            .or_default() += 1;
    }
    assert_eq!(counts, HashMap::from([('a', 300), ('b', 200), ('c', 100)]));
    // Never more than two consecutive picks of the heaviest credential.
    let run = letters(&f, "gpt-5.5", 60);
    assert!(!run.contains("aaa"), "{run}");
}

#[test]
fn weight_zero_is_excluded_from_weighted_rotation() {
    let f = fixture(&weighted(0, 2, Some(1)));
    let mut counts: HashMap<char, usize> = HashMap::new();
    for _ in 0..300 {
        *counts
            .entry(letter(&f.pick("gpt-5.5").unwrap()))
            .or_default() += 1;
    }
    assert_eq!(counts, HashMap::from([('b', 200), ('c', 100)]));

    // Even when it is the only one left untried.
    let ids: Vec<String> = f
        .all_credentials()
        .into_iter()
        .skip(1)
        .map(|c| c.id)
        .collect();
    assert_eq!(
        f.pick_with("gpt-5.5", &ids, None).unwrap_err(),
        PickError::Exhausted {
            model: "gpt-5.5".into()
        }
    );
}

#[test]
fn all_weights_zero_means_no_credentials_under_weighted() {
    let f = fixture(&weighted(0, 0, Some(0)));
    assert_eq!(
        f.pick("gpt-5.5").unwrap_err(),
        PickError::NoCredentials {
            model: "gpt-5.5".into()
        }
    );
}

#[test]
fn weight_zero_only_matters_for_the_weighted_strategy() {
    let toml =
        weighted(0, 1, None).replace("strategy = \"weighted\"", "strategy = \"round-robin\"");
    let f = fixture(&toml);
    assert_eq!(letters(&f, "gpt-5.5", 6), "abcabc");
}

#[test]
fn weighted_redistributes_while_a_credential_rests() {
    let f = fixture(&weighted(5, 1, None));
    let a = f.pick("gpt-5.5").unwrap();
    assert_eq!(letter(&a), 'a');
    f.fail_class(&a, FailureClass::Server);
    let run = letters(&f, "gpt-5.5", 20);
    assert_eq!(run.matches('b').count(), 10, "{run}");
    assert_eq!(run.matches('c').count(), 10, "{run}");
    f.advance(60);
    let run = letters(&f, "gpt-5.5", 70);
    // The heavy credential dominates again once it is back.
    assert!(run.matches('a').count() >= 50, "{run}");
}

#[test]
fn least_latency_tries_unmeasured_credentials_first() {
    let f = fixture(&three_keys("strategy = \"least-latency\""));
    // Nothing measured yet: plain rotation.
    assert_eq!(letters(&f, "gpt-5.5", 4), "abca");

    let f = fixture(&three_keys("strategy = \"least-latency\""));
    let a = f.pick("gpt-5.5").unwrap();
    f.succeed(&a, 300);
    let b = f.pick("gpt-5.5").unwrap();
    f.succeed(&b, 100);
    let c = f.pick("gpt-5.5").unwrap();
    f.succeed(&c, 200);
    assert_eq!((letter(&a), letter(&b), letter(&c)), ('a', 'b', 'c'));
    // All measured: the fastest takes everything.
    assert_eq!(letters(&f, "gpt-5.5", 10), "bbbbbbbbbb");
}

#[test]
fn least_latency_follows_the_moving_average() {
    let f = fixture(&three_keys("strategy = \"least-latency\""));
    for latency in [300, 100, 200] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.succeed(&lease, latency);
    }
    let b = f.pick("gpt-5.5").unwrap();
    assert_eq!(letter(&b), 'b');
    // One slow answer: 0.3 * 1000 + 0.7 * 100 = 370 ms, now slower than c.
    f.succeed(&b, 1000);
    let latencies: Vec<Option<u64>> = f.all_credentials().iter().map(|c| c.latency_ms).collect();
    assert_eq!(latencies, vec![Some(300), Some(370), Some(200)]);
    assert_eq!(letters(&f, "gpt-5.5", 3), "ccc");
    // The fastest rests: next best takes over.
    let c = f.pick("gpt-5.5").unwrap();
    f.fail_class(&c, FailureClass::Server);
    assert_eq!(letters(&f, "gpt-5.5", 2), "aa");
}

#[test]
fn least_latency_ties_rotate() {
    let f = fixture(&three_keys("strategy = \"least-latency\""));
    for _ in 0..3 {
        let lease = f.pick("gpt-5.5").unwrap();
        f.succeed(&lease, 100);
    }
    assert_eq!(letters(&f, "gpt-5.5", 6), "abcabc");
}

// ---------------------------------------------------------------------------
// Priority tiers
// ---------------------------------------------------------------------------

#[test]
fn lower_tier_is_used_only_when_the_higher_one_is_unavailable() {
    let f = fixture(TIERS);
    assert_eq!(letters(&f, "gpt-5.5", 6), "ababab");

    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    // One healthy credential in the top tier is enough to stay there.
    assert_eq!(letters(&f, "gpt-5.5", 3), "bbb");
    f.fail_class(&b, FailureClass::Server);

    let c = f.pick("gpt-5.5").unwrap();
    assert_eq!(letter(&c), 'c');
    assert_eq!(c.credential.provider, "backup");
    assert_eq!(c.upstream_model, "gpt-5.5-backup");
    assert_eq!(c.client_model, "gpt-5.5");
    assert_eq!(letters(&f, "gpt-5.5", 3), "ccc");

    f.advance(60);
    let back = letters(&f, "gpt-5.5", 4);
    assert!(!back.contains('c'), "{back}");
}

#[test]
fn tried_credentials_of_the_top_tier_fall_through_to_the_next() {
    let f = fixture(TIERS);
    let a = f.pick("gpt-5.5").unwrap();
    let mut tried = vec![a.credential.id.clone()];
    let b = f.pick_with("gpt-5.5", &tried, None).unwrap();
    assert_eq!(letter(&b), 'b');
    tried.push(b.credential.id.clone());
    let c = f.pick_with("gpt-5.5", &tried, None).unwrap();
    assert_eq!(letter(&c), 'c');
    tried.push(c.credential.id.clone());
    assert_eq!(
        f.pick_with("gpt-5.5", &tried, None).unwrap_err(),
        PickError::Exhausted {
            model: "gpt-5.5".into()
        }
    );
}

#[test]
fn credential_priority_overrides_the_provider_priority() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"
priority = 3

[[providers.credentials]]
api_key = "sk-key-a-0000000000000"
priority = 9

[[providers.credentials]]
api_key = "sk-key-b-0000000000000"

[[providers.credentials]]
api_key = "sk-key-c-0000000000000"
priority = 9

[[providers.credentials]]
api_key = "sk-key-d-0000000000000"
priority = -1
"#,
    );
    let priorities: Vec<i32> = f.all_credentials().iter().map(|c| c.priority).collect();
    assert_eq!(priorities, vec![9, 3, 9, -1]);
    assert_eq!(letters(&f, "gpt-5.5", 4), "acac");

    let a = f.pick("gpt-5.5").unwrap();
    let c = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    f.fail_class(&c, FailureClass::Server);
    assert_eq!(letters(&f, "gpt-5.5", 3), "bbb");
    let b = f.pick("gpt-5.5").unwrap();
    f.fail_class(&b, FailureClass::Server);
    assert_eq!(letters(&f, "gpt-5.5", 2), "dd");
}

#[test]
fn negative_priorities_rank_below_the_default() {
    let f = fixture(
        r#"
[[providers]]
name = "last-resort"
kind = "openai"
priority = -5
api_keys = ["sk-key-a-0000000000000"]

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "normal"
kind = "openai"
api_keys = ["sk-key-b-0000000000000"]

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    assert_eq!(letters(&f, "gpt-5.5", 3), "bbb");
}

#[test]
fn same_priority_providers_share_one_rotation() {
    let f = fixture(
        r#"
[[providers]]
name = "one"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]

[[providers.models]]
id = "gpt-5.5"

[[providers]]
name = "two"
kind = "openai"
api_keys = ["sk-key-c-0000000000000"]

[[providers.models]]
id = "gpt-5.5"
"#,
    );
    assert_eq!(letters(&f, "gpt-5.5", 6), "abcabc");
}

// ---------------------------------------------------------------------------
// Tried list
// ---------------------------------------------------------------------------

#[test]
fn tried_credentials_are_skipped_and_exhaustion_is_reported() {
    let f = fixture(&three_keys(""));
    let ids: Vec<String> = f.all_credentials().into_iter().map(|c| c.id).collect();

    let lease = f.pick_with("gpt-5.5", &ids[..1], None).unwrap();
    assert_eq!(letter(&lease), 'b');
    let lease = f.pick_with("gpt-5.5", &ids[..2], None).unwrap();
    assert_eq!(letter(&lease), 'c');
    let lease = f
        .pick_with("gpt-5.5", &[ids[0].clone(), ids[2].clone()], None)
        .unwrap();
    assert_eq!(letter(&lease), 'b');

    let err = f.pick_with("gpt-5.5", &ids, None).unwrap_err();
    assert_eq!(
        err,
        PickError::Exhausted {
            model: "gpt-5.5".into()
        }
    );
    let api: ApiError = err.into();
    assert_eq!(api.status, 503);
    assert_eq!(api.retry_after_secs, None);

    // Ids that match nothing are ignored.
    assert!(
        f.pick_with("gpt-5.5", &["openai:000000000000".to_string()], None)
            .is_ok()
    );
}

#[test]
fn failover_within_a_request_continues_the_rotation() {
    let f = fixture(&three_keys(""));
    let first = f.pick("gpt-5.5").unwrap();
    f.fail_class(&first, FailureClass::Transport);
    let second = f
        .pick_with("gpt-5.5", std::slice::from_ref(&first.credential.id), None)
        .unwrap();
    assert_eq!((letter(&first), letter(&second)), ('a', 'b'));
    f.succeed(&second, 120);
    // The next request carries on after b; a is resting.
    assert_eq!(letters(&f, "gpt-5.5", 3), "cbc");
}

// ---------------------------------------------------------------------------
// Alias targets
// ---------------------------------------------------------------------------

const ALIAS: &str = r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["sk-key-a-0000000000000"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-b-0000000000000"]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "gpt-5.5(high)"]
"#;

#[test]
fn alias_uses_the_next_target_only_when_the_previous_cannot_serve() {
    let f = fixture(ALIAS);
    assert_eq!(letters(&f, "smart", 4), "aaaa");
    let first = f.pick("smart").unwrap();
    assert_eq!(first.client_model, "claude-opus-4-6");
    assert_eq!(first.upstream_model, "claude-opus-4-6");

    f.fail(&first, &error(FailureClass::RateLimit, Some(30_000)));
    let second = f.pick("smart").unwrap();
    assert_eq!(letter(&second), 'b');
    assert_eq!(second.client_model, "gpt-5.5");
    assert_eq!(second.credential.provider, "openai");
    assert!(second.pinned_depth.is_some());

    f.advance(30);
    assert_eq!(letters(&f, "smart", 2), "aa");
}

#[test]
fn alias_reports_cooling_down_under_its_own_name() {
    let f = fixture(ALIAS);
    let a = f.pick("smart").unwrap();
    f.fail(&a, &error(FailureClass::RateLimit, Some(30_000)));
    let b = f.pick("smart").unwrap();
    f.fail(&b, &error(FailureClass::RateLimit, Some(12_000)));
    match f.pick("smart").unwrap_err() {
        PickError::CoolingDown {
            model, retry_after, ..
        } => {
            assert_eq!(model, "smart");
            // Soonest across all targets.
            assert_eq!(retry_after, common::secs(12));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        f.scheduler.soonest_recovery("smart"),
        Some(common::secs(12))
    );
}

#[test]
fn alias_skips_a_target_without_usable_credentials() {
    let f = fixture(
        r#"
[[providers]]
name = "claude"
kind = "anthropic"
api_keys = ["env:MISSING_ANTHROPIC_KEY"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-b-0000000000000"]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "gpt-5.5"]
"#,
    );
    assert_eq!(letters(&f, "smart", 3), "bbb");
}

// ---------------------------------------------------------------------------
// Session affinity
// ---------------------------------------------------------------------------

#[test]
fn session_sticks_to_its_credential() {
    let f = fixture(&three_keys(""));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", "s2"), 'b');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", "s2"), 'b');
    // Requests without a session keep rotating and do not disturb bindings.
    assert_eq!(letters(&f, "gpt-5.5", 4), "cabc");
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", " s1 "), 'a');
    assert_eq!(f.scheduler.session_bindings(), 2);
}

#[test]
fn blank_session_keys_are_ignored() {
    let f = fixture(&three_keys(""));
    assert_eq!(session_letter(&f, "gpt-5.5", "  "), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", ""), 'b');
    assert_eq!(f.scheduler.session_bindings(), 0);
}

#[test]
fn bindings_are_per_model() {
    let f = fixture(&three_keys(""));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(letters(&f, "gpt-6-sol", 1), "a");
    // The session's binding for one model says nothing about another.
    assert_eq!(session_letter(&f, "gpt-6-sol", "s1"), 'b');
    assert_eq!(session_letter(&f, "gpt-6-sol", "s1"), 'b');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
}

#[test]
fn binding_expires_after_the_idle_ttl() {
    let f = fixture(&three_keys("session_affinity_ttl_secs = 60"));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    f.advance(40);
    // Use within the TTL keeps the binding alive …
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    f.advance(40);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    // … 59 s idle is still fine, 60 s is not.
    f.advance(59);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    f.advance(60);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
}

#[test]
fn successful_outcome_refreshes_the_binding() {
    let f = fixture(&three_keys("session_affinity_ttl_secs = 60"));
    let lease = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    f.advance(50);
    // A long response finishing counts as use of the session.
    f.succeed(&lease, 50_000);
    f.advance(50);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
}

#[test]
fn session_moves_on_when_its_credential_cools_down() {
    let f = fixture(&three_keys(""));
    let lease = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    assert_eq!(letter(&lease), 'a');
    f.fail(&lease, &error(FailureClass::RateLimit, Some(30_000)));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
    f.advance(30);
    // a has recovered, but the conversation now lives on b.
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
}

#[test]
fn bound_credential_in_the_tried_list_is_replaced() {
    let f = fixture(&three_keys(""));
    let first = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    let second = f
        .pick_with(
            "gpt-5.5",
            std::slice::from_ref(&first.credential.id),
            Some("s1"),
        )
        .unwrap();
    assert_eq!((letter(&first), letter(&second)), ('a', 'b'));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
}

#[test]
fn binding_beats_priority_once_made() {
    let f = fixture(TIERS);
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    f.fail_class(&b, FailureClass::Server);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'c');
    f.advance(60);
    // The top tier is back; the bound conversation stays where its cache is.
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'c');
    let fresh = session_letter(&f, "gpt-5.5", "s2");
    assert!(fresh == 'a' || fresh == 'b');
}

#[test]
fn binding_keeps_a_conversation_on_its_alias_target() {
    let f = fixture(ALIAS);
    let a = f.pick("smart").unwrap();
    f.fail(&a, &error(FailureClass::RateLimit, Some(30_000)));
    assert_eq!(session_letter(&f, "smart", "s1"), 'b');
    f.advance(30);
    let lease = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(letter(&lease), 'b');
    assert_eq!(lease.client_model, "gpt-5.5");
    // New conversations go to the preferred target again.
    assert_eq!(session_letter(&f, "smart", "s2"), 'a');
}

#[test]
fn affinity_can_be_switched_off() {
    let f = fixture(&three_keys("session_affinity = false"));
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'c');
    assert_eq!(f.scheduler.session_bindings(), 0);
}

#[test]
fn transport_failures_keep_the_binding_but_upstream_faults_release_it() {
    // Cooldowns off, so only the binding decides.
    let f = fixture(&three_keys("[routing.cooldown]\nenabled = false"));
    let lease = f.pick_with("gpt-5.5", &[], Some("s1")).unwrap();
    f.fail_class(&lease, FailureClass::Transport);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    f.fail_class(&lease, FailureClass::Request);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    f.fail_class(&lease, FailureClass::Server);
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
}

#[test]
fn affinity_map_is_bounded() {
    let f = fixture(&three_keys(""));
    for i in 0..17_000 {
        f.pick_with("gpt-5.5", &[], Some(&format!("session-{i}")))
            .unwrap();
    }
    assert_eq!(f.scheduler.session_bindings(), 16_384);
    // The newest sessions are the ones remembered.
    let newest = f.pick_with("gpt-5.5", &[], Some("session-16999")).unwrap();
    let again = f.pick_with("gpt-5.5", &[], Some("session-16999")).unwrap();
    assert_eq!(newest.credential.id, again.credential.id);
}

// ---------------------------------------------------------------------------
// Disabled credentials
// ---------------------------------------------------------------------------

#[test]
fn config_disabled_credentials_are_never_selected() {
    let f = fixture(
        r#"
[[providers]]
name = "openai"
kind = "openai"

[[providers.credentials]]
api_key = "sk-key-a-0000000000000"
disabled = true

[[providers.credentials]]
api_key = "sk-key-b-0000000000000"
"#,
    );
    assert_eq!(letters(&f, "gpt-5.5", 4), "bbbb");
    assert!(f.all_credentials()[0].disabled);
}

#[test]
fn runtime_disabled_credentials_are_skipped_until_enabled_again() {
    let f = fixture(&three_keys(""));
    let ids: Vec<String> = f.all_credentials().into_iter().map(|c| c.id).collect();
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'a');
    assert!(f.scheduler.set_runtime_disabled(&ids[0], true));
    assert_eq!(letters(&f, "gpt-5.5", 4), "bcbc");
    // The session bound to the disabled credential moves.
    assert_eq!(session_letter(&f, "gpt-5.5", "s1"), 'b');
    assert!(f.scheduler.set_runtime_disabled(&ids[0], false));
    // Back in the rotation, which continues after the last pick (b).
    assert_eq!(letters(&f, "gpt-5.5", 3), "cab");
    assert!(
        !f.scheduler
            .set_runtime_disabled("openai:ffffffffffff", true)
    );

    for id in &ids {
        f.scheduler.set_runtime_disabled(id, true);
    }
    assert_eq!(
        f.pick("gpt-5.5").unwrap_err(),
        PickError::NoCredentials {
            model: "gpt-5.5".into()
        }
    );
}
