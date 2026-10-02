//! Admin API and embedded dashboard. See `docs/DESIGN.md` section 11.
//!
//! The public surface is fixed (the `switchyard` binary is written against
//! it): [`AdminOptions`] and [`router`]. Handlers rely on the server being
//! run with `ConnectInfo<SocketAddr>` (see `switchyard_server::bind`).

use switchyard_gateway::Gateway;

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

/// Builds the admin router: `/admin/api/*` and the dashboard under `/admin/`.
pub fn router(_gateway: Gateway, _options: AdminOptions) -> axum::Router {
    axum::Router::new()
}
