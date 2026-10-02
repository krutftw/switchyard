//! The settings that are not providers: listener, admin API, client keys,
//! routing, streaming, logging and usage.

use super::values::{
    boolean, clamp_u32, clamp_u64, go_duration_secs, is_bcrypt_hash, is_empty_reference,
    is_valid_host, lookup, strategy, string_list, text,
};
use super::{EMPTY_REFERENCE, Importer, PAYLOAD_FLAT, PAYLOAD_NESTED};
use switchyard_core::Config;
use switchyard_core::config::{ClientKey, RequestLogMode, TlsConfig, parse_proxy};

/// Placeholder client keys of the source program's template. It refuses to
/// serve while one of them is configured, so they are not carried over.
const TEMPLATE_KEYS: [&str; 3] = ["your-api-key-1", "your-api-key-2", "your-api-key-3"];

impl Importer<'_> {
    pub(super) fn server(&mut self, config: &mut Config) {
        match self.pick_text("server.host", "host") {
            Some(host) if !host.is_empty() => {
                // An address or a host name, nothing else.
                if is_valid_host(&host) {
                    config.server.host = host;
                } else {
                    self.not_imported.push(format!(
                        "host: not an address or a host name; server.host is the default, {}",
                        config.server.host
                    ));
                }
            }
            _ => {
                // An empty host means every interface there.
                config.server.host = "0.0.0.0".to_string();
                self.notes.push(
                    "no host was set, which means all interfaces in CLIProxyAPI: server.host \
                     is \"0.0.0.0\". Use \"127.0.0.1\" to accept local connections only"
                        .to_string(),
                );
            }
        }
        match self.pick_int("server.port", "port") {
            Some(port) if (1..=65_535).contains(&port) => {
                config.server.port = u16::try_from(port).unwrap_or(config.server.port);
            }
            Some(0) | None => self.notes.push(format!(
                "no port was set: server.port is the default, {}",
                config.server.port
            )),
            Some(_) => self.not_imported.push(format!(
                "port: not a valid port number; server.port is the default, {}",
                config.server.port
            )),
        }

        let enabled = self
            .pick_bool("server.tls.enable", "tls.enable")
            .unwrap_or(false);
        let cert = self
            .pick_text("server.tls.cert", "tls.cert")
            .unwrap_or_default();
        let key = self
            .pick_text("server.tls.key", "tls.key")
            .unwrap_or_default();
        if enabled {
            if cert.is_empty() || key.is_empty() {
                self.not_imported.push(
                    "tls: enabled without both a certificate and a key file; left off".to_string(),
                );
            } else {
                config.server.tls = Some(TlsConfig { cert, key });
                self.notes.push(
                    "server.tls: relative certificate and key paths are resolved against \
                     the directory of the new configuration file"
                        .to_string(),
                );
            }
        }
    }

    pub(super) fn admin(&mut self, config: &mut Config) {
        if let Some(allow) =
            self.pick_bool("management.allow-remote", "remote-management.allow-remote")
        {
            config.admin.allow_remote = allow;
        }
        if self
            .pick_bool(
                "management.disable-control-panel",
                "remote-management.disable-control-panel",
            )
            .unwrap_or(false)
        {
            config.admin.ui = false;
        }
        let secret = self
            .pick_text("management.secret-key", "remote-management.secret-key")
            .unwrap_or_default();
        if secret.is_empty() {
            self.notes.push(
                "no management secret was set: admin.secret is empty, so the dashboard and \
                 the admin API stay off until you set one (or SWITCHYARD_ADMIN_SECRET)"
                    .to_string(),
            );
        } else if is_bcrypt_hash(&secret) {
            self.not_imported.push(
                "the management secret-key: it is stored as a bcrypt hash, which cannot be \
                 turned back into the secret. admin.secret is empty; set it (or \
                 SWITCHYARD_ADMIN_SECRET) to use the dashboard"
                    .to_string(),
            );
        } else if is_empty_reference(&secret) {
            self.not_imported.push(format!(
                "the management secret-key: {EMPTY_REFERENCE}. admin.secret is empty; set it \
                 (or SWITCHYARD_ADMIN_SECRET) to use the dashboard"
            ));
        } else {
            config.admin.secret = secret;
        }
    }

    pub(super) fn client_keys(&mut self, config: &mut Config) {
        self.mark("access.api-keys");
        let flat = self.root.get("api-keys").filter(|value| value.is_array());
        if flat.is_some() {
            // The list form of the name that is a mapping in the nested
            // layout.
            self.mark("api-keys");
        }
        let source = match lookup(self.root, "access.api-keys") {
            Some(value) => Some(value),
            None => flat,
        };
        let keys = source.map(string_list).unwrap_or_default();
        let mut placeholders = 0usize;
        let mut unnamed = 0usize;
        for key in keys {
            if TEMPLATE_KEYS.contains(&key.as_str()) {
                placeholders += 1;
                continue;
            }
            if is_empty_reference(&key) {
                unnamed += 1;
                continue;
            }
            config.auth.keys.push(ClientKey {
                key,
                name: format!("imported-{}", config.auth.keys.len() + 1),
                enabled: true,
                models: Vec::new(),
                rate_limit_rpm: None,
            });
        }
        if placeholders > 0 {
            self.not_imported.push(format!(
                "{placeholders} of the client API keys are the placeholders of the example \
                 file (your-api-key-…)"
            ));
        }
        if unnamed > 0 {
            self.not_imported.push(format!(
                "{unnamed} of the client API keys: {EMPTY_REFERENCE}"
            ));
        }
        if config.auth.keys.is_empty() {
            self.notes.push(
                "no client API key was imported: add one under [[auth.keys]] or in the \
                 dashboard (requests without a valid key are refused)"
                    .to_string(),
            );
        }
    }

    pub(super) fn routing(&mut self, config: &mut Config) {
        if let Some(name) = self.get("routing.strategy").and_then(text) {
            match strategy(&name) {
                Some(strategy) => config.routing.strategy = strategy,
                None => self.notes.push(
                    "routing.strategy has an unknown value, which CLIProxyAPI treats as \
                     round-robin; so does the imported file"
                        .to_string(),
                ),
            }
        }
        if let Some(affinity) = self.get("routing.session-affinity").and_then(boolean) {
            config.routing.session_affinity = affinity;
        }
        if let Some(ttl) = self.get("routing.session-affinity-ttl").and_then(text) {
            let secs = match go_duration_secs(&ttl) {
                Some(secs) if secs > 0.0 => secs.ceil().clamp(1.0, 315_360_000.0) as u64,
                // Unparseable or not positive means one hour there.
                _ => 3600,
            };
            config.routing.session_affinity_ttl_secs = secs;
        }
        if let Some(force) = self.pick_bool("routing.force-model-prefix", "force-model-prefix") {
            config.routing.force_model_prefix = force;
        }
        if let Some(retry) = self.pick_int("routing.retry.request-retry", "request-retry") {
            // Extra rounds there; attempts in total here.
            config.routing.max_attempts = clamp_u32(retry).saturating_add(1).max(1);
        }
        if let Some(wait) = self.pick_int("routing.retry.max-retry-interval", "max-retry-interval")
        {
            config.routing.max_wait_secs = clamp_u64(wait);
        }
        if let Some(disabled) =
            self.pick_bool("routing.cooldown.disable-cooling", "disable-cooling")
        {
            config.routing.cooldown.enabled = !disabled;
        }
        if let Some(secs) = self.pick_int(
            "routing.cooldown.transient-error-cooldown-seconds",
            "transient-error-cooldown-seconds",
        ) {
            // 0 means the built-in minute there, a negative number none.
            if secs != 0 {
                config.routing.cooldown.transient_secs = clamp_u64(secs);
            }
        }
    }

    pub(super) fn requests(&mut self, config: &mut Config) {
        if let Some(proxy) = self.pick_text("requests.proxy-url", "proxy-url") {
            match parse_proxy(&proxy) {
                Ok(_) => config.upstream.proxy = proxy,
                Err(_) => self
                    .not_imported
                    .push("proxy-url: not a proxy URL Switchyard accepts".to_string()),
            }
        }
        if let Some(passthrough) =
            self.pick_bool("requests.passthrough-headers", "passthrough-headers")
        {
            config.upstream.passthrough_headers = passthrough;
        }
        if let Some(secs) = self.pick_int(
            "requests.streaming.keepalive-seconds",
            "streaming.keepalive-seconds",
        ) {
            config.streaming.keepalive_secs = clamp_u64(secs);
        }
        if let Some(retries) = self.pick_int(
            "requests.streaming.bootstrap-retries",
            "streaming.bootstrap-retries",
        ) {
            config.streaming.bootstrap_retries = clamp_u32(retries);
        }
        if let Some(payload) = self.pick(PAYLOAD_NESTED, PAYLOAD_FLAT) {
            self.payload(payload, self.typed_payload, config);
        }
    }

    pub(super) fn observability(&mut self, config: &mut Config) {
        if self
            .pick_bool("observability.logs.debug", "debug")
            .unwrap_or(false)
        {
            config.logging.level = "debug".to_string();
        }
        if let Some(to_file) =
            self.pick_bool("observability.logs.logging-to-file", "logging-to-file")
        {
            config.logging.file = to_file;
        }
        if let Some(size) = self.pick_int(
            "observability.logs.logs-max-total-size-mb",
            "logs-max-total-size-mb",
        ) {
            config.logging.max_total_size_mb = clamp_u64(size);
        }
        if self
            .pick_bool("observability.logs.request-log", "request-log")
            .unwrap_or(false)
        {
            config.logging.request_log = RequestLogMode::All;
        }
        if let Some(enabled) = self.pick_bool(
            "observability.usage.usage-statistics-enabled",
            "usage-statistics-enabled",
        ) {
            config.usage.enabled = enabled;
        }
    }
}
