//! What `serve` prints once it is listening.

use crate::urls;
use std::net::SocketAddr;
use std::path::PathBuf;
use switchyard_core::Config;
use switchyard_core::config::{ProviderKind, resolve_secret};

/// Whether, and where, the dashboard can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dashboard {
    /// Served at this URL.
    At(String),
    /// `admin.ui` is off: only the admin API, under this URL.
    ApiOnly(String),
    /// No admin secret is configured, so every admin route answers 404.
    NoSecret,
    /// `admin.enabled` is false.
    Disabled,
}

impl Dashboard {
    /// Works out the dashboard's state. `secret_from_env` says whether
    /// `SWITCHYARD_ADMIN_SECRET` is set; `base` is the gateway's URL.
    pub fn of(config: &Config, secret_from_env: bool, base: &str) -> Self {
        if !config.admin.enabled {
            return Dashboard::Disabled;
        }
        let secret_in_file = resolve_secret(&config.admin.secret).is_ok_and(|s| !s.is_empty());
        if !secret_from_env && !secret_in_file {
            return Dashboard::NoSecret;
        }
        if config.admin.ui {
            Dashboard::At(urls::dashboard_url(base))
        } else {
            Dashboard::ApiOnly(format!("{}/admin/api", base.trim_end_matches('/')))
        }
    }
}

/// Everything the start banner shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartInfo {
    /// The address the listener is bound to.
    pub listen: SocketAddr,
    /// HTTPS rather than HTTP.
    pub tls: bool,
    /// The configuration file in use.
    pub config_path: PathBuf,
    /// Enabled providers.
    pub providers: usize,
    /// Credentials of the enabled providers.
    pub credentials: usize,
    /// Client-facing model names.
    pub models: usize,
    /// The dashboard's state.
    pub dashboard: Dashboard,
    /// The scheduler's warnings about the configuration.
    pub warnings: Vec<String>,
    /// Advice about what is still missing.
    pub hints: Vec<String>,
}

/// The line printed on stdout for scripts: `listening on <url>`, where the
/// URL can be connected to from this machine (a wildcard address is shown as
/// loopback).
pub fn listening_line(addr: SocketAddr, tls: bool) -> String {
    format!("listening on {}", urls::base_url_of(addr, tls))
}

/// Advice for a configuration that cannot serve anyone yet.
pub fn hints(config: &Config) -> Vec<String> {
    let mut hints = Vec::new();
    let enabled: Vec<_> = config.providers.iter().filter(|p| p.enabled).collect();
    if enabled.is_empty() {
        hints.push(
            "no provider is configured, so there is no model to serve: add one under \
             [[providers]] or on the dashboard's Providers page"
                .to_string(),
        );
    } else if enabled.iter().all(|p| p.kind == ProviderKind::Mock) {
        hints.push(
            "only the built-in mock provider is configured: add a real one under \
             [[providers]] or on the dashboard's Providers page"
                .to_string(),
        );
    }
    if config.auth.required && !config.auth.keys.iter().any(|key| key.enabled) {
        hints.push(
            "no client key is configured, so every API request will be refused: add one \
             under [[auth.keys]] or on the dashboard's API keys page"
                .to_string(),
        );
    }
    hints
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The banner printed on stderr when the gateway is up.
pub fn start_banner(info: &StartInfo) -> String {
    let mut out = format!("switchyard {} is running\n", env!("CARGO_PKG_VERSION"));
    let bound = urls::bound_url(info.listen, info.tls);
    let local = urls::base_url_of(info.listen, info.tls);
    if info.listen.ip().is_unspecified() {
        out.push_str(&format!(
            "  listening  {bound} (all interfaces; from this machine: {local})\n"
        ));
    } else {
        out.push_str(&format!("  listening  {bound}\n"));
    }
    match &info.dashboard {
        Dashboard::At(url) => out.push_str(&format!("  dashboard  {url}\n")),
        Dashboard::ApiOnly(url) => out.push_str(&format!(
            "  dashboard  switched off (admin.ui = false); the admin API is at {url}\n"
        )),
        Dashboard::NoSecret => out.push_str(
            "  dashboard  off: no admin secret is set (admin.secret or SWITCHYARD_ADMIN_SECRET)\n",
        ),
        Dashboard::Disabled => out.push_str("  dashboard  off (admin.enabled = false)\n"),
    }
    out.push_str(&format!("  config     {}\n", info.config_path.display()));
    out.push_str(&format!(
        "  serving    {}, {}, {}\n",
        plural(info.providers, "provider"),
        plural(info.credentials, "credential"),
        plural(info.models, "model")
    ));
    for warning in &info.warnings {
        out.push_str(&format!("  warning    {warning}\n"));
    }
    for hint in &info.hints {
        out.push_str(&format!("  hint       {hint}\n"));
    }
    out.push_str("Press Ctrl-C to stop.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_config_store::validate_text;

    fn info(listen: &str) -> StartInfo {
        StartInfo {
            listen: listen.parse().unwrap(),
            tls: false,
            config_path: PathBuf::from("my dir/switchyard.toml"),
            providers: 2,
            credentials: 3,
            models: 1,
            dashboard: Dashboard::At("http://127.0.0.1:8317/admin/".into()),
            warnings: vec!["alias `x`: target `y` matches no model".into()],
            hints: vec![],
        }
    }

    #[test]
    fn listening_line_is_connectable() {
        assert_eq!(
            listening_line("127.0.0.1:8317".parse().unwrap(), false),
            "listening on http://127.0.0.1:8317"
        );
        assert_eq!(
            listening_line("0.0.0.0:9000".parse().unwrap(), false),
            "listening on http://127.0.0.1:9000"
        );
        assert_eq!(
            listening_line("[::1]:443".parse().unwrap(), true),
            "listening on https://[::1]:443"
        );
    }

    #[test]
    fn banner_lists_everything() {
        let text = start_banner(&info("127.0.0.1:8317"));
        assert_eq!(
            text,
            format!(
                "switchyard {} is running\n\
                 \x20 listening  http://127.0.0.1:8317\n\
                 \x20 dashboard  http://127.0.0.1:8317/admin/\n\
                 \x20 config     my dir/switchyard.toml\n\
                 \x20 serving    2 providers, 3 credentials, 1 model\n\
                 \x20 warning    alias `x`: target `y` matches no model\n\
                 Press Ctrl-C to stop.",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn banner_explains_a_wildcard_address_and_a_missing_dashboard() {
        let mut wildcard = info("0.0.0.0:8317");
        wildcard.tls = true;
        wildcard.dashboard = Dashboard::NoSecret;
        wildcard.hints = vec!["no provider".into()];
        let text = start_banner(&wildcard);
        assert!(text.contains(
            "listening  https://0.0.0.0:8317 (all interfaces; from this machine: https://127.0.0.1:8317)"
        ));
        assert!(text.contains("dashboard  off: no admin secret is set"));
        assert!(text.contains("  hint       no provider\n"));

        let mut off = info("127.0.0.1:1");
        off.dashboard = Dashboard::Disabled;
        assert!(start_banner(&off).contains("dashboard  off (admin.enabled = false)"));
        off.dashboard = Dashboard::ApiOnly("http://127.0.0.1:1/admin/api".into());
        assert!(start_banner(&off).contains("the admin API is at http://127.0.0.1:1/admin/api"));
    }

    #[test]
    fn dashboard_states() {
        let base = "http://127.0.0.1:8317";
        let none = validate_text("").unwrap();
        assert_eq!(Dashboard::of(&none, false, base), Dashboard::NoSecret);
        assert_eq!(
            Dashboard::of(&none, true, base),
            Dashboard::At("http://127.0.0.1:8317/admin/".into())
        );
        let secret = validate_text("[admin]\nsecret = \"s3cret\"\n").unwrap();
        assert_eq!(
            Dashboard::of(&secret, false, base),
            Dashboard::At("http://127.0.0.1:8317/admin/".into())
        );
        let unset =
            validate_text("[admin]\nsecret = \"env:SWITCHYARD_TEST_UNSET_VARIABLE\"\n").unwrap();
        assert_eq!(Dashboard::of(&unset, false, base), Dashboard::NoSecret);
        let no_ui = validate_text("[admin]\nsecret = \"s\"\nui = false\n").unwrap();
        assert_eq!(
            Dashboard::of(&no_ui, false, base),
            Dashboard::ApiOnly("http://127.0.0.1:8317/admin/api".into())
        );
        let disabled = validate_text("[admin]\nsecret = \"s\"\nenabled = false\n").unwrap();
        assert_eq!(Dashboard::of(&disabled, true, base), Dashboard::Disabled);
    }

    #[test]
    fn hints_for_what_is_missing() {
        let empty = validate_text("").unwrap();
        let both = hints(&empty);
        assert_eq!(both.len(), 2);
        assert!(both[0].contains("no provider is configured"));
        assert!(both[1].contains("no client key is configured"));

        let starter = validate_text(
            "[[auth.keys]]\nkey = \"k\"\n[[providers]]\nname = \"mock\"\nkind = \"mock\"\n",
        )
        .unwrap();
        let only_mock = hints(&starter);
        assert_eq!(only_mock.len(), 1);
        assert!(only_mock[0].contains("only the built-in mock provider"));

        let complete = validate_text(
            "[auth]\nrequired = false\n[[providers]]\nname = \"o\"\nkind = \"openai\"\napi_keys = [\"sk-x\"]\n",
        )
        .unwrap();
        assert!(hints(&complete).is_empty());

        let disabled = validate_text(
            "[[auth.keys]]\nkey = \"k\"\nenabled = false\n[[providers]]\nname = \"o\"\nkind = \"openai\"\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(hints(&disabled).len(), 2);
    }
}
