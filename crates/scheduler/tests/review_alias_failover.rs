//! Regression tests (review finding SCHED-2): an alias whose targets live on
//! the same credential fails over inside one client request.
//!
//! `AliasConfig` (crates/core/src/config.rs): "Requests for `name` are served
//! by `targets`, tried in order: the next target is used only when no
//! credential can serve the previous one."
//!
//! The most common alias is a same-vendor fallback on one API key
//! (`smart = [opus, sonnet]`). The gateway's attempt loop (DESIGN section 8,
//! step 5) picks, reports the failure, and picks again "excluding ones
//! already tried". The tried list names credentials, yet what must not be
//! repeated is a (credential, upstream model) pair: after `opus` failed on
//! the only key, `sonnet` on that key is still new to the request. `pick`
//! reads from the failures reported for a tried credential which targets it
//! was attempted on, offers it for the other targets only, and never offers
//! a pair twice — with or without cooldowns.

mod common;

use common::{Fixture, error, fixture, secs};
use pretty_assertions::assert_eq;
use switchyard_core::FailureClass;
use switchyard_scheduler::PickError;

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
fn single_key_alias_fails_over_to_the_next_target_within_one_request() {
    let f = fixture(ONE_KEY_ALIAS);

    // Attempt 1: preferred target, upstream says 529 overloaded.
    let first = f.pick("smart").unwrap();
    assert_eq!(first.upstream_model, "claude-opus-4-6");
    let mut overloaded = error(FailureClass::Server, None);
    overloaded.status = 529;
    f.fail(&first, &overloaded);

    // Attempt 2 of the same client request: the credential is in `tried`.
    let tried = vec![first.credential.id.clone()];
    let second = f
        .pick_with("smart", &tried, None)
        .expect("opus is resting, so the alias must fall through to sonnet");
    assert_eq!(second.upstream_model, "claude-sonnet-4-6");
    assert_eq!(second.credential.id, first.credential.id);

    // And if that fails as well, the request is over: nothing is retried twice.
    f.fail(&second, &overloaded);
    assert!(f.pick_with("smart", &tried, None).is_err());
}

/// Control: the *next* request goes straight to the second target.
#[test]
fn the_following_request_is_served_by_the_second_target() {
    let f = fixture(ONE_KEY_ALIAS);
    let first = f.pick("smart").unwrap();
    f.fail_class(&first, FailureClass::Server);
    let next = f.pick("smart").unwrap();
    assert_eq!(next.upstream_model, "claude-sonnet-4-6");
}

fn three_targets(routing: &str, keys: &str) -> String {
    format!(
        r#"
[routing]
max_attempts = 10
{routing}

[[providers]]
name = "claude"
kind = "anthropic"
api_keys = [{keys}]

[[aliases]]
name = "smart"
targets = ["claude-opus-4-6", "claude-sonnet-4-6", "claude-opus-4-7"]
"#
    )
}

/// Runs one client request the way the gateway does — pick, fail, report,
/// add to tried, pick again — until the scheduler has nothing left to offer.
/// Returns the attempts as `(key letter, upstream model)` and the final error.
fn run_failing_request(f: &Fixture, class: FailureClass) -> (Vec<(char, String)>, PickError) {
    let mut tried: Vec<String> = Vec::new();
    let mut attempts = Vec::new();
    loop {
        match f.pick_with("smart", &tried, None) {
            Ok(lease) => {
                let letter = lease.credential.api_key.chars().nth(7).unwrap();
                attempts.push((letter, lease.upstream_model.clone()));
                f.fail_class(&lease, class);
                if !tried.contains(&lease.credential.id) {
                    tried.push(lease.credential.id.clone());
                }
                assert!(attempts.len() <= 20, "runaway request: {attempts:?}");
            }
            Err(e) => return (attempts, e),
        }
    }
}

fn attempt(letter: char, model: &str) -> (char, String) {
    (letter, model.to_string())
}

const ONE_KEY: &str = r#""sk-key-a-0000000000000""#;
const TWO_KEYS: &str = r#""sk-key-a-0000000000000", "sk-key-b-0000000000000""#;

#[test]
fn every_target_is_tried_once_on_a_single_key_then_the_request_is_exhausted() {
    // The clock never moves: the order of failures must not depend on it.
    let f = fixture(&three_targets("", ONE_KEY));
    let (attempts, error) = run_failing_request(&f, FailureClass::Server);
    assert_eq!(
        attempts,
        vec![
            attempt('a', "claude-opus-4-6"),
            attempt('a', "claude-sonnet-4-6"),
            attempt('a', "claude-opus-4-7"),
        ]
    );
    assert_eq!(
        error,
        PickError::Exhausted {
            model: "smart".into()
        }
    );
}

#[test]
fn target_order_comes_before_credential_reuse() {
    // Both keys get their chance at the preferred model before anyone falls
    // back; each (key, model) pair is attempted exactly once.
    let f = fixture(&three_targets("", TWO_KEYS));
    let (attempts, error) = run_failing_request(&f, FailureClass::RateLimit);
    assert_eq!(
        attempts,
        vec![
            attempt('a', "claude-opus-4-6"),
            attempt('b', "claude-opus-4-6"),
            attempt('a', "claude-sonnet-4-6"),
            attempt('b', "claude-sonnet-4-6"),
            attempt('a', "claude-opus-4-7"),
            attempt('b', "claude-opus-4-7"),
        ]
    );
    assert!(matches!(error, PickError::Exhausted { .. }), "{error:?}");
}

#[test]
fn failover_across_targets_does_not_depend_on_cooldowns() {
    for routing in [
        "[routing.cooldown]\nenabled = false",
        "[routing.cooldown]\ntransient_secs = 0",
    ] {
        let f = fixture(&three_targets(routing, ONE_KEY));
        let (attempts, error) = run_failing_request(&f, FailureClass::Server);
        assert_eq!(
            attempts,
            vec![
                attempt('a', "claude-opus-4-6"),
                attempt('a', "claude-sonnet-4-6"),
                attempt('a', "claude-opus-4-7"),
            ],
            "{routing}"
        );
        assert!(matches!(error, PickError::Exhausted { .. }), "{error:?}");
        // Nothing rests, so the next request starts at the top again.
        assert_eq!(
            f.pick("smart").unwrap().upstream_model,
            "claude-opus-4-6",
            "{routing}"
        );
    }
}

#[test]
fn a_pair_that_failed_without_resting_is_not_offered_again() {
    // 5xx failures start no cooldown here; rate limits do.
    let f = fixture(&three_targets(
        "[routing.cooldown]\ntransient_secs = 0",
        ONE_KEY,
    ));
    // Some earlier request left the first target resting on the key.
    let earlier = f.pick("smart").unwrap();
    f.fail(&earlier, &error(FailureClass::RateLimit, Some(600_000)));
    f.advance(5);

    // This request therefore starts on the second target, which fails
    // without resting. The first target's rest is older than that failure:
    // it is no evidence that the key was tried there instead.
    let first = f.pick("smart").unwrap();
    assert_eq!(first.upstream_model, "claude-sonnet-4-6");
    f.fail_class(&first, FailureClass::Server);
    let tried = vec![first.credential.id.clone()];
    let second = f.pick_with("smart", &tried, None).unwrap();
    assert_eq!(second.upstream_model, "claude-opus-4-7");
    f.fail_class(&second, FailureClass::Server);
    // Neither of the two is offered again although neither rests. What is
    // left for this request is the first target, once its rest is over.
    assert_eq!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::CoolingDown {
            model: "smart".into(),
            retry_after: secs(595),
            last_error: Some("429 upstream said 429".into()),
        }
    );
    f.advance(595);
    let third = f.pick_with("smart", &tried, None).unwrap();
    assert_eq!(third.upstream_model, "claude-opus-4-6");
    f.fail_class(&third, FailureClass::Server);
    assert_eq!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::Exhausted {
            model: "smart".into()
        }
    );
}

#[test]
fn a_rest_that_ends_mid_request_does_not_reopen_what_was_tried() {
    // Cooldowns for 429 only. The first two targets rest on the key from
    // earlier requests, the second one for a shorter time.
    let f = fixture(&three_targets(
        "[routing.cooldown]\ntransient_secs = 0",
        ONE_KEY,
    ));
    let sonnet = f.pick("claude-sonnet-4-6").unwrap();
    f.fail(&sonnet, &error(FailureClass::RateLimit, Some(10_000)));
    let opus = f.pick("claude-opus-4-6").unwrap();
    f.fail(&opus, &error(FailureClass::RateLimit, Some(600_000)));

    // The request starts on the third target, which fails without resting.
    let mut attempts = Vec::new();
    let first = f.pick("smart").unwrap();
    attempts.push(first.upstream_model.clone());
    f.fail_class(&first, FailureClass::Server);
    let tried = vec![first.credential.id.clone()];

    // Meanwhile the second target's rest ends: that pair is new to the
    // request and is tried. After it, the third target must not come round
    // again just because an earlier target has now failed more recently.
    f.advance(10);
    let second = f.pick_with("smart", &tried, None).unwrap();
    attempts.push(second.upstream_model.clone());
    f.fail_class(&second, FailureClass::Server);
    assert_eq!(attempts, vec!["claude-opus-4-7", "claude-sonnet-4-6"]);
    assert!(matches!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::CoolingDown { .. }
    ));
}

#[test]
fn a_session_bound_to_the_fallback_target_can_still_reach_the_preferred_one() {
    let f = fixture(ONE_KEY_ALIAS);
    // The preferred target is rate limited for a while; the conversation
    // settles on the fallback.
    let opus = f.pick("smart").unwrap();
    f.fail(&opus, &error(FailureClass::RateLimit, Some(30_000)));
    let bound = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(bound.upstream_model, "claude-sonnet-4-6");
    f.succeed(&bound, 100);
    f.advance(30);

    // Its next request goes straight to the fallback (that is where its
    // cache is) and fails there. The preferred target has not been tried in
    // this request and is healthy again: use it rather than give up.
    let first = f.pick_with("smart", &[], Some("s1")).unwrap();
    assert_eq!(first.upstream_model, "claude-sonnet-4-6");
    f.fail_class(&first, FailureClass::Server);
    let tried = vec![first.credential.id.clone()];
    let second = f.pick_with("smart", &tried, Some("s1")).unwrap();
    assert_eq!(second.upstream_model, "claude-opus-4-6");
    f.fail_class(&second, FailureClass::Server);
    assert_eq!(
        f.pick_with("smart", &tried, Some("s1")).unwrap_err(),
        PickError::Exhausted {
            model: "smart".into()
        }
    );
}

/// Small deterministic generator: reproducible without a dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// Many requests in a row against one scheduler, with random outcomes,
/// failure classes, upstream waits, sessions and pauses (also in the middle
/// of a request, across cooldown ends). Whatever the history:
///
/// * a request never attempts the same (credential, upstream model) twice;
/// * a request ends after at most keys × targets attempts;
/// * when nothing ever rests, a request whose attempts all fail has gone
///   through every pair.
#[test]
fn no_pair_is_attempted_twice_in_any_request() {
    const THREE_KEYS: &str =
        r#""sk-key-a-0000000000000", "sk-key-b-0000000000000", "sk-key-c-0000000000000""#;
    let strategies = ["round-robin", "fill-first", "weighted", "least-latency"];
    let cooldowns = [
        "",
        "[routing.cooldown]\nenabled = false",
        "[routing.cooldown]\ntransient_secs = 0",
        "[routing.cooldown]\ntransient_secs = 0\nrate_limit_base_secs = 2\nrate_limit_max_secs = 8\nmodel_not_found_secs = 5\nauth_secs = 20\nquota_secs = 30",
        "[routing.cooldown]\nrate_limit_base_secs = 0\ntransient_secs = 3",
    ];
    let classes = [
        FailureClass::Server,
        FailureClass::Server,
        FailureClass::Transport,
        FailureClass::RateLimit,
        FailureClass::RateLimit,
        FailureClass::ModelNotFound,
        FailureClass::Quota,
        FailureClass::Auth,
    ];
    let pauses_ms = [0, 0, 1, 700, 2_500, 9_000, 61_000];

    for seed in 1..=400u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (keys, key_count) = rng.pick(&[(ONE_KEY, 1usize), (TWO_KEYS, 2), (THREE_KEYS, 3)]);
        let strategy = rng.pick(&strategies);
        let cooldown = rng.pick(&cooldowns);
        let nothing_rests = cooldown.contains("enabled = false");
        let f = fixture(&three_targets(
            &format!("strategy = \"{strategy}\"\n{cooldown}"),
            keys,
        ));
        let context = format!("seed {seed}: {strategy}, {key_count} key(s), {cooldown:?}");

        for request in 0..25 {
            let session = rng.pick(&[None, None, Some("s1"), Some("s2")]);
            // Mostly failing requests; some where every attempt fails.
            let success_one_in = rng.pick(&[3u64, 6, u64::MAX]);
            let mut tried: Vec<String> = Vec::new();
            let mut attempted: Vec<(String, String)> = Vec::new();
            let mut served = false;
            while let Ok(lease) = f.pick_with("smart", &tried, session) {
                let pair = (lease.credential.id.clone(), lease.upstream_model.clone());
                assert!(
                    !attempted.contains(&pair),
                    "{context}, request {request}: {pair:?} offered twice after {attempted:?}"
                );
                attempted.push(pair);
                assert!(
                    attempted.len() <= key_count * 3,
                    "{context}, request {request}: {attempted:?}"
                );
                if rng.below(success_one_in) == 0 {
                    f.succeed(&lease, 20 + rng.below(500));
                    served = true;
                    break;
                }
                let hint = rng.pick(&[None, None, Some(1_500), Some(12_000)]);
                f.fail(&lease, &error(rng.pick(&classes), hint));
                if !tried.contains(&lease.credential.id) {
                    tried.push(lease.credential.id.clone());
                }
                f.advance_ms(rng.pick(&pauses_ms));
            }
            if nothing_rests && !served && session.is_none() {
                assert_eq!(
                    attempted.len(),
                    key_count * 3,
                    "{context}, request {request}: {attempted:?}"
                );
            }
            f.advance_ms(rng.pick(&pauses_ms));
        }
    }
}

#[test]
fn a_tried_credential_without_a_reported_failure_is_not_offered_again() {
    // The scheduler learns where a credential was attempted from `report`.
    // Without a report there is no telling, and it stays excluded.
    let f = fixture(ONE_KEY_ALIAS);
    let first = f.pick("smart").unwrap();
    let tried = vec![first.credential.id.clone()];
    assert_eq!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::Exhausted {
            model: "smart".into()
        }
    );
}

#[test]
fn a_success_on_the_earlier_target_ends_the_offer() {
    let f = fixture(ONE_KEY_ALIAS);
    let first = f.pick("smart").unwrap();
    f.fail_class(&first, FailureClass::Transport);
    // A concurrent request gets through on the same pair: the first target
    // works again, there is nothing to fall back from.
    f.succeed(&first, 80);
    let tried = vec![first.credential.id.clone()];
    assert!(matches!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::Exhausted { .. }
    ));
    assert_eq!(f.pick("smart").unwrap().upstream_model, "claude-opus-4-6");
}

#[test]
fn a_later_target_that_is_resting_is_waited_for_not_given_up_on() {
    let f = fixture(ONE_KEY_ALIAS);
    // The fallback model is rate limited on the key for another 30 s.
    let sonnet = f.pick("claude-sonnet-4-6").unwrap();
    f.fail(&sonnet, &error(FailureClass::RateLimit, Some(30_000)));

    let first = f.pick("smart").unwrap();
    assert_eq!(first.upstream_model, "claude-opus-4-6");
    f.fail_class(&first, FailureClass::Server);
    let tried = vec![first.credential.id.clone()];
    // (key, sonnet) has not been attempted in this request and will be back
    // in 30 s: that is a cooldown to wait out, not exhaustion.
    match f.pick_with("smart", &tried, None).unwrap_err() {
        PickError::CoolingDown {
            model, retry_after, ..
        } => {
            assert_eq!(model, "smart");
            assert_eq!(retry_after, secs(30));
        }
        other => panic!("unexpected {other:?}"),
    }
    f.advance(30);
    let second = f.pick_with("smart", &tried, None).unwrap();
    assert_eq!(second.upstream_model, "claude-sonnet-4-6");
}

#[test]
fn credential_wide_failures_stop_the_fall_through() {
    let f = fixture(&three_targets("", ONE_KEY));
    let first = f.pick("smart").unwrap();
    // A rejected key is rejected for every model.
    f.fail_class(&first, FailureClass::Auth);
    let tried = vec![first.credential.id.clone()];
    assert!(matches!(
        f.pick_with("smart", &tried, None).unwrap_err(),
        PickError::CoolingDown { .. }
    ));
}

#[test]
fn targets_on_different_providers_are_unaffected() {
    let f = fixture(
        r#"
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
targets = ["claude-opus-4-6", "gpt-5.5", "claude-sonnet-4-6"]
"#,
    );
    let (attempts, error) = run_failing_request(&f, FailureClass::Server);
    assert_eq!(
        attempts,
        vec![
            attempt('a', "claude-opus-4-6"),
            attempt('b', "gpt-5.5"),
            attempt('a', "claude-sonnet-4-6"),
        ]
    );
    assert!(matches!(error, PickError::Exhausted { .. }), "{error:?}");
}

/// Requests racing on the same credentials share their failure history, so
/// the scheduler cannot always tell whose failure is whose. What must hold
/// regardless: a pair that rests is offered to nobody — so with cooldowns
/// on, no time passing and nothing succeeding (a success ends a rest), no
/// request sees a pair twice, however the threads interleave.
#[test]
fn concurrent_requests_never_repeat_a_resting_pair() {
    use std::sync::Arc;
    use switchyard_core::Protocol;
    use switchyard_scheduler::{Clock, Outcome, PickRequest};

    const THREADS: u64 = 8;
    for round in 0..60u64 {
        let f = fixture(&three_targets("", TWO_KEYS));
        let scheduler = Arc::new(f.scheduler);
        let clock = Arc::clone(&f.clock);
        let handles: Vec<_> = (0..THREADS)
            .map(|thread| {
                let scheduler = Arc::clone(&scheduler);
                let clock = Arc::clone(&clock);
                std::thread::spawn(move || {
                    let session = format!("thread-{thread}");
                    let failures = [
                        error(FailureClass::Server, None),
                        error(FailureClass::RateLimit, None),
                        error(FailureClass::Transport, None),
                    ];
                    let mut attempts_made = 0usize;
                    for request in 0..10 {
                        let resolved = scheduler.resolve("smart").unwrap();
                        let mut tried: Vec<String> = Vec::new();
                        let mut attempted: Vec<(String, String)> = Vec::new();
                        while let Ok(lease) = scheduler.pick(&PickRequest {
                            resolved: &resolved,
                            tried: &tried,
                            session: (request % 2 == 0).then_some(session.as_str()),
                            client_protocol: Protocol::Anthropic,
                            now: clock.now(),
                        }) {
                            let pair = (lease.credential.id.clone(), lease.upstream_model.clone());
                            assert!(
                                !attempted.contains(&pair),
                                "round {round}: {pair:?} offered twice after {attempted:?}"
                            );
                            attempted.push(pair);
                            let failure = &failures[attempts_made % failures.len()];
                            attempts_made += 1;
                            std::thread::yield_now();
                            scheduler.report(&lease, Outcome::Failure(failure), clock.now());
                            if !tried.contains(&lease.credential.id) {
                                tried.push(lease.credential.id.clone());
                            }
                        }
                    }
                    attempts_made
                })
            })
            .collect();
        let attempts: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // The bookkeeping adds up whatever the interleaving was.
        let recorded: u64 = scheduler
            .snapshot()
            .iter()
            .flat_map(|p| p.credentials.iter())
            .map(|c| c.requests)
            .sum();
        assert_eq!(recorded, attempts as u64, "round {round}");
    }
}
