//! The attempt loop shared by every operation that talks to an upstream:
//! which credential comes next, when to stop, and what the client is told
//! when nothing worked.
//!
//! # Optional endpoints
//!
//! Generation is what a credential is for: when it fails there, the
//! scheduler is told and rests the credential or the model accordingly.
//! Token counting, the raw side endpoints and upstream WebSockets are
//! *optional* — plenty of upstreams (and restricted keys) serve generation
//! and nothing else — and what such a call is refused with is decided by
//! the endpoint and by what the client put into the request, not by the
//! health of the credential. A `404` for `/v1/moderations`, a `403` for an
//! embeddings request naming a chat model, a refused upgrade: none of these
//! may take a model or a key out of rotation for everybody else's
//! generation requests. [`blames_credential`] draws the line, and
//! [`Failover::passed_over`] moves on to the next credential without
//! reporting anything.
//!
//! # What a client is told about the gateway's own credentials
//!
//! The upstream is called with the *gateway's* credential, so what an
//! upstream says when it refuses that credential is the operator's business
//! and not the client's — whose own key is fine:
//!
//! * a `401` or `403` is never handed on as it is, neither its status nor
//!   its body, whatever the failure was classified as for the scheduler's
//!   purposes ([`gateway_refused`]): vendors answer "this key's project may
//!   not use that model" with a `403` that names the project;
//! * the answer to a request that finds every credential resting does not
//!   quote the failure that started the rest ([`unpicked`]): that failure
//!   happened to another request, possibly another client's, and half an
//!   hour of "Incorrect API key provided: sk-…" is exactly what the first
//!   rule exists to prevent.
//!
//! Both are kept for the operator: every attempt is on the request record
//! with what the upstream said, and the record of a request refused during
//! a rest names the failure behind it.
//!
//! # The mock provider
//!
//! A mock model that fails on purpose (`mock-error-401`) says nothing about
//! the mock provider's one synthetic credential, which every other mock
//! model shares. Such a *scripted* failure is reported to the scheduler
//! only when it rests no more than the failing model itself
//! ([`Failure::scripted`]).

use crate::gateway::Inner;
use crate::reply::{Verbatim, json_object_text};
use std::time::Duration;
use switchyard_core::{ApiError, FailureClass, Protocol, UpstreamError};
use switchyard_scheduler::{CredentialId, Lease, Outcome, PickError, PickRequest, Resolved};
use tokio_util::sync::CancellationToken;

/// Added to a cooldown wait so the pick that follows lands after the
/// cooldown's end rather than on it.
const WAIT_MARGIN: Duration = Duration::from_millis(15);

/// Whether a failed call to an *optional* endpoint (see the module docs)
/// says something about the credential that also holds for generation
/// traffic, and so is reported to the scheduler.
///
/// * rate limits and exhausted quota — the account is throttled or out of
///   money, whatever the endpoint;
/// * `5xx` answers and transport failures — the upstream is in trouble;
/// * request faults — reported because the scheduler counts them; they
///   rest nothing.
///
/// Not reported: a rejected credential (`401`/`403` — keys restricted to
/// some endpoints are refused exactly like that; a key that is really
/// revoked is found out by the next generation request), a missing model or
/// endpoint (`404`), and every other `4xx` (`405`, `426`, `501` and the
/// like: the endpoint is not there).
pub(crate) fn blames_credential(error: &UpstreamError) -> bool {
    match error.class {
        FailureClass::RateLimit | FailureClass::Quota | FailureClass::Request => true,
        FailureClass::Transport => true,
        // "Every other status" is classified `Server` too; only a real
        // server fault counts here (`501` is the endpoint saying it does
        // not exist).
        FailureClass::Server => error.status >= 500 && error.status != 501,
        FailureClass::Auth | FailureClass::ModelNotFound => false,
    }
}

/// Whether a failure of this class rests the whole credential — every model
/// it serves — rather than one model on it.
pub(crate) fn rests_whole_credential(class: FailureClass) -> bool {
    matches!(class, FailureClass::Auth | FailureClass::Quota)
}

/// One failed upstream attempt.
#[derive(Debug)]
pub(crate) struct Failure {
    pub error: UpstreamError,
    /// Protocol the attempt spoke to the upstream.
    pub protocol: Protocol,
    /// Whether the upstream's own error body may be shown to a client that
    /// speaks `protocol`. False for failures the gateway made up itself and
    /// for the mock provider.
    pub verbatim: bool,
    /// The upstream had answered 2xx and the failure happened inside the
    /// stream, before anything reached the client. Such failures are
    /// additionally limited by `streaming.bootstrap_retries`.
    pub after_start: bool,
    /// The failure was produced on purpose by a mock model. It decides the
    /// reply like any other, but is not held against the mock credential as
    /// a whole (see the module docs).
    pub scripted: bool,
}

impl Failure {
    /// A failure reported by the upstream itself.
    pub(crate) fn upstream(error: UpstreamError, protocol: Protocol) -> Self {
        Failure {
            error,
            protocol,
            verbatim: true,
            after_start: false,
            scripted: false,
        }
    }

    /// A failure a mock model produced because its name says so.
    pub(crate) fn scripted(error: UpstreamError, protocol: Protocol) -> Self {
        Failure {
            scripted: true,
            ..Failure::local(error, protocol)
        }
    }

    /// Whether the scheduler is told about this failure. Everything is,
    /// except a mock model's scripted failure of a class that would rest
    /// the mock provider's only credential, and with it the mock models
    /// that work.
    fn reported(&self) -> bool {
        !(self.scripted && rests_whole_credential(self.error.class))
    }

    /// A failure the gateway diagnosed (unreadable body, broken stream).
    pub(crate) fn local(error: UpstreamError, protocol: Protocol) -> Self {
        Failure {
            verbatim: false,
            ..Failure::upstream(error, protocol)
        }
    }

    pub(crate) fn after_start(mut self) -> Self {
        self.after_start = true;
        self
    }
}

/// What [`Failover::next`] decided.
pub(crate) enum Next {
    /// Make an attempt with this credential.
    Lease(Box<Lease>),
    /// No further attempt: ask [`Failover::into_error`] what to answer.
    Stop,
    /// The client went away while waiting.
    Cancelled,
}

/// The error a request ends with after the attempt loop gave up.
#[derive(Debug)]
pub(crate) struct FinalError {
    /// The error in the gateway's own terms (always present; it is what the
    /// request record shows).
    pub api: ApiError,
    /// Status the last upstream attempt answered with, when one was made.
    pub upstream_status: Option<u16>,
    /// The upstream's own error body, when it may be forwarded as is.
    pub verbatim: Option<Verbatim>,
    /// More about the failure for the operator: added to the request
    /// record's error, never shown to the client.
    pub detail: Option<String>,
}

impl FinalError {
    pub(crate) fn local(api: ApiError) -> Self {
        FinalError {
            api,
            upstream_status: None,
            verbatim: None,
            detail: None,
        }
    }
}

/// What a client is told when the upstream refused the *gateway's*
/// credential, or `None` when `error` is not such a refusal.
///
/// * `Auth`-class failures (a rejected key, a service account that cannot
///   be loaded, a project without billing) are a plain `502`.
/// * So is every other `401` / `403` that is about the credential: the
///   transport classifies "this key may not use that model" as
///   `ModelNotFound` so that the scheduler rests the model rather than the
///   key, but to the client it is the same thing — the operator's key lacks
///   something — and the upstream's wording names the operator's project.
/// * A `401` / `403` the transport recognised as a fault of the *request*
///   (a file the request refers to that the upstream will not show) keeps
///   the upstream's explanation, which is about what the client sent, but
///   not the status: a `403` from the gateway would say that the client's
///   gateway key lacks a permission.
///
/// `Server`- and `Transport`-class failures (a bot-challenge page, a proxy
/// in the way) are converted like any other; [`Failover::into_error`] keeps
/// their body and status from being forwarded.
fn gateway_refused(error: &UpstreamError) -> Option<ApiError> {
    let unauthorised = matches!(error.status, 401 | 403);
    match error.class {
        FailureClass::Auth => Some(
            ApiError::upstream("the upstream provider rejected the gateway's credential")
                .with_code("upstream_auth_error"),
        ),
        FailureClass::ModelNotFound | FailureClass::Quota | FailureClass::RateLimit
            if unauthorised =>
        {
            Some(
                ApiError::upstream(
                    "the upstream provider does not let the gateway's credential serve this request",
                )
                .with_code("upstream_permission_denied"),
            )
        }
        FailureClass::Request if unauthorised => {
            let said = error.info.message.trim();
            Some(ApiError::invalid_request(if said.is_empty() {
                "the upstream provider refused the request"
            } else {
                said
            }))
        }
        FailureClass::ModelNotFound
        | FailureClass::Quota
        | FailureClass::RateLimit
        | FailureClass::Request
        | FailureClass::Server
        | FailureClass::Transport => None,
    }
}

/// What a client is told when no credential could be picked at all.
///
/// The scheduler's description of a rest quotes the upstream failure that
/// started it. That is for the operator (`detail`, which goes on the
/// request record): the failure belongs to an earlier request — possibly
/// another client's, whose input an upstream may echo — and when it was the
/// gateway's credential being rejected it would tell every client, for as
/// long as the rest lasts, what the request that ran into it was rightly
/// not told.
fn unpicked(error: PickError) -> FinalError {
    match error {
        PickError::CoolingDown {
            model,
            retry_after,
            last_error,
        } => FinalError {
            api: ApiError::from(PickError::CoolingDown {
                model,
                retry_after,
                last_error: None,
            }),
            upstream_status: None,
            verbatim: None,
            detail: last_error
                .map(|last| last.trim().to_string())
                .filter(|last| !last.is_empty())
                .map(|last| format!("last upstream error: {last}")),
        },
        other => FinalError::local(ApiError::from(other)),
    }
}

/// State of the attempt loop of one client request.
pub(crate) struct Failover<'a> {
    inner: &'a Inner,
    resolved: &'a Resolved,
    session: Option<&'a str>,
    client_protocol: Protocol,
    cancel: &'a CancellationToken,
    max_attempts: u32,
    max_wait: Duration,
    bootstrap_retries: u32,
    tried: Vec<CredentialId>,
    attempts: u32,
    waited: bool,
    bootstrap_failures: u32,
    stopped: bool,
    last: Option<Failure>,
    pick_error: Option<PickError>,
}

/// The limits of an attempt loop, from `[routing]` and `[streaming]`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_attempts: u32,
    pub max_wait: Duration,
    pub bootstrap_retries: u32,
}

impl Limits {
    pub(crate) fn from_config(config: &switchyard_core::Config) -> Self {
        Limits {
            max_attempts: config.routing.max_attempts.max(1),
            max_wait: Duration::from_secs(config.routing.max_wait_secs),
            bootstrap_retries: config.streaming.bootstrap_retries,
        }
    }
}

impl<'a> Failover<'a> {
    pub(crate) fn new(
        inner: &'a Inner,
        resolved: &'a Resolved,
        session: Option<&'a str>,
        client_protocol: Protocol,
        cancel: &'a CancellationToken,
        limits: Limits,
    ) -> Self {
        Failover {
            inner,
            resolved,
            session,
            client_protocol,
            cancel,
            max_attempts: limits.max_attempts,
            max_wait: limits.max_wait,
            bootstrap_retries: limits.bootstrap_retries,
            tried: Vec::new(),
            attempts: 0,
            waited: false,
            bootstrap_failures: 0,
            stopped: false,
            last: None,
            pick_error: None,
        }
    }

    /// Picks the credential for the next attempt.
    ///
    /// When nothing can be picked because every candidate is resting — the
    /// ones this request has not tried, or, once it has tried them all, the
    /// very credentials whose failures it just saw — and the soonest of
    /// them recovers within `routing.max_wait_secs`, waits for that — once
    /// per request — and starts over with every credential eligible again.
    /// So a request that is told "retry in one second" by its only
    /// credential is served a second later instead of failing, when the
    /// operator allows the wait. `routing.max_attempts` bounds the total
    /// either way.
    pub(crate) async fn next(&mut self) -> Next {
        if self.stopped || self.attempts >= self.max_attempts {
            return Next::Stop;
        }
        loop {
            if self.cancel.is_cancelled() {
                return Next::Cancelled;
            }
            let scheduler = &self.inner.scheduler;
            let picked = scheduler.pick(&PickRequest {
                resolved: self.resolved,
                tried: &self.tried,
                session: self.session,
                client_protocol: self.client_protocol,
                now: scheduler.now(),
            });
            match picked {
                Ok(lease) => {
                    self.attempts += 1;
                    return Next::Lease(Box::new(lease));
                }
                Err(error) => {
                    let Some(wait) = self.worthwhile_wait(&error) else {
                        self.pick_error = Some(error);
                        return Next::Stop;
                    };
                    self.waited = true;
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Next::Cancelled,
                        _ = tokio::time::sleep(wait + WAIT_MARGIN) => {}
                    }
                    // A new round: what failed before the wait may be tried
                    // again now that its rest is over.
                    self.tried.clear();
                }
            }
        }
    }

    /// How long to wait before picking again, when a pick failed for a
    /// reason that waiting can cure and the wait is one the operator allows
    /// (`routing.max_wait_secs`; once per request).
    fn worthwhile_wait(&self, error: &PickError) -> Option<Duration> {
        if self.waited || self.max_wait.is_zero() {
            return None;
        }
        let scheduler = &self.inner.scheduler;
        let wait = match error {
            // Nothing tried yet: the scheduler's figure covers everything.
            PickError::CoolingDown { retry_after, .. } if self.tried.is_empty() => *retry_after,
            // The figure covers only what this request has not tried; one
            // of the credentials it did try may be back sooner.
            PickError::CoolingDown { retry_after, .. } => scheduler
                .soonest_recovery_of(self.resolved)
                .map_or(*retry_after, |all| all.min(*retry_after)),
            // Everything was tried. Worth waiting for only if all of it
            // rests: a credential that is ready again already (its failure
            // started no cooldown) would simply fail the same way.
            PickError::Exhausted { .. } => scheduler.soonest_recovery_of(self.resolved)?,
            PickError::UnknownModel { .. } | PickError::NoCredentials { .. } => return None,
        };
        (wait <= self.max_wait).then_some(wait)
    }

    /// Records a failed attempt: reports it to the scheduler (before the
    /// next pick, as the scheduler requires) and decides whether another
    /// credential is worth trying.
    pub(crate) fn failed(&mut self, lease: &Lease, failure: Failure) {
        if failure.reported() {
            self.inner.report(lease, Outcome::Failure(&failure.error));
        }
        self.tried.push(lease.credential.id.clone());
        if failure.error.class == FailureClass::Request {
            // The request itself is at fault: no credential will do better.
            self.stopped = true;
        }
        if failure.after_start {
            self.bootstrap_failures += 1;
            if self.bootstrap_failures > self.bootstrap_retries {
                self.stopped = true;
            }
        }
        self.last = Some(failure);
    }

    /// Records an attempt on an optional endpoint that failed for a reason
    /// that says nothing about the credential (see [`blames_credential`]):
    /// the scheduler is not told, the next credential gets its chance, and
    /// the failure decides the reply if it stays the last one.
    pub(crate) fn passed_over(&mut self, lease: &Lease, failure: Failure) {
        self.tried.push(lease.credential.id.clone());
        self.last = Some(failure);
    }

    /// [`Failover::failed`] or [`Failover::passed_over`], whichever a
    /// failure on an optional endpoint calls for.
    pub(crate) fn failed_optional(&mut self, lease: &Lease, failure: Failure) {
        if blames_credential(&failure.error) {
            self.failed(lease, failure);
        } else {
            self.passed_over(lease, failure);
        }
    }

    /// What to tell the client now that the loop is over.
    ///
    /// The last upstream failure decides: its own error body and status
    /// when the client speaks the protocol it is written in, otherwise the
    /// failure converted to the gateway's terms. An upstream that refused
    /// the gateway's credential is never quoted, and no upstream `401` or
    /// `403` becomes the client's — its own key is fine
    /// ([`gateway_refused`]). When no attempt was made at all the
    /// scheduler's reason for not picking is the answer, without the
    /// upstream failure it may quote ([`unpicked`]).
    pub(crate) fn into_error(self) -> FinalError {
        let Some(failure) = self.last else {
            return match self.pick_error {
                Some(error) => unpicked(error),
                None => {
                    FinalError::local(ApiError::unavailable("no upstream attempt could be made"))
                }
            };
        };
        final_error(&failure, self.client_protocol)
    }
}

/// The error a request ends with when `failure` was its last upstream
/// attempt and the client speaks `client_protocol`.
fn final_error(failure: &Failure, client_protocol: Protocol) -> FinalError {
    let error = &failure.error;
    if let Some(api) = gateway_refused(error) {
        return FinalError {
            api,
            upstream_status: Some(error.status),
            verbatim: None,
            detail: None,
        };
    }
    let mut api = error.to_api_error();
    if api.message.trim().is_empty() {
        api.message = match error.status {
            0 => "the upstream could not be reached".to_string(),
            status => format!("the upstream answered with status {status}"),
        };
    }
    let forwardable = failure.verbatim
        && failure.protocol == client_protocol
        && (400..=599).contains(&error.status)
        // Whatever class it was given: such a status is about the gateway's
        // standing with the upstream, not about the client's with the
        // gateway.
        && !matches!(error.status, 401 | 403);
    let verbatim = if forwardable {
        error
            .body
            .as_deref()
            .and_then(json_object_text)
            .map(|body| Verbatim {
                status: error.status,
                body: body.to_string(),
            })
    } else {
        None
    };
    FinalError {
        api,
        upstream_status: Some(error.status),
        verbatim,
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::local_failure;
    use switchyard_core::ErrorKind;

    fn error(status: u16, class: FailureClass) -> UpstreamError {
        local_failure(class, status, "x")
    }

    /// An upstream failure as the transport reports it: the vendor's
    /// message, and the vendor's JSON envelope as the body.
    fn answered(status: u16, class: FailureClass, message: &str) -> Failure {
        let mut error = local_failure(class, status, message);
        error.body = Some(serde_json::json!({"error": {"message": message}}).to_string());
        error.content_type = Some("application/json".to_string());
        Failure::upstream(error, Protocol::OpenaiChat)
    }

    #[test]
    fn no_upstream_401_or_403_becomes_the_clients() {
        const SAID: &str = "Project `proj_OPERATOR` does not have access to model `gpt-5`";
        for class in [
            FailureClass::Auth,
            FailureClass::ModelNotFound,
            FailureClass::Quota,
            FailureClass::RateLimit,
            FailureClass::Server,
            FailureClass::Transport,
        ] {
            for status in [401, 403] {
                let failure = answered(status, class, SAID);
                // The client speaks the very protocol the body is in.
                let last = final_error(&failure, Protocol::OpenaiChat);
                assert_eq!(last.verbatim, None, "{status} {class:?}");
                assert_eq!(last.upstream_status, Some(status));
                assert!(
                    !matches!(last.api.status, 401 | 403),
                    "{status} {class:?}: {}",
                    last.api.status
                );
                if !matches!(class, FailureClass::Server | FailureClass::Transport) {
                    assert_eq!(last.api.status, 502, "{status} {class:?}");
                    assert!(
                        !last.api.message.contains("proj_OPERATOR"),
                        "{status} {class:?}: {}",
                        last.api.message
                    );
                    assert!(last.api.message.contains("the gateway's credential"));
                }
            }
        }
        // A rejected credential is not quoted whatever the status was
        // (Google answers a bad key with a 400).
        let last = final_error(
            &answered(400, FailureClass::Auth, "API key not valid: AIza-OPERATOR"),
            Protocol::OpenaiChat,
        );
        assert_eq!((last.api.status, last.verbatim), (502, None));
        assert_eq!(last.api.code.as_deref(), Some("upstream_auth_error"));
        assert!(!last.api.message.contains("AIza-OPERATOR"));
    }

    #[test]
    fn a_403_about_the_request_keeps_its_explanation_but_not_its_status() {
        let said = "You do not have permission to access the File abc123 or it may not exist.";
        let last = final_error(
            &answered(403, FailureClass::Request, said),
            Protocol::OpenaiChat,
        );
        assert_eq!(last.api.status, 400);
        assert_eq!(last.api.kind, ErrorKind::InvalidRequest);
        assert_eq!(last.api.message, said);
        assert_eq!(last.verbatim, None);

        let silent = final_error(
            &answered(403, FailureClass::Request, "  "),
            Protocol::OpenaiChat,
        );
        assert_eq!(silent.api.status, 400);
        assert!(!silent.api.message.trim().is_empty());
    }

    #[test]
    fn other_upstream_errors_are_still_forwarded_to_clients_of_their_protocol() {
        for (status, class) in [
            (429, FailureClass::RateLimit),
            (404, FailureClass::ModelNotFound),
            (400, FailureClass::Request),
            (500, FailureClass::Server),
        ] {
            let failure = answered(status, class, "said so");
            let same = final_error(&failure, Protocol::OpenaiChat);
            assert_eq!(
                same.verbatim.as_ref().map(|verbatim| verbatim.status),
                Some(status)
            );
            let other = final_error(&failure, Protocol::Anthropic);
            assert_eq!(other.verbatim, None);
            assert_eq!(other.api.message, "said so");
        }
    }

    #[test]
    fn a_rest_is_announced_without_the_failure_that_started_it() {
        let resting = PickError::CoolingDown {
            model: "m".into(),
            retry_after: Duration::from_secs(1800),
            last_error: Some("401 Incorrect API key provided: sk-proj-****wxyz.".into()),
        };
        let last = unpicked(resting);
        assert_eq!(last.api.status, 429);
        assert_eq!(last.api.retry_after_secs, Some(1800));
        assert_eq!(last.api.code.as_deref(), Some("model_cooldown"));
        assert!(last.api.message.contains("cooling down"));
        assert!(
            !last.api.message.contains("Incorrect"),
            "{}",
            last.api.message
        );
        assert!(!last.api.message.contains("wxyz"));
        assert!(!last.api.message.contains("last upstream error"));
        // The operator still learns why.
        assert_eq!(
            last.detail.as_deref(),
            Some("last upstream error: 401 Incorrect API key provided: sk-proj-****wxyz.")
        );

        let unexplained = unpicked(PickError::CoolingDown {
            model: "m".into(),
            retry_after: Duration::from_secs(2),
            last_error: Some("  ".into()),
        });
        assert_eq!(unexplained.detail, None);

        let unknown = unpicked(PickError::UnknownModel { model: "m".into() });
        assert_eq!((unknown.api.status, unknown.detail), (404, None));
    }

    #[test]
    fn scripted_failures_are_reported_unless_they_would_rest_the_credential() {
        let scripted =
            |status, class| Failure::scripted(error(status, class), Protocol::OpenaiChat);
        assert!(scripted(429, FailureClass::RateLimit).reported());
        assert!(scripted(500, FailureClass::Server).reported());
        assert!(!scripted(401, FailureClass::Auth).reported());
        assert!(!scripted(402, FailureClass::Quota).reported());
        assert!(!scripted(401, FailureClass::Auth).verbatim);
        // A real upstream's are reported whatever they rest.
        for class in [
            FailureClass::Auth,
            FailureClass::Quota,
            FailureClass::Server,
        ] {
            assert!(Failure::upstream(error(401, class), Protocol::OpenaiChat).reported());
            assert!(Failure::local(error(401, class), Protocol::OpenaiChat).reported());
        }
    }

    #[test]
    fn only_failures_that_hold_for_all_traffic_blame_the_credential() {
        for (status, class) in [
            (429, FailureClass::RateLimit),
            (402, FailureClass::Quota),
            (429, FailureClass::Quota),
            (500, FailureClass::Server),
            (503, FailureClass::Server),
            (529, FailureClass::Server),
            (0, FailureClass::Transport),
            (408, FailureClass::Transport),
            (400, FailureClass::Request),
        ] {
            assert!(
                blames_credential(&error(status, class)),
                "{status} {class:?}"
            );
        }
        for (status, class) in [
            (401, FailureClass::Auth),
            (403, FailureClass::Auth),
            (404, FailureClass::ModelNotFound),
            (403, FailureClass::ModelNotFound),
            (405, FailureClass::Server),
            (501, FailureClass::Server),
            (403, FailureClass::Server),
            (301, FailureClass::Server),
        ] {
            assert!(
                !blames_credential(&error(status, class)),
                "{status} {class:?}"
            );
        }
    }
}
