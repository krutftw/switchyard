//! Regression tests (review finding SCHED-4): under `least-latency`, a
//! credential that fails without resting must not take every request.
//!
//! "Unmeasured" credentials go first, and a failure yields no latency
//! sample: a credential that only fails stays unmeasured, and one that was
//! fast before it broke keeps its flattering average. With
//! `routing.cooldown.enabled = false` ("When false failed credentials stay in
//! rotation", `CooldownConfig`) — or with the relevant `*_secs` set to 0 —
//! nothing else takes such a credential out of first place, so it would be
//! chosen for the first attempt of *every* request while a healthy one sits
//! idle. A credential whose last attempt on the model failed within the last
//! minute therefore queues behind the others.

mod common;

use common::fixture;
use switchyard_core::FailureClass;

const TWO_KEYS: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]
"#;

fn least_latency(cooldown: &str) -> String {
    format!("[routing]\nstrategy = \"least-latency\"\n{cooldown}\n{TWO_KEYS}")
}

fn letter(lease: &switchyard_scheduler::Lease) -> char {
    lease.credential.api_key.chars().nth(7).unwrap()
}

#[test]
fn a_fast_credential_that_starts_failing_loses_first_place() {
    let f = fixture(&least_latency("[routing.cooldown]\nenabled = false"));
    // a is measured fast, b slow.
    let a = f.pick("gpt-5.5").unwrap();
    f.succeed(&a, 50);
    let b = f.pick("gpt-5.5").unwrap();
    f.succeed(&b, 500);
    assert_eq!((letter(&a), letter(&b)), ('a', 'b'));
    assert_eq!(letter(&f.pick("gpt-5.5").unwrap()), 'a');

    // a breaks. Its average still says 50 ms, but it just failed.
    f.fail_class(&a, FailureClass::Server);
    for _ in 0..5 {
        assert_eq!(letter(&f.pick("gpt-5.5").unwrap()), 'b');
        f.advance(10);
    }
    // After a minute it is given another chance …
    f.advance(10);
    let probe = f.pick("gpt-5.5").unwrap();
    assert_eq!(letter(&probe), 'a');
    // … and a success puts it back in first place for good.
    f.succeed(&probe, 50);
    assert_eq!(letter(&f.pick("gpt-5.5").unwrap()), 'a');
    f.advance(120);
    assert_eq!(letter(&f.pick("gpt-5.5").unwrap()), 'a');
}

#[test]
fn a_broken_credential_is_probed_about_once_a_minute() {
    let f = fixture(&least_latency("[routing.cooldown]\ntransient_secs = 0"));
    let mut picks = String::new();
    // Five minutes of traffic, one request a second; b never works.
    for _ in 0..300 {
        let lease = f.pick("gpt-5.5").unwrap();
        if letter(&lease) == 'a' {
            picks.push('a');
            f.succeed(&lease, 100);
        } else {
            picks.push('b');
            f.fail_class(&lease, FailureClass::Server);
        }
        f.advance(1);
    }
    let probes = picks.matches('b').count();
    assert!((4..=6).contains(&probes), "{probes} probes: {picks}");
}

#[test]
fn when_every_credential_just_failed_the_request_is_still_served() {
    let f = fixture(&least_latency("[routing.cooldown]\nenabled = false"));
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    f.fail_class(&b, FailureClass::Server);
    // Nobody is in good standing: fall back to the plain ordering.
    let picks: String = (0..4)
        .map(|_| letter(&f.pick("gpt-5.5").unwrap()))
        .collect();
    assert_eq!(picks, "abab");
}

#[test]
fn a_failure_on_one_model_does_not_demote_the_credential_for_another() {
    let f = fixture(&least_latency("[routing.cooldown]\nenabled = false"));
    for latency in [50, 500] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.succeed(&lease, latency);
    }
    let a = f.pick("gpt-5.5").unwrap();
    assert_eq!(letter(&a), 'a');
    f.fail_class(&a, FailureClass::ModelNotFound);
    assert_eq!(letter(&f.pick("gpt-5.5").unwrap()), 'b');
    // Another model on the same key is none the worse for it.
    assert_eq!(letter(&f.pick("gpt-6-sol").unwrap()), 'a');
}

fn run(routing: &str) -> String {
    let f = fixture(&format!(
        r#"
[routing]
strategy = "least-latency"
{routing}

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]
"#
    ));
    let mut picks = String::new();
    for _ in 0..20 {
        let lease = f.pick("gpt-5.5").unwrap();
        if lease.credential.api_key.contains("-a-") {
            picks.push('a');
            f.succeed(&lease, 100);
        } else {
            // Key b is broken: every attempt fails upstream.
            picks.push('b');
            f.fail_class(&lease, FailureClass::Server);
        }
        f.advance(1);
    }
    picks
}

#[test]
fn a_credential_that_only_fails_does_not_monopolise_least_latency_with_cooldowns_off() {
    let picks = run("[routing.cooldown]\nenabled = false");
    let healthy = picks.chars().filter(|c| *c == 'a').count();
    assert!(
        healthy >= 10,
        "the healthy, measured credential must get at least its round-robin share; picks: {picks}"
    );
}

#[test]
fn a_credential_that_only_fails_does_not_monopolise_least_latency_with_zero_transient_cooldown() {
    let picks = run("[routing.cooldown]\ntransient_secs = 0");
    let healthy = picks.chars().filter(|c| *c == 'a').count();
    assert!(
        healthy >= 10,
        "the healthy, measured credential must get at least its round-robin share; picks: {picks}"
    );
}
