//! The [`Gateway`] handle: construction, accessors, background work.

use crate::auth::{ClientIdentity, KeyTable};
use crate::ops::Discoveries;
use crate::reply::{Served, error_reply};
use crate::summary::SummaryRefusals;
use crate::ticket::{TICKET_TTL, Tickets};
use crate::types::{
    ClientRequest, DiscoveryState, FullReply, GatewayOptions, PresentedCredentials, ProviderTest,
    RawRequest, Reply, StartError, WsOpenRequest, WsTicket,
};
use crate::ws::UpstreamWsSession;
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};
use switchyard_config_store::{ConfigEvent, ConfigStore};
use switchyard_core::config::{Config, ConfigIssue, ProviderConfig, ProviderKind, resolve_secret};
use switchyard_core::util::now_unix_ms;
use switchyard_core::{ApiError, Codec, ModelInfo, Protocol};
use switchyard_scheduler::{Lease, Outcome, Scheduler};
use switchyard_telemetry::{Event, Telemetry, TelemetryOptions, new_request_id};
use switchyard_translate::ReasoningStore;
use switchyard_upstream::{ServiceAccount, UpstreamClient, mock_models};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Tool calls whose reasoning state is remembered for clients that cannot
/// carry it themselves.
const REASONING_STORE_ENTRIES: usize = 8_192;

/// How long such state is kept after it was last stored or used: long
/// enough for a person to come back to a conversation.
const REASONING_STORE_TTL: Duration = Duration::from_secs(3 * 3600);

/// Memory budget of the reasoning store (encrypted reasoning is large).
const REASONING_STORE_BYTES: usize = 128 * 1024 * 1024;

/// Issues of a rejected configuration quoted in the `config.reloaded`
/// event; the rest are counted.
const REJECTION_ISSUES_SHOWN: usize = 3;

/// A hook the binary registers to apply `logging.level` to its subscriber.
type LogLevelHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything a running gateway owns. Shared by all [`Gateway`] clones and
/// by the stream tasks of requests in progress.
pub(crate) struct Inner {
    pub(crate) store: ConfigStore,
    pub(crate) telemetry: Telemetry,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) upstream: UpstreamClient,
    pub(crate) reasoning: ReasoningStore,
    pub(crate) keys: ArcSwap<KeyTable>,
    /// Single-use tickets for client WebSockets.
    tickets: Tickets,
    /// Parsed service-account key files, by resolved path. Cleared when the
    /// configuration changes, so an edited file is read again.
    pub(crate) service_accounts: Mutex<HashMap<PathBuf, Arc<ServiceAccount>>>,
    /// The credentials (by id) that the check of the service-account files
    /// has marked unusable in the scheduler, so that a mark can be taken
    /// off once its credential names no file any more. The lock is held
    /// for the length of a check, which makes checks run one at a time.
    file_marks: tokio::sync::Mutex<HashSet<String>>,
    /// Providers (and single models of providers) whose upstream refused
    /// to generate reasoning summaries, under the configuration in effect.
    /// Started afresh when the configuration changes.
    pub(crate) summary_refusals: SummaryRefusals,
    /// Where the discovery of each provider's model list stands.
    pub(crate) discoveries: Discoveries,
    log_level_hook: Mutex<Option<LogLevelHook>>,
    started_at: SystemTime,
    shutdown: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// The gateway engine: authenticates clients, routes requests to upstream
/// credentials, translates between protocols and records what happened.
///
/// Cloning is cheap — clones share everything — and the handle can be used
/// from any number of tasks at once.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("config", &self.inner.store.path())
            .field("scheduler", &self.inner.scheduler)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// Starts a gateway.
    ///
    /// Loads and validates the configuration file (it must exist), builds
    /// the telemetry (data directory = `server.data_dir` resolved against
    /// the file's directory; usage history is loaded), the scheduler, the
    /// upstream client and the reasoning store, registers the mock
    /// provider's models, and spawns the background tasks: the config file
    /// watcher (when [`GatewayOptions::watch_config`]), the task that
    /// applies configuration changes, model discovery, the telemetry
    /// writers and the hourly prune.
    ///
    /// Must be called inside a tokio runtime.
    pub async fn start(options: GatewayOptions) -> Result<Gateway, StartError> {
        let store = ConfigStore::load(&options.config_path)?;
        let config = store.current();

        let data_dir = store.resolve_path(&config.server.data_dir);
        let telemetry = Telemetry::new(TelemetryOptions {
            data_dir: Some(data_dir),
            usage: config.usage.clone(),
            logging: config.logging.clone(),
        });
        {
            // Reading the usage files is blocking I/O.
            let telemetry = telemetry.clone();
            let loaded =
                tokio::task::spawn_blocking(move || telemetry.load_history(now_unix_ms())).await;
            match loaded {
                Ok(report) => tracing::debug!(?report, "usage history loaded"),
                Err(error) => tracing::warn!(%error, "usage history could not be loaded"),
            }
        }
        let handle = Handle::current();
        telemetry.spawn_background(&handle);

        let scheduler = Arc::new(Scheduler::new(&config, &resolve_secret));
        let upstream =
            UpstreamClient::new().map_err(|error| StartError::Upstream(error.info.message))?;
        let reasoning = ReasoningStore::new(REASONING_STORE_ENTRIES, REASONING_STORE_TTL)
            .with_max_bytes(REASONING_STORE_BYTES);
        let keys = KeyTable::build(&config, None, switchyard_config_store::client_key_id);

        // Subscribed before anything can change the configuration, so no
        // change is missed between now and the task starting.
        let changes = store.subscribe();
        let verdicts = store.events();
        if options.watch_config {
            store.spawn_watcher(&handle);
        }

        let inner = Arc::new(Inner {
            store,
            telemetry,
            scheduler,
            upstream,
            reasoning,
            keys: ArcSwap::from_pointee(keys),
            tickets: Tickets::default(),
            service_accounts: Mutex::new(HashMap::new()),
            file_marks: tokio::sync::Mutex::new(HashSet::new()),
            summary_refusals: SummaryRefusals::new(Arc::clone(&config)),
            discoveries: Discoveries::default(),
            log_level_hook: Mutex::new(None),
            started_at: SystemTime::now(),
            shutdown: CancellationToken::new(),
            tasks: Mutex::new(Vec::new()),
        });
        // All mock providers in one go: the model table is derived once.
        inner
            .scheduler
            .set_discovered_many(mock_model_lists(&config));
        inner.check_service_accounts(&config).await;
        // Once everything that decides them is in place: the mock models
        // (an alias may target one) and the state of the key files.
        for warning in inner.scheduler.warnings() {
            tracing::warn!("configuration: {warning}");
        }

        let follower = handle.spawn(follow_config(
            Arc::downgrade(&inner),
            changes,
            verdicts,
            inner.shutdown.clone(),
        ));
        inner.tasks.lock().push(follower);
        inner.spawn_discovery(None, config);

        Ok(Gateway { inner })
    }

    /// The gateway's version (the crate version).
    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// When this gateway was started.
    pub fn started_at(&self) -> SystemTime {
        self.inner.started_at
    }

    /// The configuration in effect right now.
    pub fn config(&self) -> Arc<Config> {
        self.inner.store.current()
    }

    /// The configuration store: the admin API edits the configuration
    /// through it, and the gateway applies whatever it publishes.
    pub fn config_store(&self) -> &ConfigStore {
        &self.inner.store
    }

    /// Request records, usage statistics, the event bus and the log buffer.
    pub fn telemetry(&self) -> &Telemetry {
        &self.inner.telemetry
    }

    /// The scheduler: model table, credential states, cooldown control.
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.inner.scheduler
    }

    /// The transport to upstream providers.
    pub fn upstream(&self) -> &UpstreamClient {
        &self.inner.upstream
    }

    /// The codec of a protocol.
    pub fn codec(&self, protocol: Protocol) -> &'static dyn Codec {
        switchyard_codecs::codec(protocol)
    }

    /// Identifies the client behind a request.
    ///
    /// Candidates are tried in this order: the `Authorization` header (the
    /// token of `Bearer <token>`, otherwise the whole value), `x-api-key`,
    /// `x-goog-api-key`, the `key` query parameter. The first one that
    /// *matches* an enabled configured key wins, so a wrong `Authorization`
    /// next to a right `x-api-key` authenticates. Keys are compared in
    /// constant time, after secret references in the configuration have
    /// been resolved.
    ///
    /// Nothing presented → 401 "missing API key"; presented but no match →
    /// 401 "invalid API key". With `auth.required = false` both cases yield
    /// an anonymous identity instead.
    ///
    /// A WebSocket ticket ([`PresentedCredentials::ws_ticket`]) is the last
    /// candidate, looked at only when no key matched; looking at it uses it
    /// up. A live ticket authenticates as the key that bought it — that
    /// key's name, id, allow-list and rate limit, as configured now (a key
    /// disabled or removed since takes its tickets with it); a ticket
    /// bought anonymously, as an anonymous client while those are admitted.
    /// An unknown, expired or used ticket counts as a wrong key.
    pub fn authenticate(
        &self,
        presented: &PresentedCredentials,
    ) -> Result<ClientIdentity, ApiError> {
        let now = Instant::now();
        self.inner
            .keys
            .load()
            .authenticate_with(presented, |ticket| self.inner.tickets.redeem(ticket, now))
    }

    /// Revalidates a previously authenticated client and returns its current
    /// restrictions, without consuming a request from its rate limit.
    /// Long-lived relays should call this periodically as well as on messages.
    pub fn refresh_identity(&self, identity: &ClientIdentity) -> Result<ClientIdentity, ApiError> {
        self.inner.keys.load().refresh(identity)
    }

    /// Checks that a long-lived session still has the access it opened with.
    /// A changed model allow-list or rate limit requires reconnecting.
    pub fn validate_session_identity(&self, identity: &ClientIdentity) -> Result<(), ApiError> {
        let current = self.refresh_identity(identity)?;
        if !identity.same_permissions(&current) {
            return Err(
                ApiError::authentication("client access changed; reconnect required")
                    .with_code("invalid_api_key"),
            );
        }
        Ok(())
    }

    /// Mints a single-use ticket for a client WebSocket, for the client
    /// `identity` (from [`Gateway::authenticate`]): browsers cannot set
    /// headers on a WebSocket, and a ticket in the URL is worth nothing
    /// after one use or 30 seconds, where a key would be.
    ///
    /// Not a request: no record, no usage, no rate-limit hit. Each client
    /// key (and the anonymous client) has at most 32 tickets outstanding;
    /// minting another drops its oldest. The gateway's built-in clients
    /// (the playground) get an error.
    pub fn issue_ws_ticket(&self, identity: &ClientIdentity) -> Result<WsTicket, ApiError> {
        let identity = self.refresh_identity(identity)?;
        let holder = identity.ticket_holder().ok_or_else(|| {
            ApiError::permission("built-in clients of the gateway do not use WebSocket tickets")
        })?;
        Ok(WsTicket {
            ticket: self.inner.tickets.issue(holder, Instant::now()),
            expires_in: TICKET_TTL.as_secs(),
        })
    }

    /// The identity of the admin playground: internal, no model
    /// restrictions, no rate limit, key name `dashboard`.
    pub fn dashboard_identity(&self) -> ClientIdentity {
        ClientIdentity::dashboard()
    }

    /// Serves one generation request through the pipeline: parse, check the
    /// client key's allow-list and rate limit, resolve the model, then try
    /// credentials until one answers (passthrough when the upstream speaks
    /// the client's protocol, translation otherwise).
    ///
    /// Never fails as a Rust call: every outcome is a [`Reply`]. A
    /// [`Reply::Stream`] is returned only once the upstream has produced
    /// its first event; a streaming request that fails before that gets a
    /// [`Reply::Full`] with the error status, like any other request.
    ///
    /// With `routing.max_wait_secs` set, a request that finds every
    /// credential resting — or has just seen its last one fail — waits for
    /// the soonest recovery within that limit, once, and tries again.
    ///
    /// An upstream's own words about a failure reach the client without the
    /// upstream credential, should the upstream have quoted it; and a Chat
    /// Completions client that did not ask for the usage chunk of a stream
    /// is not sent the one the gateway requested for its accounting.
    pub async fn generate(&self, request: ClientRequest) -> Reply {
        self.inner.generate(request).await
    }

    /// Counts the input tokens of a request: through the upstream's own
    /// counting endpoint when it has one, by a local estimate otherwise —
    /// also when the upstream refuses the counting call for a reason that
    /// says nothing about the credential (no such endpoint, a key that may
    /// not count). Such refusals never rest a credential.
    /// Always answers with a [`Reply::Full`] in the client protocol's
    /// counting shape; protocols without one (Chat Completions) get a 404.
    pub async fn count_tokens(&self, request: ClientRequest) -> Reply {
        self.inner.count_tokens(request).await
    }

    /// The model listing in `protocol`'s shape: every visible client-facing
    /// model name the identity's allow-list admits.
    pub fn models(&self, protocol: Protocol, identity: &ClientIdentity) -> Value {
        let models: Vec<ModelInfo> = self.inner.visible_models(identity);
        self.codec(protocol).encode_models(&models)
    }

    /// One model of the listing, by exact id (a `models/` prefix is
    /// accepted for Gemini). 404 when the id is not listed for this
    /// identity.
    pub fn model(
        &self,
        protocol: Protocol,
        identity: &ClientIdentity,
        id: &str,
    ) -> Result<Value, ApiError> {
        let wanted = match protocol {
            Protocol::Gemini => id.strip_prefix("models/").unwrap_or(id),
            _ => id,
        };
        self.inner
            .visible_models(identity)
            .iter()
            .find(|model| model.id == wanted)
            .map(|model| self.codec(protocol).encode_model(model))
            .ok_or_else(|| ApiError::unknown_model(id))
    }

    /// Forwards a request for an OpenAI-style side endpoint (embeddings,
    /// images, speech, moderations) to an `openai` / `openai-compat`
    /// provider that serves the request's model, body unchanged apart from
    /// the model name. Always answers with a [`Reply::Full`].
    ///
    /// These endpoints are optional: a refusal that only says "not here" or
    /// "not for this key" (401, 403, 404, 405, 501) is passed on to the
    /// client and the next credential is tried, but nothing is held against
    /// the credential — the model stays available for generation. Rate
    /// limits, exhausted quota, 5xx and transport failures count as usual.
    pub async fn raw(&self, request: RawRequest) -> Reply {
        self.inner.raw(request).await
    }

    /// Opens a WebSocket to an upstream that serves the request's model,
    /// with failover across credentials on handshake failures. The session
    /// reports to the scheduler and the request log when it ends.
    ///
    /// A handshake that fails because the upstream has no WebSocket
    /// endpoint, does not let this key use it, or cannot be reached over
    /// the WebSocket route is an `Err` that rests nothing: the model stays
    /// available over HTTP, which is what the server falls back to. Only
    /// what the upstream's API would answer any call with (429, exhausted
    /// quota, 5xx) is held against the credential.
    pub async fn open_upstream_ws(
        &self,
        request: WsOpenRequest,
    ) -> Result<UpstreamWsSession, ApiError> {
        self.inner.open_upstream_ws(request).await
    }

    /// Sends one tiny generation request ("ping", at most 16 output tokens)
    /// through the first usable credential of `provider`, for `model` or
    /// the provider's first model. The credential is used whatever its
    /// cooldown state, and the outcome is reported to the scheduler. Never
    /// fails as a Rust call: problems are in the result.
    ///
    /// The test passes when the upstream answers with a response a request
    /// could be served with. A `2xx` whose body reports a failed generation
    /// (a Responses body with `status: "failed"`) or is no response at all
    /// fails it, and is reported to the scheduler like the same answer to
    /// a request.
    pub async fn test_provider(&self, provider: &str, model: Option<&str>) -> ProviderTest {
        self.inner.test_provider(provider, model).await
    }

    /// Asks `provider`'s upstream for its model list now, feeds it to the
    /// scheduler and returns it. The outcome also becomes the provider's
    /// [discovery state](Gateway::discovery_states), unless that is `off`.
    pub async fn discover(&self, provider: &str) -> Result<Vec<ModelInfo>, ApiError> {
        self.inner.discover(provider).await
    }

    /// Where the discovery of each provider's model list stands, by
    /// provider name. Every provider of the configuration in effect has an
    /// entry.
    ///
    /// Discovery runs in the background at start and, after a configuration
    /// change, only for the providers whose discovery-relevant settings
    /// changed: a new provider, another `kind`, `base_url`, `api_keys`,
    /// `credentials`, `proxy` (the provider's or `upstream.proxy`),
    /// `headers`, `project` or `location`, and a provider that wants
    /// discovery now and did not before (enabled, `discover` switched on,
    /// its `models` list emptied). A configuration applied without any
    /// change — a manual reload — asks every provider that wants discovery
    /// again.
    ///
    /// Until a listing has answered, the state is `pending`; a listing that
    /// fails keeps the list of the last success in use and says why it
    /// failed, with credentials removed.
    pub fn discovery_states(&self) -> HashMap<String, DiscoveryState> {
        self.inner.discoveries.states()
    }

    /// Renders any error in a protocol's envelope: status, body and the
    /// `retry-after` header when the error carries a wait. For errors the
    /// server detects before a request reaches the pipeline (authentication,
    /// unknown routes, oversized bodies).
    pub fn error_reply(&self, protocol: Protocol, error: &ApiError) -> FullReply {
        error_reply(
            self.codec(protocol),
            error,
            &new_request_id(),
            Served::default(),
        )
    }

    /// Registers the function that applies `logging.level` (`trace` …
    /// `error`) to the process's log subscriber. It is called whenever a
    /// configuration is applied; the binary sets the level at start itself.
    pub fn on_log_level(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        *self.inner.log_level_hook.lock() = Some(Arc::new(f));
    }

    /// Stops the background tasks and writes out everything the telemetry
    /// has queued. Requests in progress are not interrupted; call this once
    /// the server has stopped accepting new ones.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let tasks: Vec<JoinHandle<()>> = self.inner.tasks.lock().drain(..).collect();
        for task in tasks {
            task.abort();
        }
        if let Err(error) = self.inner.telemetry.shutdown().await {
            tracing::warn!(%error, "telemetry could not be flushed at shutdown");
        }
    }
}

#[cfg(test)]
impl Gateway {
    /// The shared state, for unit tests that drive internals directly.
    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl Inner {
    /// The listing shown to `identity`.
    fn visible_models(&self, identity: &ClientIdentity) -> Vec<ModelInfo> {
        self.scheduler
            .visible_models()
            .into_iter()
            .filter(|model| identity.allows_model(&model.id))
            .collect()
    }

    /// Reports the outcome of an attempt to the scheduler and, when it
    /// failed, tells the live dashboard that the credential may have
    /// changed state (a cooldown started).
    pub(crate) fn report(&self, lease: &Lease, outcome: Outcome<'_>) {
        self.scheduler.report(lease, outcome, self.scheduler.now());
        if matches!(outcome, Outcome::Failure(_)) {
            self.publish_credential(&lease.credential.provider, &lease.credential.id);
        }
    }

    /// Publishes the current state of a credential on the event bus
    /// (`credential` topic): `{"provider": <name>, "credential":
    /// <CredentialSnapshot>}`. Skipped while nobody is listening.
    pub(crate) fn publish_credential(&self, provider: &str, credential_id: &str) {
        if self.telemetry.bus().subscriber_count() == 0 {
            return;
        }
        let snapshot = self
            .scheduler
            .credential_snapshot(credential_id)
            .and_then(|snapshot| serde_json::to_value(snapshot).ok());
        if let Some(credential) = snapshot {
            self.telemetry.publish(Event::Credential(serde_json::json!({
                "provider": provider,
                "credential": credential,
            })));
        }
    }

    /// Marks every credential whose service-account file is missing or is
    /// not a usable key file as unusable, with the reason, and takes the
    /// mark off those whose file is fine (again) — and off those that no
    /// longer name a file at all.
    ///
    /// Without this such a credential looks ready until the first request
    /// fails on it. The reason names the file and what is wrong with it,
    /// never anything the file holds. The check runs when a configuration
    /// is applied, and before the two things an operator does to see
    /// whether a provider works — a provider test and a model listing on
    /// request — so a file repaired afterwards is noticed by the next
    /// configuration change, reload, test or listing.
    ///
    /// The scheduler keeps a mark for as long as the credential's id stays
    /// the same, and the id of a credential with an API key comes from the
    /// key, not from the file. So the marks this check has set are
    /// remembered here, and one whose credential has stopped naming a file
    /// is taken off; left alone it would keep the credential out of use,
    /// for a file it no longer refers to, until the gateway is restarted.
    pub(crate) async fn check_service_accounts(&self, config: &Config) {
        // One check at a time, each from start to end: a check that began
        // under an earlier configuration must not put a mark back after
        // the check of the configuration that replaced it took it off.
        // (The check that follows a rebuild always runs after any that
        // began before it.)
        let mut marked = self.file_marks.lock().await;
        let any_file = config.providers.iter().any(|provider| {
            provider
                .credentials
                .iter()
                .any(|credential| !credential.service_account_file.trim().is_empty())
        });
        // The usual case, and with hundreds of providers the cheap one: no
        // credential names a file and no mark stands.
        if !any_file && marked.is_empty() {
            return;
        }
        // Every credential, the unusable ones included: the snapshot lists
        // them all, the scheduler's selection would leave marked ones out.
        let ids: Vec<String> = self
            .scheduler
            .snapshot()
            .into_iter()
            .flat_map(|provider| provider.credentials)
            .map(|credential| credential.id)
            .collect();
        let mut standing = HashSet::new();
        for id in ids {
            let Some(view) = self.scheduler.credential(&id) else {
                continue;
            };
            let file = view.service_account_file.trim();
            let reason = if file.is_empty() {
                if !marked.contains(&id) {
                    continue;
                }
                None
            } else {
                match self.service_account(file).await {
                    Ok(_) => None,
                    Err(error) => Some(error.info.message),
                }
            };
            if reason.is_some() {
                standing.insert(id.clone());
            }
            // Logged by whoever applies the configuration, with the
            // scheduler's other warnings.
            self.scheduler.set_unusable(&id, reason);
        }
        // Marks of credentials that are gone went with their state.
        *marked = standing;
    }

    /// Applies a configuration the store published.
    async fn apply_config(self: &Arc<Self>, config: Arc<Config>) {
        let replaced = self.scheduler.config();
        // One rebuild for everything: the mock providers' built-in lists go
        // in with the configuration (handing them over one provider at a
        // time derived the model table once per mock provider), and
        // discovered lists are kept by the scheduler for providers whose
        // endpoint did not change.
        self.scheduler
            .rebuild(&config, &resolve_secret, mock_model_lists(&config));
        // A service-account file may have been replaced along with the
        // configuration that names it.
        self.service_accounts.lock().clear();
        self.check_service_accounts(&config).await;
        for warning in self.scheduler.warnings() {
            tracing::warn!("configuration: {warning}");
        }

        let previous = self.keys.load_full();
        self.keys.store(Arc::new(KeyTable::build(
            &config,
            Some(&previous),
            switchyard_config_store::client_key_id,
        )));

        self.telemetry.reconfigure(&config.usage, &config.logging);
        // A key may have been replaced: another organisation may well be
        // allowed the reasoning summaries the previous one was refused.
        // Requests still running under an earlier configuration add nothing
        // to what is known from here on.
        self.summary_refusals.reset(Arc::clone(&config));

        let hook = self.log_level_hook.lock().clone();
        if let Some(hook) = hook {
            hook(config.logging.level.trim());
        }

        self.config_applied();
        tracing::info!(
            providers = config.providers.len(),
            client_keys = config.auth.keys.len(),
            "configuration applied"
        );
        self.spawn_discovery(Some(&replaced), config);
    }

    /// Tells the live dashboard that the configuration in effect is the
    /// one the store took last: `config.reloaded` with `ok: true`.
    fn config_applied(&self) {
        self.telemetry.publish(Event::ConfigReloaded {
            at: now_unix_ms(),
            ok: true,
            message: "configuration applied".to_string(),
        });
    }

    /// Tells the live dashboard that a configuration was refused (an edit
    /// of the file on disk that does not validate, say) and the previous
    /// one stays in effect: `config.reloaded` with `ok: false`.
    fn config_rejected(&self, issues: &[ConfigIssue]) {
        let mut message = String::from("configuration rejected; the previous one stays in effect");
        for (index, issue) in issues.iter().take(REJECTION_ISSUES_SHOWN).enumerate() {
            message.push_str(if index == 0 { ": " } else { "; " });
            message.push_str(&issue.to_string());
        }
        if issues.len() > REJECTION_ISSUES_SHOWN {
            message.push_str(&format!(
                " (and {} more)",
                issues.len() - REJECTION_ISSUES_SHOWN
            ));
        }
        self.telemetry.publish(Event::ConfigReloaded {
            at: now_unix_ms(),
            ok: false,
            message,
        });
    }

    /// Runs model discovery in the background for the providers of
    /// `config` that need it: at start (`previous` is `None`) all that want
    /// discovery, after a configuration change those whose
    /// discovery-relevant settings changed (see
    /// [`Gateway::discovery_states`]). The other providers keep their
    /// lists, and their upstreams are left alone.
    fn spawn_discovery(self: &Arc<Self>, previous: Option<&Config>, config: Arc<Config>) {
        let runs = self.discoveries.plan(previous, &config, |provider| {
            self.scheduler.discovered_models(provider)
        });
        if runs.is_empty() {
            return;
        }
        let inner = Arc::clone(self);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = inner.discover_all(&config, &runs) => {}
            }
        });
    }
}

/// The built-in model list of every enabled mock provider, by provider
/// name: what the scheduler is told the mock "upstreams" serve.
fn mock_model_lists(config: &Config) -> HashMap<String, Vec<ModelInfo>> {
    config
        .providers
        .iter()
        .filter(|provider| provider.kind == ProviderKind::Mock && provider.enabled)
        .map(|provider| (provider.name.clone(), mock_models()))
        .collect()
}

/// Whether the upstream of `provider` is asked for its model list: enabled,
/// `discover` on, no models configured, and a real network provider.
pub(crate) fn wants_discovery(provider: &ProviderConfig) -> bool {
    provider.enabled
        && provider.discover
        && provider.models.is_empty()
        && provider.kind != ProviderKind::Mock
}

/// Applies every configuration the store publishes, and announces every
/// one it refuses, until the gateway is shut down or dropped.
///
/// What is announced (`config.reloaded` with `ok: true` or `ok: false`)
/// goes out in the order the store decided things, so that the last
/// announcement is true of the file as it is: a dashboard that was told
/// "refused" is told "applied" afterwards when the file was put right, and
/// never the other way round.
///
/// The store says what it did on two channels. The verdicts — applied or
/// refused — come in order on one; the configuration itself is the latest
/// value of the other, which the store sets *before* it sends the verdict
/// for it. Two things waiting on two channels have no order between them,
/// so this task takes its order from the verdicts alone: they are looked at
/// first, and an `Applied` among them is the moment to apply whatever
/// configuration is waiting. The configuration channel on its own only
/// serves the moment between the store setting the value and sending the
/// verdict, when no verdict is waiting and nothing can be overtaken.
async fn follow_config(
    inner: Weak<Inner>,
    mut changes: tokio::sync::watch::Receiver<Arc<Config>>,
    mut verdicts: tokio::sync::broadcast::Receiver<ConfigEvent>,
    shutdown: CancellationToken,
) {
    use tokio::sync::broadcast::error::RecvError;
    // A refusal was announced and nothing has been announced as applied
    // since.
    let mut refused = false;
    loop {
        // Whether a configuration is waiting to be applied, and whether the
        // store said in so many words that it applied one.
        let (waiting, applied) = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            verdict = verdicts.recv() => match verdict {
                Ok(ConfigEvent::Rejected { issues, .. }) => {
                    let Some(inner) = inner.upgrade() else {
                        break;
                    };
                    inner.config_rejected(&issues);
                    refused = true;
                    continue;
                }
                Ok(ConfigEvent::Applied { .. }) => {
                    (changes.has_changed().unwrap_or(false), true)
                }
                // This task fell far behind and the oldest verdicts are
                // gone. A configuration applied among them is still waiting
                // on the other channel, and comes before every verdict that
                // is left.
                Err(RecvError::Lagged(_)) => (changes.has_changed().unwrap_or(false), false),
                Err(RecvError::Closed) => break,
            },
            changed = changes.changed() => {
                if changed.is_err() {
                    // The store is gone, and the gateway with it.
                    break;
                }
                (true, false)
            }
        };
        let Some(inner) = inner.upgrade() else {
            break;
        };
        if waiting {
            let config = changes.borrow_and_update().clone();
            inner.apply_config(config).await;
            refused = false;
        } else if applied && refused {
            // The configuration this verdict is about was applied already,
            // on an earlier verdict that found it waiting — and a refusal
            // has been announced since. The verdict still says that the
            // store took the file after refusing it, and that has to be
            // the last word.
            inner.config_applied();
            refused = false;
        }
    }
}
