//! Shared test fixtures: a scheduler driven by a manual clock.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::config::Config;
use switchyard_core::{FailureClass, Protocol, UpstreamError, UpstreamErrorInfo};
use switchyard_scheduler::{
    Clock, CredentialSnapshot, Lease, ManualClock, Outcome, PickError, PickRequest, Scheduler,
};

/// Unix seconds the manual clock starts at.
pub const START_SECS: u64 = 1_800_000_000;

/// Test secret resolver: `env:SET_*` variables are "set", every other
/// reference is unset, literals pass through.
pub fn resolver(value: &str) -> Result<String, String> {
    let value = value.trim();
    let reference = value.strip_prefix("env:").or_else(|| {
        value
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
    });
    match reference {
        Some(name) if name.starts_with("SET_") => Ok(format!("sk-from-env-{name}")),
        Some(name) => Err(name.to_string()),
        None => Ok(value.to_string()),
    }
}

pub fn config(toml: &str) -> Config {
    Config::from_toml(toml).unwrap_or_else(|e| panic!("test config must be valid: {e}"))
}

pub struct Fixture {
    pub clock: Arc<ManualClock>,
    pub scheduler: Scheduler,
}

pub fn fixture(toml: &str) -> Fixture {
    let clock = Arc::new(ManualClock::at_unix_secs(START_SECS));
    let scheduler = Scheduler::with_clock(&config(toml), &resolver, clock.clone());
    Fixture { clock, scheduler }
}

impl Fixture {
    pub fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    pub fn advance_ms(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    pub fn now_ms(&self) -> i64 {
        self.clock
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    pub fn pick(&self, model: &str) -> Result<Lease, PickError> {
        self.pick_with(model, &[], None)
    }

    pub fn pick_with(
        &self,
        model: &str,
        tried: &[String],
        session: Option<&str>,
    ) -> Result<Lease, PickError> {
        self.pick_as(model, tried, session, Protocol::OpenaiChat)
    }

    pub fn pick_as(
        &self,
        model: &str,
        tried: &[String],
        session: Option<&str>,
        client_protocol: Protocol,
    ) -> Result<Lease, PickError> {
        let resolved = self.scheduler.resolve(model)?;
        self.scheduler.pick(&PickRequest {
            resolved: &resolved,
            tried,
            session,
            client_protocol,
            now: self.clock.now(),
        })
    }

    /// The API key of the credential picked for `model`.
    pub fn pick_key(&self, model: &str) -> String {
        self.pick(model).expect("a credential").credential.api_key
    }

    pub fn succeed(&self, lease: &Lease, latency_ms: u64) {
        self.scheduler
            .report(lease, Outcome::Success { latency_ms }, self.clock.now());
    }

    pub fn fail(&self, lease: &Lease, error: &UpstreamError) {
        self.scheduler
            .report(lease, Outcome::Failure(error), self.clock.now());
    }

    pub fn fail_class(&self, lease: &Lease, class: FailureClass) {
        self.fail(lease, &error(class, None));
    }

    /// Snapshot of the credential with this label.
    pub fn credential_by_label(&self, label: &str) -> CredentialSnapshot {
        self.scheduler
            .snapshot()
            .into_iter()
            .flat_map(|p| p.credentials)
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no credential labelled {label}"))
    }

    pub fn all_credentials(&self) -> Vec<CredentialSnapshot> {
        self.scheduler
            .snapshot()
            .into_iter()
            .flat_map(|p| p.credentials)
            .collect()
    }

    pub fn rebuild(&self, toml: &str) {
        self.scheduler
            .rebuild(&config(toml), &resolver, HashMap::new());
    }
}

/// An upstream failure of the given class with a typical status.
pub fn error(class: FailureClass, retry_after_ms: Option<u64>) -> UpstreamError {
    let status = match class {
        FailureClass::Request => 400,
        FailureClass::Auth => 401,
        FailureClass::Quota => 402,
        FailureClass::RateLimit => 429,
        FailureClass::ModelNotFound => 404,
        FailureClass::Server => 500,
        FailureClass::Transport => 0,
    };
    UpstreamError {
        status,
        class,
        info: UpstreamErrorInfo {
            message: format!("upstream said {status}"),
            ..UpstreamErrorInfo::default()
        },
        retry_after_ms,
        body: None,
        content_type: None,
    }
}

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
