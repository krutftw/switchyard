//! The registry: credentials built from the config, the models each provider
//! serves, and the table of client-facing names (models and aliases).
//!
//! A registry is immutable. The scheduler replaces it wholesale when the
//! config changes or a provider's model list is discovered; runtime state
//! lives elsewhere and is matched to credentials by id.

use crate::catalog::{catalog, strip_version_suffix};
use crate::error::PickError;
use crate::types::{CredentialId, CredentialView, Resolved, ResolvedTarget, RouteRef};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use switchyard_core::config::{Config, ProviderConfig, ProviderKind, is_secret_reference};
use switchyard_core::reasoning::parse_model_suffix;
use switchyard_core::util::mask_secret;
use switchyard_core::{Depth, ModelInfo, Protocol, Quirks};

/// Resolves a secret as written in the config (`sk-…`, `env:NAME`,
/// `${NAME}`) to its value. `Err` carries the name of the unset variable.
/// Production passes `switchyard_core::config::resolve_secret`.
pub type SecretResolver<'a> = &'a dyn Fn(&str) -> Result<String, String>;

/// Alias-in-alias nesting deeper than this is cut (and reported).
const MAX_ALIAS_DEPTH: usize = 8;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// One credential as configured.
#[derive(Debug)]
pub(crate) struct CredentialEntry {
    pub id: CredentialId,
    /// Index into `Config::providers`.
    pub provider: usize,
    /// Position in config order across all providers; the stable order used
    /// by fill-first and for rotation.
    pub order: usize,
    pub view: CredentialView,
    pub masked_key: String,
    /// Switched off in the config.
    pub disabled: bool,
    /// Why the credential can never be selected, if so.
    pub unusable: Option<String>,
    pub weight: u32,
    pub priority: i32,
}

/// Every credential of a config, with secrets resolved.
#[derive(Debug, Default)]
pub(crate) struct CredentialTable {
    pub entries: Vec<CredentialEntry>,
    /// Credential indexes per provider, parallel to `Config::providers`.
    pub by_provider: Vec<Vec<usize>>,
    pub index: HashMap<CredentialId, usize>,
    pub warnings: Vec<String>,
}

impl CredentialTable {
    pub fn build(config: &Config, resolve: SecretResolver<'_>) -> CredentialTable {
        let mut table = CredentialTable::default();
        let mut seen_ids: HashMap<String, usize> = HashMap::new();
        for (provider_index, provider) in config.providers.iter().enumerate() {
            let base_url = provider.effective_base_url();
            let headers: IndexMap<String, String> = provider
                .headers
                .iter()
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .collect();
            let mut indexes = Vec::new();
            for (position, cred) in provider.all_credentials().iter().enumerate() {
                let raw_key = cred.api_key.trim();
                let service_account = if provider.kind == ProviderKind::Vertex {
                    cred.service_account_file.trim()
                } else {
                    ""
                };

                let mut api_key = String::new();
                let mut unusable = None;
                let masked_key;
                // What identifies the credential for hashing. Never stored.
                let material;
                if !raw_key.is_empty() {
                    match resolve(raw_key) {
                        Ok(value) if !value.trim().is_empty() => {
                            api_key = value.trim().to_string();
                            masked_key = mask_secret(&api_key);
                            material = api_key.clone();
                        }
                        Ok(_) => {
                            unusable = Some("api key is empty".to_string());
                            masked_key = shown_reference(raw_key);
                            material = format!("unresolved:{raw_key}");
                        }
                        Err(variable) => {
                            unusable = Some(format!("environment variable {variable} is not set"));
                            masked_key = shown_reference(raw_key);
                            material = format!("unresolved:{raw_key}");
                        }
                    }
                } else if !service_account.is_empty() {
                    masked_key = file_name(service_account).to_string();
                    material = format!("service-account:{service_account}");
                } else {
                    if provider.kind.needs_credentials() {
                        unusable = Some("no api key configured".to_string());
                    }
                    masked_key = String::new();
                    material = String::new();
                }

                let hash = credential_hash(&provider.name, provider.kind, &material, &base_url);
                let mut id = format!("{}:{hash}", provider.name.trim());
                let repeats = seen_ids.entry(id.clone()).or_insert(0);
                if *repeats > 0 {
                    id = format!("{id}-{repeats}");
                }
                *repeats += 1;

                let label = if !cred.label.trim().is_empty() {
                    cred.label.trim().to_string()
                } else if !masked_key.is_empty() {
                    masked_key.clone()
                } else {
                    provider.name.trim().to_string()
                };

                // Nobody expects a switched-off credential to work; warning
                // about it would only be noise.
                if let Some(reason) = &unusable
                    && provider.enabled
                    && !cred.disabled
                {
                    table.warnings.push(format!(
                        "provider `{}`: credential {} ({label}) is unusable: {reason}",
                        provider.name,
                        position + 1
                    ));
                }

                let proxy = [
                    cred.proxy.trim(),
                    provider.proxy.trim(),
                    config.upstream.proxy.trim(),
                ]
                .into_iter()
                .find(|p| !p.is_empty())
                .unwrap_or("")
                .to_string();

                let order = table.entries.len();
                indexes.push(order);
                table.index.insert(id.clone(), order);
                table.entries.push(CredentialEntry {
                    id: id.clone(),
                    provider: provider_index,
                    order,
                    view: CredentialView {
                        id,
                        provider: provider.name.clone(),
                        kind: provider.kind,
                        label,
                        api_key,
                        service_account_file: service_account.to_string(),
                        base_url: base_url.clone(),
                        proxy,
                        headers: headers.clone(),
                        project: provider.project.trim().to_string(),
                        location: provider.location.trim().to_string(),
                    },
                    masked_key,
                    disabled: cred.disabled,
                    unusable,
                    weight: cred.effective_weight(),
                    priority: cred.priority.unwrap_or(provider.priority),
                });
            }
            table.by_provider.push(indexes);
        }
        table
    }
}

/// What to show for a secret that could not be resolved: references are safe
/// to display as written, anything else is masked.
fn shown_reference(raw: &str) -> String {
    if is_secret_reference(raw) {
        raw.to_string()
    } else {
        mask_secret(raw)
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// First 12 hex characters of SHA-256 over the NUL-separated parts.
fn credential_hash(provider: &str, kind: ProviderKind, material: &str, base_url: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [provider, kind.as_str(), material, base_url] {
        hasher.update(part.trim().as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Providers and names
// ---------------------------------------------------------------------------

/// A model served by one provider.
#[derive(Debug)]
pub(crate) struct ProviderModel {
    /// Id sent upstream.
    pub upstream: String,
    /// Client-facing name, without the provider prefix.
    pub client: String,
    pub info: Arc<ModelInfo>,
}

#[derive(Debug)]
pub(crate) struct ProviderEntry {
    pub config: Arc<ProviderConfig>,
    /// Indexes into [`CredentialTable::entries`], in config order.
    pub credentials: Vec<usize>,
    pub models: Vec<ProviderModel>,
    pub protocols: Vec<Protocol>,
    pub quirks: Quirks,
}

/// A client-facing model name and the providers behind it.
#[derive(Debug)]
pub(crate) struct ModelName {
    pub name: String,
    pub info: ModelInfo,
    pub routes: Vec<RouteRef>,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AliasTarget {
    /// Index into [`Registry::models`].
    pub model: usize,
    pub pinned: Option<Depth>,
    /// Index into [`AliasName::raw_targets`] of the configured target this
    /// one came from (directly, or through nested aliases). When several
    /// configured targets lead to the same model and depth, the first.
    pub origin: usize,
}

/// A virtual model.
#[derive(Debug)]
pub(crate) struct AliasName {
    pub name: String,
    pub info: ModelInfo,
    /// Targets as written in the config.
    pub raw_targets: Vec<String>,
    /// Targets resolved to models, nested aliases flattened.
    pub targets: Vec<AliasTarget>,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Alias(usize),
    Model(usize),
}

#[derive(Debug, Default)]
struct NameMaps {
    exact: HashMap<String, usize>,
    lower: HashMap<String, usize>,
}

impl NameMaps {
    fn insert(&mut self, name: &str, index: usize) {
        self.exact.entry(name.to_string()).or_insert(index);
        self.lower.entry(name.to_lowercase()).or_insert(index);
    }
}

#[derive(Debug, Default)]
struct ModelTable {
    models: Vec<ModelName>,
    maps: NameMaps,
    /// Lower-cased name without its version suffix → the newest model
    /// carrying such a suffix (`claude-sonnet-4-5` →
    /// `claude-sonnet-4-5-20250929`).
    undated: HashMap<String, usize>,
    /// (model index, provider ordinal) of every route added, so that a
    /// provider is listed once per name without scanning the name's routes:
    /// a popular model has one route per provider, and comparing each new
    /// one with all the others made building the table quadratic.
    routed: HashSet<(usize, usize)>,
}

impl ModelTable {
    /// Adds `route` to `name`. `provider` identifies the route's provider
    /// (providers of one name share an ordinal); a second route of the same
    /// provider to the same name is dropped.
    fn add_route(&mut self, name: String, provider: usize, route: RouteRef) {
        let index = match self.maps.exact.get(&name) {
            Some(&index) => index,
            None => {
                let index = self.models.len();
                let mut info = (*route.info).clone();
                info.id = name.clone();
                self.maps.insert(&name, index);
                if let Some(base) = strip_version_suffix(&name) {
                    let key = base.to_lowercase();
                    let newer = self.undated.get(&key).is_none_or(|&current| {
                        self.models[current].name.to_lowercase() < name.to_lowercase()
                    });
                    if newer {
                        self.undated.insert(key, index);
                    }
                }
                self.models.push(ModelName {
                    name,
                    info,
                    routes: Vec::new(),
                    hidden: false,
                });
                index
            }
        };
        if self.routed.insert((index, provider)) {
            self.models[index].routes.push(route);
        }
    }

    /// Model-only lookup with every fallback.
    fn find(&self, name: &str) -> Option<usize> {
        match find_name(&NameMaps::default(), self, name) {
            Some(Found::Model(index)) => Some(index),
            _ => None,
        }
    }
}

/// Looks a client-facing name up: exact, then ignoring case (an alias beats
/// a model of the same spelling at each step). Failing that, a *floating*
/// name is matched the way the vendors' own aliases work:
///
/// * an undated name finds the newest registered snapshot of it
///   (`claude-sonnet-4-5` → `claude-sonnet-4-5-20250929`);
/// * `name-latest` finds `name`, or the newest registered snapshot of it.
///
/// The opposite is deliberately not done: `gpt-4o-2024-05-13` names one
/// particular snapshot, with its own behaviour and price, and must not be
/// answered by whatever `gpt-4o` currently is. A pinned name that is not
/// registered is unknown.
fn find_name(aliases: &NameMaps, models: &ModelTable, name: &str) -> Option<Found> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let direct = |name: &str| -> Option<Found> {
        if let Some(&i) = aliases.exact.get(name) {
            return Some(Found::Alias(i));
        }
        if let Some(&i) = models.maps.exact.get(name) {
            return Some(Found::Model(i));
        }
        let lower = name.to_lowercase();
        if let Some(&i) = aliases.lower.get(&lower) {
            return Some(Found::Alias(i));
        }
        models.maps.lower.get(&lower).map(|&i| Found::Model(i))
    };
    let newest_snapshot = |name: &str| -> Option<Found> {
        models
            .undated
            .get(&name.to_lowercase())
            .map(|&i| Found::Model(i))
    };
    direct(name).or_else(|| newest_snapshot(name)).or_else(|| {
        let base = strip_latest_suffix(name)?;
        direct(base).or_else(|| newest_snapshot(base))
    })
}

/// `name-latest` → `name` (the suffix in any case). `None` for other names.
fn strip_latest_suffix(name: &str) -> Option<&str> {
    const SUFFIX: &str = "-latest";
    let cut = name
        .len()
        .checked_sub(SUFFIX.len())
        .filter(|&cut| cut > 0)?;
    name.get(cut..)
        .filter(|tail| tail.eq_ignore_ascii_case(SUFFIX))
        .and_then(|_| name.get(..cut))
}

/// The immutable routing table.
#[derive(Debug)]
pub(crate) struct Registry {
    pub config: Arc<Config>,
    pub credentials: Arc<CredentialTable>,
    /// Model lists reported by upstreams (or supplied for `mock`), by
    /// provider name.
    pub discovered: HashMap<String, Vec<ModelInfo>>,
    pub providers: Vec<ProviderEntry>,
    provider_index: HashMap<String, usize>,
    table: ModelTable,
    pub aliases: Vec<AliasName>,
    alias_maps: NameMaps,
    pub warnings: Vec<String>,
}

impl Registry {
    pub fn assemble(
        config: Arc<Config>,
        credentials: Arc<CredentialTable>,
        discovered: HashMap<String, Vec<ModelInfo>>,
    ) -> Registry {
        let mut warnings = credentials.warnings.clone();

        // Providers and the models each one serves.
        let mut providers = Vec::with_capacity(config.providers.len());
        let mut provider_index = HashMap::new();
        for (index, provider) in config.providers.iter().enumerate() {
            let models = provider_models(provider, discovered.get(&provider.name));
            let quirks = provider.quirks();
            provider_index.entry(provider.name.clone()).or_insert(index);
            providers.push(ProviderEntry {
                config: Arc::new(provider.clone()),
                credentials: credentials
                    .by_provider
                    .get(index)
                    .cloned()
                    .unwrap_or_default(),
                models,
                protocols: provider.protocols(),
                quirks,
            });
        }

        // Client-facing model names.
        let mut table = ModelTable::default();
        for (index, provider) in providers.iter().enumerate() {
            if !provider.config.enabled {
                continue;
            }
            // Providers are told apart by name, as routes are.
            let ordinal = provider_index
                .get(&provider.config.name)
                .copied()
                .unwrap_or(index);
            let prefix = provider.config.normalized_prefix();
            for model in &provider.models {
                let route = RouteRef {
                    provider: provider.config.name.clone(),
                    kind: provider.config.kind,
                    upstream_model: model.upstream.clone(),
                    info: Arc::clone(&model.info),
                };
                if !prefix.is_empty() {
                    table.add_route(format!("{prefix}/{}", model.client), ordinal, route.clone());
                }
                if prefix.is_empty() || !config.routing.force_model_prefix {
                    table.add_route(model.client.clone(), ordinal, route);
                }
            }
        }

        let (aliases, alias_maps) = build_aliases(&config, &mut table, &mut warnings);

        let mut unique = HashSet::new();
        warnings.retain(|w| unique.insert(w.clone()));

        Registry {
            config,
            credentials,
            discovered,
            providers,
            provider_index,
            table,
            aliases,
            alias_maps,
            warnings,
        }
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderEntry> {
        self.provider_index
            .get(name)
            .and_then(|&i| self.providers.get(i))
    }

    pub fn credential(&self, id: &str) -> Option<&CredentialEntry> {
        self.credentials
            .index
            .get(id)
            .and_then(|&i| self.credentials.entries.get(i))
    }

    pub fn models(&self) -> &[ModelName] {
        &self.table.models
    }

    /// Whether an alias of exactly this name replaces the model in lookups
    /// and listings.
    pub fn is_shadowed(&self, model_name: &str) -> bool {
        self.alias_maps.exact.contains_key(model_name)
    }

    /// Whether a provider serves a model of this name, ignoring case: the
    /// model an alias of that name hides (see [`ModelEntry::shadows_model`]).
    ///
    /// [`ModelEntry::shadows_model`]: crate::ModelEntry::shadows_model
    pub fn serves_model_named(&self, name: &str) -> bool {
        self.table.maps.lower.contains_key(&name.to_lowercase())
    }

    /// How many client-facing names a request can be routed by: every model
    /// that no alias replaces and every alias with at least one routable
    /// target. Names hidden from listings count; ignored aliases do not.
    pub fn routable_names(&self) -> usize {
        let models = self
            .table
            .models
            .iter()
            .filter(|m| !self.is_shadowed(&m.name))
            .count();
        let aliases = self
            .aliases
            .iter()
            .filter(|a| !a.targets.is_empty())
            .count();
        models + aliases
    }

    /// Resolves a client model name (possibly with a reasoning suffix).
    pub fn resolve(&self, model: &str) -> Result<Resolved, PickError> {
        let requested = model.trim();
        let unknown = || PickError::UnknownModel {
            model: requested.to_string(),
        };
        let parsed = parse_model_suffix(requested);
        let (found, suffix_depth) = match find_name(&self.alias_maps, &self.table, parsed.base) {
            Some(found) => (found, parsed.depth),
            // The parentheses may be part of a configured model id.
            None if parsed.raw.is_some() => (
                find_name(&self.alias_maps, &self.table, requested).ok_or_else(unknown)?,
                None,
            ),
            None => return Err(unknown()),
        };
        let (base, targets) = match found {
            Found::Model(index) => {
                let model = &self.table.models[index];
                (
                    model.name.clone(),
                    vec![ResolvedTarget {
                        client_model: model.name.clone(),
                        pinned_depth: None,
                        routes: model.routes.clone(),
                    }],
                )
            }
            Found::Alias(index) => {
                let alias = &self.aliases[index];
                let targets = alias
                    .targets
                    .iter()
                    .map(|t| {
                        let model = &self.table.models[t.model];
                        ResolvedTarget {
                            client_model: model.name.clone(),
                            pinned_depth: t.pinned,
                            routes: model.routes.clone(),
                        }
                    })
                    .collect();
                (alias.name.clone(), targets)
            }
        };
        Ok(Resolved {
            requested: requested.to_string(),
            base,
            suffix_depth,
            targets,
        })
    }

    /// Names shown in model listings: every alias and model with at least
    /// one route that is not hidden, without duplicates, sorted.
    pub fn visible_models(&self) -> Vec<ModelInfo> {
        let mut out: Vec<ModelInfo> = self
            .aliases
            .iter()
            .filter(|a| !a.hidden && !a.targets.is_empty())
            .map(|a| a.info.clone())
            .chain(
                self.table
                    .models
                    .iter()
                    .filter(|m| !m.hidden && !m.routes.is_empty() && !self.is_shadowed(&m.name))
                    .map(|m| m.info.clone()),
            )
            .collect();
        out.sort_by(|a, b| {
            a.id.to_lowercase()
                .cmp(&b.id.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

/// The models one provider serves: configured ones if any, else the
/// discovered list, else the catalog defaults for its kind; minus `exclude`.
fn provider_models(
    provider: &ProviderConfig,
    discovered: Option<&Vec<ModelInfo>>,
) -> Vec<ProviderModel> {
    let kind = provider.kind;
    let normalise = |id: &str| -> String {
        let id = id.trim();
        // Google list endpoints return resource names
        // (`models/x`, `publishers/google/models/x`); the model id is the tail.
        if matches!(kind, ProviderKind::Gemini | ProviderKind::Vertex)
            && let Some((_, tail)) = id.rsplit_once("models/")
        {
            return tail.to_string();
        }
        id.to_string()
    };

    // Whether a remembered list still applies (`discover` switched off, the
    // endpoint changed) is decided where lists are carried across rebuilds;
    // a list that reaches this point is used.
    let discovered: &[ModelInfo] = discovered.map(Vec::as_slice).unwrap_or(&[]);
    let mut discovered_by_id: HashMap<String, &ModelInfo> = HashMap::new();
    for info in discovered {
        let id = normalise(&info.id);
        if !id.is_empty() {
            discovered_by_id.entry(id.to_lowercase()).or_insert(info);
        }
    }

    let build = |upstream: &str, client: &str| -> ModelInfo {
        let mut info = catalog()
            .lookup(upstream)
            .cloned()
            .unwrap_or_else(|| ModelInfo::bare(upstream));
        if let Some(found) = discovered_by_id.get(&upstream.to_lowercase()) {
            overlay(&mut info, found);
        }
        info.id = client.to_string();
        info
    };

    let mut candidates: Vec<(String, String, ModelInfo)> = Vec::new();
    if provider.models.iter().any(|m| !m.id.trim().is_empty()) {
        for model in &provider.models {
            let upstream = model.id.trim();
            if upstream.is_empty() {
                continue;
            }
            let client = model.client_name();
            let mut info = build(upstream, client);
            model.apply_to(&mut info);
            candidates.push((upstream.to_string(), client.to_string(), info));
        }
    } else if !discovered_by_id.is_empty() {
        for found in discovered {
            let upstream = normalise(&found.id);
            if upstream.is_empty() {
                continue;
            }
            let info = build(&upstream, &upstream);
            candidates.push((upstream.clone(), upstream, info));
        }
    } else {
        for found in catalog().models_for_kind(kind) {
            candidates.push((found.id.clone(), found.id.clone(), found.clone()));
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (upstream, client, mut info) in candidates {
        if provider.excludes(&upstream) || provider.excludes(&client) {
            continue;
        }
        if !seen.insert(client.to_lowercase()) {
            continue;
        }
        if info.owned_by.as_deref().is_none_or(|o| o.trim().is_empty()) {
            info.owned_by = Some(default_owner(provider));
        }
        out.push(ProviderModel {
            upstream,
            client,
            info: Arc::new(info),
        });
    }
    out
}

fn default_owner(provider: &ProviderConfig) -> String {
    match provider.kind {
        ProviderKind::Openai => "openai".to_string(),
        ProviderKind::Anthropic => "anthropic".to_string(),
        ProviderKind::Gemini | ProviderKind::Vertex => "google".to_string(),
        ProviderKind::OpenaiCompat | ProviderKind::Mock => provider.name.trim().to_string(),
    }
}

/// Lays metadata reported by an upstream over catalog metadata: fields the
/// upstream states win, fields it omits keep the catalog's value.
fn overlay(base: &mut ModelInfo, over: &ModelInfo) {
    if over.display_name.is_some() {
        base.display_name = over.display_name.clone();
    }
    if over.description.is_some() {
        base.description = over.description.clone();
    }
    if over.owned_by.is_some() {
        base.owned_by = over.owned_by.clone();
    }
    if over.created.is_some() {
        base.created = over.created;
    }
    if over.context_window.is_some() {
        base.context_window = over.context_window;
    }
    if over.max_output_tokens.is_some() {
        base.max_output_tokens = over.max_output_tokens;
    }
    if over.thinking.is_some() {
        base.thinking = over.thinking.clone();
    }
    base.known |= over.known;
}

// ---------------------------------------------------------------------------
// Aliases
// ---------------------------------------------------------------------------

struct AliasDef<'a> {
    name: &'a str,
    targets: Vec<&'a str>,
    hide_targets: bool,
}

struct AliasExpander<'a> {
    defs: &'a [AliasDef<'a>],
    def_maps: &'a NameMaps,
    table: &'a ModelTable,
    warnings: &'a mut Vec<String>,
}

impl AliasExpander<'_> {
    /// Flattens the targets of alias `index` into `out`. `stack` holds the
    /// aliases currently being expanded (cycle guard); `inherited` is the
    /// depth pinned by the alias target that led here, and `origin` the
    /// position of that target among the targets of the alias being built
    /// (`None` while its own targets are walked). Returns what each *direct*
    /// target matched, for `hide_targets`.
    fn expand(
        &mut self,
        index: usize,
        stack: &mut Vec<usize>,
        inherited: Option<Depth>,
        origin: Option<usize>,
        out: &mut Vec<AliasTarget>,
    ) -> Vec<Found> {
        let def = &self.defs[index];
        let root = self.defs[stack[0]].name;
        let mut direct = Vec::new();
        for (position, &target) in def.targets.iter().enumerate() {
            let origin = origin.unwrap_or(position);
            let parsed = parse_model_suffix(target);
            if parsed.raw.is_some() && parsed.depth.is_none() {
                self.warnings.push(format!(
                    "alias `{}`: target `{target}` has an unrecognised reasoning suffix; it is ignored",
                    def.name
                ));
            }
            // The innermost pin wins: it is the most specific statement
            // about the model that finally serves the request.
            let pinned = parsed.depth.or(inherited);
            // One entry per model and depth, whichever target got there
            // first: a second one would only be the same attempt again.
            let mut push = |model: usize, pinned: Option<Depth>| {
                if !out.iter().any(|t| t.model == model && t.pinned == pinned) {
                    out.push(AliasTarget {
                        model,
                        pinned,
                        origin,
                    });
                }
            };
            match find_name(self.def_maps, self.table, parsed.base) {
                Some(Found::Alias(next)) if stack.contains(&next) => {
                    // `gpt-5 -> gpt-5(high)` pins the real model of that
                    // name; anything else pointing back is a cycle.
                    match self.table.find(parsed.base) {
                        Some(model) => {
                            push(model, pinned);
                            direct.push(Found::Model(model));
                        }
                        None => self.warnings.push(format!(
                            "alias `{root}`: target `{target}` of `{}` forms a cycle; it is ignored",
                            def.name
                        )),
                    }
                }
                Some(Found::Alias(next)) => {
                    if stack.len() >= MAX_ALIAS_DEPTH {
                        self.warnings.push(format!(
                            "alias `{root}`: aliases are nested too deeply at `{target}`; it is ignored"
                        ));
                        continue;
                    }
                    stack.push(next);
                    self.expand(next, stack, pinned, Some(origin), out);
                    stack.pop();
                    direct.push(Found::Alias(next));
                }
                Some(Found::Model(model)) => {
                    push(model, pinned);
                    direct.push(Found::Model(model));
                }
                None => {
                    // The parentheses may belong to a configured model id.
                    let literal = parsed
                        .raw
                        .is_some()
                        .then(|| self.table.find(target))
                        .flatten();
                    match literal {
                        Some(model) => {
                            push(model, inherited);
                            direct.push(Found::Model(model));
                        }
                        None => self.warnings.push(format!(
                            "alias `{}`: target `{target}` matches no model",
                            def.name
                        )),
                    }
                }
            }
        }
        direct
    }
}

fn build_aliases(
    config: &Config,
    table: &mut ModelTable,
    warnings: &mut Vec<String>,
) -> (Vec<AliasName>, NameMaps) {
    // Valid definitions; a repeated name keeps its first definition.
    let mut defs: Vec<AliasDef<'_>> = Vec::new();
    let mut def_maps = NameMaps::default();
    for alias in &config.aliases {
        let name = alias.name.trim();
        if name.is_empty() {
            continue;
        }
        if def_maps.lower.contains_key(&name.to_lowercase()) {
            warnings.push(format!(
                "alias `{name}` is defined more than once; the first definition is used"
            ));
            continue;
        }
        def_maps.insert(name, defs.len());
        defs.push(AliasDef {
            name,
            targets: alias
                .targets
                .iter()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .collect(),
            hide_targets: alias.hide_targets,
        });
    }

    let mut expanded: Vec<Vec<AliasTarget>> = Vec::with_capacity(defs.len());
    let mut hidden_models = HashSet::new();
    let mut hidden_aliases = HashSet::new();
    for index in 0..defs.len() {
        let mut out = Vec::new();
        let mut expander = AliasExpander {
            defs: &defs,
            def_maps: &def_maps,
            table,
            warnings,
        };
        let direct = expander.expand(index, &mut vec![index], None, None, &mut out);
        if defs[index].hide_targets {
            for found in direct {
                match found {
                    Found::Model(model) => hidden_models.insert(model),
                    Found::Alias(alias) => hidden_aliases.insert(alias),
                };
            }
        }
        expanded.push(out);
    }

    let mut aliases = Vec::with_capacity(defs.len());
    let mut alias_maps = NameMaps::default();
    for (index, (def, targets)) in defs.iter().zip(expanded).enumerate() {
        let info = match targets.first() {
            Some(first) => {
                let mut info = table.models[first.model].info.clone();
                info.id = def.name.to_string();
                info
            }
            None => ModelInfo::bare(def.name),
        };
        if targets.is_empty() {
            // Left out of the lookup maps: a real model of the same name
            // stays reachable, and otherwise the name is simply unknown.
            warnings.push(format!(
                "alias `{}` has no routable target and is ignored",
                def.name
            ));
        } else {
            if let Some(&shadowed) = table.maps.exact.get(def.name)
                && !targets.iter().any(|t| t.model == shadowed)
            {
                warnings.push(format!(
                    "alias `{}` hides the model of the same name",
                    def.name
                ));
            }
            alias_maps.insert(def.name, aliases.len());
        }
        aliases.push(AliasName {
            name: def.name.to_string(),
            info,
            raw_targets: def.targets.iter().map(|t| t.to_string()).collect(),
            targets,
            hidden: hidden_aliases.contains(&index),
        });
    }
    for model in hidden_models {
        table.models[model].hidden = true;
    }
    (aliases, alias_maps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_separates_parts() {
        let a = credential_hash("openai", ProviderKind::Openai, "sk-a", "https://x/v1");
        let b = credential_hash("openai", ProviderKind::Openai, "sk-a", "https://x/v1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // Moving a character across a part boundary changes the hash.
        let c = credential_hash("openai", ProviderKind::Openai, "sk-ah", "ttps://x/v1");
        assert_ne!(a, c);
        assert_ne!(
            a,
            credential_hash("openai", ProviderKind::OpenaiCompat, "sk-a", "https://x/v1")
        );
        assert_ne!(
            a,
            credential_hash("other", ProviderKind::Openai, "sk-a", "https://x/v1")
        );
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("keys/vertex-sa.json"), "vertex-sa.json");
        assert_eq!(file_name(r"C:\keys\sa.json"), "sa.json");
        assert_eq!(file_name("sa.json"), "sa.json");
    }

    #[test]
    fn overlay_keeps_catalog_values_the_upstream_omits() {
        let mut base = catalog().lookup("gemini-2.5-pro").cloned().unwrap();
        let over = ModelInfo {
            id: "gemini-2.5-pro".into(),
            display_name: Some("Shown by upstream".into()),
            max_output_tokens: Some(1234),
            ..ModelInfo::default()
        };
        overlay(&mut base, &over);
        assert_eq!(base.display_name.as_deref(), Some("Shown by upstream"));
        assert_eq!(base.max_output_tokens, Some(1234));
        assert_eq!(base.context_window, Some(1_048_576));
        assert!(base.thinking.is_some());
        assert!(base.known);
    }
}
