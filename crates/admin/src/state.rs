//! What the admin routes share: the gateway, the sign-in bookkeeping and
//! the helpers every mutation goes through.

use crate::AdminOptions;
use crate::auth::{Lockouts, Tickets};
use crate::error::ApiFailure;
use crate::key_usage::KeyLedger;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use switchyard_config_store::ConfigStoreError;
use switchyard_core::Config;
use switchyard_core::config::{ConfigIssue, resolve_secret};
use switchyard_gateway::Gateway;
use switchyard_telemetry::Event;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

/// Longest a handler waits for the gateway to apply a configuration it has
/// just saved. Applying takes milliseconds; the limit only matters when the
/// gateway is shutting down and no longer follows the store.
const APPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// State behind every admin route.
pub(crate) struct AdminState {
    pub gateway: Gateway,
    pub options: AdminOptions,
    pub lockouts: Mutex<Lockouts>,
    pub tickets: Mutex<Tickets>,
    /// Cancelled when the process shuts down: live sockets close.
    pub shutdown: CancellationToken,
    /// Live-event WebSockets currently open.
    pub live_sockets: AtomicUsize,
    /// Usage per client key id, as read from the usage files so far.
    pub key_usage: KeyLedger,
}

pub(crate) type Shared = Arc<AdminState>;

/// The part of the configuration a request body stands for, as a path
/// prefix: `providers[3]` for a provider entry, `auth.keys[0]` for a client
/// key, `aliases` for the alias list. Empty: the body is the configuration
/// itself (the settings patch), or the request has no body.
///
/// An edit sets it as soon as it knows the place — which, for an entry of a
/// list, is only known inside the store's lock.
///
/// An edit may also name settings it put back to their default and that
/// should leave the file rather than be written out ([`Scope::unset`]).
#[derive(Debug, Default)]
pub(crate) struct Scope {
    prefix: String,
    unset: Vec<String>,
}

impl Scope {
    pub fn set(&mut self, prefix: impl Into<String>) {
        self.prefix = prefix.into();
    }

    /// Takes the settings at these dotted paths (`server.port`,
    /// `routing.cooldown`) out of the file, so their default applies — see
    /// [`ConfigStore::update_unsetting`](switchyard_config_store::ConfigStore::update_unsetting).
    pub fn unset(&mut self, paths: Vec<String>) {
        self.unset = paths;
    }
}

/// The admin settings in effect for one request: the live configuration
/// with the environment overrides applied.
#[derive(Clone, Debug)]
pub(crate) struct Access {
    enabled: bool,
    secret: Option<String>,
    pub allow_remote: bool,
    ui: bool,
}

impl Access {
    /// The secret requests must present; `None` when the admin interface is
    /// switched off or has no secret, in which case it does not exist as
    /// far as clients can tell.
    pub fn secret(&self) -> Option<&str> {
        if self.enabled {
            self.secret.as_deref()
        } else {
            None
        }
    }

    /// Whether the dashboard's files are served.
    pub fn serves_dashboard(&self) -> bool {
        self.secret().is_some() && self.ui
    }
}

impl AdminState {
    pub fn new(gateway: Gateway, options: AdminOptions) -> Self {
        AdminState {
            gateway,
            options,
            lockouts: Mutex::new(Lockouts::default()),
            tickets: Mutex::new(Tickets::default()),
            shutdown: CancellationToken::new(),
            live_sockets: AtomicUsize::new(0),
            key_usage: KeyLedger::default(),
        }
    }

    pub fn access(&self) -> Access {
        let config = self.gateway.config();
        let from_env = self
            .options
            .secret_override
            .as_deref()
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
            .map(str::to_string);
        // A reference to a variable that is not set is no secret at all.
        let secret = from_env.or_else(|| {
            resolve_secret(&config.admin.secret)
                .ok()
                .filter(|secret| !secret.is_empty())
        });
        Access {
            enabled: config.admin.enabled,
            secret,
            allow_remote: config.admin.allow_remote || self.options.allow_remote_override,
            ui: config.admin.ui,
        }
    }

    /// Edits the configuration and waits until the gateway runs on the
    /// result.
    ///
    /// `edit` runs inside the store's writer lock on a copy of the live
    /// configuration, so whatever it reads is what it changes: concurrent
    /// edits cannot overwrite each other. Its error is returned as is; a
    /// result that does not validate, or that holds a value the file cannot
    /// (see [`writable`]), is a 422 listing the issues; in every such case
    /// nothing changes.
    ///
    /// `edit` also says, through its [`Scope`], which part of the
    /// configuration the request body is — `providers[3]`, `aliases` — so
    /// that the issues of a 422 name fields by their place in that body,
    /// whoever found them: the edit itself, [`writable`] or the store's
    /// validation.
    pub async fn edit_config<T, F>(&self, edit: F) -> Result<(Arc<Config>, T), ApiFailure>
    where
        T: Send,
        F: FnOnce(&mut Config, &mut Scope) -> Result<T, ApiFailure> + Send,
    {
        let mut scope = Scope::default();
        let result = self.edit_config_scoped(edit, &mut scope).await;
        result.map_err(|failure| failure.relative_to(&scope.prefix))
    }

    async fn edit_config_scoped<T, F>(
        &self,
        edit: F,
        scope: &mut Scope,
    ) -> Result<(Arc<Config>, T), ApiFailure>
    where
        T: Send,
        F: FnOnce(&mut Config, &mut Scope) -> Result<T, ApiFailure> + Send,
    {
        // Subscribed before the edit so its announcement cannot be missed.
        let events = self.gateway.telemetry().subscribe();
        let mut output = None;
        let mut failure = None;
        let mut changed = false;
        let result = self
            .gateway
            .config_store()
            .update_unsetting(|config| {
                let before = config.clone();
                let outcome = edit(config, scope).and_then(|value| {
                    changed = *config != before;
                    if changed {
                        writable(config)?;
                        // As for the raw editor: a result that breaks a rule
                        // is refused by the store with its issues (the same
                        // wording on every route), and only a valid one is
                        // checked for what it would do.
                        if config.validate().is_empty() {
                            self.stays_reachable(config)?;
                        }
                    }
                    Ok(value)
                });
                match outcome {
                    Ok(value) => {
                        output = Some(value);
                        Ok(std::mem::take(&mut scope.unset))
                    }
                    Err(error) => {
                        let message = error.message.clone();
                        failure = Some(error);
                        Err(message)
                    }
                }
            })
            .await;
        match result {
            Ok(config) => {
                if changed {
                    self.wait_applied(events).await;
                }
                match output {
                    Some(value) => Ok((config, value)),
                    None => Err(ApiFailure::internal("the edit produced no result")),
                }
            }
            Err(ConfigStoreError::Edit(message)) => Err(failure.unwrap_or_else(|| {
                ApiFailure::internal(format!("the configuration could not be written: {message}"))
            })),
            Err(error) => Err(error.into()),
        }
    }

    /// Refuses a configuration that would switch off the admin interface
    /// this very request came through: `admin.enabled = false`, or no admin
    /// secret left (and none from the environment). The request would be
    /// answered and every one after it would be a 404, with no way back
    /// from the dashboard — an empty text pasted into the raw editor does
    /// exactly that. Turning the admin interface off stays possible where it
    /// cannot happen by accident: in the configuration file itself.
    fn stays_reachable(&self, config: &Config) -> Result<(), ApiFailure> {
        let issues = self.lockout_issues(config);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ApiFailure::invalid_config(issues))
        }
    }

    /// What would lock the dashboard out (see
    /// [`stays_reachable`](Self::stays_reachable)), as issues: empty when
    /// the admin interface stays on with a secret. For a configuration that
    /// already passed validation; `POST /config/validate` lists these the
    /// way `PUT /config/raw` refuses them.
    pub fn lockout_issues(&self, config: &Config) -> Vec<ConfigIssue> {
        const WAY_OUT: &str = "keep an admin secret, or edit the configuration file itself to \
                               switch the admin interface off";
        let mut issues = Vec::new();
        if !config.admin.enabled {
            issues.push(ConfigIssue {
                path: "admin.enabled".to_string(),
                message: "turning this off here would lock the dashboard out; to switch the \
                          admin interface off, edit the configuration file itself"
                    .to_string(),
            });
        }
        let from_env = self
            .options
            .secret_override
            .as_deref()
            .is_some_and(|secret| !secret.trim().is_empty());
        if !from_env {
            let message = match resolve_secret(&config.admin.secret) {
                Ok(secret) if !secret.is_empty() => None,
                Ok(_) => Some(format!(
                    "is missing: saving this would leave the admin interface without a secret \
                     and lock the dashboard out; {WAY_OUT}"
                )),
                // A reference names a variable, not a secret: it may be
                // shown.
                Err(variable) => Some(format!(
                    "names the environment variable `{variable}`, which is not set (or empty) \
                     for the gateway: saving this would leave the admin interface without a \
                     secret and lock the dashboard out; set the variable and restart first, \
                     or {WAY_OUT}"
                )),
            };
            if let Some(message) = message {
                issues.push(ConfigIssue {
                    path: "admin.secret".to_string(),
                    message,
                });
            }
        }
        issues
    }

    /// Replaces the whole file (the raw editor) and waits for the gateway.
    pub async fn replace_config_text(&self, text: &str) -> Result<Arc<Config>, ApiFailure> {
        // A text that does not validate is refused by the store below, with
        // its issues; only a valid one can be checked for what it would do.
        // `POST /config/validate` follows the same order.
        if let Ok(config) = switchyard_config_store::validate_text(text) {
            self.stays_reachable(&config)?;
        }
        let events = self.gateway.telemetry().subscribe();
        let config = self.gateway.config_store().replace_text(text).await?;
        self.wait_applied(events).await;
        Ok(config)
    }

    /// Re-reads the file and waits for the gateway.
    pub async fn reload_config(&self) -> Result<Arc<Config>, ApiFailure> {
        let events = self.gateway.telemetry().subscribe();
        let config = self.gateway.config_store().reload_from_disk().await?;
        self.wait_applied(events).await;
        Ok(config)
    }

    /// Waits until the gateway has applied the store's current
    /// configuration: scheduler rebuilt, client keys replaced, telemetry
    /// reconfigured. The gateway announces that with `config.reloaded` once
    /// all of it is done.
    ///
    /// The store publishes changes through a `watch` channel, so two quick
    /// edits may be applied in one go; what is waited for is therefore "the
    /// scheduler runs on what the store holds now", which covers this edit
    /// and any that followed it.
    async fn wait_applied(&self, mut events: broadcast::Receiver<Event>) {
        let applied =
            || *self.gateway.scheduler().config() == *self.gateway.config_store().current();
        let wait = async {
            loop {
                match events.recv().await {
                    Ok(Event::ConfigReloaded { ok: true, .. }) | Err(RecvError::Lagged(_)) => {
                        if applied() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        };
        if tokio::time::timeout(APPLY_TIMEOUT, wait).await.is_err() && !applied() {
            tracing::warn!(
                "the gateway did not confirm the new configuration in time; \
                 the response may describe the previous one"
            );
        }
    }
}

/// Refuses a configuration that holds a value its file cannot: 422 naming
/// every such field.
///
/// The configuration lives in a TOML file, and two things a JSON request
/// can say have no place there: `null` (TOML has no null — it can turn up
/// wherever the configuration keeps free-form JSON, such as the values a
/// payload rule sets) and an integer above the signed 64-bit range (every
/// unsigned setting accepts one as far as its type goes). The store would
/// refuse to write either, but with a bare message and no field, which the
/// caller could only report as its own failure; the mistake is the
/// request's, so it is found here first and reported like any other value
/// the configuration cannot hold. Problems validation finds are listed
/// along with it, so one round trip shows everything.
fn writable(config: &Config) -> Result<(), ApiFailure> {
    let unwritable = unwritable_values(config);
    if unwritable.is_empty() {
        return Ok(());
    }
    let mut issues = config.validate();
    issues.extend(unwritable);
    Err(ApiFailure::invalid_config(issues))
}

/// Every value of `config` that TOML cannot express, by its place in the
/// configuration (`payload.override[0].set.response_format`).
fn unwritable_values(config: &Config) -> Vec<ConfigIssue> {
    fn walk(value: &Value, path: &mut String, issues: &mut Vec<ConfigIssue>) {
        let mut issue = |path: &str, message: &str| {
            issues.push(ConfigIssue {
                path: path.to_string(),
                message: message.to_string(),
            });
        };
        match value {
            Value::Null => issue(
                path,
                "is null, which a TOML file cannot hold; leave the field out instead",
            ),
            Value::Number(number) if number.as_i64().is_none() && number.is_u64() => issue(
                path,
                "is larger than 9223372036854775807, the largest integer a TOML file can hold",
            ),
            Value::Array(items) => {
                let length = path.len();
                for (index, item) in items.iter().enumerate() {
                    path.push_str(&format!("[{index}]"));
                    walk(item, path, issues);
                    path.truncate(length);
                }
            }
            Value::Object(fields) => {
                let length = path.len();
                for (key, item) in fields {
                    if length > 0 {
                        path.push('.');
                    }
                    path.push_str(key);
                    walk(item, path, issues);
                    path.truncate(length);
                }
            }
            Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    let mut issues = Vec::new();
    // A configuration that does not serialise at all is the store's to
    // report.
    if let Ok(tree) = serde_json::to_value(config) {
        walk(&tree, &mut String::new(), &mut issues);
    }
    issues
}

/// Runs blocking work (file reads, scans of the usage store) off the async
/// threads.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, ApiFailure>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        tracing::error!(%error, "an admin background task failed");
        ApiFailure::internal("the request could not be completed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_config_store::validate_text;

    /// A configuration that leaves every optional field out, the way a file
    /// may: none of them must show up as a null, or every edit of such a
    /// file would be refused.
    const SPARSE: &str = r#"
[admin]
secret = "s"

[server]
port = 8317

[[auth.keys]]
key = "sy-0123456789abcdef0123456789abcdef"
name = "laptop"

[[providers]]
name = "one"
kind = "openai"
api_keys = ["sk-0123456789abcdef"]
[[providers.credentials]]
api_key = "sk-fedcba9876543210"
[[providers.models]]
id = "gpt-test"

[[pricing]]
model = "gpt-*"
input = 1.5
output = 2.0

[[aliases]]
name = "fast"
targets = ["gpt-test"]

[[payload.override]]
models = ["*"]
set = { "temperature" = 0.5, "nested" = { "list" = [1, "two", { "three" = 3 }] } }

[[payload.filter]]
models = ["*"]
remove = ["user"]
"#;

    #[test]
    fn what_a_file_can_say_is_writable() {
        assert_eq!(unwritable_values(&Config::default()), []);
        let config = validate_text(SPARSE).unwrap();
        assert_eq!(unwritable_values(&config), []);
        assert!(writable(&config).is_ok());
    }

    #[test]
    fn nulls_and_oversized_integers_are_named_by_their_place() {
        let mut config = validate_text(SPARSE).unwrap();
        config.payload = serde_json::from_value(json!({
            "default": [{"models": ["*"], "set": {"seed": u64::MAX}}],
            "override": [
                {"models": ["a"], "set": {"fine": 1}},
                {"models": ["b"], "set": {
                    "response_format": null,
                    "reasoning": {"effort": null},
                    "stop": ["END", null],
                    "largest": i64::MAX,
                    "negative": i64::MIN,
                    "float": 1e300,
                }},
            ],
        }))
        .unwrap();
        config.routing.max_wait_secs = u64::MAX;
        config.server.body_limit_mb = (i64::MAX as u64) + 1;

        let issues = unwritable_values(&config);
        let paths: Vec<&str> = issues.iter().map(|issue| issue.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "server.body_limit_mb",
                "routing.max_wait_secs",
                "payload.default[0].set.seed",
                "payload.override[1].set.response_format",
                "payload.override[1].set.reasoning.effort",
                "payload.override[1].set.stop[1]",
            ]
        );
        assert!(issues[0].message.contains("largest integer"), "{issues:?}");
        assert!(issues[3].message.contains("null"), "{issues:?}");

        let refused = writable(&config).unwrap_err();
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.issues.len(), 6);
    }

    #[test]
    fn a_refusal_lists_what_validation_finds_as_well() {
        let mut config = validate_text(SPARSE).unwrap();
        config.server.body_limit_mb = 0;
        config.routing.max_wait_secs = u64::MAX;
        let refused = writable(&config).unwrap_err();
        let paths: Vec<&str> = refused.issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["server.body_limit_mb", "routing.max_wait_secs"]);
    }
}
