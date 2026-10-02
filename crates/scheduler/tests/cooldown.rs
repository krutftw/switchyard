//! Outcome reporting: what each failure class rests, for how long, how the
//! rate-limit backoff grows, and how success resets it.

mod common;

use common::{Fixture, error, fixture, secs};
use pretty_assertions::assert_eq;
use std::time::Duration;
use switchyard_core::{ApiError, FailureClass, UpstreamError, UpstreamErrorInfo};
use switchyard_scheduler::{CredentialStatus, PickError};

const KEY: &str = "sk-only-key-0000000000000000";

fn one_key(routing: &str) -> String {
    format!(
        r#"
{routing}

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["{KEY}"]
"#
    )
}

const TWO_KEYS: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-key-a-0000000000000", "sk-key-b-0000000000000"]
"#;

/// The wait a pick for `model` reports, or `None` when a credential is ready.
fn wait(f: &Fixture, model: &str) -> Option<Duration> {
    match f.pick(model) {
        Ok(_) => None,
        Err(PickError::CoolingDown { retry_after, .. }) => Some(retry_after),
        Err(other) => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Per class
// ---------------------------------------------------------------------------

#[test]
fn request_faults_cool_nothing_and_are_not_failures() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Request);
    assert_eq!(wait(&f, "gpt-5.5"), None);
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.successes, c.failures), (1, 0, 0));
    assert_eq!(c.consecutive_failures, 0);
    assert_eq!(c.last_error, None);
    assert_eq!(c.status, CredentialStatus::Ready);
}

#[test]
fn rate_limit_rests_one_model_on_one_credential() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::RateLimit);

    assert_eq!(
        f.pick("gpt-5.5").unwrap_err(),
        PickError::CoolingDown {
            model: "gpt-5.5".into(),
            retry_after: secs(1),
            last_error: Some("429 upstream said 429".into()),
        }
    );
    // Other models on the same credential are untouched.
    assert_eq!(wait(&f, "gpt-6-sol"), None);

    let c = &f.all_credentials()[0];
    assert_eq!(c.status, CredentialStatus::Ready);
    assert_eq!(c.cooldown_until, None);
    assert_eq!(c.model_cooldowns.len(), 1);
    assert_eq!(c.model_cooldowns[0].model, "gpt-5.5");
    assert_eq!(c.model_cooldowns[0].until, f.now_ms() + 1_000);
    assert_eq!(c.model_cooldowns[0].reason, FailureClass::RateLimit);

    f.advance_ms(999);
    assert_eq!(wait(&f, "gpt-5.5"), Some(Duration::from_millis(1)));
    f.advance_ms(1);
    assert_eq!(wait(&f, "gpt-5.5"), None);
    assert!(f.all_credentials()[0].model_cooldowns.is_empty());
}

#[test]
fn rate_limit_backoff_doubles_per_consecutive_failure() {
    let f = fixture(&one_key(""));
    for expected in [1, 2, 4, 8, 16, 32, 64] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        assert_eq!(wait(&f, "gpt-5.5"), Some(secs(expected)), "step {expected}");
        f.advance(expected);
    }
}

#[test]
fn rate_limit_backoff_is_capped() {
    let f = fixture(&one_key(
        "[routing.cooldown]\nrate_limit_base_secs = 2\nrate_limit_max_secs = 10",
    ));
    for expected in [2, 4, 8, 10, 10, 10] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        assert_eq!(wait(&f, "gpt-5.5"), Some(secs(expected)));
        f.advance(expected);
    }
}

#[test]
fn default_rate_limit_cap_is_thirty_minutes() {
    let f = fixture(&one_key(""));
    let mut last = Duration::ZERO;
    for _ in 0..14 {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        last = wait(&f, "gpt-5.5").unwrap();
        f.advance(last.as_secs());
    }
    assert_eq!(last, secs(1800));
}

#[test]
fn upstream_retry_after_overrides_the_backoff() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail(&lease, &error(FailureClass::RateLimit, Some(7_500)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(Duration::from_millis(7_500)));
    f.advance_ms(7_500);

    // Longer than the configured cap is honoured too.
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail(&lease, &error(FailureClass::RateLimit, Some(4_000_000)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(4_000)));
    f.advance(4_000);

    // A hint of zero carries no information: the ladder applies (third
    // consecutive rate limit → 4 s).
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail(&lease, &error(FailureClass::RateLimit, Some(0)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(4)));
}

#[test]
fn a_burst_inside_one_window_escalates_only_once() {
    let f = fixture(&one_key(""));
    // Three requests were in flight when the limit hit.
    let leases = [
        f.pick("gpt-5.5").unwrap(),
        f.pick("gpt-5.5").unwrap(),
        f.pick("gpt-5.5").unwrap(),
    ];
    for lease in &leases {
        f.fail_class(lease, FailureClass::RateLimit);
    }
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1)));
    assert_eq!(f.all_credentials()[0].failures, 3);
    f.advance(1);
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::RateLimit);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(2)));
}

#[test]
fn success_resets_the_backoff_ladder() {
    let f = fixture(&one_key(""));
    for expected in [1, 2, 4] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        f.advance(expected);
    }
    let lease = f.pick("gpt-5.5").unwrap();
    f.succeed(&lease, 200);
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::RateLimit);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1)));
}

#[test]
fn backoff_ladders_are_per_model() {
    let f = fixture(&one_key(""));
    for expected in [1, 2, 4] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        f.advance(expected);
    }
    let lease = f.pick("gpt-6-sol").unwrap();
    f.fail_class(&lease, FailureClass::RateLimit);
    assert_eq!(wait(&f, "gpt-6-sol"), Some(secs(1)));
}

#[test]
fn quota_rests_the_whole_credential() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Quota);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(3600)));
    assert_eq!(wait(&f, "gpt-6-sol"), Some(secs(3600)));

    let c = &f.all_credentials()[0];
    assert_eq!(c.status, CredentialStatus::Cooling);
    assert_eq!(c.cooldown_until, Some(f.now_ms() + 3_600_000));
    assert_eq!(c.cooldown_reason, Some(FailureClass::Quota));
    assert!(c.model_cooldowns.is_empty());

    f.advance(3599);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1)));
    f.advance(1);
    assert_eq!(wait(&f, "gpt-5.5"), None);
    assert_eq!(f.all_credentials()[0].status, CredentialStatus::Ready);
}

#[test]
fn quota_honours_a_longer_upstream_wait_but_not_a_shorter_one() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail(&lease, &error(FailureClass::Quota, Some(7_200_000)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(7200)));

    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail(&lease, &error(FailureClass::Quota, Some(10_000)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(3600)));
}

#[test]
fn auth_rests_the_whole_credential() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Auth);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1800)));
    assert_eq!(wait(&f, "gpt-5.6-sol"), Some(secs(1800)));
    let c = &f.all_credentials()[0];
    assert_eq!(c.status, CredentialStatus::Cooling);
    assert_eq!(c.cooldown_reason, Some(FailureClass::Auth));
    f.advance(1800);
    assert_eq!(wait(&f, "gpt-5.5"), None);
}

#[test]
fn model_not_found_rests_only_that_model_for_twelve_hours() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::ModelNotFound);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(43_200)));
    assert_eq!(wait(&f, "gpt-6-sol"), None);
    let c = &f.all_credentials()[0];
    assert_eq!(c.status, CredentialStatus::Ready);
    assert_eq!(c.model_cooldowns[0].reason, FailureClass::ModelNotFound);
    f.advance(43_200);
    assert_eq!(wait(&f, "gpt-5.5"), None);
}

#[test]
fn server_and_transport_errors_rest_the_model_briefly() {
    for class in [FailureClass::Server, FailureClass::Transport] {
        let f = fixture(&one_key(""));
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, class);
        assert_eq!(wait(&f, "gpt-5.5"), Some(secs(60)), "{class:?}");
        assert_eq!(wait(&f, "gpt-6-sol"), None);
        assert_eq!(f.all_credentials()[0].model_cooldowns[0].reason, class);
        f.advance(60);
        assert_eq!(wait(&f, "gpt-5.5"), None);
    }
}

#[test]
fn retry_after_wins_for_per_model_classes() {
    for (class, hint_ms) in [
        (FailureClass::Server, 5_000),
        (FailureClass::Transport, 2_000),
        (FailureClass::ModelNotFound, 90_000),
    ] {
        let f = fixture(&one_key(""));
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail(&lease, &error(class, Some(hint_ms)));
        assert_eq!(
            wait(&f, "gpt-5.5"),
            Some(Duration::from_millis(hint_ms)),
            "{class:?}"
        );
    }
}

#[test]
fn durations_come_from_the_configuration() {
    let f = fixture(&one_key(
        r#"
[routing.cooldown]
transient_secs = 5
auth_secs = 7
quota_secs = 11
model_not_found_secs = 13
rate_limit_base_secs = 3
"#,
    ));
    let cases = [
        (FailureClass::Server, 5),
        (FailureClass::Transport, 5),
        (FailureClass::ModelNotFound, 13),
        (FailureClass::RateLimit, 3),
        (FailureClass::Auth, 7),
        (FailureClass::Quota, 11),
    ];
    for (class, expected) in cases {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, class);
        assert_eq!(wait(&f, "gpt-5.5"), Some(secs(expected)), "{class:?}");
        f.advance(expected);
        let lease = f.pick("gpt-5.5").unwrap();
        f.succeed(&lease, 10);
    }
}

#[test]
fn a_zero_duration_disables_that_cooldown() {
    let f = fixture(&one_key("[routing.cooldown]\ntransient_secs = 0"));
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Server);
    assert_eq!(wait(&f, "gpt-5.5"), None);
    assert_eq!(f.all_credentials()[0].failures, 1);
}

#[test]
fn disabled_cooldowns_keep_counters_only() {
    let f = fixture(&one_key("[routing.cooldown]\nenabled = false"));
    for class in [
        FailureClass::RateLimit,
        FailureClass::Auth,
        FailureClass::Quota,
        FailureClass::ModelNotFound,
        FailureClass::Server,
        FailureClass::Transport,
    ] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail(&lease, &error(class, Some(60_000)));
        assert_eq!(wait(&f, "gpt-5.5"), None, "{class:?}");
    }
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.failures, c.consecutive_failures), (6, 6, 6));
    assert_eq!(c.status, CredentialStatus::Ready);
    assert_eq!(c.cooldown_until, None);
    assert!(c.model_cooldowns.is_empty());
    assert_eq!(
        c.last_error.as_ref().unwrap().class,
        FailureClass::Transport
    );
}

#[test]
fn a_running_cooldown_is_never_shortened() {
    let f = fixture(&one_key(""));
    let first = f.pick("gpt-5.5").unwrap();
    let second = f.pick("gpt-5.5").unwrap();
    f.fail_class(&first, FailureClass::Server);
    f.advance(10);
    f.fail(&second, &error(FailureClass::RateLimit, Some(1_000)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(50)));
    // … but it can be extended.
    f.fail(&second, &error(FailureClass::Server, Some(120_000)));
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(120)));
}

#[test]
fn model_and_credential_cooldowns_combine_to_the_later_deadline() {
    let f = fixture(&one_key(""));
    let first = f.pick("gpt-5.5").unwrap();
    let second = f.pick("gpt-5.5").unwrap();
    f.fail_class(&first, FailureClass::ModelNotFound);
    f.fail_class(&second, FailureClass::Auth);
    // The model is gone for 12 h even though the key is back after 30 min.
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(43_200)));
    assert_eq!(wait(&f, "gpt-6-sol"), Some(secs(1800)));
    f.advance(1800);
    assert_eq!(wait(&f, "gpt-6-sol"), None);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(43_200 - 1800)));
}

// ---------------------------------------------------------------------------
// Success
// ---------------------------------------------------------------------------

#[test]
fn success_clears_the_models_cooldown() {
    let f = fixture(&one_key(""));
    let failing = f.pick("gpt-5.5").unwrap();
    let straggler = f.pick("gpt-5.5").unwrap();
    f.fail_class(&failing, FailureClass::Server);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(60)));
    // A request that was already in flight comes back fine: the model works.
    f.succeed(&straggler, 800);
    assert_eq!(wait(&f, "gpt-5.5"), None);
    let c = &f.all_credentials()[0];
    assert_eq!(c.consecutive_failures, 0);
    assert!(c.model_cooldowns.is_empty());
}

#[test]
fn success_does_not_lift_a_credential_wide_cooldown() {
    let f = fixture(&one_key(""));
    let failing = f.pick("gpt-5.5").unwrap();
    let straggler = f.pick("gpt-5.5").unwrap();
    f.fail_class(&failing, FailureClass::Auth);
    f.succeed(&straggler, 800);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1800)));
}

#[test]
fn latency_is_an_exponential_moving_average() {
    let f = fixture(&one_key(""));
    assert_eq!(f.all_credentials()[0].latency_ms, None);
    let lease = f.pick("gpt-5.5").unwrap();
    f.succeed(&lease, 100);
    assert_eq!(f.all_credentials()[0].latency_ms, Some(100));
    f.succeed(&lease, 200);
    // 0.3 * 200 + 0.7 * 100
    assert_eq!(f.all_credentials()[0].latency_ms, Some(130));
    f.succeed(&lease, 200);
    // 0.3 * 200 + 0.7 * 130
    assert_eq!(f.all_credentials()[0].latency_ms, Some(151));
    // Failures do not move it.
    f.fail_class(&lease, FailureClass::Server);
    assert_eq!(f.all_credentials()[0].latency_ms, Some(151));
}

#[test]
fn counters_and_last_used_follow_the_outcomes() {
    let f = fixture(&one_key("[routing.cooldown]\nenabled = false"));
    assert_eq!(f.all_credentials()[0].last_used_at, None);
    let lease = f.pick("gpt-5.5").unwrap();
    for _ in 0..3 {
        f.advance(1);
        f.fail_class(&lease, FailureClass::Server);
    }
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.successes, c.failures), (3, 0, 3));
    assert_eq!(c.consecutive_failures, 3);
    assert_eq!(c.last_used_at, Some(f.now_ms()));

    f.advance(5);
    f.succeed(&lease, 42);
    let c = &f.all_credentials()[0];
    assert_eq!((c.requests, c.successes, c.failures), (4, 1, 3));
    assert_eq!(c.consecutive_failures, 0);
    assert_eq!(c.last_used_at, Some(f.now_ms()));
    // The last error stays on record after a success.
    assert!(c.last_error.is_some());
}

#[test]
fn last_error_records_status_message_time_and_model() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.advance(3);
    f.fail(
        &lease,
        &UpstreamError {
            status: 529,
            class: FailureClass::Server,
            info: UpstreamErrorInfo {
                message: "Overloaded\n  please retry".into(),
                error_type: Some("overloaded_error".into()),
                ..UpstreamErrorInfo::default()
            },
            retry_after_ms: None,
            body: Some("{\"type\":\"error\"}".into()),
            content_type: Some("application/json".into()),
        },
    );
    let last = f.all_credentials()[0].last_error.clone().unwrap();
    assert_eq!(last.status, 529);
    assert_eq!(last.class, FailureClass::Server);
    assert_eq!(last.message, "Overloaded please retry");
    assert_eq!(last.at, f.now_ms());
    assert_eq!(last.model, "gpt-5.5");
    assert_eq!(last.summary(), "529 Overloaded please retry");
}

#[test]
fn last_error_never_echoes_the_key_and_is_short() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    let mut failure = error(FailureClass::Auth, None);
    failure.info.message = format!("Incorrect API key provided: {KEY}. {}", "x".repeat(2_000));
    f.fail(&lease, &failure);
    let last = f.all_credentials()[0].last_error.clone().unwrap();
    assert!(!last.message.contains(KEY));
    assert!(
        last.message
            .starts_with("Incorrect API key provided: [redacted].")
    );
    assert!(last.message.chars().count() <= 201);

    match f.pick("gpt-5.5").unwrap_err() {
        PickError::CoolingDown { last_error, .. } => {
            assert!(!last_error.unwrap().contains(KEY));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn cooling_down_quotes_the_error_that_explains_the_rest() {
    let last_error = |f: &Fixture, model: &str| match f.pick(model).unwrap_err() {
        PickError::CoolingDown { last_error, .. } => last_error,
        other => panic!("unexpected {other:?}"),
    };
    let named = |class: FailureClass, text: &str| {
        let mut failure = error(class, None);
        failure.info.message = text.into();
        failure
    };

    let f = fixture(&one_key(""));
    let first = f.pick("gpt-5.5").unwrap();
    let second = f.pick("gpt-6-sol").unwrap();
    f.fail(
        &first,
        &named(FailureClass::ModelNotFound, "no such model gpt-5.5"),
    );
    f.advance(1);
    f.fail(
        &second,
        &named(FailureClass::Server, "gpt-6-sol overloaded"),
    );

    // The credential's `last_error` is the latest one, about gpt-6-sol. It
    // must not be presented as the reason gpt-5.5 is resting.
    assert_eq!(
        f.all_credentials()[0].last_error.as_ref().unwrap().model,
        "gpt-6-sol"
    );
    assert_eq!(
        last_error(&f, "gpt-6-sol").as_deref(),
        Some("500 gpt-6-sol overloaded")
    );
    assert_eq!(
        last_error(&f, "gpt-5.5").as_deref(),
        Some("404 no such model gpt-5.5")
    );

    // An error that rests the whole credential explains the rest of every
    // model that would otherwise be back sooner …
    let third = f.pick("gpt-6-luna").unwrap();
    let fourth = f.pick("gpt-6-astra").unwrap();
    f.advance(1);
    f.fail(&third, &named(FailureClass::Auth, "key revoked"));
    for model in ["gpt-6-sol", "gpt-6-luna", "gpt-6-astra"] {
        assert_eq!(
            last_error(&f, model).as_deref(),
            Some("401 key revoked"),
            "{model}"
        );
    }
    // … while gpt-5.5 stays away longer than the key does, for its own reason.
    assert_eq!(
        last_error(&f, "gpt-5.5").as_deref(),
        Some("404 no such model gpt-5.5")
    );

    // A straggler failing differently does not change why the key rests.
    f.advance(1);
    f.fail(&fourth, &named(FailureClass::Transport, "connection reset"));
    assert_eq!(
        f.all_credentials()[0].last_error.as_ref().unwrap().message,
        "connection reset"
    );
    assert_eq!(
        last_error(&f, "gpt-6-luna").as_deref(),
        Some("401 key revoked")
    );
    assert_eq!(
        last_error(&f, "gpt-6-astra").as_deref(),
        Some("401 key revoked")
    );

    // Once the key is back, what is left are the models' own reasons.
    f.advance(1800);
    assert_eq!(wait(&f, "gpt-6-luna"), None);
    assert_eq!(
        last_error(&f, "gpt-5.5").as_deref(),
        Some("404 no such model gpt-5.5")
    );
}

// ---------------------------------------------------------------------------
// Errors from pick
// ---------------------------------------------------------------------------

#[test]
fn cooling_down_reports_the_soonest_recovery() {
    let f = fixture(TWO_KEYS);
    let a = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    f.advance_ms(10);
    let b = f.pick("gpt-5.5").unwrap();
    assert_ne!(a.credential.id, b.credential.id);
    f.fail(&b, &error(FailureClass::RateLimit, Some(7_000)));

    let err = f.pick("gpt-5.5").unwrap_err();
    assert_eq!(
        err,
        PickError::CoolingDown {
            model: "gpt-5.5".into(),
            retry_after: secs(7),
            // The most recent failure among the resting credentials.
            last_error: Some("429 upstream said 429".into()),
        }
    );
    assert_eq!(err.retry_after(), Some(secs(7)));
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), Some(secs(7)));
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5(high)"), Some(secs(7)));

    let api: ApiError = err.into();
    assert_eq!(api.status, 429);
    assert_eq!(api.retry_after_secs, Some(7));
    assert_eq!(api.code.as_deref(), Some("model_cooldown"));

    f.advance(3);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(4)));
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), Some(secs(4)));
    f.advance(4);
    // b is back; a still has 60 s - 7 s to go.
    assert_eq!(f.pick("gpt-5.5").unwrap().credential.id, b.credential.id);
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), None);
}

#[test]
fn soonest_recovery_is_none_for_ready_and_unknown_models() {
    let f = fixture(TWO_KEYS);
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), None);
    assert_eq!(f.scheduler.soonest_recovery("no-such-model"), None);
    let a = f.pick("gpt-5.5").unwrap();
    f.fail_class(&a, FailureClass::Server);
    // One of two credentials resting: still servable now.
    assert_eq!(f.scheduler.soonest_recovery("gpt-5.5"), None);
}

#[test]
fn tried_and_cooling_credentials_mix_into_the_right_error() {
    let f = fixture(TWO_KEYS);
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail(&b, &error(FailureClass::RateLimit, Some(30_000)));

    // a was tried in this request, b rests: waiting for b could still help.
    let tried = vec![a.credential.id.clone()];
    assert_eq!(
        f.pick_with("gpt-5.5", &tried, None).unwrap_err(),
        PickError::CoolingDown {
            model: "gpt-5.5".into(),
            retry_after: secs(30),
            last_error: Some("429 upstream said 429".into()),
        }
    );
    // Both tried: nothing left for this request.
    let tried = vec![a.credential.id.clone(), b.credential.id.clone()];
    assert_eq!(
        f.pick_with("gpt-5.5", &tried, None).unwrap_err(),
        PickError::Exhausted {
            model: "gpt-5.5".into()
        }
    );
}

#[test]
fn retry_after_ignores_credentials_already_tried() {
    let f = fixture(TWO_KEYS);
    let a = f.pick("gpt-5.5").unwrap();
    let b = f.pick("gpt-5.5").unwrap();
    f.fail(&a, &error(FailureClass::RateLimit, Some(5_000)));
    f.fail(&b, &error(FailureClass::RateLimit, Some(30_000)));
    // For a fresh request the soonest is a (5 s) …
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(5)));
    // … for a request that already used a, only b counts.
    let tried = vec![a.credential.id.clone()];
    match f.pick_with("gpt-5.5", &tried, None).unwrap_err() {
        PickError::CoolingDown { retry_after, .. } => assert_eq!(retry_after, secs(30)),
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Manual reset
// ---------------------------------------------------------------------------

#[test]
fn reset_clears_cooldowns_and_the_backoff_ladder() {
    let f = fixture(&one_key(""));
    for expected in [1, 2, 4] {
        let lease = f.pick("gpt-5.5").unwrap();
        f.fail_class(&lease, FailureClass::RateLimit);
        f.advance(expected);
    }
    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::Auth);
    assert!(wait(&f, "gpt-5.5").is_some());

    let id = f.all_credentials()[0].id.clone();
    assert!(f.scheduler.reset_cooldowns(&id));
    assert!(!f.scheduler.reset_cooldowns("openai:ffffffffffff"));
    assert_eq!(wait(&f, "gpt-5.5"), None);
    let c = &f.all_credentials()[0];
    assert_eq!(c.status, CredentialStatus::Ready);
    assert_eq!(c.consecutive_failures, 0);
    // Counters are history, not state: they stay.
    assert_eq!(c.failures, 4);

    let lease = f.pick("gpt-5.5").unwrap();
    f.fail_class(&lease, FailureClass::RateLimit);
    assert_eq!(wait(&f, "gpt-5.5"), Some(secs(1)));
}

#[test]
fn reports_for_removed_credentials_are_ignored() {
    let f = fixture(&one_key(""));
    let lease = f.pick("gpt-5.5").unwrap();
    f.rebuild(
        r#"
[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-a-different-key-000000000"]
"#,
    );
    f.fail_class(&lease, FailureClass::Auth);
    f.succeed(&lease, 10);
    let all = f.all_credentials();
    assert_eq!(all.len(), 1);
    assert_ne!(all[0].id, lease.credential.id);
    assert_eq!(all[0].requests, 0);
    assert_eq!(wait(&f, "gpt-5.5"), None);
}
