//! Regression tests (review finding SCHED-3): configured cooldown durations
//! are honoured as written, however long.
//!
//! *Upstream retry hints* are capped at seven days (`MAX_RETRY_HINT_MS`,
//! "Longest upstream-requested wait that is honoured"), which guards against
//! absurd `Retry-After` values. The operator's own `routing.cooldown.*_secs`
//! values are not the upstream's wish and must not be shortened by that cap:
//! `auth_secs = 2592000` ("park a rejected key for 30 days") rests the key
//! for 30 days. `CooldownConfig` documents each `*_secs` as the cooldown
//! applied; there is no documented upper bound and `Config::validate` accepts
//! the value.

mod common;

use common::{Fixture, fixture, secs};
use pretty_assertions::assert_eq;
use std::time::Duration;
use switchyard_core::FailureClass;
use switchyard_scheduler::PickError;

const THIRTY_DAYS: u64 = 30 * 24 * 3600;

fn wait(f: &Fixture) -> Duration {
    match f.pick("gpt-5.5") {
        Err(PickError::CoolingDown { retry_after, .. }) => retry_after,
        other => panic!("expected a cooldown, got {other:?}"),
    }
}

fn thirty_day_fixture() -> Fixture {
    fixture(&format!(
        r#"
[routing.cooldown]
auth_secs = {THIRTY_DAYS}
quota_secs = {THIRTY_DAYS}
model_not_found_secs = {THIRTY_DAYS}
transient_secs = {THIRTY_DAYS}

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-only-key-0000000000000000"]
"#
    ))
}

#[test]
fn configured_auth_cooldown_longer_than_a_week_is_honoured() {
    let f = thirty_day_fixture();
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Auth);
    assert_eq!(wait(&f), secs(THIRTY_DAYS));
}

#[test]
fn configured_quota_cooldown_longer_than_a_week_is_honoured() {
    let f = thirty_day_fixture();
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Quota);
    assert_eq!(wait(&f), secs(THIRTY_DAYS));
}

#[test]
fn configured_model_not_found_cooldown_longer_than_a_week_is_honoured() {
    let f = thirty_day_fixture();
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::ModelNotFound);
    assert_eq!(wait(&f), secs(THIRTY_DAYS));
    // Eight days later the model must still be resting on this credential.
    f.advance(8 * 24 * 3600);
    assert_eq!(wait(&f), secs(THIRTY_DAYS - 8 * 24 * 3600));
}
