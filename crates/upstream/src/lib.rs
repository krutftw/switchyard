//! Transport to upstream providers. See `docs/DESIGN.md` section 5.
//!
//! This crate knows how to *reach* a provider — URLs, authentication,
//! proxies, TLS, time limits — and how to read what comes back when a call
//! fails. It makes no routing decisions and knows nothing about request or
//! response bodies beyond error envelopes.
//!
//! * [`Target`], [`Auth`], [`Operation`], [`Timeouts`] — what to call and
//!   how ([`target`]).
//! * [`build_request`] — the pure mapping from a target and an operation to
//!   method, URL and headers ([`request`]).
//! * [`UpstreamClient`] — sends requests ([`client`]), lists models
//!   ([`discovery`]) and opens WebSockets ([`ws`]).
//! * [`classify()`] — turns a non-2xx response into an
//!   [`switchyard_core::UpstreamError`] with a failure class and retry hint
//!   ([`mod@classify`]).
//! * [`ServiceAccount`], [`TokenSource`] — Google service-account
//!   authentication for Vertex AI ([`vertex`]).
//! * [`HttpClients`], [`resolve_proxy`], [`tls`] — connection plumbing.
//! * [`mock_stream`], [`mock_response`], [`mock_models`] — the built-in mock
//!   provider ([`mock`]).
//!
//! # Credentials
//!
//! Nothing here prints a credential: `Debug` of [`Target`], [`Auth`] and
//! [`BuiltRequest`] masks keys and hides the values of configured headers
//! (except a few harmless names such as `user-agent`), and URLs are logged
//! without their query string. Errors returned by [`UpstreamClient::send`]
//! and [`UpstreamClient::connect_ws`] have the credentials the call presented
//! replaced by `[redacted]`, should the upstream have quoted them. For text
//! that reaches the gateway by another route — an error event inside a
//! stream, a WebSocket close reason — use [`Target::redact`].

// `switchyard_core::UpstreamError` is the error type the whole workspace
// agreed on for upstream failures; it is large (it carries the raw error
// body) and returned by value throughout. Boxing it here would only move
// the allocation to the one path that is already slow.
#![allow(clippy::result_large_err)]

pub mod classify;
pub mod client;
pub mod discovery;
pub mod http;
pub mod mock;
pub mod proxy;
pub mod request;
mod secrets;
pub mod target;
pub mod tls;
pub mod vertex;
pub mod ws;

pub use classify::{
    classify, classify_at, filter_response_headers, parse_duration_ms, parse_error_body,
    retry_after_from_headers,
};
pub use client::{UpstreamBody, UpstreamClient, UpstreamResponse};
pub use discovery::{
    ModelPage, parse_anthropic_models, parse_gemini_models, parse_models, parse_openai_models,
    parse_vertex_models,
};
pub use http::HttpClients;
pub use mock::{mock_error, mock_models, mock_response, mock_stream};
pub use proxy::{redact_proxy_url, resolve_proxy};
pub use request::{
    ANTHROPIC_VERSION, BuiltRequest, USER_AGENT, VERTEX_ANTHROPIC_VERSION, VERTEX_SCOPE,
    adapt_vertex_anthropic_body, build_request, build_ws_request, is_vertex_anthropic_model,
    redact_url, vertex_protocol_for_model,
};
pub use target::{Auth, Operation, Target, Timeouts};
pub use tls::TlsConfigs;
pub use vertex::{ServiceAccount, ServiceAccountError, TokenSource};
pub use ws::{AsyncIo, BoxedIo, DEFAULT_WS_CONNECT_TIMEOUT, UpstreamWebSocket, WsConnection};

/// The error type produced while reading from or writing to an
/// [`UpstreamWebSocket`].
pub use tokio_tungstenite::tungstenite::Error as WsError;
/// The WebSocket message type of [`UpstreamWebSocket`], re-exported so that
/// callers need no direct dependency on the WebSocket library.
pub use tokio_tungstenite::tungstenite::Message as WsMessage;
