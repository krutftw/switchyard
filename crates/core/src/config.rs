//! Configuration schema (`switchyard.toml`).
//!
//! The file is the single source of truth: humans edit it, the admin API edits
//! it (preserving comments), and the running gateway hot-reloads it. Every
//! field has a default, so an empty file is a valid configuration.
//!
//! Secrets (API keys, the admin secret) may be written literally or as a
//! reference to an environment variable — `env:NAME` or `${NAME}` — which is
//! resolved when the value is used (see [`resolve_secret`]).

use crate::model::ModelInfo;
use crate::protocol::Protocol;
use crate::reasoning::ThinkingSupport;
use crate::util::wildcard_match;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Default listen port.
pub const DEFAULT_PORT: u16 = 8317;

/// Root of the configuration file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub admin: AdminConfig,
    pub auth: AuthConfig,
    pub routing: RoutingConfig,
    pub streaming: StreamingConfig,
    pub upstream: UpstreamConfig,
    pub logging: LoggingConfig,
    pub usage: UsageConfig,
    /// Upstream providers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderConfig>,
    /// Virtual models: a client-facing name that routes to one or more real
    /// models.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<AliasConfig>,
    /// Rules that patch upstream request bodies.
    #[serde(skip_serializing_if = "PayloadConfig::is_empty")]
    pub payload: PayloadConfig,
    /// Token prices used for cost estimates. First matching entry wins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pricing: Vec<PriceConfig>,
}

// ---------------------------------------------------------------------------
// [server]
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to bind. Use `0.0.0.0` (or `::`) to accept remote connections.
    pub host: String,
    pub port: u16,
    /// Largest accepted request body, in MiB.
    pub body_limit_mb: u64,
    /// Answer CORS preflights and add permissive CORS headers on the client
    /// API, so browser apps can call the gateway directly.
    pub cors: bool,
    /// Directory for state the gateway writes: usage history, request logs.
    /// Relative paths are resolved against the config file's directory.
    pub data_dir: String,
    /// Serve HTTPS when both `cert` and `key` are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            body_limit_mb: 64,
            cors: true,
            data_dir: "data".to_string(),
            tls: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain file.
    pub cert: String,
    /// PEM private key file.
    pub key: String,
}

// ---------------------------------------------------------------------------
// [admin]
// ---------------------------------------------------------------------------

/// The admin API (`/admin/api/…`) and the dashboard (`/admin/`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Master switch for the admin API and dashboard.
    pub enabled: bool,
    /// Secret required by the admin API (secret reference allowed). The
    /// `SWITCHYARD_ADMIN_SECRET` environment variable overrides it. With no
    /// secret from either source the admin API answers 404.
    pub secret: String,
    /// Accept admin requests from non-loopback addresses.
    pub allow_remote: bool,
    /// Serve the embedded dashboard.
    pub ui: bool,
}

impl Default for AdminConfig {
    fn default() -> Self {
        AdminConfig {
            enabled: true,
            secret: String::new(),
            allow_remote: false,
            ui: true,
        }
    }
}

// ---------------------------------------------------------------------------
// [auth]
// ---------------------------------------------------------------------------

/// Client authentication for the model API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// When false, requests without a key are accepted. Intended for
    /// loopback-only setups.
    pub required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ClientKey>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            required: true,
            keys: Vec::new(),
        }
    }
}

/// A key that clients present to use the gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientKey {
    /// The key itself (secret reference allowed).
    pub key: String,
    /// Label shown in the dashboard and usage reports.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Wildcard patterns of models this key may use. Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Requests per minute allowed for this key. `None` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_rpm: Option<u32>,
}

impl ClientKey {
    /// Whether this key may call `model` (client-facing name, suffix removed).
    pub fn allows_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|p| wildcard_match(p, model))
    }
}

// ---------------------------------------------------------------------------
// [routing]
// ---------------------------------------------------------------------------

/// How a credential is chosen among those that can serve a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Rotate through credentials in turn.
    #[default]
    RoundRobin,
    /// Always use the first available credential; move on only when it is
    /// cooling down. Keeps prompt caches warm and drains quotas one by one.
    FillFirst,
    /// Rotate in proportion to each credential's `weight`.
    Weighted,
    /// Prefer the credential with the lowest recent latency.
    LeastLatency,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub strategy: Strategy,
    /// Keep a conversation on the credential that served it last, so provider
    /// prompt caches stay effective.
    pub session_affinity: bool,
    /// How long an idle conversation stays pinned.
    pub session_affinity_ttl_secs: u64,
    /// When a provider has a `prefix`, serve its models only as
    /// `prefix/model`, not also under the bare name.
    pub force_model_prefix: bool,
    /// Upper bound on upstream attempts for one request, across credentials.
    pub max_attempts: u32,
    /// If every candidate credential is cooling down, wait up to this long for
    /// the soonest one to recover instead of failing immediately. `0` never
    /// waits.
    pub max_wait_secs: u64,
    pub cooldown: CooldownConfig,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        RoutingConfig {
            strategy: Strategy::RoundRobin,
            session_affinity: true,
            session_affinity_ttl_secs: 3600,
            force_model_prefix: false,
            max_attempts: 3,
            max_wait_secs: 0,
            cooldown: CooldownConfig::default(),
        }
    }
}

/// How long a credential is rested after a failure. For rate limits and
/// transient errors a wait requested by the upstream (`Retry-After`) replaces
/// these numbers; for quota and authentication failures the longer of the two
/// is used, so a rejected or exhausted credential is not hammered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CooldownConfig {
    /// Master switch. When false failed credentials stay in rotation.
    pub enabled: bool,
    /// First rate-limit (429) cooldown; doubles on each consecutive failure.
    pub rate_limit_base_secs: u64,
    /// Cap for the exponential rate-limit backoff.
    pub rate_limit_max_secs: u64,
    /// Cooldown after 5xx responses and transport errors.
    pub transient_secs: u64,
    /// Cooldown after the upstream rejects the credential (401/403).
    pub auth_secs: u64,
    /// Cooldown after an out-of-quota / billing failure (402, quota 429).
    pub quota_secs: u64,
    /// Cooldown of one model on one credential after a 404 for that model.
    pub model_not_found_secs: u64,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        CooldownConfig {
            enabled: true,
            rate_limit_base_secs: 1,
            rate_limit_max_secs: 1800,
            transient_secs: 60,
            auth_secs: 1800,
            quota_secs: 3600,
            model_not_found_secs: 43_200,
        }
    }
}

// ---------------------------------------------------------------------------
// [streaming]
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Send an SSE comment (or WebSocket ping) after this many seconds of
    /// silence so proxies keep the connection open. `0` disables.
    pub keepalive_secs: u64,
    /// How many times a streaming request may be retried on another
    /// credential when the upstream fails before producing its first event.
    pub bootstrap_retries: u32,
    /// Abort a stream when the upstream sends nothing for this long. `0`
    /// disables.
    pub idle_timeout_secs: u64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        StreamingConfig {
            keepalive_secs: 15,
            bootstrap_retries: 2,
            idle_timeout_secs: 300,
        }
    }
}

// ---------------------------------------------------------------------------
// [upstream]
// ---------------------------------------------------------------------------

/// Defaults for outbound connections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Outbound proxy for all providers: `http://`, `https://`, `socks5://` or
    /// `socks5h://` URL; `direct` to bypass even the proxy environment
    /// variables; empty to use the environment.
    pub proxy: String,
    pub connect_timeout_secs: u64,
    /// Total time allowed for a non-streaming upstream call.
    pub request_timeout_secs: u64,
    /// Forward upstream rate-limit and request-id response headers to clients.
    pub passthrough_headers: bool,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        UpstreamConfig {
            proxy: String::new(),
            connect_timeout_secs: 30,
            request_timeout_secs: 600,
            passthrough_headers: true,
        }
    }
}

// ---------------------------------------------------------------------------
// [logging] / [usage]
// ---------------------------------------------------------------------------

/// Which requests have their bodies captured for inspection in the dashboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestLogMode {
    /// Record metadata only (model, tokens, latency, status).
    #[default]
    Off,
    /// Also keep request and response bodies of failed requests.
    Errors,
    /// Keep bodies of every request.
    All,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `trace`, `debug`, `info`, `warn` or `error`.
    pub level: String,
    /// Also write application logs to rotating files under
    /// `<data_dir>/logs`.
    pub file: bool,
    /// Delete the oldest log files once the log directory exceeds this size.
    /// `0` keeps everything.
    pub max_total_size_mb: u64,
    pub request_log: RequestLogMode,
    /// Captured bodies are truncated to this many KiB each.
    pub request_log_max_body_kb: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            level: "info".to_string(),
            file: false,
            max_total_size_mb: 200,
            request_log: RequestLogMode::Off,
            request_log_max_body_kb: 256,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageConfig {
    /// Record per-request usage and aggregate statistics.
    pub enabled: bool,
    /// Persist usage records under `<data_dir>/usage` so statistics survive
    /// restarts.
    pub persist: bool,
    /// Days of usage history to keep.
    pub retention_days: u32,
}

impl Default for UsageConfig {
    fn default() -> Self {
        UsageConfig {
            enabled: true,
            persist: true,
            retention_days: 30,
        }
    }
}

// ---------------------------------------------------------------------------
// [[providers]]
// ---------------------------------------------------------------------------

/// Kind of upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// api.openai.com (or an Azure OpenAI v1 endpoint): Responses and Chat
    /// Completions.
    Openai,
    /// api.anthropic.com: Messages.
    Anthropic,
    /// generativelanguage.googleapis.com: Gemini API with an API key.
    Gemini,
    /// Google Cloud Vertex AI with a service account or API key.
    Vertex,
    /// Any server exposing OpenAI-compatible Chat Completions (OpenRouter,
    /// Groq, DeepSeek, Ollama, vLLM, LM Studio, …).
    OpenaiCompat,
    /// Built-in fake model that needs no network or key. For demos and tests.
    Mock,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 6] = [
        ProviderKind::Openai,
        ProviderKind::Anthropic,
        ProviderKind::Gemini,
        ProviderKind::Vertex,
        ProviderKind::OpenaiCompat,
        ProviderKind::Mock,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Vertex => "vertex",
            ProviderKind::OpenaiCompat => "openai-compat",
            ProviderKind::Mock => "mock",
        }
    }

    /// Base URL used when the provider entry sets none. `None` means the
    /// entry must supply one.
    pub const fn default_base_url(self) -> Option<&'static str> {
        match self {
            ProviderKind::Openai => Some("https://api.openai.com/v1"),
            ProviderKind::Anthropic => Some("https://api.anthropic.com"),
            ProviderKind::Gemini => Some("https://generativelanguage.googleapis.com"),
            ProviderKind::Vertex => Some("https://aiplatform.googleapis.com"),
            ProviderKind::OpenaiCompat => None,
            ProviderKind::Mock => Some("mock://local"),
        }
    }

    /// Whether credentials of this kind need a secret.
    pub const fn needs_credentials(self) -> bool {
        !matches!(self, ProviderKind::OpenaiCompat | ProviderKind::Mock)
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which OpenAI API an `openai` / `openai-compat` provider is driven through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    /// `openai`: use whichever of Responses / Chat Completions the client
    /// speaks, and Responses for clients of other protocols.
    /// `openai-compat`: Chat Completions.
    #[default]
    Auto,
    /// Always Chat Completions.
    Chat,
    /// Always Responses.
    Responses,
}

impl WireApi {
    fn is_auto(&self) -> bool {
        matches!(self, WireApi::Auto)
    }
}

/// One upstream provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Unique name: lowercase letters, digits, `-` and `_`.
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Endpoint root. Defaults per kind; required for `openai-compat`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// API keys, one credential each (secret references allowed). Shorthand
    /// for `credentials` entries that need no other setting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<String>,
    /// Credentials with per-credential settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialConfig>,
    /// Serve this provider's models as `prefix/model`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// Providers with a higher priority are tried first; lower ones are only
    /// used when every higher one is unavailable.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub priority: i32,
    /// Outbound proxy for this provider (same syntax as `upstream.proxy`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proxy: String,
    /// Extra headers sent on every upstream request.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, String>,
    /// Models this provider serves. When empty the built-in catalog for the
    /// kind is used and, if `discover` is on, the upstream's model list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelConfig>,
    /// Wildcard patterns of model names to hide from this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Ask the upstream for its model list at start-up and on reload when
    /// `models` is empty.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub discover: bool,
    #[serde(default, skip_serializing_if = "WireApi::is_auto")]
    pub wire_api: WireApi,
    /// Chat Completions only: send the legacy `max_tokens` field instead of
    /// `max_completion_tokens`. Defaults to true for `openai-compat` and false
    /// for `openai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_max_tokens: Option<bool>,
    /// Chat Completions only: ask for token usage in streams with
    /// `stream_options: {"include_usage": true}`. Defaults to true; turn off
    /// for servers that reject `stream_options`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_usage: Option<bool>,
    /// `vertex` only: Google Cloud project id. Defaults to the service
    /// account's project.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// `vertex` only: region, or `global`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
}

impl ProviderConfig {
    /// A minimal provider entry.
    pub fn new(name: impl Into<String>, kind: ProviderKind) -> Self {
        ProviderConfig {
            name: name.into(),
            kind,
            enabled: true,
            base_url: String::new(),
            api_keys: Vec::new(),
            credentials: Vec::new(),
            prefix: String::new(),
            priority: 0,
            proxy: String::new(),
            headers: IndexMap::new(),
            models: Vec::new(),
            exclude: Vec::new(),
            discover: true,
            wire_api: WireApi::Auto,
            legacy_max_tokens: None,
            stream_usage: None,
            project: String::new(),
            location: String::new(),
        }
    }

    /// The effective base URL without a trailing slash.
    pub fn effective_base_url(&self) -> String {
        let raw = if self.base_url.trim().is_empty() {
            self.kind.default_base_url().unwrap_or("")
        } else {
            self.base_url.trim()
        };
        raw.trim_end_matches('/').to_string()
    }

    /// The prefix, normalised: trimmed, without leading/trailing slashes. A
    /// prefix containing an inner slash is invalid and treated as empty.
    pub fn normalized_prefix(&self) -> &str {
        let p = self.prefix.trim().trim_matches('/');
        if p.contains('/') { "" } else { p }
    }

    /// Every credential of this provider in a uniform shape: `api_keys`
    /// entries first, then `credentials`. Kinds that need no secret
    /// (`openai-compat` against a local server, `mock`) get one keyless
    /// credential when none is configured.
    pub fn all_credentials(&self) -> Vec<CredentialConfig> {
        let mut out: Vec<CredentialConfig> = self
            .api_keys
            .iter()
            .filter(|k| !k.trim().is_empty())
            .map(|k| CredentialConfig {
                api_key: k.trim().to_string(),
                ..CredentialConfig::default()
            })
            .collect();
        out.extend(self.credentials.iter().cloned());
        if out.is_empty() && !self.kind.needs_credentials() {
            out.push(CredentialConfig::default());
        }
        out
    }

    /// Protocols this provider can be spoken to in, in order of preference
    /// for clients whose own protocol is not among them.
    pub fn protocols(&self) -> Vec<Protocol> {
        match self.kind {
            ProviderKind::Openai => match self.wire_api {
                WireApi::Auto => vec![Protocol::OpenaiResponses, Protocol::OpenaiChat],
                WireApi::Chat => vec![Protocol::OpenaiChat],
                WireApi::Responses => vec![Protocol::OpenaiResponses],
            },
            ProviderKind::OpenaiCompat => match self.wire_api {
                WireApi::Auto | WireApi::Chat => vec![Protocol::OpenaiChat],
                WireApi::Responses => vec![Protocol::OpenaiResponses],
            },
            ProviderKind::Anthropic => vec![Protocol::Anthropic],
            ProviderKind::Gemini | ProviderKind::Vertex => vec![Protocol::Gemini],
            ProviderKind::Mock => Protocol::ALL.to_vec(),
        }
    }

    /// Whether Chat Completions requests to this provider use `max_tokens`.
    pub fn uses_legacy_max_tokens(&self) -> bool {
        self.legacy_max_tokens
            .unwrap_or(matches!(self.kind, ProviderKind::OpenaiCompat))
    }

    /// The protocol quirks of this provider, for [`crate::codec::UpstreamCtx`].
    pub fn quirks(&self) -> crate::codec::Quirks {
        crate::codec::Quirks {
            max_tokens_field: if self.uses_legacy_max_tokens() {
                crate::codec::MaxTokensField::MaxTokens
            } else {
                crate::codec::MaxTokensField::MaxCompletionTokens
            },
            stream_usage: self.stream_usage.unwrap_or(true),
        }
    }

    /// Whether `model` (an upstream id or alias) is hidden by `exclude`.
    pub fn excludes(&self, model: &str) -> bool {
        self.exclude.iter().any(|p| wildcard_match(p.trim(), model))
    }
}

/// One credential of a provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialConfig {
    /// API key (secret reference allowed).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Label shown in the dashboard. Defaults to a masked form of the key.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Relative share of traffic under the `weighted` strategy. `0` removes
    /// the credential from weighted rotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// Overrides the provider's priority for this credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Outbound proxy for this credential.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proxy: String,
    /// `vertex` only: path to a Google service-account JSON key file,
    /// relative to the config file's directory.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service_account_file: String,
}

impl CredentialConfig {
    pub fn effective_weight(&self) -> u32 {
        self.weight.unwrap_or(1)
    }
}

/// A model served by a provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// The upstream's model id.
    pub id: String,
    /// Client-facing name. Defaults to `id`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub alias: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Reasoning support, overriding the built-in catalog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingSupport>,
}

impl ModelConfig {
    /// The name clients use for this model.
    pub fn client_name(&self) -> &str {
        let alias = self.alias.trim();
        if alias.is_empty() {
            self.id.trim()
        } else {
            alias
        }
    }

    /// Applies the explicitly configured fields on top of catalog metadata.
    pub fn apply_to(&self, info: &mut ModelInfo) {
        if !self.display_name.trim().is_empty() {
            info.display_name = Some(self.display_name.trim().to_string());
        }
        if self.context_window.is_some() {
            info.context_window = self.context_window;
        }
        if self.max_output_tokens.is_some() {
            info.max_output_tokens = self.max_output_tokens;
        }
        if let Some(t) = &self.thinking {
            info.thinking = Some(t.clone());
            info.known = true;
        }
    }
}

// ---------------------------------------------------------------------------
// [[aliases]]
// ---------------------------------------------------------------------------

/// A virtual model. Requests for `name` are served by `targets`, tried in
/// order: the next target is used only when no credential can serve the
/// previous one. A target may carry a reasoning suffix (`gpt-5(high)`), which
/// then pins the effort.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasConfig {
    pub name: String,
    pub targets: Vec<String>,
    /// Hide the targets' own names from model listings (they stay routable).
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_targets: bool,
}

// ---------------------------------------------------------------------------
// [payload]
// ---------------------------------------------------------------------------

/// Rules that patch the JSON body sent upstream, applied after translation in
/// this order: `default`, `override`, `filter`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PayloadConfig {
    /// Set a field only when the client's own request did not specify it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub default: Vec<PayloadRule>,
    /// Always set a field.
    #[serde(rename = "override", skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<PayloadRule>,
    /// Remove fields.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<PayloadRule>,
}

impl PayloadConfig {
    pub fn is_empty(&self) -> bool {
        self.default.is_empty() && self.overrides.is_empty() && self.filter.is_empty()
    }
}

/// One payload rule.
///
/// Paths are dot-separated object keys; a numeric segment indexes an array
/// (`messages.0.role`). A literal dot inside a key is written `\.`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PayloadRule {
    /// Wildcard patterns matched against the upstream model id and the
    /// client-requested model name. Required.
    pub models: Vec<String>,
    /// Only apply when the *upstream* request uses this protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// Only apply to this provider (by name).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// `default` / `override` rules: path → JSON value to set.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub set: IndexMap<String, Value>,
    /// `filter` rules: paths to delete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

// ---------------------------------------------------------------------------
// [[pricing]]
// ---------------------------------------------------------------------------

/// Price of a model in USD per million tokens, for cost estimates.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceConfig {
    /// Wildcard pattern matched against the upstream model id.
    pub model: String,
    pub input: f64,
    pub output: f64,
    /// Price of cached input tokens. Defaults to `input`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Price of tokens written to cache. Defaults to `input`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl PriceConfig {
    /// Estimated cost in USD of `usage` at this price.
    pub fn cost(&self, usage: &crate::usage::Usage) -> f64 {
        let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        per(usage.input_tokens, self.input)
            + per(
                usage.cache_read_tokens,
                self.cache_read.unwrap_or(self.input),
            )
            + per(
                usage.cache_write_tokens,
                self.cache_write.unwrap_or(self.input),
            )
            + per(usage.output_tokens, self.output)
    }
}

// ---------------------------------------------------------------------------
// Loading, validation, secrets
// ---------------------------------------------------------------------------

/// A problem found by [`Config::validate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    /// Dotted path of the offending field, e.g. `providers[1].base_url`.
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Why a configuration could not be loaded.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// The text is not valid TOML or does not match the schema.
    #[error("invalid configuration: {0}")]
    Parse(String),
    /// The configuration parsed but is semantically wrong.
    #[error("invalid configuration: {}", format_issues(.0))]
    Invalid(Vec<ConfigIssue>),
}

fn format_issues(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .map(ConfigIssue::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl Config {
    /// Parses and validates a configuration file.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let issues = config.validate();
        if issues.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid(issues))
        }
    }

    /// Serialises the configuration as TOML (comments are not preserved; the
    /// config store uses a format-preserving editor for in-place updates).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Checks semantic rules the type system cannot express. An empty result
    /// means the configuration is usable.
    pub fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        let mut issue = |path: String, message: &str| {
            issues.push(ConfigIssue {
                path,
                message: message.to_string(),
            })
        };

        if self.server.port == 0 {
            issue("server.port".into(), "must be between 1 and 65535");
        }
        if self.server.host.trim().is_empty() {
            issue("server.host".into(), "must not be empty");
        } else if !is_valid_host(self.server.host.trim()) {
            issue(
                "server.host".into(),
                "must be an IP address or a host name, such as 127.0.0.1, 0.0.0.0, :: or localhost",
            );
        }
        if self.server.data_dir.trim().is_empty() {
            issue("server.data_dir".into(), "must not be empty");
        }
        if let Some(message) = empty_reference_issue(&self.admin.secret)
            .or_else(|| header_value_issue(&self.admin.secret))
        {
            issue("admin.secret".into(), message);
        }
        if self.routing.cooldown.rate_limit_max_secs < self.routing.cooldown.rate_limit_base_secs {
            issue(
                "routing.cooldown.rate_limit_max_secs".into(),
                "must not be smaller than rate_limit_base_secs",
            );
        }
        if self.server.body_limit_mb == 0 {
            issue("server.body_limit_mb".into(), "must be at least 1");
        }
        if let Some(tls) = &self.server.tls
            && (tls.cert.trim().is_empty() || tls.key.trim().is_empty())
        {
            issue("server.tls".into(), "both `cert` and `key` are required");
        }
        if self.logging.request_log_max_body_kb == 0 {
            issue(
                "logging.request_log_max_body_kb".into(),
                "must be at least 1",
            );
        }
        if self.usage.retention_days == 0 {
            issue("usage.retention_days".into(), "must be at least 1");
        }
        if !matches!(
            self.logging.level.trim().to_ascii_lowercase().as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            issue(
                "logging.level".into(),
                "must be one of trace, debug, info, warn, error",
            );
        }
        if self.routing.max_attempts == 0 {
            issue("routing.max_attempts".into(), "must be at least 1");
        }

        let mut seen_keys = HashSet::new();
        for (i, key) in self.auth.keys.iter().enumerate() {
            let k = key.key.trim();
            if k.is_empty() {
                issue(format!("auth.keys[{i}].key"), "must not be empty");
            } else if let Some(message) = empty_reference_issue(k) {
                issue(format!("auth.keys[{i}].key"), message);
            } else if !seen_keys.insert(k.to_string()) {
                issue(format!("auth.keys[{i}].key"), "duplicate key");
            }
            if key.rate_limit_rpm == Some(0) {
                issue(
                    format!("auth.keys[{i}].rate_limit_rpm"),
                    "must be at least 1; leave it out for no limit",
                );
            }
        }

        let mut seen_names = HashSet::new();
        for (i, p) in self.providers.iter().enumerate() {
            let path = format!("providers[{i}]");
            let name = p.name.trim();
            if name.is_empty() {
                issue(format!("{path}.name"), "must not be empty");
            } else if !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            {
                issue(
                    format!("{path}.name"),
                    "may only contain lowercase letters, digits, `-` and `_`",
                );
            } else if !seen_names.insert(name.to_string()) {
                issue(format!("{path}.name"), "duplicate provider name");
            }
            let base = p.effective_base_url();
            if base.is_empty() {
                issue(
                    format!("{path}.base_url"),
                    "is required for this provider kind",
                );
            } else if p.kind != ProviderKind::Mock
                && !(base.starts_with("http://") || base.starts_with("https://"))
            {
                issue(
                    format!("{path}.base_url"),
                    "must start with http:// or https://",
                );
            }
            if !p.prefix.trim().is_empty() && p.normalized_prefix().is_empty() {
                issue(
                    format!("{path}.prefix"),
                    "must be a single path segment without `/`",
                );
            }
            for (label, proxy) in [(format!("{path}.proxy"), p.proxy.as_str())] {
                if let Err(msg) = validate_proxy(proxy) {
                    issue(label, msg);
                }
            }
            for (name, value) in &p.headers {
                if !is_valid_header_name(name.trim()) {
                    issue(
                        format!("{path}.headers.{name}"),
                        "is not a valid header name: use letters, digits and `-`, without spaces",
                    );
                } else if value.trim().is_empty() {
                    issue(format!("{path}.headers.{name}"), "needs a value");
                } else if value.chars().any(|c| c.is_control()) {
                    issue(
                        format!("{path}.headers.{name}"),
                        "must not contain control characters or line breaks",
                    );
                }
            }
            let mut seen_secrets = HashSet::new();
            for (j, k) in p.api_keys.iter().enumerate() {
                let k = k.trim();
                if let Some(message) = empty_reference_issue(k) {
                    issue(format!("{path}.api_keys[{j}]"), message);
                } else if !k.is_empty() && !seen_secrets.insert(k.to_string()) {
                    issue(
                        format!("{path}.api_keys[{j}]"),
                        "the same key is listed twice",
                    );
                }
            }
            for (j, c) in p.credentials.iter().enumerate() {
                let k = c.api_key.trim();
                if let Some(message) = empty_reference_issue(k) {
                    issue(format!("{path}.credentials[{j}].api_key"), message);
                } else if !k.is_empty() && !seen_secrets.insert(k.to_string()) {
                    issue(
                        format!("{path}.credentials[{j}].api_key"),
                        "the same key is listed twice",
                    );
                }
                if let Err(msg) = validate_proxy(&c.proxy) {
                    issue(format!("{path}.credentials[{j}].proxy"), msg);
                }
                let has_key = !c.api_key.trim().is_empty();
                let has_sa = !c.service_account_file.trim().is_empty();
                if has_sa && p.kind != ProviderKind::Vertex {
                    issue(
                        format!("{path}.credentials[{j}].service_account_file"),
                        "is only valid for `vertex` providers",
                    );
                }
                if p.kind.needs_credentials() && !has_key && !has_sa {
                    issue(
                        format!("{path}.credentials[{j}]"),
                        "needs an `api_key` (or `service_account_file` for vertex)",
                    );
                }
                if c.weight.is_some_and(|w| w > 1_000_000) {
                    issue(
                        format!("{path}.credentials[{j}].weight"),
                        "must not exceed 1000000",
                    );
                }
            }
            let mut seen_models = HashSet::new();
            for (j, m) in p.models.iter().enumerate() {
                if m.id.trim().is_empty() {
                    issue(format!("{path}.models[{j}].id"), "must not be empty");
                    continue;
                }
                if !seen_models.insert(m.client_name().to_ascii_lowercase()) {
                    issue(
                        format!("{path}.models[{j}]"),
                        "duplicate client-facing model name within this provider",
                    );
                }
                if let Some(t) = &m.thinking
                    && t.max > 0
                    && t.min > t.max
                {
                    issue(
                        format!("{path}.models[{j}].thinking"),
                        "`min` must not be larger than `max`",
                    );
                }
            }
        }

        if let Err(msg) = validate_proxy(&self.upstream.proxy) {
            issue("upstream.proxy".into(), msg);
        }

        let mut seen_aliases = HashSet::new();
        for (i, a) in self.aliases.iter().enumerate() {
            let name = a.name.trim();
            if name.is_empty() {
                issue(format!("aliases[{i}].name"), "must not be empty");
            } else if name != a.name {
                issue(
                    format!("aliases[{i}].name"),
                    "must not start or end with a space",
                );
            } else if name.chars().any(char::is_whitespace) {
                issue(
                    format!("aliases[{i}].name"),
                    "must not contain spaces: clients send it as a model name",
                );
            } else if crate::reasoning::parse_model_suffix(name).depth.is_some() {
                issue(
                    format!("aliases[{i}].name"),
                    "must not end with a reasoning suffix such as `(high)`: clients add that themselves",
                );
            } else if !seen_aliases.insert(name.to_ascii_lowercase()) {
                issue(format!("aliases[{i}].name"), "duplicate alias");
            }
            if a.targets.is_empty() {
                issue(format!("aliases[{i}].targets"), "needs at least one target");
            }
            for (j, t) in a.targets.iter().enumerate() {
                let target = t.trim();
                if target.is_empty() {
                    issue(format!("aliases[{i}].targets[{j}]"), "must not be empty");
                } else if target != t {
                    issue(
                        format!("aliases[{i}].targets[{j}]"),
                        "must not start or end with a space",
                    );
                } else if !name.is_empty() && target.eq_ignore_ascii_case(name) {
                    issue(
                        format!("aliases[{i}].targets[{j}]"),
                        "an alias cannot target itself",
                    );
                }
            }
        }

        let sections: [(&str, &Vec<PayloadRule>, bool); 3] = [
            ("payload.default", &self.payload.default, false),
            ("payload.override", &self.payload.overrides, false),
            ("payload.filter", &self.payload.filter, true),
        ];
        for (section, rules, is_filter) in sections {
            for (i, r) in rules.iter().enumerate() {
                if r.models.iter().all(|m| m.trim().is_empty()) {
                    issue(
                        format!("{section}[{i}].models"),
                        "needs at least one model pattern",
                    );
                }
                // Only the field of the rule's own section is ever read, so
                // only its paths are checked. A key of `set` is named after
                // `set.` exactly as written, dots and all.
                if is_filter {
                    if r.remove.is_empty() {
                        issue(format!("{section}[{i}].remove"), "needs at least one path");
                    }
                    for (j, path) in r.remove.iter().enumerate() {
                        if let Some(message) = payload_path_issue(path) {
                            issue(format!("{section}[{i}].remove[{j}]"), message);
                        }
                    }
                } else {
                    if r.set.is_empty() {
                        issue(format!("{section}[{i}].set"), "needs at least one field");
                    }
                    for path in r.set.keys() {
                        if let Some(message) = payload_path_issue(path) {
                            issue(format!("{section}[{i}].set.{path}"), message);
                        }
                    }
                }
            }
        }

        for (i, p) in self.pricing.iter().enumerate() {
            if p.model.trim().is_empty() {
                issue(format!("pricing[{i}].model"), "must not be empty");
            }
            let prices = [
                ("input", Some(p.input)),
                ("output", Some(p.output)),
                ("cache_read", p.cache_read),
                ("cache_write", p.cache_write),
            ];
            for (field, price) in prices {
                if price.is_some_and(|v| !v.is_finite() || v < 0.0) {
                    issue(
                        format!("pricing[{i}].{field}"),
                        "must be a number of 0 or more (USD per million tokens)",
                    );
                }
            }
        }

        issues
    }

    /// The price entry for an upstream model id, if any.
    pub fn price_for(&self, model: &str) -> Option<&PriceConfig> {
        self.pricing
            .iter()
            .find(|p| wildcard_match(p.model.trim(), model))
    }

    /// Looks a provider up by name.
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }
}

/// How an outbound proxy setting should be interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxySetting {
    /// Nothing configured at this level: fall through to the next one.
    Inherit,
    /// Connect directly, ignoring proxy environment variables.
    Direct,
    /// Use this proxy URL.
    Url(String),
}

/// Parses a proxy setting: empty → inherit, `direct`/`none` → direct,
/// otherwise an `http`, `https`, `socks5` or `socks5h` URL.
pub fn parse_proxy(value: &str) -> Result<ProxySetting, &'static str> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(ProxySetting::Inherit);
    }
    if v.eq_ignore_ascii_case("direct") || v.eq_ignore_ascii_case("none") {
        return Ok(ProxySetting::Direct);
    }
    let parsed = url::Url::parse(v).map_err(|_| "is not a valid proxy URL")?;
    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        _ => return Err("proxy scheme must be http, https, socks5 or socks5h"),
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("proxy URL needs a host");
    }
    Ok(ProxySetting::Url(v.to_string()))
}

fn validate_proxy(value: &str) -> Result<(), &'static str> {
    parse_proxy(value).map(|_| ())
}

/// Whether `host` can be bound: an IP address (v4 or v6, optionally in
/// brackets) or a DNS host name.
fn is_valid_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Whether `name` is a valid HTTP header field name (an RFC 9110 token).
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

/// The issue of a secret reference that names no variable (`env:`, `${}`),
/// worded for the form it is written in; `None` for anything else.
pub fn empty_reference_issue(value: &str) -> Option<&'static str> {
    let v = value.trim();
    match v.strip_prefix("env:") {
        Some(name) => name
            .trim()
            .is_empty()
            .then_some("names no environment variable after `env:`"),
        None => v
            .strip_prefix("${")
            .and_then(|r| r.strip_suffix('}'))
            .filter(|name| name.trim().is_empty())
            .map(|_| "names no environment variable between `${` and `}`"),
    }
}

/// What makes a value unfit for an HTTP header — where the admin secret
/// travels (`Authorization: Bearer …`) — as the issue to report: spaces or
/// line breaks around it, which a header cannot keep, or control
/// characters inside it, which a header cannot carry at all.
pub fn header_value_issue(value: &str) -> Option<&'static str> {
    if value.chars().any(char::is_control) {
        Some(
            "must not contain control characters such as tabs or line breaks: an HTTP header \
             cannot carry them",
        )
    } else if value.trim() != value {
        Some("must not start or end with a space: an HTTP header cannot carry it")
    } else {
        None
    }
}

/// The issue of a payload rule path (a key of `set`, an entry of `remove`)
/// that can never address a field of a request body; `None` for a usable
/// path.
///
/// The grammar is the gateway's (see `switchyard_translate::jsonpath`):
/// segments separated by `.`, `\.` a literal dot and `\\` a literal
/// backslash inside a segment, a segment of digits an array index when the
/// value it is applied to is an array. There are no wildcards: `*` is a key
/// like any other. Refused are the paths the gateway would ignore (an empty
/// one) and those that only match a key no request body has — an empty key
/// (`a..b`, `a.`, `.a`) or one with spaces or control characters in it —
/// which is how a typo looks.
pub fn payload_path_issue(path: &str) -> Option<&'static str> {
    if path.trim().is_empty() {
        return Some("is empty: name the field, such as `temperature` or `reasoning.effort`");
    }
    if path.trim() != path {
        return Some("must not start or end with a space");
    }
    let mut segments: Vec<String> = vec![String::new()];
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('.' | '\\')) => {
                if let (Some(escaped), Some(segment)) = (chars.next(), segments.last_mut()) {
                    segment.push(escaped);
                }
            }
            '.' => segments.push(String::new()),
            other => {
                if let Some(segment) = segments.last_mut() {
                    segment.push(other);
                }
            }
        }
    }
    if segments.iter().any(String::is_empty) {
        return Some(
            "has an empty part (two dots in a row, or a dot at the start or end); write a dot \
             inside a field name as `\\.`",
        );
    }
    if segments
        .iter()
        .any(|segment| segment.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        return Some(
            "must not contain spaces or control characters; check the field name for a typo",
        );
    }
    None
}

/// Resolves a secret value: `env:NAME` and `${NAME}` read the environment
/// variable `NAME`; anything else is returned as written (trimmed).
///
/// Returns `Err` with the variable name when a referenced variable is unset
/// or empty.
pub fn resolve_secret(value: &str) -> Result<String, String> {
    let v = value.trim();
    let var = match v.strip_prefix("env:") {
        Some(name) => Some(name.trim()),
        None => v
            .strip_prefix("${")
            .and_then(|r| r.strip_suffix('}'))
            .map(str::trim),
    };
    match var {
        None => Ok(v.to_string()),
        Some(name) => match std::env::var(name) {
            Ok(val) if !val.trim().is_empty() => Ok(val.trim().to_string()),
            _ => Err(name.to_string()),
        },
    }
}

/// Whether a secret value is an environment reference rather than a literal.
pub fn is_secret_reference(value: &str) -> bool {
    let v = value.trim();
    v.starts_with("env:") || (v.starts_with("${") && v.ends_with('}'))
}

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::Usage;
    use pretty_assertions::assert_eq;

    #[test]
    fn empty_file_is_valid_and_has_defaults() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.server.port, DEFAULT_PORT);
        assert_eq!(c.server.host, "127.0.0.1");
        assert!(c.auth.required);
        assert_eq!(c.routing.strategy, Strategy::RoundRobin);
    }

    #[test]
    fn full_example_round_trips() {
        let text = r#"
[server]
host = "0.0.0.0"
port = 9000

[admin]
secret = "env:ADMIN"

[[auth.keys]]
key = "sy-test"
name = "laptop"
models = ["gpt-*"]

[routing]
strategy = "fill-first"

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-a", "sk-b"]

[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://localhost:11434/v1/"
prefix = "local"

[[providers.models]]
id = "llama3.3"
alias = "llama"

[[providers]]
name = "vx"
kind = "vertex"
location = "global"

[[providers.credentials]]
service_account_file = "sa.json"
weight = 3

[[aliases]]
name = "smart"
targets = ["claude-opus-4-5", "gpt-5(high)"]

[[payload.override]]
models = ["gpt-*"]
protocol = "openai-responses"
set = { "reasoning.summary" = "auto" }

[[payload.filter]]
models = ["*"]
remove = ["metadata"]

[[pricing]]
model = "gpt-5*"
input = 1.25
output = 10.0
cache_read = 0.125
"#;
        let c = Config::from_toml(text).unwrap();
        assert_eq!(c.providers.len(), 3);
        assert_eq!(c.providers[0].all_credentials().len(), 2);
        assert_eq!(
            c.providers[1].effective_base_url(),
            "http://localhost:11434/v1"
        );
        assert_eq!(c.providers[1].all_credentials().len(), 1);
        assert!(c.providers[1].uses_legacy_max_tokens());
        assert!(!c.providers[0].uses_legacy_max_tokens());
        assert_eq!(c.providers[1].models[0].client_name(), "llama");
        assert_eq!(c.providers[2].credentials[0].effective_weight(), 3);
        assert_eq!(
            c.payload.overrides[0].protocol,
            Some(Protocol::OpenaiResponses)
        );
        assert!(c.auth.keys[0].allows_model("gpt-5"));
        assert!(!c.auth.keys[0].allows_model("claude"));

        let again = Config::from_toml(&c.to_toml().unwrap()).unwrap();
        assert_eq!(c, again);
    }

    /// The shipped example documents every setting with its default, so with
    /// its commented-out sections left alone it must parse to exactly the
    /// defaults. Catches the example drifting from the schema.
    #[test]
    fn example_file_matches_defaults() {
        let text = include_str!("../../../switchyard.example.toml");
        let c = Config::from_toml(text).expect("example config must be valid");
        assert_eq!(c, Config::default());
    }

    /// Every commented-out block of the example must also be valid once
    /// uncommented (comment lines that start with `# ` followed by TOML).
    #[test]
    fn example_file_commented_blocks_are_valid() {
        let text = include_str!("../../../switchyard.example.toml");
        let mut out = String::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            let candidate = trimmed.strip_prefix("# ").unwrap_or("");
            let looks_like_toml = candidate.starts_with('[')
                || candidate.split_once('=').is_some_and(|(k, _)| {
                    let k = k.trim();
                    !k.is_empty()
                        && k.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '"' || c == '.')
                });
            if looks_like_toml {
                out.push_str(candidate);
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        // The "every provider option" block reuses names that must be unique
        // only among providers; the uncommented file has distinct names.
        let c = Config::from_toml(&out).unwrap_or_else(|e| panic!("{e}\n---\n{out}"));
        assert!(c.providers.len() >= 7);
        assert_eq!(c.aliases.len(), 1);
        assert_eq!(c.pricing.len(), 1);
        assert_eq!(c.payload.default.len(), 1);
        assert_eq!(c.payload.overrides.len(), 1);
        assert_eq!(c.payload.filter.len(), 1);
        assert!(c.server.tls.is_some());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::from_toml("[server]\nprot = 1\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn validation_reports_every_problem() {
        let text = r#"
[server]
port = 0

[[providers]]
name = "Bad Name"
kind = "openai-compat"

[[providers]]
name = "a"
kind = "anthropic"
prefix = "x/y"
proxy = "ftp://nope"

[[providers.credentials]]
label = "no key"

[[aliases]]
name = "loop"
targets = ["loop"]
"#;
        let err = Config::from_toml(text).unwrap_err();
        let ConfigError::Invalid(issues) = err else {
            panic!("expected Invalid");
        };
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"server.port"));
        assert!(paths.contains(&"providers[0].name"));
        assert!(paths.contains(&"providers[0].base_url"));
        assert!(paths.contains(&"providers[1].prefix"));
        assert!(paths.contains(&"providers[1].proxy"));
        assert!(paths.contains(&"providers[1].credentials[0]"));
        assert!(paths.contains(&"aliases[0].targets[0]"));
    }

    fn issue_paths(text: &str) -> Vec<String> {
        match Config::from_toml(text) {
            Ok(_) => Vec::new(),
            Err(ConfigError::Invalid(issues)) => issues.into_iter().map(|i| i.path).collect(),
            Err(other) => panic!("unexpected parse error: {other}"),
        }
    }

    #[test]
    fn server_and_routing_values_are_checked() {
        for good in [
            "127.0.0.1",
            "0.0.0.0",
            "::",
            "::1",
            "[::1]",
            "localhost",
            "gw.internal",
        ] {
            assert!(
                issue_paths(&format!("[server]\nhost = \"{good}\"\n")).is_empty(),
                "{good} should be accepted"
            );
        }
        for bad in ["not a host", "http://x", "a..b", "-x", "host:8317", ""] {
            assert_eq!(
                issue_paths(&format!("[server]\nhost = \"{bad}\"\n")),
                vec!["server.host".to_string()],
                "{bad:?} should be refused"
            );
        }
        assert_eq!(
            issue_paths("[server]\ndata_dir = \" \"\n"),
            vec!["server.data_dir".to_string()]
        );
        assert_eq!(
            issue_paths(
                "[routing.cooldown]\nrate_limit_base_secs = 100\nrate_limit_max_secs = 10\n"
            ),
            vec!["routing.cooldown.rate_limit_max_secs".to_string()]
        );
    }

    #[test]
    fn secret_references_need_a_variable_name() {
        assert_eq!(
            issue_paths("[admin]\nsecret = \"env:\"\n"),
            vec!["admin.secret".to_string()]
        );
        assert_eq!(
            issue_paths("[[auth.keys]]\nkey = \"${ }\"\n"),
            vec!["auth.keys[0].key".to_string()]
        );
        let text = "[[providers]]\nname = \"a\"\nkind = \"anthropic\"\napi_keys = [\"env:\"]\n\n[[providers.credentials]]\napi_key = \"${}\"\n";
        assert_eq!(
            issue_paths(text),
            vec![
                "providers[0].api_keys[0]".to_string(),
                "providers[0].credentials[0].api_key".to_string()
            ]
        );
        assert!(issue_paths("[admin]\nsecret = \"env:REAL_NAME\"\n").is_empty());

        // Regression (A2-3): the message is worded for the form written.
        let message = |text: &str| match Config::from_toml(text) {
            Err(ConfigError::Invalid(issues)) => issues[0].message.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message("[admin]\nsecret = \"env:\"\n"),
            "names no environment variable after `env:`"
        );
        assert_eq!(
            message("[admin]\nsecret = \"${}\"\n"),
            "names no environment variable between `${` and `}`"
        );
        assert_eq!(
            message("[[auth.keys]]\nkey = \"${ }\"\n"),
            "names no environment variable between `${` and `}`"
        );
        assert_eq!(empty_reference_issue("env:NAME"), None);
        assert_eq!(empty_reference_issue("${NAME}"), None);
        assert_eq!(empty_reference_issue("literal"), None);
    }

    #[test]
    fn client_key_rate_limit_of_zero_is_refused() {
        assert_eq!(
            issue_paths("[[auth.keys]]\nkey = \"sy-x\"\nrate_limit_rpm = 0\n"),
            vec!["auth.keys[0].rate_limit_rpm".to_string()]
        );
        assert!(issue_paths("[[auth.keys]]\nkey = \"sy-x\"\nrate_limit_rpm = 1\n").is_empty());
    }

    #[test]
    fn provider_headers_and_thinking_ranges_are_checked() {
        let base = "[[providers]]\nname = \"a\"\nkind = \"mock\"\n";
        assert_eq!(
            issue_paths(&format!("{base}headers = {{ \"bad header\" = \"x\" }}\n")),
            vec!["providers[0].headers.bad header".to_string()]
        );
        assert_eq!(
            issue_paths(&format!("{base}headers = {{ \"X-Empty\" = \" \" }}\n")),
            vec!["providers[0].headers.X-Empty".to_string()]
        );
        assert_eq!(
            issue_paths(&format!(
                "{base}headers = {{ \"X-Split\" = \"a\\r\\nb\" }}\n"
            )),
            vec!["providers[0].headers.X-Split".to_string()]
        );
        assert!(
            issue_paths(&format!("{base}headers = {{ \"X-Title\" = \"my app\" }}\n")).is_empty()
        );
        assert_eq!(
            issue_paths(&format!(
                "{base}\n[[providers.models]]\nid = \"m\"\nthinking = {{ min = 5000, max = 100 }}\n"
            )),
            vec!["providers[0].models[0].thinking".to_string()]
        );
    }

    #[test]
    fn alias_names_and_targets_are_checked() {
        let alias = |name: &str, targets: &str| {
            issue_paths(&format!(
                "[[aliases]]\nname = \"{name}\"\ntargets = {targets}\n"
            ))
        };
        assert!(alias("smart", "[\"a\", \"b(high)\"]").is_empty());
        assert_eq!(
            alias(" padded", "[\"a\"]"),
            vec!["aliases[0].name".to_string()]
        );
        assert_eq!(
            alias("has space", "[\"a\"]"),
            vec!["aliases[0].name".to_string()]
        );
        assert_eq!(
            alias("fast(high)", "[\"a\"]"),
            vec!["aliases[0].name".to_string()]
        );
        // A parenthesised part that is not a reasoning suffix is just a name.
        assert!(alias("fast(v2)", "[\"a\"]").is_empty());
        assert_eq!(alias("x", "[]"), vec!["aliases[0].targets".to_string()]);
        assert_eq!(
            alias("x", "[\"a\", \"\", \" b \", \"X\"]"),
            vec![
                "aliases[0].targets[1]".to_string(),
                "aliases[0].targets[2]".to_string(),
                "aliases[0].targets[3]".to_string()
            ]
        );
    }

    #[test]
    fn prices_must_be_finite_and_not_negative() {
        let price = |body: &str| issue_paths(&format!("[[pricing]]\nmodel = \"m\"\n{body}"));
        assert!(price("input = 1.0\noutput = 2.0\ncache_read = 0.0\n").is_empty());
        // Regression (A2-7): each bad price is named by its own field.
        assert_eq!(
            price("input = nan\noutput = 1.0\n"),
            vec!["pricing[0].input".to_string()]
        );
        assert_eq!(
            price("input = 1.0\noutput = inf\n"),
            vec!["pricing[0].output".to_string()]
        );
        assert_eq!(
            price("input = 1.0\noutput = 1.0\ncache_write = -0.5\n"),
            vec!["pricing[0].cache_write".to_string()]
        );
        assert_eq!(
            price("input = -1.0\noutput = 1.0\ncache_read = -inf\n"),
            vec![
                "pricing[0].input".to_string(),
                "pricing[0].cache_read".to_string()
            ]
        );
    }

    /// Regression (A2-6): payload rule paths that can never address a field
    /// are refused at the path itself; the gateway's grammar (indexes,
    /// escapes, no wildcards) is accepted as it is.
    #[test]
    fn payload_rule_paths_are_checked() {
        for good in [
            "temperature",
            "messages.0.role",
            "metadata.trace\\.id",
            "generationConfig.thinkingConfig.thinkingBudget",
            "a\\\\b",
            "*",
            "0",
        ] {
            assert_eq!(payload_path_issue(good), None, "{good:?}");
        }
        for bad in [
            "",
            " ",
            "a..b",
            "a.",
            ".a",
            " a",
            "a ",
            "a. b",
            "a b",
            "a\tb",
            "a.\\.\u{7}",
        ] {
            assert!(payload_path_issue(bad).is_some(), "{bad:?}");
        }
        assert!(payload_path_issue("a..b").unwrap().contains("empty part"));
        assert!(payload_path_issue("").unwrap().starts_with("is empty"));

        let text = r#"
[[payload.default]]
models = ["*"]
set = { "ok" = 1, "a..b" = 2, " padded" = 3 }

[[payload.override]]
models = ["*"]
set = { "reasoning.effort" = "low", "" = 1 }
remove = ["not read here.."]

[[payload.filter]]
models = ["*"]
remove = ["user", "metadata.", "has space"]
"#;
        assert_eq!(
            issue_paths(text),
            vec![
                "payload.default[0].set.a..b".to_string(),
                "payload.default[0].set. padded".to_string(),
                "payload.override[0].set.".to_string(),
                "payload.filter[0].remove[1]".to_string(),
                "payload.filter[0].remove[2]".to_string(),
            ]
        );
    }

    /// Regression (A2-4): an admin secret a header cannot carry is refused.
    #[test]
    fn admin_secrets_must_fit_in_a_header() {
        assert!(issue_paths("[admin]\nsecret = \"plain-secret-123\"\n").is_empty());
        for bad in [
            " leading",
            "trailing ",
            "line\\nbreak",
            "tab\\there",
            "\\u0000nul",
            "ends\\r\\n",
        ] {
            let text = format!("[admin]\nsecret = \"{bad}\"\n");
            let Err(ConfigError::Invalid(issues)) = Config::from_toml(&text) else {
                panic!("{bad:?} should be refused");
            };
            assert_eq!(issues.len(), 1, "{issues:?}");
            assert_eq!(issues[0].path, "admin.secret");
            assert!(
                issues[0].message.contains("HTTP header"),
                "{bad:?}: {}",
                issues[0].message
            );
        }
    }

    /// Regression (A2-8): the fields a payload rule sets, and the keys of
    /// the objects it sets them to, keep the order of the file.
    #[test]
    fn payload_set_keeps_the_order_of_the_file() {
        let c = Config::from_toml(
            "[[payload.override]]\nmodels = [\"*\"]\n\
             set = { \"zeta\" = 1, \"alpha\" = { \"y\" = 1, \"b\" = 2 }, \"mid\" = 3 }\n\n\
             [[payload.default]]\nmodels = [\"*\"]\n\
             [payload.default.set]\nzz = 1\naa = 2\n",
        )
        .unwrap();
        let keys: Vec<&str> = c.payload.overrides[0]
            .set
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
        let inner: Vec<&str> = c.payload.overrides[0].set["alpha"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(inner, ["y", "b"]);
        let keys: Vec<&str> = c.payload.default[0]
            .set
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zz", "aa"]);
    }

    #[test]
    fn provider_protocol_preferences() {
        let mut p = ProviderConfig::new("o", ProviderKind::Openai);
        assert_eq!(
            p.protocols(),
            vec![Protocol::OpenaiResponses, Protocol::OpenaiChat]
        );
        p.wire_api = WireApi::Chat;
        assert_eq!(p.protocols(), vec![Protocol::OpenaiChat]);
        let p = ProviderConfig::new("c", ProviderKind::OpenaiCompat);
        assert_eq!(p.protocols(), vec![Protocol::OpenaiChat]);
        let p = ProviderConfig::new("m", ProviderKind::Mock);
        assert_eq!(p.protocols().len(), 4);
        assert_eq!(p.all_credentials().len(), 1);
    }

    #[test]
    fn prefix_normalisation() {
        let mut p = ProviderConfig::new("o", ProviderKind::Openai);
        p.prefix = " /team-a/ ".into();
        assert_eq!(p.normalized_prefix(), "team-a");
        p.prefix = "a/b".into();
        assert_eq!(p.normalized_prefix(), "");
    }

    #[test]
    fn proxy_parsing() {
        assert_eq!(parse_proxy(""), Ok(ProxySetting::Inherit));
        assert_eq!(parse_proxy(" Direct "), Ok(ProxySetting::Direct));
        assert_eq!(parse_proxy("none"), Ok(ProxySetting::Direct));
        assert_eq!(
            parse_proxy("socks5://user:pw@127.0.0.1:1080"),
            Ok(ProxySetting::Url("socks5://user:pw@127.0.0.1:1080".into()))
        );
        assert!(parse_proxy("ftp://x").is_err());
        assert!(parse_proxy("not a url").is_err());
    }

    #[test]
    fn secret_resolution() {
        assert_eq!(resolve_secret(" sk-literal ").unwrap(), "sk-literal");
        assert!(!is_secret_reference("sk-literal"));
        assert!(is_secret_reference("env:FOO"));
        assert!(is_secret_reference("${FOO}"));
        assert_eq!(
            resolve_secret("env:SWITCHYARD_TEST_UNSET_VARIABLE").unwrap_err(),
            "SWITCHYARD_TEST_UNSET_VARIABLE"
        );
        // PATH is set on every platform the tests run on.
        assert!(!resolve_secret("${PATH}").unwrap().is_empty());
    }

    #[test]
    fn pricing() {
        let c = Config::from_toml(
            "[[pricing]]\nmodel = \"gpt-5*\"\ninput = 2.0\noutput = 8.0\ncache_read = 0.5\n",
        )
        .unwrap();
        let p = c.price_for("gpt-5-mini").unwrap();
        let cost = p.cost(&Usage {
            input_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 0,
            output_tokens: 500_000,
            reasoning_tokens: 100,
        });
        assert!((cost - (2.0 + 1.0 + 4.0)).abs() < 1e-9);
        assert!(c.price_for("claude").is_none());
    }
}
