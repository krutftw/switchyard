//! `Scheduler::report_credential`: outcome reporting for calls made with one
//! credential directly (connectivity tests), outside the pick/report cycle.

mod common;

use common::{error, fixture, secs};
use pretty_assertions::assert_eq;
use switchyard_core::FailureClass;
use switchyard_scheduler::{CredentialStatus, Outcome, PickError};

const TWO_KEYS: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]
"#;

const ONE_KEY: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-only-key-0000000000000000"]
"#;

#[test]
fn a_successful_test_counts_and_measures_latency() {
    let f = fixture(ONE_KEY);
    let id = f.all_credentials()[0].id.clone();
    assert!(f.scheduler.report_credential(
        &id,
        "gpt-5.5",
        Outcome::Success { latency_ms: 120 },
        f.clock_now()
    ));
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.successes, c.failures), (1, 1, 0));
    assert_eq!(c.latency_ms, Some(120));
    assert_eq!(c.status, CredentialStatus::Ready);
}

#[test]
fn a_failed_test_rests_the_model_like_a_failed_request() {
    let f = fixture(ONE_KEY);
    let id = f.all_credentials()[0].id.clone();
    let failure = error(FailureClass::RateLimit, Some(5_000));
    assert!(f.scheduler.report_credential(
        &id,
        "gpt-5.5",
        Outcome::Failure(&failure),
        f.clock_now()
    ));
    match f.pick("gpt-5.5") {
        Err(PickError::CoolingDown { retry_after, .. }) => assert_eq!(retry_after, secs(5)),
        other => panic!("expected a cooldown, got {other:?}"),
    }
    // Only that model rests.
    assert!(f.pick("gpt-6-sol").is_ok());
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.successes, c.failures), (1, 0, 1));
    assert_eq!(c.last_error.as_ref().map(|e| e.status), Some(429));
}

#[test]
fn an_auth_failure_rests_the_whole_credential() {
    let f = fixture(TWO_KEYS);
    let id = f.all_credentials()[0].id.clone();
    let failure = error(FailureClass::Auth, None);
    f.scheduler
        .report_credential(&id, "gpt-5.5", Outcome::Failure(&failure), f.clock_now());
    let rested = f
        .all_credentials()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap();
    assert_eq!(rested.status, CredentialStatus::Cooling);
    assert_eq!(rested.cooldown_reason, Some(FailureClass::Auth));
    // The other key keeps serving.
    for _ in 0..4 {
        assert_ne!(f.pick("gpt-5.5").unwrap().credential.id, id);
    }
}

#[test]
fn a_successful_test_puts_a_resting_model_back_into_rotation() {
    let f = fixture(ONE_KEY);
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Server);
    assert!(matches!(
        f.pick("gpt-5.5"),
        Err(PickError::CoolingDown { .. })
    ));

    assert!(f.scheduler.report_credential(
        &lease.credential.id,
        "gpt-5.5",
        Outcome::Success { latency_ms: 80 },
        f.clock_now()
    ));
    assert!(f.pick("gpt-5.5").is_ok());
    let c = &f.all_credentials()[0];
    assert_eq!(c.consecutive_failures, 0);
    assert!(c.model_cooldowns.is_empty());
}

#[test]
fn request_faults_cool_nothing() {
    let f = fixture(ONE_KEY);
    let id = f.all_credentials()[0].id.clone();
    let failure = error(FailureClass::Request, None);
    f.scheduler
        .report_credential(&id, "gpt-5.5", Outcome::Failure(&failure), f.clock_now());
    assert!(f.pick("gpt-5.5").is_ok());
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.failures), (1, 0));
}

#[test]
fn unknown_credentials_are_reported_as_such_and_leave_no_state() {
    let f = fixture(ONE_KEY);
    let before = f.all_credentials();
    assert!(!f.scheduler.report_credential(
        "openai:does-not-exist",
        "gpt-5.5",
        Outcome::Success { latency_ms: 1 },
        f.clock_now()
    ));
    assert_eq!(f.all_credentials(), before);
}

trait ClockNow {
    fn clock_now(&self) -> std::time::SystemTime;
}

impl ClockNow for common::Fixture {
    fn clock_now(&self) -> std::time::SystemTime {
        self.scheduler.now()
    }
}
