//! Upstream credentials: the four native key families and the
//! OpenAI-compatible providers.

use super::values::{
    boolean, clamp_i32, clamp_u32, clamp_u64, dedupe_models, integer, is_empty_reference,
    is_http_url, is_valid_alias_name, normalize_prefix, sanitize_name, string_list, text, thinking,
    unique_name,
};
use super::{EMPTY_REFERENCE, Importer, Layer};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use switchyard_core::Config;
use switchyard_core::ThinkingSupport;
use switchyard_core::config::{
    AliasConfig, CredentialConfig, ModelConfig, ProviderConfig, ProviderKind, WireApi, parse_proxy,
};

/// Largest credential weight the configuration accepts.
const MAX_WEIGHT: i64 = 1_000_000;

/// Fields of a `models` entry that are read.
const MODEL_FIELDS: [&str; 5] = [
    "name",
    "alias",
    "display-name",
    "max-context-length",
    "thinking",
];

/// Where the fields of one upstream credential come from. In the flat
/// layout all three are the entry itself; in the nested layout a key
/// inherits the shared fields from its group and the endpoint belongs to
/// the group alone.
struct EntrySource<'v> {
    /// Key-only fields: `api-key`, `weight`.
    own: Vec<Layer<'v>>,
    /// Shared fields, most specific first.
    shared: Vec<Layer<'v>>,
    /// `base-url`.
    base: Vec<Layer<'v>>,
    /// Where the entry is, for messages (with real list positions).
    location: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Gemini,
    Claude,
    Codex,
    Vertex,
}

impl Family {
    const ALL: [Family; 4] = [
        Family::Gemini,
        Family::Claude,
        Family::Codex,
        Family::Vertex,
    ];

    fn nested_key(self) -> &'static str {
        match self {
            Family::Gemini => "gemini",
            Family::Claude => "claude",
            Family::Codex => "codex",
            Family::Vertex => "vertex",
        }
    }

    fn flat_key(self) -> &'static str {
        match self {
            Family::Gemini => "gemini-api-key",
            Family::Claude => "claude-api-key",
            Family::Codex => "codex-api-key",
            Family::Vertex => "vertex-api-key",
        }
    }

    fn kind(self) -> ProviderKind {
        match self {
            Family::Gemini => ProviderKind::Gemini,
            Family::Claude => ProviderKind::Anthropic,
            Family::Codex => ProviderKind::Openai,
            Family::Vertex => ProviderKind::Vertex,
        }
    }

    fn provider_name(self) -> &'static str {
        self.kind().as_str()
    }
}

/// One upstream credential of the flat layout, after inheritance.
struct FlatEntry {
    api_key: String,
    base_url: String,
    priority: i32,
    weight: Option<u32>,
    prefix: String,
    proxy: String,
    headers: IndexMap<String, String>,
    models: Vec<ModelConfig>,
    exclude: Vec<String>,
}

impl<'a> Importer<'a> {
    pub(super) fn providers(&mut self, config: &mut Config) {
        let nested = self.root.get("api-keys").and_then(Value::as_object);
        let mut used_names: HashSet<String> = HashSet::new();

        for family in Family::ALL {
            let entries = self.family_entries(family, nested);
            self.add_family(family, entries, config, &mut used_names);
        }

        // OpenAI-compatible providers: one entry is one provider.
        let nested_compat = nested.and_then(|map| map.get("openai-compatibility"));
        let (list, path) = match nested_compat {
            Some(value) => {
                self.mark("openai-compatibility");
                (Some(value), "api-keys.openai-compatibility")
            }
            None => (
                self.root.get("openai-compatibility"),
                "openai-compatibility",
            ),
        };
        if let Some(list) = list.filter(|value| !value.is_null()) {
            match list.as_array() {
                Some(items) => {
                    for (index, item) in items.iter().enumerate() {
                        let location = format!("{path}[{index}]");
                        match item.as_object() {
                            Some(map) => {
                                let layer = Layer {
                                    map,
                                    path: format!("{path}[]"),
                                };
                                if let Some(provider) =
                                    self.compat_provider(&layer, &location, config, &mut used_names)
                                {
                                    config.providers.push(provider);
                                }
                            }
                            None => self
                                .not_imported
                                .push(format!("{location}: not a provider entry; skipped")),
                        }
                    }
                }
                None => {
                    self.mark(path);
                    self.wrong_type(path);
                }
            }
        }
    }

    /// The credentials of one provider family as flat entries, from the
    /// nested groups when the family is present there, else from the flat
    /// list.
    fn family_entries(
        &mut self,
        family: Family,
        nested: Option<&'a Map<String, Value>>,
    ) -> Vec<FlatEntry> {
        let mut entries = Vec::new();
        let flat_key = family.flat_key();
        if let Some(groups) = nested.and_then(|map| map.get(family.nested_key())) {
            // Present in the nested layout: the flat twin is ignored.
            self.mark(flat_key);
            let path = format!("api-keys.{}", family.nested_key());
            let Some(groups) = groups.as_array() else {
                if !groups.is_null() {
                    self.mark(&path);
                    self.wrong_type(&path);
                }
                return entries;
            };
            for (g, group) in groups.iter().enumerate() {
                let group_location = format!("{path}[{g}]");
                let Some(group_map) = group.as_object() else {
                    self.not_imported
                        .push(format!("{group_location}: not a credential group; skipped"));
                    continue;
                };
                let group_layer = Layer {
                    map: group_map,
                    path: format!("{path}[]"),
                };
                // The group label has no meaning beyond the file.
                self.mark(&format!("{path}[].name"));
                // Not marked as a whole: the keys' fields are marked one by
                // one, so that the ones nobody reads show up in the report.
                let keys = self.unmarked_field(std::slice::from_ref(&group_layer), "keys");
                let Some(keys) = keys.and_then(Value::as_array) else {
                    self.not_imported
                        .push(format!("{group_location}: no `keys` list; skipped"));
                    continue;
                };
                for (k, key) in keys.iter().enumerate() {
                    let location = format!("{group_location}.keys[{k}]");
                    let Some(key_map) = key.as_object() else {
                        self.not_imported
                            .push(format!("{location}: not a key entry; skipped"));
                        continue;
                    };
                    let key_layer = Layer {
                        map: key_map,
                        path: format!("{path}[].keys[]"),
                    };
                    let source = EntrySource {
                        own: vec![key_layer.clone()],
                        shared: vec![key_layer, group_layer.clone()],
                        base: vec![group_layer.clone()],
                        location,
                    };
                    entries.extend(self.native_entry(family, &source));
                }
            }
        } else if let Some(list) = self.root.get(flat_key).filter(|value| !value.is_null()) {
            let Some(items) = list.as_array() else {
                self.mark(flat_key);
                self.wrong_type(flat_key);
                return entries;
            };
            for (index, item) in items.iter().enumerate() {
                let location = format!("{flat_key}[{index}]");
                let Some(map) = item.as_object() else {
                    self.not_imported
                        .push(format!("{location}: not a credential entry; skipped"));
                    continue;
                };
                let layer = Layer {
                    map,
                    path: format!("{flat_key}[]"),
                };
                let source = EntrySource {
                    own: vec![layer.clone()],
                    shared: vec![layer.clone()],
                    base: vec![layer],
                    location,
                };
                entries.extend(self.native_entry(family, &source));
            }
        }
        entries
    }

    fn native_entry(&mut self, family: Family, source: &EntrySource<'_>) -> Option<FlatEntry> {
        let location = &source.location;
        let api_key = self
            .field(&source.own, "api-key")
            .and_then(text)
            .unwrap_or_default();
        let base_url = self
            .field(&source.base, "base-url")
            .and_then(text)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let weight = self.weight(&source.own, location);
        // `websockets` (relay to the upstream's Responses WebSocket) is not
        // read: Switchyard reaches upstreams over HTTP streaming only, so a
        // key that switches it on is listed in the "not imported" report.
        let priority = self
            .field(&source.shared, "priority")
            .and_then(integer)
            .map(clamp_i32)
            .unwrap_or(0);
        let prefix = self
            .field(&source.shared, "prefix")
            .and_then(text)
            .map(|prefix| normalize_prefix(&prefix))
            .unwrap_or_default();
        let proxy = self.proxy(&source.shared, location);
        let headers = self.headers(&source.shared, location);
        let models = self.models(&source.shared, location);
        let exclude = self
            .field(&source.shared, "excluded-models")
            .map(string_list)
            .unwrap_or_default();

        if api_key.is_empty() {
            self.not_imported
                .push(format!("{location}: no api-key; skipped"));
            return None;
        }
        if is_empty_reference(&api_key) {
            self.not_imported
                .push(format!("{location}: the api-key is {EMPTY_REFERENCE}"));
            return None;
        }
        if base_url.is_empty() {
            if family == Family::Codex {
                // The source program drops such entries when it loads.
                self.not_imported.push(format!(
                    "{location}: a codex key without base-url, which CLIProxyAPI ignores; skipped"
                ));
                return None;
            }
        } else if !is_http_url(&base_url) {
            self.not_imported.push(format!(
                "{location}: base-url does not start with http:// or https://; skipped"
            ));
            return None;
        }
        Some(FlatEntry {
            api_key,
            base_url,
            priority,
            weight,
            prefix,
            proxy,
            headers,
            models: dedupe_models(models),
            exclude,
        })
    }

    /// The weight of a credential, when it is not the default of 1. Zero and
    /// below mean "not in weighted rotation", as in the source program.
    fn weight(&mut self, layers: &[Layer<'_>], location: &str) -> Option<u32> {
        let value = self.field(layers, "weight")?;
        match integer(value) {
            Some(weight) if weight > MAX_WEIGHT => {
                self.notes.push(format!(
                    "{location}: weight is above the maximum; set to {MAX_WEIGHT}"
                ));
                Some(clamp_u32(MAX_WEIGHT))
            }
            Some(1) => None,
            Some(weight) => Some(clamp_u32(weight)),
            None => {
                self.not_imported
                    .push(format!("{location}: weight is not an integer; ignored"));
                None
            }
        }
    }

    fn proxy(&mut self, layers: &[Layer<'_>], location: &str) -> String {
        let Some(proxy) = self.field(layers, "proxy-url").and_then(text) else {
            return String::new();
        };
        match parse_proxy(&proxy) {
            Ok(_) => proxy,
            Err(_) => {
                self.not_imported.push(format!(
                    "{location}: proxy-url is not a proxy URL Switchyard accepts; ignored"
                ));
                String::new()
            }
        }
    }

    fn headers(&mut self, layers: &[Layer<'_>], location: &str) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        let Some(map) = self.field(layers, "headers").and_then(Value::as_object) else {
            return out;
        };
        let mut dynamic = 0usize;
        let mut malformed = 0usize;
        for (name, value) in map {
            let name = name.trim();
            let Some(value) = text(value) else {
                continue;
            };
            if name.is_empty() || value.is_empty() {
                continue;
            }
            // What could not be sent as an HTTP header anyway.
            let is_token = name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
            if !is_token || value.chars().any(char::is_control) {
                malformed += 1;
                continue;
            }
            // `$Name` copies a header of the client's request and
            // `$CPA-SESSION-ID` inserts a session id; neither exists here.
            if value.starts_with('$') || value.to_ascii_uppercase().contains("$CPA-SESSION-ID") {
                dynamic += 1;
                continue;
            }
            out.insert(name.to_string(), value);
        }
        if dynamic > 0 {
            self.not_imported.push(format!(
                "{location}: {dynamic} of the headers copy their value from the client request \
                 (`$Name`) or the session id; provider headers are fixed values here"
            ));
        }
        if malformed > 0 {
            self.not_imported.push(format!(
                "{location}: {malformed} of the headers are not valid HTTP headers"
            ));
        }
        out
    }

    /// The `models` list of an entry. Entries without an upstream name are
    /// skipped; duplicates are left to the caller.
    fn models(&mut self, layers: &[Layer<'_>], location: &str) -> Vec<ModelConfig> {
        for layer in layers {
            for key in MODEL_FIELDS {
                self.consumed
                    .insert(format!("{}.models[].{key}", layer.path));
            }
        }
        let Some(items) = self
            .unmarked_field(layers, "models")
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut nameless = 0usize;
        let mut inverted = 0usize;
        for item in items {
            let Some(item) = item.as_object() else {
                continue;
            };
            let get = |key: &str| item.get(key).filter(|value| !value.is_null());
            let id = get("name").and_then(text).unwrap_or_default();
            let alias = get("alias").and_then(text).unwrap_or_default();
            if id.is_empty() {
                if !alias.is_empty() {
                    nameless += 1;
                }
                continue;
            }
            // A budget range that is upside down says nothing usable, and
            // the configuration refuses it: the levels are kept, the range
            // is not.
            let mut support = get("thinking").and_then(thinking);
            if let Some(range) = support.as_mut().filter(|t| t.max > 0 && t.min > t.max) {
                range.min = 0;
                range.max = 0;
                inverted += 1;
            }
            let support = support.filter(|t| *t != ThinkingSupport::default());
            out.push(ModelConfig {
                alias: if alias == id { String::new() } else { alias },
                id,
                display_name: get("display-name").and_then(text).unwrap_or_default(),
                context_window: get("max-context-length")
                    .and_then(integer)
                    .filter(|length| *length > 0)
                    .map(clamp_u64),
                max_output_tokens: None,
                thinking: support,
            });
        }
        if inverted > 0 {
            self.not_imported.push(format!(
                "{location}: {inverted} of the models have a thinking range whose min is above \
                 its max; the range was left out"
            ));
        }
        if nameless > 0 {
            self.not_imported.push(format!(
                "{location}: {nameless} of the models have an alias but no upstream name; skipped"
            ));
        }
        out
    }

    /// Turns the flat entries of a family into providers: entries with the
    /// same endpoint and policy share one provider.
    fn add_family(
        &mut self,
        family: Family,
        entries: Vec<FlatEntry>,
        config: &mut Config,
        used_names: &mut HashSet<String>,
    ) {
        let default_base = family.kind().default_base_url().unwrap_or("");
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<FlatEntry>> = HashMap::new();
        for entry in entries {
            let effective_base = if entry.base_url.is_empty() {
                default_base
            } else {
                entry.base_url.as_str()
            };
            let mut headers: Vec<(&String, &String)> = entry.headers.iter().collect();
            headers.sort();
            // Same endpoint, prefix, headers and proxy, and the same
            // provider-level policy (models, exclusions).
            let key = serde_json::to_string(&(
                effective_base,
                &entry.prefix,
                &headers,
                &entry.proxy,
                &entry.models,
                &entry.exclude,
            ))
            .unwrap_or_default();
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(entry);
        }

        for key in order {
            let Some(mut entries) = groups.remove(&key) else {
                continue;
            };
            let mut rest = entries.split_off(1);
            let Some(first) = entries.pop() else {
                continue;
            };
            let name = unique_name(family.provider_name(), used_names);
            let mut provider = ProviderConfig::new(name, family.kind());
            provider.base_url = first.base_url.clone();
            provider.prefix = first.prefix.clone();
            provider.priority = first.priority;
            provider.proxy = first.proxy.clone();
            provider.headers = first.headers.clone();
            provider.models = first.models.clone();
            provider.exclude = first.exclude.clone();
            if family == Family::Codex {
                provider.wire_api = WireApi::Responses;
            }
            let mut seen = HashSet::new();
            let mut duplicates = 0usize;
            rest.insert(0, first);
            for entry in rest {
                if !seen.insert(entry.api_key.clone()) {
                    duplicates += 1;
                    continue;
                }
                if entry.weight.is_none() && entry.priority == provider.priority {
                    provider.api_keys.push(entry.api_key);
                } else {
                    provider.credentials.push(CredentialConfig {
                        api_key: entry.api_key,
                        weight: entry.weight,
                        priority: (entry.priority != provider.priority).then_some(entry.priority),
                        ..CredentialConfig::default()
                    });
                }
            }
            if duplicates > 0 {
                self.notes.push(format!(
                    "provider `{}`: keys listed more than once were imported once \
                     ({duplicates} left out)",
                    provider.name
                ));
            }
            if family == Family::Vertex {
                self.notes.push(format!(
                    "provider `{}`: Vertex API keys were imported; set `project` and `location` \
                     if the endpoint needs them",
                    provider.name
                ));
            }
            config.providers.push(provider);
        }
    }

    fn compat_provider(
        &mut self,
        layer: &Layer<'_>,
        location: &str,
        config: &mut Config,
        used_names: &mut HashSet<String>,
    ) -> Option<ProviderConfig> {
        let layers = std::slice::from_ref(layer);
        let raw_name = self
            .field(layers, "name")
            .and_then(text)
            .unwrap_or_default();
        let base_url = self
            .field(layers, "base-url")
            .and_then(text)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let disabled = self
            .field(layers, "disabled")
            .and_then(boolean)
            .unwrap_or(false);
        let priority = self
            .field(layers, "priority")
            .and_then(integer)
            .map(clamp_i32)
            .unwrap_or(0);
        let prefix = self
            .field(layers, "prefix")
            .and_then(text)
            .map(|prefix| normalize_prefix(&prefix))
            .unwrap_or_default();
        let headers = self.headers(layers, location);
        let models = self.models(layers, location);

        // Credentials: `keys` (nested layout), `api-key-entries` (flat
        // layout) and the long-retired plain `api-keys` list.
        let mut api_keys: Vec<String> = Vec::new();
        let mut credentials: Vec<CredentialConfig> = Vec::new();
        let mut seen = HashSet::new();
        let mut duplicates = 0usize;
        let mut unnamed = 0usize;
        for list_key in ["keys", "api-key-entries"] {
            for field in ["api-key", "weight", "proxy-url"] {
                self.consumed
                    .insert(format!("{}.{list_key}[].{field}", layer.path));
            }
            let Some(items) = layer.map.get(list_key).and_then(Value::as_array) else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let Some(map) = item.as_object() else {
                    continue;
                };
                let key_location = format!("{location}.{list_key}[{index}]");
                let key_layer = [Layer {
                    map,
                    path: format!("{}.{list_key}[]", layer.path),
                }];
                let api_key = self
                    .field(&key_layer, "api-key")
                    .and_then(text)
                    .unwrap_or_default();
                let weight = self.weight(&key_layer, &key_location);
                let proxy = self.proxy(&key_layer, &key_location);
                if api_key.is_empty() {
                    continue;
                }
                if is_empty_reference(&api_key) {
                    unnamed += 1;
                    continue;
                }
                if !seen.insert(api_key.clone()) {
                    duplicates += 1;
                    continue;
                }
                if weight.is_none() && proxy.is_empty() {
                    api_keys.push(api_key);
                } else {
                    credentials.push(CredentialConfig {
                        api_key,
                        weight,
                        proxy,
                        ..CredentialConfig::default()
                    });
                }
            }
        }
        if let Some(old) = self.field(layers, "api-keys") {
            for api_key in string_list(old) {
                if is_empty_reference(&api_key) {
                    unnamed += 1;
                } else if seen.insert(api_key.clone()) {
                    api_keys.push(api_key);
                } else {
                    duplicates += 1;
                }
            }
        }

        if base_url.is_empty() {
            self.not_imported.push(format!(
                "{location}: an OpenAI-compatible provider without base-url; skipped"
            ));
            return None;
        }
        if !is_http_url(&base_url) {
            self.not_imported.push(format!(
                "{location}: base-url does not start with http:// or https://; skipped"
            ));
            return None;
        }

        let base_name = match sanitize_name(&raw_name) {
            name if name.is_empty() => "openai-compat".to_string(),
            name => name,
        };
        let name = unique_name(&base_name, used_names);
        let mut provider = ProviderConfig::new(name, ProviderKind::OpenaiCompat);
        provider.enabled = !disabled;
        provider.base_url = base_url;
        provider.priority = priority;
        provider.prefix = prefix;
        provider.headers = headers;
        provider.api_keys = api_keys;
        provider.credentials = credentials;
        provider.models = self.pool_models(models, &provider, config);
        if unnamed > 0 {
            self.not_imported.push(format!(
                "provider `{}`: {unnamed} of the keys are {EMPTY_REFERENCE}",
                provider.name
            ));
        }
        if duplicates > 0 {
            self.notes.push(format!(
                "provider `{}`: keys listed more than once were imported once \
                 ({duplicates} left out)",
                provider.name
            ));
        }
        Some(provider)
    }

    /// Resolves repeated client-facing names in the model list of an
    /// OpenAI-compatible provider.
    ///
    /// There, several upstream models under one alias form a pool that
    /// requests rotate across. Here a provider's model names are unique and
    /// a virtual model (`[[aliases]]`) lists targets tried in order. So the
    /// pool's members are imported under their upstream names and an alias
    /// with those targets, hiding them from listings, takes the pool's name.
    fn pool_models(
        &mut self,
        models: Vec<ModelConfig>,
        provider: &ProviderConfig,
        config: &mut Config,
    ) -> Vec<ModelConfig> {
        let mut by_name: IndexMap<String, Vec<ModelConfig>> = IndexMap::new();
        for model in models {
            by_name
                .entry(model.client_name().to_ascii_lowercase())
                .or_default()
                .push(model);
        }
        let prefix = provider.normalized_prefix();
        let qualified = |id: &str| {
            if prefix.is_empty() {
                id.to_string()
            } else {
                format!("{prefix}/{id}")
            }
        };

        let mut out: Vec<ModelConfig> = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        let mut dropped = 0usize;
        for (_, mut members) in by_name {
            // The same upstream model twice under one name is a repeat, not
            // a pool.
            let mut ids = HashSet::new();
            members.retain(|model| ids.insert(model.id.to_ascii_lowercase()));
            let Some(first) = members.first() else {
                continue;
            };
            let pool_name = first.client_name().to_string();
            // An alias may not name itself among its targets.
            let self_targeting = members
                .iter()
                .any(|model| qualified(&model.id).eq_ignore_ascii_case(&pool_name));
            // Nor may its name hold a space or end in a reasoning suffix.
            let nameable = is_valid_alias_name(&pool_name);
            if members.len() > 1 && !self_targeting && !nameable {
                self.not_imported.push(format!(
                    "provider `{}`: the model pool `{pool_name}` cannot become a virtual model, \
                     because its name has a space or ends in a reasoning suffix such as (high); \
                     its first model keeps the name",
                    provider.name
                ));
            }
            if members.len() == 1 || self_targeting || !nameable {
                // One model: the first entry of that name.
                let mut members = members.into_iter();
                if let Some(model) = members.next() {
                    if taken.insert(model.client_name().to_ascii_lowercase()) {
                        out.push(model);
                    } else {
                        dropped += 1;
                    }
                }
                dropped += members.count();
                continue;
            }

            let mut targets = Vec::new();
            for mut model in members {
                model.alias = String::new();
                targets.push(qualified(&model.id));
                if taken.insert(model.id.to_ascii_lowercase()) {
                    out.push(model);
                }
            }
            match config
                .aliases
                .iter_mut()
                .find(|alias| alias.name.eq_ignore_ascii_case(&pool_name))
            {
                Some(alias) => {
                    for target in targets {
                        if !alias.targets.contains(&target) {
                            alias.targets.push(target);
                        }
                    }
                }
                None => config.aliases.push(AliasConfig {
                    name: pool_name.clone(),
                    targets,
                    hide_targets: true,
                }),
            }
            self.notes.push(format!(
                "provider `{}`: the model pool `{pool_name}` became a virtual model \
                 ([[aliases]]) whose targets are tried in order, not in rotation",
                provider.name
            ));
        }
        if dropped > 0 {
            self.not_imported.push(format!(
                "provider `{}`: {dropped} of the model entries have a client-facing name that \
                 an earlier entry already took",
                provider.name
            ));
        }
        out
    }
}
