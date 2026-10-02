//! `GET /status`, `POST /login`, `POST /ws-ticket`.

use crate::Shared;
use crate::auth::{AuthContext, TICKET_TTL};
use crate::error::{ApiResult, ok_json};
use axum::Extension;
use axum::extract::State;
use serde_json::json;
use std::time::Instant;
use switchyard_core::util::now_unix_ms;
use switchyard_gateway::Gateway;
use switchyard_scheduler::CredentialStatus;

/// What the dashboard's shell needs to know about the running gateway.
pub(crate) async fn status(
    State(state): State<Shared>,
    Extension(context): Extension<AuthContext>,
) -> ApiResult {
    let gateway = &state.gateway;
    let config = gateway.config();
    let store = gateway.config_store();
    let scheduler = gateway.scheduler();
    let telemetry = gateway.telemetry();
    let now = now_unix_ms();

    // What is in service: the credentials of a provider that is switched
    // off are not, and neither is an alias without a routable target.
    let snapshot = scheduler.snapshot();
    let credentials = snapshot
        .iter()
        .flat_map(|p| p.credentials.iter())
        .filter(|c| !c.provider_disabled());
    let credentials_total = credentials.clone().count();
    let credentials_ready = credentials
        .filter(|c| c.status == CredentialStatus::Ready)
        .count();
    // One clock for the whole answer: the top-level start time and uptime
    // are the gauges' own.
    let live = telemetry.status(now);
    let access = state.access();

    ok_json(&json!({
        "version": Gateway::version(),
        "started_at": live.started_at,
        "uptime_ms": live.uptime_ms,
        "config_path": store.path().display().to_string(),
        "data_dir": telemetry.data_dir().map(|dir| dir.display().to_string()),
        "listen": state.options.listen.map(|addr| addr.to_string()),
        "tls": state.options.tls,
        "restart_required": store.restart_required(),
        "warnings": scheduler.warnings(),
        "counts": {
            "providers": config.providers.len(),
            "credentials": credentials_total,
            "credentials_ready": credentials_ready,
            "models": scheduler.models_routable(),
            "client_keys": config.auth.keys.len(),
        },
        "live": live,
        "admin": {
            "allow_remote": access.allow_remote,
            "remote": context.peer.remote,
        },
        "auth_required": config.auth.required,
    }))
}

/// The guard in front of this route checked the secret; getting here means
/// it was right.
pub(crate) async fn login() -> ApiResult {
    ok_json(&json!({ "ok": true }))
}

/// Sells a single-use ticket for `GET /ws`.
pub(crate) async fn ws_ticket(State(state): State<Shared>) -> ApiResult {
    let ticket = state.tickets.lock().issue(Instant::now());
    ok_json(&json!({
        "ticket": ticket,
        "expires_in": TICKET_TTL.as_secs(),
    }))
}
