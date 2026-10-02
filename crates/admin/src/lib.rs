//! Admin API and embedded dashboard. See `docs/DESIGN.md` section 11 and
//! `API.md` next to this crate's manifest, which documents every endpoint
//! with real examples.
//!
//! [`router`] returns everything under `/admin`:
//!
//! * `/admin/api/*` — the admin REST API and the live-event WebSocket. Every
//!   route is behind the admin secret (the WebSocket behind a single-use
//!   ticket bought with it), refused to remote peers unless remote access is
//!   switched on, and answers 404 when the admin interface is off or no
//!   secret is configured;
//! * `/admin/` — the dashboard's static files, embedded in the binary.
//!
//! The server must be run with `ConnectInfo<SocketAddr>` (see
//! `switchyard_server::bind`): the peer address decides what is local. A
//! request without it is treated as remote.
//!
//! # Who can reach what
//!
//! The dashboard's *files* are served to anyone who can reach the listener
//! while the admin interface is enabled: they contain no data, and the
//! sign-in page has to load before a secret can be entered. Only the *API*
//! is restricted to loopback peers (unless `admin.allow_remote`). A loopback
//! peer that sends `X-Forwarded-For`, `Forwarded` or `X-Real-IP` is a local
//! reverse proxy relaying someone else and counts as remote.
//!
//! # Secrets
//!
//! Responses never contain a literal secret, with three exceptions that
//! exist to show one: `POST /keys` (the key just created),
//! `POST /keys/{id}/reveal` and `GET /config/raw` (the operator's own file).
//! Everything else shows `switchyard_core::util::mask_secret` output;
//! references such as `env:NAME` are shown as written. A masked or empty
//! secret in an update means "keep the stored one".
//!
//! # Configuration edits
//!
//! Every mutation goes through `ConfigStore::update` / `replace_text`: the
//! read-modify-write happens inside the store's writer lock, so concurrent
//! edits are applied one after the other and none is lost; the file keeps
//! its comments; a configuration that does not validate is refused with 422
//! and changes nothing. Handlers answer once the gateway has applied the new
//! configuration, so the view they return is the one in effect.

#![forbid(unsafe_code)]

mod assets;
mod auth;
mod error;
mod key_usage;
mod routes;
mod state;
mod views;
mod ws;

use axum::middleware;
use axum::routing::{any, delete, get, patch, post};
use axum::{Extension, Router};
use std::sync::Arc;
use switchyard_gateway::Gateway;

pub(crate) use state::{Access, AdminState, Shared};

/// Settings that come from the process environment rather than the config
/// file.
#[derive(Clone, Debug, Default)]
pub struct AdminOptions {
    /// Value of `SWITCHYARD_ADMIN_SECRET`, which overrides `admin.secret`.
    pub secret_override: Option<String>,
    /// `SWITCHYARD_ADMIN_ALLOW_REMOTE` is `1` / `true`: accept non-loopback
    /// peers regardless of `admin.allow_remote`.
    pub allow_remote_override: bool,
    /// The address the server listens on, for `GET /admin/api/status`.
    pub listen: Option<std::net::SocketAddr>,
}

impl AdminOptions {
    /// Reads the overrides from the environment.
    pub fn from_env() -> Self {
        let secret_override = std::env::var("SWITCHYARD_ADMIN_SECRET")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let allow_remote_override = std::env::var("SWITCHYARD_ADMIN_ALLOW_REMOTE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        AdminOptions {
            secret_override,
            allow_remote_override,
            listen: None,
        }
    }
}

/// Lets the process end the admin interface's long-lived connections.
///
/// A graceful HTTP shutdown does not wait for upgraded connections, so the
/// live-event WebSockets would simply be cut when the runtime stops. Two
/// things prevent that:
///
/// * when the router itself is dropped — which is what a server does once
///   it has stopped serving — every socket gets a proper close frame (1001,
///   "going away"), provided the runtime keeps running for a moment
///   afterwards (flushing telemetry with `Gateway::shutdown` is enough);
/// * [`AdminHandle::shutdown`] does the same on request, for a process
///   that wants the sockets gone *before* it waits for in-flight requests.
///
/// Either way the dashboard reconnects on its own once the gateway is back.
#[derive(Clone)]
pub struct AdminHandle {
    state: Shared,
}

impl std::fmt::Debug for AdminHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminHandle")
            .field("live_connections", &self.live_connections())
            .finish()
    }
}

impl AdminHandle {
    /// Closes every live-event WebSocket and refuses new ones. Returns at
    /// once; the sockets close in the background.
    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
    }

    /// Live-event WebSockets currently open.
    pub fn live_connections(&self) -> usize {
        self.state
            .live_sockets
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Builds the admin router: `/admin/api/*` and the dashboard under `/admin/`.
///
/// The router has no fallback of its own, so it can be merged with the
/// client API's router.
pub fn router(gateway: Gateway, options: AdminOptions) -> Router {
    router_with_handle(gateway, options).0
}

/// [`router`], plus the handle that closes the live-event WebSockets at
/// shutdown.
pub fn router_with_handle(gateway: Gateway, options: AdminOptions) -> (Router, AdminHandle) {
    let state: Shared = Arc::new(AdminState::new(gateway, options));
    (build(Arc::clone(&state)), AdminHandle { state })
}

fn build(state: Shared) -> Router {
    use routes::{config, keys, models, playground, providers, status, usage};

    let api = Router::new()
        .route("/status", get(status::status))
        .route("/login", post(status::login))
        .route("/ws-ticket", post(status::ws_ticket))
        // `any`: over HTTP/2 a WebSocket is opened with CONNECT, not GET.
        .route("/ws", any(ws::live))
        .route("/config", get(config::get_config))
        .route("/config/raw", get(config::get_raw).put(config::put_raw))
        .route("/config/validate", post(config::validate))
        .route("/settings", patch(config::patch_settings))
        .route("/reload", post(config::reload))
        .route("/providers", get(providers::list).post(providers::create))
        .route(
            "/providers/{name}",
            get(providers::get_one)
                .put(providers::replace)
                .delete(providers::remove),
        )
        .route("/providers/{name}/test", post(providers::test))
        .route("/providers/{name}/discover", post(providers::discover))
        .route("/credentials/{id}/reset", post(providers::reset_credential))
        .route(
            "/credentials/{id}/enable",
            post(providers::enable_credential),
        )
        .route(
            "/credentials/{id}/disable",
            post(providers::disable_credential),
        )
        .route("/models", get(models::models))
        .route("/catalog", get(models::catalog))
        .route(
            "/aliases",
            get(models::get_aliases).put(models::put_aliases),
        )
        .route(
            "/payload",
            get(models::get_payload).put(models::put_payload),
        )
        .route(
            "/pricing",
            get(models::get_pricing).put(models::put_pricing),
        )
        .route("/keys", get(keys::list).post(keys::create))
        .route("/keys/{id}", patch(keys::update).delete(keys::remove))
        .route("/keys/{id}/reveal", post(keys::reveal))
        .route("/usage/summary", get(usage::summary))
        .route("/usage/timeseries", get(usage::timeseries))
        .route("/usage", delete(usage::clear))
        .route("/requests", get(usage::requests))
        .route("/requests/{id}", get(usage::request))
        .route("/logs", get(usage::logs))
        .route("/playground", post(playground::playground))
        .fallback(routes::unknown_route)
        .method_not_allowed_fallback(routes::method_not_allowed)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::guard,
        ))
        .with_state(Arc::clone(&state));

    Router::new()
        .route("/admin", get(assets::redirect))
        .route("/admin/", get(assets::index))
        .route("/admin/{*path}", get(assets::file))
        // Mounted as an opaque service, not with `nest`: `nest` would
        // flatten the API's routes into this router and hand every unknown
        // `/admin/api/…` path to the file route above, around the guard.
        // This way the whole prefix belongs to the API, its own fallback
        // included.
        .nest_service("/admin/api", api)
        .with_state(Arc::clone(&state))
        // Lives exactly as long as the router (and its clones) does.
        .layer(Extension(Arc::new(RouterAlive(state.shutdown.clone()))))
}

/// Ends the live-event sockets when the last clone of the router is gone:
/// a server that has stopped serving drops its router, and the sockets,
/// which run in tasks of their own, would otherwise outlive it until the
/// runtime is torn down under them.
struct RouterAlive(tokio_util::sync::CancellationToken);

impl Drop for RouterAlive {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
