//! Client-facing API routes. See `docs/DESIGN.md` sections 9 and 10.
//!
//! The public surface is fixed (the `switchyard` binary and the admin crate
//! are written against it):
//!
//! * [`router`] — every client route (HTTP, SSE, WebSocket) as an
//!   [`axum::Router`];
//! * [`ServeOptions`], [`bind`], [`BoundServer`] — listening socket, optional
//!   TLS, graceful shutdown. Connections are served with
//!   `ConnectInfo<SocketAddr>` so handlers (including the admin crate's) can
//!   see the peer address.
//!
//! # Routes
//!
//! | Method | Path | |
//! |---|---|---|
//! | GET | `/` | banner |
//! | GET, HEAD | `/healthz` | |
//! | GET | `/v1/models`, `/v1/models/{id}` | OpenAI shape; Anthropic's with an `anthropic-version` header |
//! | POST | `/v1/chat/completions` | |
//! | POST | `/v1/completions` | legacy shim over chat |
//! | POST | `/v1/responses` | |
//! | GET | `/v1/responses` | WebSocket |
//! | POST | `/v1/responses/input_tokens` | token count |
//! | POST | `/v1/messages`, `/v1/messages/count_tokens` | |
//! | GET | `/v1beta/models`, `/v1beta/models/{name}` | Gemini shape |
//! | POST | `/v1beta/models/{model}:{method}` | `generateContent`, `streamGenerateContent`, `countTokens`; also under `/v1/models/` |
//! | GET | `/v1/realtime` | WebSocket relay |
//! | POST | `/v1/embeddings`, `/v1/images/generations`, `/v1/moderations`, `/v1/audio/speech` | raw JSON proxy |
//!
//! Everything else is a `404`, a known path with another method a `405`,
//! both as JSON in the error envelope of the API family the path belongs to
//! (OpenAI's, Anthropic's under `/v1/messages`, Google's under `/v1beta`).
//!
//! # What every response carries
//!
//! `x-request-id` (the id of the request record when there is one) and
//! `server: switchyard`; with `server.cors` on, permissive CORS headers —
//! and `OPTIONS` is then answered `204` before authentication. A handler
//! that panics is answered with a `500` in the route's envelope.
//!
//! With `server.cors` off, other sites' web pages are kept out altogether:
//! a request that a browser attributes to a page of another origin
//! (`Sec-Fetch-Site`, or an `Origin` that does not name this host) is
//! answered `403` on every route but the model listings — WebSocket
//! upgrades included, which browsers do not subject to CORS. Clients that
//! are not browsers send neither header and are unaffected.
//!
//! Settings are read from the live configuration on each request:
//! `server.body_limit_mb`, `server.cors`, `streaming.keepalive_secs`.
//!
//! # Responses over WebSocket
//!
//! `GET /v1/responses` speaks the vendor's WebSocket mode to any provider:
//! the gateway remembers the latest response of each lane (`stream_id`) of
//! a connection and rebuilds the full input for every turn.
//! `previous_response_id` alone decides what a request continues, as it
//! does at the vendor: a `response.create` without one is a request of its
//! own (only `model` is defaulted from the connection's last request), one
//! that names a response the connection no longer remembers is answered
//! with an in-band `409 previous_response_not_found`, and the legacy
//! `response.append` continues its lane's latest response.
//!
//! The connection is pinged every `streaming.keepalive_secs`. A client that
//! leaves two pings in a row unanswered during a turn is dropped; anything
//! it sends counts as an answer, including the bytes of a request still
//! being uploaded. Between turns, when many clients do not read their
//! socket at all, it is given ten minutes.
//!
//! # Not implemented
//!
//! Responses WebSocket turns always reach the upstream over HTTP. The
//! optional relay to an upstream's own Responses WebSocket (`websocket =
//! true` on an `openai` provider) is not implemented; such providers are
//! served like any other. WebSockets are served over HTTP/1.1 only.

#![forbid(unsafe_code)]

mod app;
mod body;
mod handlers;
mod lifecycle;
mod origin;
mod respond;
mod route;
mod serve;
mod stall;
mod ws;

use std::path::{Path, PathBuf};
use std::time::Duration;

use switchyard_core::Config;
use switchyard_gateway::Gateway;

pub use serve::{BoundServer, bind};

/// Builds the client API router.
///
/// The router matches every path — unknown ones are its `404`s — but brings
/// no fallback, so it can be merged with a router that serves other,
/// more specific routes (the admin API under `/admin`). Its layers (request
/// id, CORS, panic recovery) apply to its own routes only.
pub fn router(gateway: Gateway) -> axum::Router {
    app::router(gateway)
}

/// Certificate chain and private key files for HTTPS.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    /// PEM file with the certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM file with the private key (PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
}

/// Where and how to listen.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Address to bind: an IP address or a host name.
    pub host: String,
    /// Port to bind; `0` lets the operating system choose.
    pub port: u16,
    /// Serve HTTPS with this certificate and key.
    pub tls: Option<TlsFiles>,
    /// How long in-flight requests may run after shutdown was requested.
    pub shutdown_grace: Duration,
}

impl ServeOptions {
    /// Listening options from `[server]`; relative TLS paths resolve against
    /// `config_dir`.
    pub fn from_config(config: &Config, config_dir: &Path) -> Self {
        ServeOptions {
            host: config.server.host.clone(),
            port: config.server.port,
            tls: config.server.tls.as_ref().map(|t| TlsFiles {
                cert: config_dir.join(&t.cert),
                key: config_dir.join(&t.key),
            }),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}
