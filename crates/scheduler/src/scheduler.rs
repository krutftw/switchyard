//! The scheduler: registry + runtime state behind one thread-safe handle.

use crate::clock::{Clock, Ms, SystemClock, unix_ms};
use crate::error::PickError;
use crate::registry::{CredentialEntry, CredentialTable, ProviderEntry, Registry, SecretResolver};
use crate::state::{Affinity, CredState, Rotation, RotationKey, State};
use crate::types::{
    CredentialId, CredentialSnapshot, CredentialStatus, CredentialView, Lease, ModelCooldown,
    ModelEntry, ModelRoute, Outcome, PickRequest, ProviderSnapshot, Resolved, ResolvedTarget,
    RouteRef,
};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use switchyard_core::config::{Config, ProviderConfig, ProviderKind, Strategy};
use switchyard_core::{FailureClass, ModelInfo};

/// Decides which credential serves which model.
///
/// Cheap to share: wrap it in an `Arc` and call it from any thread. All
/// methods take `&self`; `pick` and `report` hold an internal lock only for
/// the few microseconds the bookkeeping takes.
///
/// # Time
///
/// [`Scheduler::pick`] and [`Scheduler::report`] take the current time as an
/// argument. The introspection calls ([`Scheduler::snapshot`],
/// [`Scheduler::models`], [`Scheduler::soonest_recovery`]) read the
/// scheduler's [`Clock`]. Use [`Scheduler::now`] as the source of the explicit
/// time so both agree.
pub struct Scheduler {
    clock: Arc<dyn Clock>,
    /// Serialises registry replacement so two rebuilds cannot interleave.
    rebuild: Mutex<()>,
    /// The immutable routing table, swapped wholesale.
    registry: RwLock<Arc<Registry>>,
    /// Everything that changes per request. Lock order: `rebuild`, then
    /// `state`, then `registry`.
    state: Mutex<State>,
}

impl Scheduler {
    /// Builds a scheduler from a configuration, using the system clock.
    ///
    /// `resolve` turns a secret as written in the config into its value
    /// (`switchyard_core::config::resolve_secret` in production). A credential
    /// whose secret cannot be resolved is kept — it shows up in
    /// [`Scheduler::snapshot`] with the reason — but is never selected.
    pub fn new(config: &Config, resolve: SecretResolver<'_>) -> Scheduler {
        Scheduler::with_clock(config, resolve, Arc::new(SystemClock::new()))
    }

    /// Like [`Scheduler::new`] with an explicit time source.
    pub fn with_clock(
        config: &Config,
        resolve: SecretResolver<'_>,
        clock: Arc<dyn Clock>,
    ) -> Scheduler {
        let credentials = Arc::new(CredentialTable::build(config, resolve));
        let registry = Registry::assemble(Arc::new(config.clone()), credentials, HashMap::new());
        Scheduler {
            clock,
            rebuild: Mutex::new(()),
            registry: RwLock::new(Arc::new(registry)),
            state: Mutex::new(State::default()),
        }
    }

    /// The scheduler's current time. Pass it to [`Scheduler::pick`] and
    /// [`Scheduler::report`].
    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry.read())
    }

    fn now_ms(&self) -> Ms {
        unix_ms(self.clock.now())
    }

    // ------------------------------------------------------------------
    // Building
    // ------------------------------------------------------------------

    /// Replaces the registry with one built from `config`.
    ///
    /// Runtime state (cooldowns, counters, latency, runtime disable) is kept
    /// for every credential whose id is unchanged and dropped for credentials
    /// that no longer exist. An id is a hash of provider name, kind, key and
    /// base URL, so editing models, weights, priorities, labels or proxies
    /// keeps the state while changing the key starts afresh.
    ///
    /// `discovered` holds upstream model lists by provider name. A provider
    /// missing from it keeps the list remembered from an earlier
    /// [`Scheduler::set_discovered`] / `rebuild`, provided the provider still
    /// exists with the same kind and base URL and still has `discover`
    /// enabled (mock providers always keep theirs). An empty list forgets the
    /// remembered one.
    ///
    /// When the new configuration switches cooldowns off, running cooldowns
    /// are cleared; when it switches session affinity off, bindings are
    /// forgotten.
    pub fn rebuild(
        &self,
        config: &Config,
        resolve: SecretResolver<'_>,
        discovered: HashMap<String, Vec<ModelInfo>>,
    ) {
        let _serial = self.rebuild.lock();
        let old = self.registry();
        let credentials = Arc::new(CredentialTable::build(config, resolve));

        let mut lists: HashMap<String, Vec<ModelInfo>> = HashMap::new();
        for provider in &config.providers {
            if let Some(list) = discovered.get(&provider.name) {
                if !list.is_empty() {
                    lists.insert(provider.name.clone(), list.clone());
                }
                continue;
            }
            let Some(remembered) = old.discovered.get(&provider.name) else {
                continue;
            };
            let same_endpoint = old.provider(&provider.name).is_some_and(|previous| {
                previous.config.kind == provider.kind
                    && previous.config.effective_base_url() == provider.effective_base_url()
            });
            let wanted = provider.discover || provider.kind == ProviderKind::Mock;
            if same_endpoint && wanted {
                lists.insert(provider.name.clone(), remembered.clone());
            }
        }

        let new = Arc::new(Registry::assemble(
            Arc::new(config.clone()),
            credentials,
            lists,
        ));

        let mut state = self.state.lock();
        *self.registry.write() = Arc::clone(&new);
        state
            .credentials
            .retain(|id, _| new.credential(id).is_some());
        state
            .rotation
            .reset_after_rebuild(|id| new.credential(id).is_some());
        state
            .affinity
            .retain_credentials(|id| new.credential(id).is_some());
        if !config.routing.cooldown.enabled {
            for credential in state.credentials.values_mut() {
                credential.clear_cooldowns();
            }
        }
        if !config.routing.session_affinity {
            state.affinity.clear();
        }
    }

    /// Records the model list an upstream reported for `provider` (or, for a
    /// `mock` provider, the list of mock models) and re-derives the model
    /// table. The list replaces the one remembered before; an empty list
    /// forgets it. Returns false when no such provider is configured.
    ///
    /// The list only decides *which* models the provider serves when its
    /// config lists none; its metadata is laid over the catalog's either way.
    pub fn set_discovered(&self, provider: &str, models: Vec<ModelInfo>) -> bool {
        let _serial = self.rebuild.lock();
        let old = self.registry();
        if old.provider(provider).is_none() {
            return false;
        }
        let mut lists = old.discovered.clone();
        if models.is_empty() {
            lists.remove(provider);
        } else {
            lists.insert(provider.to_string(), models);
        }
        let new = Registry::assemble(Arc::clone(&old.config), Arc::clone(&old.credentials), lists);
        // Taken so a concurrent pick sees either the old or the new table,
        // never a half-applied change.
        let _state = self.state.lock();
        *self.registry.write() = Arc::new(new);
        true
    }

    /// Problems found while building the registry that do not make the
    /// configuration invalid: credentials with unresolvable secrets, alias
    /// targets that match no model, alias cycles, shadowed names.
    pub fn warnings(&self) -> Vec<String> {
        self.registry().warnings.clone()
    }

    /// The configuration the registry was built from.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.registry().config)
    }

    /// The configuration of one provider.
    pub fn provider_config(&self, name: &str) -> Option<Arc<ProviderConfig>> {
        self.registry()
            .provider(name)
            .map(|p| Arc::clone(&p.config))
    }

    /// The credentials of a provider that could be used at all (enabled,
    /// usable, not disabled at runtime), in config order, cooldowns ignored.
    /// For calls that are not model requests: model discovery, connectivity
    /// tests. **The result contains secrets.**
    pub fn credentials(&self, provider: &str) -> Vec<CredentialView> {
        let state = self.state.lock();
        let registry = self.registry();
        let Some(entry) = registry.provider(provider) else {
            return Vec::new();
        };
        entry
            .credentials
            .iter()
            .filter_map(|&index| registry.credentials.entries.get(index))
            .filter(|c| selectable(c, state.credentials.get(&c.id), Strategy::RoundRobin))
            .map(|c| c.view.clone())
            .collect()
    }

    /// One credential by id, whatever its state. **Contains the secret.**
    pub fn credential(&self, id: &str) -> Option<CredentialView> {
        self.registry().credential(id).map(|c| c.view.clone())
    }

    // ------------------------------------------------------------------
    // Resolution
    // ------------------------------------------------------------------

    /// Resolves a client model name, which may carry a reasoning suffix
    /// (`gpt-5(high)`), to what can serve it.
    ///
    /// Lookup order for the name without its suffix: exact, then ignoring
    /// case (an alias wins over a model of the same spelling). After that
    /// only *floating* names are tolerated, the way the vendors' own aliases
    /// work: an undated name finds the newest registered snapshot
    /// (`claude-sonnet-4-5` → `claude-sonnet-4-5-20250929`), and `-latest`
    /// finds the name without it or its newest snapshot. A request that pins
    /// a snapshot (`gpt-4o-2024-05-13`) is never served by another model: if
    /// that exact name is not registered the model is unknown. If the name
    /// is still unknown and had a reasoning suffix, the whole string is
    /// tried as a literal model name, for model ids that contain
    /// parentheses.
    ///
    /// A plain model yields one target. An alias yields one target per
    /// configured target, in order, each with the depth its own suffix pins.
    pub fn resolve(&self, model: &str) -> Result<Resolved, PickError> {
        self.registry().resolve(model)
    }

    // ------------------------------------------------------------------
    // Selection
    // ------------------------------------------------------------------

    /// Chooses a credential for one upstream attempt.
    ///
    /// Targets are walked in order and the first one with a selectable
    /// credential wins. A credential is selectable for a target when it is
    /// enabled, usable, not ruled out by `tried` (see below), not resting as
    /// a whole and not resting for the target's upstream model (and, under
    /// the `weighted` strategy, has a non-zero weight). Among the selectable
    /// credentials only the
    /// highest priority present is considered; inside that tier the
    /// configured strategy decides:
    ///
    /// * `round-robin` — rotate in config order, per model and tier;
    /// * `fill-first` — always the first in config order;
    /// * `weighted` — smooth weighted round-robin (exact proportions over
    ///   each cycle of total weight);
    /// * `least-latency` — unmeasured credentials first, then the lowest
    ///   latency average; ties rotate. A credential whose last attempt on the
    ///   model failed within the last minute queues behind the others, so
    ///   one that keeps failing without resting (cooldowns switched off or
    ///   set to zero) cannot take the first attempt of every request.
    ///
    /// # Session affinity
    ///
    /// With `routing.session_affinity` and a session key, a session is bound
    /// to the credential that served it *and the model it served* (prompt
    /// caches are per model). While that pair is selectable it is used again:
    /// the binding beats priority, and a conversation that was served by a
    /// later alias target stays there after an earlier target recovers,
    /// because moving it would cost its cache. When the pair is not
    /// selectable the binding carries no weight — the same credential on
    /// another target is a different model — and the regular walk applies:
    /// first target with a selectable credential, then the strategy. Every
    /// pick (re)binds the session.
    ///
    /// # The tried list
    ///
    /// `tried` holds the ids of the credentials already attempted for this
    /// client request: add `lease.credential.id` when its attempt failed,
    /// and [`Scheduler::report`] the failure before picking again.
    ///
    /// A tried credential is not offered again for the model it failed on,
    /// so no (credential, upstream model) pair is attempted twice in one
    /// request. For a model with a single target that takes the credential
    /// out of the request altogether. For an alias it is still offered for
    /// the *other* targets: with `smart = [opus, sonnet]` on a single key, a
    /// failed `opus` attempt is followed by `sonnet` on the same key within
    /// the same request, as the alias contract demands ("the next target is
    /// used only when no credential can serve the previous one").
    ///
    /// Which targets a tried credential was already attempted on is read
    /// from the failures reported for it, so this holds with cooldowns off
    /// too. Requests running concurrently on one credential share that
    /// record, which can blur it. A pair is never offered while it rests;
    /// one whose cooldown is already over (or whose failure started none)
    /// may then come round to a request that attempted it before — by that
    /// time any other request would be given it too. `routing.max_attempts`
    /// is the bound on a request's attempts in every case.
    ///
    /// # Errors
    ///
    /// [`PickError::UnknownModel`] when no enabled provider stands behind
    /// the resolved routes any more; [`PickError::NoCredentials`] when none
    /// of the credentials could ever be selected; [`PickError::Exhausted`]
    /// when the request has attempted everything that could serve it;
    /// otherwise [`PickError::CoolingDown`] with the time until the first
    /// credential the request has not attempted (for that model) recovers.
    pub fn pick(&self, req: &PickRequest<'_>) -> Result<Lease, PickError> {
        let now = unix_ms(req.now);
        let mut guard = self.state.lock();
        let registry = self.registry();
        let state = &mut *guard;
        let routing = &registry.config.routing;

        let survey = survey(&registry, state, req.resolved, req.tried, now);

        let affinity_key = if routing.session_affinity {
            req.session
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Affinity::key(s, &req.resolved.base))
        } else {
            None
        };

        let mut chosen: Option<&Candidate<'_>> = None;
        if let Some(key) = &affinity_key {
            let ttl = secs_to_ms(routing.session_affinity_ttl_secs);
            if let Some(bound) = state.affinity.get(key, now, ttl) {
                // The binding is to a credential *serving a model*. The same
                // credential on another alias target would be a different
                // model with a cold cache: no reason to prefer it over the
                // regular target order.
                chosen = survey
                    .ready
                    .iter()
                    .flat_map(|target| target.iter())
                    .find(|c| {
                        c.entry.id == bound.credential && c.target.client_model == bound.model
                    });
            }
        }
        if chosen.is_none() {
            chosen = survey
                .ready
                .iter()
                .find(|target| !target.is_empty())
                .and_then(|target| {
                    select(routing.strategy, target, &mut state.rotation, &registry)
                });
        }

        let Some(candidate) = chosen else {
            let model = req.resolved.base.clone();
            return Err(if !survey.routed {
                PickError::UnknownModel { model }
            } else if survey.usable == 0 {
                PickError::NoCredentials { model }
            } else if survey.untried == 0 {
                PickError::Exhausted { model }
            } else {
                PickError::CoolingDown {
                    model,
                    retry_after: survey
                        .soonest
                        .map(|until| ms_to_duration(until.saturating_sub(now)))
                        .unwrap_or(Duration::ZERO),
                    last_error: survey.last_error.map(|(_, summary)| summary),
                }
            });
        };

        if let Some(key) = &affinity_key {
            state.affinity.bind(
                key,
                &candidate.entry.id,
                &candidate.target.client_model,
                now,
            );
        }
        // First use of the credential by this request: its earlier failures
        // are other requests'. (No state yet means no failures yet.)
        if !req.tried.contains(&candidate.entry.id)
            && let Some(credential) = state.credentials.get_mut(&candidate.entry.id)
        {
            credential.begin_request();
        }

        let client_model = candidate.target.client_model.clone();
        let mut info = (*candidate.route.info).clone();
        info.id = client_model.clone();
        let protocols = candidate.provider.protocols.clone();
        let upstream_protocol = if protocols.contains(&req.client_protocol) {
            req.client_protocol
        } else {
            protocols.first().copied().unwrap_or(req.client_protocol)
        };
        Ok(Lease {
            credential: candidate.entry.view.clone(),
            upstream_model: candidate.route.upstream_model.clone(),
            client_model,
            info,
            pinned_depth: candidate.target.pinned_depth,
            protocols,
            upstream_protocol,
            quirks: candidate.provider.quirks,
            provider_config: Arc::clone(&candidate.provider.config),
            affinity_key,
        })
    }

    // ------------------------------------------------------------------
    // Outcome
    // ------------------------------------------------------------------

    /// Records the result of the attempt made with `lease`.
    ///
    /// **Success** counts the request, clears the failure streak and the
    /// cooldown of the lease's model on that credential (a credential-wide
    /// cooldown is left to run out), folds `latency_ms` into the latency
    /// average (EWMA, alpha 0.3) and refreshes the session binding.
    ///
    /// **Failure** counts the request and, by [`FailureClass`]:
    ///
    /// | class | rests | for |
    /// |---|---|---|
    /// | `Request` | nothing (not counted as a failure either) | — |
    /// | `RateLimit` | this model on this credential | upstream wait if given, else `rate_limit_base_secs * 2^(streak-1)` capped at `rate_limit_max_secs` |
    /// | `Quota` | the whole credential | the larger of `quota_secs` and the upstream wait |
    /// | `Auth` | the whole credential | the larger of `auth_secs` and the upstream wait |
    /// | `ModelNotFound` | this model on this credential | upstream wait if given, else `model_not_found_secs` |
    /// | `Server`, `Transport` | this model on this credential | upstream wait if given, else `transient_secs` |
    ///
    /// The rate-limit streak grows by one per cooldown window, not per
    /// failure: requests that were already in flight when the window opened
    /// do not escalate it. Any success on the model resets it. A running
    /// cooldown is never shortened by a later failure. With
    /// `routing.cooldown.enabled = false` nothing rests: only the counters,
    /// `last_error` and the record of what failed (which [`Scheduler::pick`]
    /// reads for the tried list and for `least-latency`) are updated.
    ///
    /// The `*_secs` settings are used as written, however long; only an
    /// upstream-requested wait is bounded (seven days).
    ///
    /// A failure that is the credential's fault also releases the session
    /// binding so the conversation can move on: `Auth` and `Quota` release
    /// it whatever model the session was on, the per-model classes only when
    /// the session was bound to this model. `Request` and `Transport`
    /// failures leave it alone.
    ///
    /// Reports for credentials that no longer exist are ignored.
    pub fn report(&self, lease: &Lease, outcome: Outcome<'_>, now: SystemTime) {
        let now = unix_ms(now);
        let mut guard = self.state.lock();
        let registry = self.registry();
        let Some(entry) = registry.credential(&lease.credential.id) else {
            return;
        };
        let state = &mut *guard;
        let credential = state.credentials.entry(entry.id.clone()).or_default();
        match outcome {
            Outcome::Success { latency_ms } => {
                credential.record_success(&lease.upstream_model, latency_ms, now);
                if let Some(key) = &lease.affinity_key {
                    state
                        .affinity
                        .refresh_if(key, &entry.id, &lease.client_model, now);
                }
            }
            Outcome::Failure(error) => {
                credential.record_failure(
                    &lease.upstream_model,
                    error,
                    &registry.config.routing.cooldown,
                    &entry.view.api_key,
                    now,
                );
                if let Some(key) = &lease.affinity_key {
                    match error.class {
                        // Not the credential's fault: the binding stays.
                        FailureClass::Request | FailureClass::Transport => {}
                        // The credential as a whole is out, whichever model
                        // the session was on.
                        FailureClass::Auth | FailureClass::Quota => {
                            state.affinity.unbind_if(key, &entry.id, None);
                        }
                        // Only this model on this credential is in trouble.
                        FailureClass::RateLimit
                        | FailureClass::ModelNotFound
                        | FailureClass::Server => {
                            state
                                .affinity
                                .unbind_if(key, &entry.id, Some(&lease.client_model));
                        }
                    }
                }
            }
        }
        if let Some(credential) = state.credentials.get_mut(&entry.id) {
            credential.prune(now);
        }
    }

    /// Records the result of a call that was made with a credential outside
    /// the [`Scheduler::pick`] → [`Scheduler::report`] cycle: a connectivity
    /// test from the admin API, which addresses one credential directly
    /// whatever its state.
    ///
    /// The bookkeeping is that of [`Scheduler::report`] — counters, latency,
    /// failure streak and cooldowns by [`FailureClass`], for
    /// `upstream_model` on that credential — so a test that succeeds puts a
    /// resting model back into rotation and one that fails rests it like any
    /// failed request would. No session binding is involved: the call was
    /// not made on behalf of a conversation.
    ///
    /// Returns false (and records nothing) when no such credential exists.
    pub fn report_credential(
        &self,
        credential_id: &str,
        upstream_model: &str,
        outcome: Outcome<'_>,
        now: SystemTime,
    ) -> bool {
        let now = unix_ms(now);
        let mut guard = self.state.lock();
        let registry = self.registry();
        let Some(entry) = registry.credential(credential_id) else {
            return false;
        };
        let credential = guard.credentials.entry(entry.id.clone()).or_default();
        match outcome {
            Outcome::Success { latency_ms } => {
                credential.record_success(upstream_model, latency_ms, now);
            }
            Outcome::Failure(error) => {
                credential.record_failure(
                    upstream_model,
                    error,
                    &registry.config.routing.cooldown,
                    &entry.view.api_key,
                    now,
                );
            }
        }
        credential.prune(now);
        true
    }

    // ------------------------------------------------------------------
    // Introspection and control
    // ------------------------------------------------------------------

    /// Runtime view of every provider and credential, in config order.
    /// Contains no secrets.
    pub fn snapshot(&self) -> Vec<ProviderSnapshot> {
        let now = self.now_ms();
        let state = self.state.lock();
        let registry = self.registry();
        registry
            .providers
            .iter()
            .map(|provider| ProviderSnapshot {
                name: provider.config.name.clone(),
                kind: provider.config.kind,
                enabled: provider.config.enabled,
                credentials: provider
                    .credentials
                    .iter()
                    .filter_map(|&index| registry.credentials.entries.get(index))
                    .map(|entry| {
                        credential_snapshot(provider, entry, state.credentials.get(&entry.id), now)
                    })
                    .collect(),
                models: provider.models.len(),
            })
            .collect()
    }

    /// Runtime view of one credential.
    pub fn credential_snapshot(&self, id: &str) -> Option<CredentialSnapshot> {
        let now = self.now_ms();
        let state = self.state.lock();
        let registry = self.registry();
        let entry = registry.credential(id)?;
        let provider = registry.providers.get(entry.provider)?;
        Some(credential_snapshot(
            provider,
            entry,
            state.credentials.get(&entry.id),
            now,
        ))
    }

    /// The client-facing model table: every alias and every model name, with
    /// the providers behind it and how many of their credentials could serve
    /// it right now. Sorted by name. Includes names hidden from listings.
    pub fn models(&self) -> Vec<ModelEntry> {
        let now = self.now_ms();
        let state = self.state.lock();
        let registry = self.registry();
        let strategy = registry.config.routing.strategy;

        let route_view = |route: &RouteRef| -> ModelRoute {
            let credentials: Vec<&CredentialEntry> = registry
                .provider(&route.provider)
                .map(|p| {
                    p.credentials
                        .iter()
                        .filter_map(|&i| registry.credentials.entries.get(i))
                        .collect()
                })
                .unwrap_or_default();
            let available = credentials
                .iter()
                .filter(|entry| {
                    let st = state.credentials.get(&entry.id);
                    selectable(entry, st, strategy)
                        && st.is_none_or(|s| s.blocked(&route.upstream_model, now).is_none())
                })
                .count();
            ModelRoute {
                provider: route.provider.clone(),
                upstream_model: route.upstream_model.clone(),
                credentials_total: credentials.len(),
                credentials_available: available,
            }
        };

        let mut names: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for model in registry.models() {
            if registry.is_shadowed(&model.name) {
                continue;
            }
            names.insert(model.name.as_str());
            out.push(ModelEntry {
                name: model.name.clone(),
                info: model.info.clone(),
                hidden: model.hidden,
                alias_targets: None,
                routes: model.routes.iter().map(route_view).collect(),
            });
        }
        for alias in &registry.aliases {
            // An alias without any routable target is shown (so the mistake
            // is visible) unless a real model owns the name.
            if alias.targets.is_empty() && names.contains(alias.name.as_str()) {
                continue;
            }
            let mut routes: Vec<ModelRoute> = Vec::new();
            for target in &alias.targets {
                let Some(model) = registry.models().get(target.model) else {
                    continue;
                };
                for route in &model.routes {
                    let view = route_view(route);
                    if !routes.contains(&view) {
                        routes.push(view);
                    }
                }
            }
            out.push(ModelEntry {
                name: alias.name.clone(),
                info: alias.info.clone(),
                hidden: alias.hidden,
                alias_targets: Some(alias.raw_targets.clone()),
                routes,
            });
        }
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// What model listings show: every client-facing name (aliases included)
    /// that has at least one route and is not hidden, without duplicates,
    /// sorted by name. `id` is the client-facing name.
    pub fn visible_models(&self) -> Vec<ModelInfo> {
        self.registry().visible_models()
    }

    /// How long until a request for `model` could be served, when it cannot
    /// be right now: the time until the first resting credential recovers.
    /// `None` when a credential is ready, the model is unknown, or no
    /// credential could ever serve it.
    pub fn soonest_recovery(&self, model: &str) -> Option<Duration> {
        let resolved = self.resolve(model).ok()?;
        self.soonest_recovery_of(&resolved)
    }

    /// [`Scheduler::soonest_recovery`] for routes that are already resolved
    /// — and possibly narrowed with [`Resolved::retain_routes`], which a
    /// lookup by name would undo.
    ///
    /// Every credential behind the routes counts, whether or not a request
    /// has tried it: the answer to "how long until *anything* could serve
    /// this", which is what a request that has run out of credentials needs
    /// to decide whether waiting is worth it. Changes nothing (no rotation,
    /// no session binding).
    pub fn soonest_recovery_of(&self, resolved: &Resolved) -> Option<Duration> {
        let now = self.now_ms();
        let state = self.state.lock();
        let registry = self.registry();
        let survey = survey(&registry, &state, resolved, &[], now);
        if survey.ready.iter().any(|target| !target.is_empty()) {
            return None;
        }
        survey
            .soonest
            .map(|until| ms_to_duration(until.saturating_sub(now)))
    }

    /// Number of session-affinity bindings currently remembered (expired
    /// ones are dropped lazily, so this is an upper bound on live sessions).
    pub fn session_bindings(&self) -> usize {
        self.state.lock().affinity.len()
    }

    /// Clears every cooldown and failure streak of a credential (counters
    /// stay). Returns false when no such credential exists.
    pub fn reset_cooldowns(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        if self.registry().credential(id).is_none() {
            return false;
        }
        if let Some(credential) = state.credentials.get_mut(id) {
            credential.clear_cooldowns();
        }
        true
    }

    /// Takes a credential out of rotation (or puts it back) until the next
    /// call or a restart; the config file is not touched. The flag survives
    /// rebuilds while the credential's id is unchanged. Returns false when no
    /// such credential exists.
    pub fn set_runtime_disabled(&self, id: &str, disabled: bool) -> bool {
        let mut state = self.state.lock();
        if self.registry().credential(id).is_none() {
            return false;
        }
        state
            .credentials
            .entry(id.to_string())
            .or_default()
            .runtime_disabled = disabled;
        true
    }
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registry = self.registry();
        f.debug_struct("Scheduler")
            .field("providers", &registry.providers.len())
            .field("credentials", &registry.credentials.entries.len())
            .field("models", &registry.models().len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Selection internals
// ---------------------------------------------------------------------------

/// A credential that could serve one target right now.
struct Candidate<'a> {
    entry: &'a CredentialEntry,
    provider: &'a ProviderEntry,
    route: &'a RouteRef,
    target: &'a ResolvedTarget,
    /// Latency average in whole milliseconds, when measured.
    latency: Option<u64>,
    /// The credential's last attempt on this model failed moments ago.
    on_probation: bool,
}

/// Availability of everything behind a resolved request.
struct Survey<'a> {
    /// Ready candidates per target, in target order.
    ready: Vec<Vec<Candidate<'a>>>,
    /// Whether any route leads to an enabled provider.
    routed: bool,
    /// Credentials that could be selected if nothing were resting or tried.
    usable: usize,
    /// Of those, the ones this request may still use: not in the tried
    /// list, or tried on other targets only.
    untried: usize,
    /// Earliest recovery among untried resting credentials.
    soonest: Option<Ms>,
    /// Most recent failure among them: (time, summary).
    last_error: Option<(Ms, String)>,
}

fn survey<'a>(
    registry: &'a Registry,
    state: &State,
    resolved: &'a Resolved,
    tried: &[CredentialId],
    now: Ms,
) -> Survey<'a> {
    let strategy = registry.config.routing.strategy;
    let mut survey = Survey {
        ready: Vec::with_capacity(resolved.targets.len()),
        routed: false,
        usable: 0,
        untried: 0,
        soonest: None,
        last_error: None,
    };
    for (position, target) in resolved.targets.iter().enumerate() {
        let mut ready = Vec::new();
        for route in &target.routes {
            let Some(provider) = registry.provider(&route.provider) else {
                continue;
            };
            if !provider.config.enabled {
                continue;
            }
            survey.routed = true;
            for &index in &provider.credentials {
                let Some(entry) = registry.credentials.entries.get(index) else {
                    continue;
                };
                let credential = state.credentials.get(&entry.id);
                if !selectable(entry, credential, strategy) {
                    continue;
                }
                survey.usable += 1;
                if tried.contains(&entry.id)
                    && !credential.is_some_and(|c| {
                        tried_on_other_targets_only(c, resolved, position, route, now)
                    })
                {
                    continue;
                }
                survey.untried += 1;
                let blocked = credential.and_then(|c| c.blocked(&route.upstream_model, now));
                if let Some(cooldown) = blocked {
                    if survey.soonest.is_none_or(|s| cooldown.until < s) {
                        survey.soonest = Some(cooldown.until);
                    }
                    // Quote the failure that explains *this* rest. The
                    // credential's `last_error` may be about another model.
                    if let Some((at, summary)) =
                        credential.and_then(|c| c.rest_cause(&route.upstream_model, now))
                        && survey
                            .last_error
                            .as_ref()
                            .is_none_or(|(seen, _)| at > *seen)
                    {
                        survey.last_error = Some((at, summary.to_string()));
                    }
                    continue;
                }
                ready.push(Candidate {
                    entry,
                    provider,
                    route,
                    target,
                    latency: credential
                        .and_then(|c| c.latency_ms)
                        .map(|ms| ms.round().max(0.0) as u64),
                    on_probation: credential
                        .is_some_and(|c| c.on_probation(&route.upstream_model, now)),
                });
            }
        }
        survey.ready.push(ready);
    }
    survey
}

/// Whether a credential from the tried list was attempted, in this client
/// request, on *other* targets only — so that the model of the target at
/// `position` is still new to the request and may be offered.
///
/// The tried list names credentials, not models, so this is read off the
/// credential's failure history. The credential is offered when
///
/// * this target's model has not failed on it since the request first got
///   the credential, and
/// * another target's model failed on it more recently than this one did —
///   the attempt that put the credential on the tried list — and
/// * every *earlier* target it serves either rests on it or failed on it
///   more recently than this target's model did: targets are walked in
///   order, so the request cannot have got here otherwise.
///
/// A failed attempt on this target ends the offer: no (credential, model)
/// pair is attempted twice in one request, with or without cooldowns.
/// Usually the other target is an earlier one (`[opus, sonnet]`: opus
/// failed, sonnet is next). It is a later one when session affinity sent
/// the request straight to the fallback target it was bound to; the
/// preferred target is then still open to it.
///
/// For one request at a time this is exact. Requests that use a credential
/// concurrently share its history (the first condition is reckoned from the
/// latest request that was newly given the credential), so for them it is a
/// judgement, not a proof. What always holds: a pair that rests is not
/// offered. A pair whose rest is already over, or whose failure started
/// none — where the configuration says it may be used again — can come
/// round a second time when other requests' picks and failures fell in
/// between; `routing.max_attempts` bounds that as it bounds everything.
fn tried_on_other_targets_only(
    credential: &CredState,
    resolved: &Resolved,
    position: usize,
    route: &RouteRef,
    now: Ms,
) -> bool {
    let model = route.upstream_model.as_str();
    if credential.failed_since_request_began(model, now) {
        return false;
    }
    let mut attempted_elsewhere = false;
    for (index, target) in resolved.targets.iter().enumerate() {
        if index == position {
            continue;
        }
        // The credential's provider serves a target through one route.
        for other in target
            .routes
            .iter()
            .filter(|r| r.provider == route.provider)
        {
            if credential.failed_more_recently(&other.upstream_model, model, now) {
                attempted_elsewhere = true;
            } else if index < position && credential.blocked(&other.upstream_model, now).is_none() {
                return false;
            }
        }
    }
    attempted_elsewhere
}

/// Whether a credential may be selected at all, cooldowns aside.
fn selectable(entry: &CredentialEntry, state: Option<&CredState>, strategy: Strategy) -> bool {
    if entry.disabled || entry.unusable.is_some() || state.is_some_and(|s| s.runtime_disabled) {
        return false;
    }
    // Weight 0 removes a credential from weighted rotation altogether.
    strategy != Strategy::Weighted || entry.weight > 0
}

/// Applies priority tiers and the strategy to the ready candidates of one
/// target.
fn select<'c, 'a>(
    strategy: Strategy,
    ready: &'c [Candidate<'a>],
    rotation: &mut Rotation,
    registry: &Registry,
) -> Option<&'c Candidate<'a>> {
    let top = ready.iter().map(|c| c.entry.priority).max()?;
    let mut tier: Vec<&'c Candidate<'a>> =
        ready.iter().filter(|c| c.entry.priority == top).collect();
    tier.sort_by_key(|c| c.entry.order);
    let first = *tier.first()?;
    let key: RotationKey = (first.target.client_model.to_lowercase(), top);

    match strategy {
        Strategy::FillFirst => Some(first),
        Strategy::RoundRobin => rotate(rotation, key, &tier, registry),
        Strategy::Weighted => {
            let weights: Vec<(&str, u32)> = tier
                .iter()
                .map(|c| (c.entry.id.as_str(), c.entry.weight))
                .collect();
            let index = rotation.weighted(key, &weights);
            tier.get(index).copied()
        }
        Strategy::LeastLatency => {
            // A failure yields no latency sample, so without this a
            // credential that keeps failing would stay "unmeasured" (or keep
            // a flattering old average) and, whenever its failures start no
            // cooldown, take the first attempt of every request. One that
            // failed moments ago therefore queues behind the others.
            let steady: Vec<&'c Candidate<'a>> =
                tier.iter().copied().filter(|c| !c.on_probation).collect();
            let field = if steady.is_empty() { tier } else { steady };
            // Unmeasured credentials go first so every one gets a sample;
            // after that the fastest wins and equals share the load.
            let unmeasured: Vec<&'c Candidate<'a>> = field
                .iter()
                .copied()
                .filter(|c| c.latency.is_none())
                .collect();
            let pool = if unmeasured.is_empty() {
                let best = field.iter().filter_map(|c| c.latency).min();
                field
                    .iter()
                    .copied()
                    .filter(|c| c.latency == best)
                    .collect()
            } else {
                unmeasured
            };
            rotate(rotation, key, &pool, registry)
        }
    }
}

/// Round-robin over `pool` (sorted by config order).
fn rotate<'c, 'a>(
    rotation: &mut Rotation,
    key: RotationKey,
    pool: &[&'c Candidate<'a>],
    registry: &Registry,
) -> Option<&'c Candidate<'a>> {
    let candidates: Vec<(usize, &str)> = pool
        .iter()
        .map(|c| (c.entry.order, c.entry.id.as_str()))
        .collect();
    let index = rotation.round_robin(key, &candidates, |id| {
        registry.credential(id).map(|entry| entry.order)
    });
    pool.get(index).copied()
}

fn credential_snapshot(
    provider: &ProviderEntry,
    entry: &CredentialEntry,
    state: Option<&CredState>,
    now: Ms,
) -> CredentialSnapshot {
    let runtime_disabled = state.is_some_and(|s| s.runtime_disabled);
    let disabled = entry.disabled || runtime_disabled;

    let mut model_cooldowns: Vec<ModelCooldown> = state
        .map(|s| {
            s.models
                .iter()
                .filter_map(|(model, m)| {
                    let cooldown = m.cooldown.filter(|c| c.until > now)?;
                    Some(ModelCooldown {
                        model: model.clone(),
                        until: cooldown.until,
                        reason: cooldown.reason,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    model_cooldowns.sort_by(|a, b| a.model.cmp(&b.model));

    // The credential counts as cooling when it rests as a whole, or when
    // every model its provider serves rests on it — either way it cannot
    // take a request. In the second case the first model to recover ends it.
    let whole = state.and_then(|s| s.credential_cooldown(now));
    let all_models = || {
        let state = state?;
        if provider.models.is_empty() {
            return None;
        }
        let mut earliest = None;
        for model in &provider.models {
            let cooldown = state.model_cooldown(&model.upstream, now)?;
            if earliest.is_none_or(|e: crate::state::Cooldown| cooldown.until < e.until) {
                earliest = Some(cooldown);
            }
        }
        earliest
    };
    let cooling = whole.or_else(all_models);

    let status = if disabled {
        CredentialStatus::Disabled
    } else if entry.unusable.is_some() {
        CredentialStatus::Unusable
    } else if cooling.is_some() {
        CredentialStatus::Cooling
    } else {
        CredentialStatus::Ready
    };

    CredentialSnapshot {
        id: entry.id.clone(),
        label: entry.view.label.clone(),
        masked_key: entry.masked_key.clone(),
        disabled,
        usable: entry.unusable.is_none(),
        unusable_reason: entry.unusable.clone(),
        status,
        cooldown_until: cooling.map(|c| c.until),
        cooldown_reason: cooling.map(|c| c.reason),
        model_cooldowns,
        requests: state.map_or(0, |s| s.requests),
        successes: state.map_or(0, |s| s.successes),
        failures: state.map_or(0, |s| s.failures),
        consecutive_failures: state.map_or(0, |s| s.consecutive_failures),
        latency_ms: state
            .and_then(|s| s.latency_ms)
            .map(|ms| ms.round().max(0.0) as u64),
        last_used_at: state.and_then(|s| s.last_used),
        last_error: state.and_then(|s| s.last_error.clone()),
        weight: entry.weight,
        priority: entry.priority,
    }
}

fn secs_to_ms(secs: u64) -> Ms {
    Ms::try_from(secs.saturating_mul(1000)).unwrap_or(Ms::MAX)
}

fn ms_to_duration(ms: Ms) -> Duration {
    Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Scheduler>();
        assert_send_sync::<Lease>();
        assert_send_sync::<Resolved>();
    }

    #[test]
    fn time_conversions_saturate() {
        assert_eq!(secs_to_ms(2), 2_000);
        assert_eq!(secs_to_ms(u64::MAX), Ms::MAX);
        assert_eq!(ms_to_duration(1_500), Duration::from_millis(1_500));
        assert_eq!(ms_to_duration(-5), Duration::ZERO);
    }
}
