//! The Switchyard engine: the request pipeline. See `docs/DESIGN.md`
//! section 8 and `API.md` next to this crate's manifest.
//!
//! A [`Gateway`] owns everything a running gateway needs — the
//! configuration store, the scheduler, the upstream transport, telemetry and
//! the reasoning store — and exposes a small API to the `server` and `admin`
//! crates:
//!
//! * [`Gateway::authenticate`] turns the credentials on a request into a
//!   [`ClientIdentity`];
//! * [`Gateway::generate`] serves a generation request and answers with a
//!   [`Reply`]: a complete response, or a stream whose first event is ready;
//! * [`Gateway::count_tokens`], [`Gateway::raw`],
//!   [`Gateway::open_upstream_ws`], [`Gateway::models`] cover the other
//!   client routes;
//! * [`Gateway::test_provider`], [`Gateway::discover`] and
//!   [`Gateway::discovery_states`] serve the admin API.
//!
//! # One request
//!
//! 1. The body is parsed and the client's codec reads the model and the
//!    stream flag. Errors are rendered in the client's protocol.
//! 2. The client key's model allow-list and rate limit are checked.
//! 3. The scheduler resolves the model (reasoning suffix split off) and
//!    picks a credential; the attempt is built by *passthrough* when the
//!    upstream speaks the client's protocol and by *translation* through
//!    the canonical model otherwise.
//! 4. A failed attempt is reported to the scheduler and another credential
//!    is tried, up to `routing.max_attempts`. A request fault (`400`) ends
//!    the loop at once. For streams, a failure before the first event is a
//!    failed attempt like any other (limited by
//!    `streaming.bootstrap_retries`); after it, the failure is delivered
//!    in-band. When every credential rests and the soonest is back within
//!    `routing.max_wait_secs`, the request waits for it, once.
//!    A Responses upstream that refuses the reasoning summary a translated
//!    request asked it for is asked once more without it, on the same
//!    credential, and — when the refusal is about the organisation or the
//!    model, not about a detail level the client chose — not asked again
//!    until the configuration changes.
//! 5. Whatever happens, exactly one request record is published.
//!
//! # What clients cannot do to each other
//!
//! * Token counting, raw side endpoints and upstream WebSockets are
//!   optional: a refusal there that says nothing about a credential's
//!   ability to generate never rests the credential or the model.
//! * A client's own vendor-account headers (`OpenAI-Organization`,
//!   `OpenAI-Project`) are not sent upstream next to the gateway's key.
//! * What an upstream says about a failure is shown without the upstream
//!   credential it may quote.
//! * An upstream's refusal of the *gateway's* credential is the operator's
//!   business: no upstream `401` / `403` becomes a client's status or body,
//!   and a request that finds every credential resting is told so without
//!   the upstream failure — some earlier request's — that started the rest.
//!   Both are on the request record.
//! * A mock model that fails on purpose never rests the mock credential the
//!   working mock models share.
//! * A reasoning-summary detail level that one client chose and the
//!   upstream refused costs nobody else the reasoning text they ask for:
//!   only a refusal about the organisation (the provider) or about the
//!   plain default (that model) is remembered.
//! * The request types' `Debug` output never prints the client's key (or
//!   any header value that could carry one) or a body.
//!
//! No lock is held across an `.await`, upstream bodies are never buffered
//! beyond `server.body_limit_mb`, and gauges and records are released by
//! RAII, so a cancelled request or a panicking stream task cannot leak
//! them.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `switchyard_core::UpstreamError` is the workspace's error type for
// upstream failures; it carries the raw error body and is passed by value.
#![allow(clippy::result_large_err, clippy::large_enum_variant)]

mod auth;
mod count;
mod failover;
mod gateway;
mod generate;
mod ops;
mod prepare;
mod raw;
mod recorder;
mod reply;
mod session;
mod stream;
mod summary;
mod target;
mod types;
mod ws;

pub use auth::ClientIdentity;
pub use gateway::Gateway;
pub use types::{
    ClientRequest, DiscoveryState, DiscoveryStatus, FullReply, GatewayOptions,
    PresentedCredentials, ProviderTest, RawRequest, Reply, StartError, StreamReply, WsOpenRequest,
};
pub use ws::{UpstreamWsSession, WsEnd, WsOutcome};

// Types of the gateway's dependencies that appear in its API, so the server
// and admin crates need not name those crates for the common cases.
pub use switchyard_config_store::ConfigStore;
pub use switchyard_telemetry::Transport;
pub use switchyard_upstream::{UpstreamWebSocket, WsMessage};
