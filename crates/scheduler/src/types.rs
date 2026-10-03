//! Public data types of the scheduler API.

use indexmap::IndexMap;
use serde::Serialize;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;
use switchyard_core::config::{ProviderConfig, ProviderKind};
use switchyard_core::util::mask_secret;
use switchyard_core::{Depth, FailureClass, ModelInfo, Protocol, Quirks, UpstreamError};

/// Stable identifier of a credential:
/// `<provider>:<first 12 hex of sha256(provider, kind, key material, base url)>`,
/// with `-1`, `-2`, … appended when the same tuple is configured twice. It
/// never contains the key itself and survives edits to models, weight,
/// priority, label and proxy.
pub type CredentialId = String;

/// Everything needed to reach one credential's upstream. Handed to the
/// transport layer; **contains the secret**, so it is not serialisable and
/// its `Debug` output masks the key.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialView {
    pub id: CredentialId,
    /// Name of the provider entry the credential belongs to.
    pub provider: String,
    pub kind: ProviderKind,
    /// Display label: the configured one, else a masked key, else the
    /// service-account file name.
    pub label: String,
    /// The resolved API key. Empty for keyless and service-account
    /// credentials (and for credentials whose secret could not be resolved).
    pub api_key: String,
    /// Path of the Google service-account key file as written in the config
    /// (relative to the config file's directory). Empty when unused.
    pub service_account_file: String,
    /// Effective endpoint root, without a trailing slash.
    pub base_url: String,
    /// Effective outbound proxy setting in `config::parse_proxy` syntax:
    /// the credential's own, else the provider's, else `upstream.proxy`.
    pub proxy: String,
    /// Extra headers for every upstream request (provider `headers`).
    pub headers: IndexMap<String, String>,
    /// `vertex` only: Google Cloud project id (may be empty).
    pub project: String,
    /// `vertex` only: region or `global` (may be empty).
    pub location: String,
}

impl fmt::Debug for CredentialView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialView")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("api_key", &mask_secret(&self.api_key))
            .field("service_account_file", &self.service_account_file)
            .field("base_url", &redact_userinfo(&self.base_url))
            .field("proxy", &redact_userinfo(&self.proxy))
            // Header values may be secrets (extra auth headers).
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("project", &self.project)
            .field("location", &self.location)
            .finish()
    }
}

/// Hides the user-info part of a URL for display:
/// `socks5://user:pw@host:1080` becomes `socks5://***@host:1080`.
fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://***@{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// One way to serve a client-facing model: a provider and the model id to
/// send it.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteRef {
    /// Provider entry name.
    pub provider: String,
    pub kind: ProviderKind,
    /// Model id to send upstream.
    pub upstream_model: String,
    /// Model metadata for this route: catalog entry for the upstream id,
    /// overlaid with discovered metadata and the provider's model config.
    pub info: Arc<ModelInfo>,
}

/// A client model name resolved against the registry.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// The name as the client wrote it (trimmed), suffix included.
    pub requested: String,
    /// The registered client-facing name the request matched, in the
    /// registry's spelling, without the reasoning suffix.
    pub base: String,
    /// Reasoning depth asked for by a `model(suffix)` in the request.
    pub suffix_depth: Option<Depth>,
    /// What can serve the request, in order of preference. A plain model has
    /// one target; an alias has one per configured target.
    pub targets: Vec<ResolvedTarget>,
}

impl Resolved {
    /// The reasoning depth that applies when `lease` serves this request: a
    /// depth pinned by the alias target beats the client's own suffix.
    pub fn depth_for(&self, lease: &Lease) -> Option<Depth> {
        lease.pinned_depth.or(self.suffix_depth)
    }

    /// Keeps only the routes for which `keep` returns true and drops targets
    /// left without any. Use it to restrict a request to certain providers
    /// (for example `openai`-kind ones for the realtime relay).
    pub fn retain_routes(&mut self, mut keep: impl FnMut(&RouteRef) -> bool) {
        for target in &mut self.targets {
            target.routes.retain(|r| keep(r));
        }
        self.targets.retain(|t| !t.routes.is_empty());
    }

    /// Whether any route remains.
    pub fn has_routes(&self) -> bool {
        self.targets.iter().any(|t| !t.routes.is_empty())
    }
}

/// One alternative of a [`Resolved`] request.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTarget {
    /// Client-facing name of the model this target stands for. Equal to
    /// [`Resolved::base`] unless the request named an alias.
    pub client_model: String,
    /// Reasoning depth pinned by the alias target (`"gpt-5(high)"`).
    pub pinned_depth: Option<Depth>,
    /// Providers that serve the model, in config order.
    pub routes: Vec<RouteRef>,
}

/// Input of [`crate::Scheduler::pick`].
#[derive(Clone, Copy, Debug)]
pub struct PickRequest<'a> {
    pub resolved: &'a Resolved,
    /// Credentials already attempted (and failed) for this client request:
    /// `lease.credential.id` of every earlier attempt, each reported with
    /// [`crate::Scheduler::report`] before the next pick. A tried credential
    /// is not offered again for the model it failed on; for an alias it may
    /// still serve the other targets (see [`crate::Scheduler::pick`]).
    pub tried: &'a [CredentialId],
    /// Conversation key for session affinity, when the client supplied one.
    pub session: Option<&'a str>,
    /// Protocol the client speaks; decides [`Lease::upstream_protocol`].
    pub client_protocol: Protocol,
    /// Current time (see [`crate::Scheduler::now`]).
    pub now: SystemTime,
}

/// A credential chosen for one upstream attempt. Report what happened with
/// [`crate::Scheduler::report`].
#[derive(Clone)]
pub struct Lease {
    pub credential: CredentialView,
    /// Model id to send upstream.
    pub upstream_model: String,
    /// Client-facing name of the model being served: the alias target's name
    /// for alias requests, otherwise the requested model's registered name.
    pub client_model: String,
    /// Metadata of the served model; `id` is [`Lease::client_model`].
    pub info: ModelInfo,
    /// Reasoning depth pinned by the alias target, if any.
    pub pinned_depth: Option<Depth>,
    /// Protocols the provider can be spoken to in, most preferred first.
    pub protocols: Vec<Protocol>,
    /// Protocol to use for this attempt: the client's when the provider
    /// supports it, else the provider's first.
    pub upstream_protocol: Protocol,
    pub quirks: Quirks,
    pub provider_config: Arc<ProviderConfig>,
    /// Session-affinity key the pick was made under, so the outcome can
    /// refresh or drop the binding.
    pub(crate) affinity_key: Option<String>,
}

impl fmt::Debug for Lease {
    // `ProviderConfig` holds the raw key list, so it is not printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("credential", &self.credential)
            .field("upstream_model", &self.upstream_model)
            .field("client_model", &self.client_model)
            .field("pinned_depth", &self.pinned_depth)
            .field("protocols", &self.protocols)
            .field("upstream_protocol", &self.upstream_protocol)
            .field("quirks", &self.quirks)
            .finish_non_exhaustive()
    }
}

/// Result of one upstream attempt.
#[derive(Clone, Copy, Debug)]
pub enum Outcome<'a> {
    /// The upstream answered successfully. `latency_ms` is the time to the
    /// first byte (or to the full response for non-streaming calls).
    Success {
        latency_ms: u64,
    },
    Failure(&'a UpstreamError),
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// Runtime view of one provider, for the admin API.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderSnapshot {
    pub name: String,
    pub kind: ProviderKind,
    pub enabled: bool,
    pub credentials: Vec<CredentialSnapshot>,
    /// Number of models the provider serves (after `exclude`).
    pub models: usize,
}

/// Runtime view of one credential. Never contains the secret.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CredentialSnapshot {
    pub id: CredentialId,
    pub label: String,
    /// Masked key, the secret reference as written (`env:NAME`) when it
    /// could not be resolved, the service-account file name, or empty.
    pub masked_key: String,
    /// The credential itself is switched off, in the config or at runtime.
    /// (A credential of a provider that is switched off keeps `false` here
    /// unless it is switched off too; its `status` is `disabled` and
    /// `disabled_by` says `provider`.)
    pub disabled: bool,
    /// What takes the credential out of rotation, present exactly when
    /// `status` is `disabled`: `provider` (its provider is switched off),
    /// `credential` (switched off in the config) or `runtime` (switched off
    /// with [`crate::Scheduler::set_runtime_disabled`]). When several apply,
    /// the first of that order is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_by: Option<DisabledBy>,
    /// False when the credential cannot be used at all (see
    /// `unusable_reason`).
    pub usable: bool,
    /// Why not: an unset environment variable or a missing key, found when
    /// the configuration was read, or the reason the gateway gave with
    /// [`crate::Scheduler::set_unusable`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unusable_reason: Option<String>,
    /// `ready`, `cooling`, `disabled` or `unusable`. `cooling` means the
    /// whole credential is resting, or every model of its provider is.
    /// `disabled` covers the credentials of a switched-off provider, which
    /// are therefore never `ready`.
    pub status: CredentialStatus,
    /// End of the cooldown that makes the credential `cooling`, unix ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<FailureClass>,
    /// Active per-model cooldowns, sorted by model.
    pub model_cooldowns: Vec<ModelCooldown>,
    /// Upstream attempts made with this credential.
    pub requests: u64,
    pub successes: u64,
    /// Failed attempts, not counting request faults (which are the client's).
    pub failures: u64,
    pub consecutive_failures: u32,
    /// Exponentially weighted moving average of the response latency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Unix ms of the last attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,
    pub weight: u32,
    pub priority: i32,
}

impl CredentialSnapshot {
    /// Whether the credential belongs to a provider that is switched off.
    /// Such a credential is configured but not part of the gateway's
    /// rotation: counts of credentials (and of ready ones) leave it out.
    pub fn provider_disabled(&self) -> bool {
        self.disabled_by == Some(DisabledBy::Provider)
    }
}

/// What switched a credential off. See [`CredentialSnapshot::disabled_by`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DisabledBy {
    /// The provider it belongs to is disabled (`enabled = false`).
    Provider,
    /// The credential is disabled in the configuration.
    Credential,
    /// The credential was disabled at runtime; the configuration still has
    /// it enabled.
    Runtime,
}

impl DisabledBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            DisabledBy::Provider => "provider",
            DisabledBy::Credential => "credential",
            DisabledBy::Runtime => "runtime",
        }
    }
}

/// Coarse credential state shown in the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStatus {
    Ready,
    Cooling,
    Disabled,
    Unusable,
}

impl CredentialStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            CredentialStatus::Ready => "ready",
            CredentialStatus::Cooling => "cooling",
            CredentialStatus::Disabled => "disabled",
            CredentialStatus::Unusable => "unusable",
        }
    }
}

/// One model resting on one credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelCooldown {
    /// Upstream model id.
    pub model: String,
    /// Unix ms.
    pub until: i64,
    pub reason: FailureClass,
}

/// The most recent upstream failure of a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LastError {
    /// HTTP status, `0` when no response arrived.
    pub status: u16,
    pub class: FailureClass,
    /// Short excerpt of the upstream's message.
    pub message: String,
    /// Unix ms.
    pub at: i64,
    /// Upstream model id the failed attempt was for.
    pub model: String,
}

impl LastError {
    /// `"429 rate limit exceeded"`-style one-liner.
    pub fn summary(&self) -> String {
        if self.message.is_empty() {
            self.status.to_string()
        } else {
            format!("{} {}", self.status, self.message)
        }
    }
}

/// One row of the client-facing model table.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelEntry {
    /// Client-facing name.
    pub name: String,
    /// Metadata shown in listings (`id` equals `name`).
    pub info: ModelInfo,
    /// Hidden from listings by an alias with `hide_targets`; still routable.
    pub hidden: bool,
    /// An alias none of whose targets is routable. The gateway ignores it:
    /// the name is unknown to clients, absent from listings and not counted
    /// as a model ([`crate::Scheduler::models_routable`]). It is in the
    /// table only so that the mistake can be seen. Always false for models.
    pub ignored: bool,
    /// For aliases: whether the name equals, ignoring case, the name of a
    /// model a provider serves — a model the alias hides (also when it
    /// targets that model). Spelled exactly alike, the alias takes the name
    /// over in lookups and listings; spelled differently, each exact
    /// spelling reaches its own entry and other spellings reach the alias.
    /// False for an ignored alias (which hides nothing) and for models.
    pub shadows_model: bool,
    /// For aliases: the targets as configured (`"gpt-5(high)"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_targets: Option<Vec<String>>,
    /// For aliases: the routes of every target, in target order (see
    /// [`ModelRoute::target`]).
    pub routes: Vec<ModelRoute>,
}

/// One provider behind a [`ModelEntry`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelRoute {
    pub provider: String,
    pub upstream_model: String,
    /// For the routes of an alias: the alias target the route belongs to,
    /// as written in the configuration, with the reasoning suffix it pins
    /// (`"gpt-5(high)"`). For a target that is itself an alias, the routes
    /// of what it expands to carry that target. Absent for models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The priority tier this route competes in: the highest effective
    /// priority (the credential's own, else the provider's) among the
    /// credentials that could serve the model right now; when none can,
    /// among those that could be selected at all; when there is none of
    /// those either, among all of the provider's credentials; and the
    /// provider's priority when it has no credentials. Of the routes of one
    /// model (or one alias target) that have a credential available, those
    /// with the highest priority take the requests.
    pub priority: i32,
    /// Credentials configured on the provider.
    pub credentials_total: usize,
    /// Of those, the ones that could serve this model right now: enabled,
    /// usable and not resting.
    pub credentials_available: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_hidden_in_urls() {
        assert_eq!(
            redact_userinfo("socks5://user:p%40ss@127.0.0.1:1080"),
            "socks5://***@127.0.0.1:1080"
        );
        assert_eq!(
            redact_userinfo("https://key@api.example.com/v1?x=a@b"),
            "https://***@api.example.com/v1?x=a@b"
        );
        for plain in [
            "https://api.openai.com/v1",
            "http://localhost:11434/v1/path@with-at",
            "direct",
            "",
            "mock://local",
        ] {
            assert_eq!(redact_userinfo(plain), plain);
        }
    }

    #[test]
    fn credential_debug_masks_key_proxy_password_and_header_values() {
        let view = CredentialView {
            id: "p:0123456789ab".into(),
            provider: "p".into(),
            kind: ProviderKind::Openai,
            label: "main".into(),
            api_key: "sk-proj-abcdefghijklmnopqrstuvwxyz".into(),
            service_account_file: String::new(),
            base_url: "https://api.openai.com/v1".into(),
            proxy: "http://corp:hunter2secret@proxy.test:3128".into(),
            headers: IndexMap::from([("X-Extra-Auth".to_string(), "tok-12345".to_string())]),
            project: String::new(),
            location: String::new(),
        };
        let printed = format!("{view:?}");
        assert!(!printed.contains("abcdefghijklmnopqrstuv"), "{printed}");
        assert!(!printed.contains("hunter2secret"), "{printed}");
        assert!(!printed.contains("tok-12345"), "{printed}");
        assert!(printed.contains("sk-pro…wxyz"));
        assert!(printed.contains("http://***@proxy.test:3128"));
        assert!(printed.contains("X-Extra-Auth"));
    }

    #[test]
    fn last_error_summary() {
        let mut e = LastError {
            status: 429,
            class: FailureClass::RateLimit,
            message: "slow down".into(),
            at: 1,
            model: "m".into(),
        };
        assert_eq!(e.summary(), "429 slow down");
        e.message.clear();
        assert_eq!(e.summary(), "429");
        assert_eq!(CredentialStatus::Cooling.as_str(), "cooling");
        assert_eq!(
            serde_json::to_string(&CredentialStatus::Unusable).unwrap(),
            "\"unusable\""
        );
    }
}
