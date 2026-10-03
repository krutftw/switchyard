//! The JSON the admin API answers with. Everything here is built from a
//! configuration whose secrets are already masked ([`mask_config`]) and from
//! runtime snapshots that never held one.
//!
//! Shapes are stable: a field is always present (`null`, `""`, `[]` when it
//! has no value), so the dashboard never has to guess whether a key exists.

use crate::state::AdminState;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use switchyard_config_store::{ConfigStore, Rejection, client_key_id, mask_config};
use switchyard_core::Config;
use switchyard_core::config::{
    AliasConfig, ClientKey, CredentialConfig, PayloadConfig, PayloadRule, PriceConfig,
    ProviderConfig, is_secret_reference, resolve_secret,
};
use switchyard_core::util::mask_secret;
use switchyard_gateway::DiscoveryState;
use switchyard_scheduler::{CredentialSnapshot, ModelEntry, ProviderSnapshot};
use switchyard_telemetry::Totals;

/// `{"config", "path", "restart_required", "command_line_overrides",
/// "config_rejected"}` — the answer of `GET /config` and of every mutation
/// that returns the whole configuration.
pub(crate) fn config_view(state: &AdminState, config: &Config) -> Value {
    let store = state.gateway.config_store();
    json!({
        "config": to_value(&mask_config(config)),
        "path": store.path().display().to_string(),
        "restart_required": restart_required(state),
        "command_line_overrides": state.options.command_line,
        "config_rejected": rejection_view(store),
    })
}

/// The settings that were changed and only take effect after a restart —
/// leaving out those the command line fixes (`--host`, `--port`): a restart
/// with the same command line does not apply the file's value of those.
pub(crate) fn restart_required(state: &AdminState) -> Vec<String> {
    let overridden = &state.options.command_line;
    state
        .gateway
        .config_store()
        .restart_required()
        .into_iter()
        .filter(|setting| !overridden.contains(setting))
        .collect()
}

/// Issues of a refused file quoted in its message; the rest are counted.
const REJECTION_ISSUES_SHOWN: usize = 3;

/// The sentence that says the file on disk is refused, naming its first
/// issues. Also listed in `warnings` of `GET /status`.
pub(crate) fn rejection_message(rejection: &Rejection) -> String {
    let mut message = String::from(
        "configuration file: the file on disk was refused and is not in effect; the gateway \
         keeps running on the last valid configuration until the file is fixed",
    );
    let issues = &rejection.issues;
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
    message
}

/// `config_rejected`: `null` while the file on disk is in effect, else
/// `{"at", "message", "issues"}` — when it was refused (Unix ms), the
/// sentence of [`rejection_message`] and every issue of the file.
pub(crate) fn rejection_view(store: &ConfigStore) -> Value {
    let Some(rejection) = store.rejection() else {
        return Value::Null;
    };
    let at = rejection
        .at
        .duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    json!({
        "at": at,
        "message": rejection_message(&rejection),
        "issues": rejection.issues,
    })
}

/// Serialises a value that cannot fail to serialise (plain data); a failure
/// would be a bug and shows as `null` rather than a panic.
pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// A provider entry with every field present, in a fixed order. This is
/// also the shape `POST /providers` and `PUT /providers/{name}` accept.
pub(crate) fn provider_config_json(masked: &ProviderConfig) -> Map<String, Value> {
    let mut sparse = match to_value(masked) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let defaults: [(&str, Value); 18] = [
        ("name", json!("")),
        ("kind", Value::Null),
        ("enabled", json!(true)),
        ("base_url", json!("")),
        ("api_keys", json!([])),
        ("credentials", json!([])),
        ("prefix", json!("")),
        ("priority", json!(0)),
        ("proxy", json!("")),
        ("headers", json!({})),
        ("models", json!([])),
        ("exclude", json!([])),
        ("discover", json!(true)),
        ("wire_api", json!("auto")),
        ("legacy_max_tokens", Value::Null),
        ("stream_usage", Value::Null),
        ("project", json!("")),
        ("location", json!("")),
    ];
    let mut full = Map::new();
    for (key, default) in defaults {
        let value = sparse.remove(key).unwrap_or(default);
        full.insert(key.to_string(), value);
    }
    // Fields a later schema adds are passed through rather than lost.
    full.extend(sparse);
    if let Some(Value::Array(credentials)) = full.get_mut("credentials") {
        for credential in credentials {
            spell_out_missing_key(credential);
        }
    }
    full
}

/// A credential entry without a key says so with `"api_key": null`, the
/// way `POST /providers` and `PUT /providers/{name}` read it: left out (or
/// `""`) would mean "keep the key stored at this place", which a keyless
/// credential moved onto the row of one with a key would inherit. So the
/// entry sent back as it was shown stays keyless.
fn spell_out_missing_key(credential: &mut Value) {
    let Value::Object(fields) = credential else {
        return;
    };
    let keyless = fields
        .get("api_key")
        .is_none_or(|key| key.as_str().is_some_and(|key| key.trim().is_empty()));
    if !keyless {
        return;
    }
    // First, where the field stands in an entry that has a key.
    let mut spelled = Map::with_capacity(fields.len() + 1);
    spelled.insert("api_key".to_string(), Value::Null);
    for (name, value) in std::mem::take(fields) {
        if name != "api_key" {
            spelled.insert(name, value);
        }
    }
    *fields = spelled;
}

/// Where a runtime credential comes from in the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    /// `api_keys[index]`.
    ApiKeys(usize),
    /// `credentials[index]`.
    Credentials(usize),
    /// The keyless credential a provider that needs no secret gets when it
    /// lists none.
    Implicit,
}

impl CredentialSource {
    fn name(self) -> &'static str {
        match self {
            CredentialSource::ApiKeys(_) => "api_keys",
            CredentialSource::Credentials(_) => "credentials",
            CredentialSource::Implicit => "implicit",
        }
    }

    fn index(self) -> Option<usize> {
        match self {
            CredentialSource::ApiKeys(index) | CredentialSource::Credentials(index) => Some(index),
            CredentialSource::Implicit => None,
        }
    }
}

/// The configuration entry behind each runtime credential of a provider, in
/// the scheduler's order (`ProviderConfig::all_credentials`): non-blank
/// `api_keys` first, then `credentials`.
pub(crate) fn credential_sources(provider: &ProviderConfig) -> Vec<CredentialSource> {
    let mut sources: Vec<CredentialSource> = provider
        .api_keys
        .iter()
        .enumerate()
        .filter(|(_, key)| !key.trim().is_empty())
        .map(|(index, _)| CredentialSource::ApiKeys(index))
        .collect();
    sources.extend((0..provider.credentials.len()).map(CredentialSource::Credentials));
    if sources.is_empty() && !provider.kind.needs_credentials() {
        sources.push(CredentialSource::Implicit);
    }
    sources
}

/// How a secret is shown: references as written, literals masked.
pub(crate) fn mask_value(value: &str) -> String {
    if is_secret_reference(value) {
        value.trim().to_string()
    } else {
        mask_secret(value.trim())
    }
}

fn credential_view(
    masked: &ProviderConfig,
    source: CredentialSource,
    runtime: Option<&CredentialSnapshot>,
) -> Value {
    let blank = CredentialConfig::default();
    let entry = match source {
        CredentialSource::Credentials(index) => masked.credentials.get(index).unwrap_or(&blank),
        _ => &blank,
    };
    // Already masked: `masked` came through `mask_config`.
    let configured_key = match source {
        CredentialSource::ApiKeys(index) => masked.api_keys.get(index).map(String::as_str),
        CredentialSource::Credentials(_) => Some(entry.api_key.as_str()),
        CredentialSource::Implicit => None,
    }
    .unwrap_or_default();

    let mut view = Map::new();
    let mut put = |key: &str, value: Value| {
        view.insert(key.to_string(), value);
    };
    put("id", json!(runtime.map(|r| r.id.as_str())));
    put(
        "label",
        json!(runtime.map_or(entry.label.as_str(), |r| r.label.as_str())),
    );
    put(
        "masked_key",
        json!(runtime.map_or(configured_key, |r| r.masked_key.as_str())),
    );
    put("source", json!(source.name()));
    put("index", json!(source.index()));
    put(
        "disabled",
        json!(runtime.map_or(entry.disabled, |r| r.disabled)),
    );
    // What switched it off, when its status is `disabled`: the provider,
    // the credential's own entry, or a switch flipped at runtime.
    put("disabled_by", json!(runtime.and_then(|r| r.disabled_by)));
    put(
        "weight",
        json!(runtime.map_or(entry.effective_weight(), |r| r.weight)),
    );
    put(
        "priority",
        json!(runtime.map_or(entry.priority.unwrap_or(masked.priority), |r| r.priority)),
    );
    put("proxy", json!(entry.proxy));
    put("service_account_file", json!(entry.service_account_file));
    match runtime {
        Some(r) => {
            put("status", json!(r.status));
            put("cooldown_until", json!(r.cooldown_until));
            put("cooldown_reason", json!(r.cooldown_reason));
            put("model_cooldowns", json!(r.model_cooldowns));
            put("requests", json!(r.requests));
            put("successes", json!(r.successes));
            put("failures", json!(r.failures));
            put("consecutive_failures", json!(r.consecutive_failures));
            put("latency_ms", json!(r.latency_ms));
            put("last_used_at", json!(r.last_used_at));
            put("last_error", json!(r.last_error));
            put("usable", json!(r.usable));
            put("unusable_reason", json!(r.unusable_reason));
        }
        // The scheduler does not know the credential (yet): show what the
        // configuration says and no runtime state.
        None => {
            put("status", json!("unknown"));
            put("cooldown_until", Value::Null);
            put("cooldown_reason", Value::Null);
            put("model_cooldowns", json!([]));
            put("requests", json!(0));
            put("successes", json!(0));
            put("failures", json!(0));
            put("consecutive_failures", json!(0));
            put("latency_ms", Value::Null);
            put("last_used_at", Value::Null);
            put("last_error", Value::Null);
            put("usable", json!(false));
            put("unusable_reason", Value::Null);
        }
    }
    Value::Object(view)
}

/// Where the discovery of a provider's model list stands:
/// `{state, at, error, models}`. A provider the gateway has not got to yet
/// (the configuration that brings it was stored a moment ago) is `pending`
/// without a time.
fn discovery_view(discovery: Option<&DiscoveryState>) -> Value {
    match discovery {
        Some(discovery) => to_value(discovery),
        None => json!({"state": "pending", "at": null, "error": null, "models": 0}),
    }
}

/// One provider as `GET /providers` shows it: its configuration (secrets
/// masked), its credentials merged with their runtime state, the
/// client-facing names of its models and where the discovery of its model
/// list stands.
pub(crate) fn provider_view(
    masked: &ProviderConfig,
    runtime: Option<&ProviderSnapshot>,
    names: &[&str],
    discovery: Option<&DiscoveryState>,
) -> Value {
    let editable = provider_config_json(masked);
    let mut view = editable.clone();

    let sources = credential_sources(masked);
    let credentials: Vec<Value> = sources
        .iter()
        .enumerate()
        .map(|(position, source)| {
            let snapshot = runtime.and_then(|r| r.credentials.get(position));
            credential_view(masked, *source, snapshot)
        })
        .collect();
    // A disabled provider serves nothing: the scheduler still knows its
    // model list (for when it is switched on again), but no name routes to
    // it, so its count is that of its (empty) `models`.
    let model_count = match runtime {
        Some(runtime) if masked.enabled => runtime.models,
        _ => names.len(),
    };

    view.insert("credentials".to_string(), Value::Array(credentials));
    view.insert("models".to_string(), json!(names));
    view.insert("model_count".to_string(), json!(model_count));
    view.insert(
        "effective_base_url".to_string(),
        json!(masked.effective_base_url()),
    );
    view.insert("protocols".to_string(), json!(masked.protocols()));
    view.insert("discovery".to_string(), discovery_view(discovery));
    view.insert("config".to_string(), Value::Object(editable));
    Value::Object(view)
}

/// Every provider of `config`, in configuration order, with the runtime
/// state the scheduler holds for it.
///
/// Credentials are matched to their configuration rows by position, which
/// is only right when the scheduler was built from this very configuration.
/// In the instant between a configuration being stored and the gateway
/// applying it the two differ; the views then show the configuration alone
/// (credential status `unknown`) rather than another row's state.
pub(crate) fn provider_views(state: &AdminState, config: &Config) -> Vec<Value> {
    views_of(state, config, None)
}

/// The client-facing names each provider serves, by provider name, each
/// list in the model table's order (sorted).
///
/// Collected in one pass over the table: looking through every model's
/// routes once per provider made the provider list quadratic in the number
/// of providers.
fn names_by_provider(models: &[ModelEntry]) -> HashMap<&str, Vec<&str>> {
    let mut served: HashMap<&str, Vec<&str>> = HashMap::new();
    for entry in models.iter().filter(|e| e.alias_targets.is_none()) {
        for route in &entry.routes {
            let names = served.entry(route.provider.as_str()).or_default();
            // Two routes of one provider under one name are one name.
            if names.last() != Some(&entry.name.as_str()) {
                names.push(entry.name.as_str());
            }
        }
    }
    served
}

/// The views of every provider of `config`, or of the one at `only`.
fn views_of(state: &AdminState, config: &Config, only: Option<usize>) -> Vec<Value> {
    let masked = mask_config(config);
    let scheduler = state.gateway.scheduler();
    let in_step = *scheduler.config() == *config;
    let snapshots = if in_step {
        scheduler.snapshot()
    } else {
        Vec::new()
    };
    let by_name: HashMap<&str, &ProviderSnapshot> = snapshots
        .iter()
        .map(|snapshot| (snapshot.name.as_str(), snapshot))
        .collect();
    let models = if in_step {
        scheduler.models()
    } else {
        Vec::new()
    };
    let served = names_by_provider(&models);
    let discoveries = state.gateway.discovery_states();
    masked
        .providers
        .iter()
        .enumerate()
        .filter(|(index, _)| only.is_none_or(|only| only == *index))
        .map(|(_, provider)| {
            let runtime = by_name
                .get(provider.name.as_str())
                .copied()
                // A rebuild between the comparison above and the snapshot
                // shows as a different number of credentials.
                .filter(|runtime| runtime.credentials.len() == credential_sources(provider).len());
            let names = served
                .get(provider.name.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            provider_view(provider, runtime, names, discoveries.get(&provider.name))
        })
        .collect()
}

/// The configuration provider views are built from for a plain read: the
/// one the scheduler runs on, so that configuration and runtime state
/// belong together.
pub(crate) fn scheduled_config(state: &AdminState) -> std::sync::Arc<Config> {
    state.gateway.scheduler().config()
}

/// One provider by name.
pub(crate) fn provider_view_by_name(
    state: &AdminState,
    config: &Config,
    name: &str,
) -> Option<Value> {
    let index = config.providers.iter().position(|p| p.name == name)?;
    views_of(state, config, Some(index)).pop()
}

// ---------------------------------------------------------------------------
// Client keys
// ---------------------------------------------------------------------------

/// The stable id of a client key entry: [`client_key_id`] of the resolved
/// key, which is what request records carry. A reference that cannot be
/// resolved right now is identified by its own text.
pub(crate) fn key_id(key: &ClientKey) -> String {
    match resolve_secret(&key.key) {
        Ok(resolved) if !resolved.is_empty() => client_key_id(&resolved),
        _ => client_key_id(&key.key),
    }
}

/// Whether the key's value is known right now (a literal, or a reference to
/// a variable that is set).
fn key_resolved(key: &ClientKey) -> bool {
    resolve_secret(&key.key).is_ok_and(|value| !value.is_empty())
}

/// Usage of one client key over the listed range.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct KeyUsage {
    pub totals: Totals,
    /// Start of the key's most recent request on record, unix ms.
    pub last_used_at: Option<i64>,
}

/// Model patterns as the gateway reads them: trimmed, without blanks, each
/// one once (the first place it appears in, in the spelling it has there).
/// Patterns match model names ignoring case, so `GPT-*` repeats `gpt-*`.
/// This is how `POST /keys` and `PATCH /keys/{id}` store a list, and how a
/// list written by hand into the file is shown.
pub(crate) fn clean_models<S: AsRef<str>>(models: &[S]) -> Vec<String> {
    let mut clean: Vec<String> = Vec::with_capacity(models.len());
    // Lower-cased the way `wildcard_match` compares.
    let mut seen: Vec<String> = Vec::with_capacity(models.len());
    for pattern in models {
        let pattern = pattern.as_ref().trim();
        let folded: String = pattern.chars().flat_map(char::to_lowercase).collect();
        if !pattern.is_empty() && !seen.contains(&folded) {
            seen.push(folded);
            clean.push(pattern.to_string());
        }
    }
    clean
}

/// A client key as `GET /keys` shows it. Name and model patterns are shown
/// the way the endpoints store them — a name padded with spaces or a list
/// with blank and repeated patterns can only come from a hand-edited file,
/// and means what its tidy form means.
pub(crate) fn key_view(key: &ClientKey, usage: &KeyUsage) -> Value {
    json!({
        "id": key_id(key),
        "name": key.name.trim(),
        "masked": mask_value(&key.key),
        "is_reference": is_secret_reference(&key.key),
        "resolved": key_resolved(key),
        "enabled": key.enabled,
        "models": clean_models(&key.models),
        "rate_limit_rpm": key.rate_limit_rpm,
        "usage": {
            "requests": usage.totals.requests,
            "errors": usage.totals.errors,
            "tokens": usage.totals.total_tokens(),
            "cost": usage.totals.cost,
            "last_used_at": usage.last_used_at,
        },
    })
}

// ---------------------------------------------------------------------------
// Aliases, payload rules, pricing
// ---------------------------------------------------------------------------

pub(crate) fn aliases_view(aliases: &[AliasConfig]) -> Value {
    Value::Array(
        aliases
            .iter()
            .map(|alias| {
                json!({
                    "name": alias.name,
                    "targets": alias.targets,
                    "hide_targets": alias.hide_targets,
                })
            })
            .collect(),
    )
}

fn payload_rules(rules: &[PayloadRule]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| {
                json!({
                    "models": rule.models,
                    "protocol": rule.protocol,
                    "provider": rule.provider,
                    "set": rule.set,
                    "remove": rule.remove,
                })
            })
            .collect(),
    )
}

pub(crate) fn payload_view(payload: &PayloadConfig) -> Value {
    json!({
        "default": payload_rules(&payload.default),
        "override": payload_rules(&payload.overrides),
        "filter": payload_rules(&payload.filter),
    })
}

pub(crate) fn pricing_view(pricing: &[PriceConfig]) -> Value {
    Value::Array(
        pricing
            .iter()
            .map(|price| {
                json!({
                    "model": price.model,
                    "input": price.input,
                    "output": price.output,
                    "cache_read": price.cache_read,
                    "cache_write": price.cache_write,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::config::ProviderKind;

    #[test]
    fn provider_json_has_every_field_in_order() {
        let provider = ProviderConfig::new("p", ProviderKind::Openai);
        let json = provider_config_json(&provider);
        let keys: Vec<&str> = json.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "name",
                "kind",
                "enabled",
                "base_url",
                "api_keys",
                "credentials",
                "prefix",
                "priority",
                "proxy",
                "headers",
                "models",
                "exclude",
                "discover",
                "wire_api",
                "legacy_max_tokens",
                "stream_usage",
                "project",
                "location",
            ]
        );
        assert_eq!(json["kind"], "openai");
        assert_eq!(json["wire_api"], "auto");
        // The full shape is accepted back as a provider entry.
        let back: ProviderConfig = serde_json::from_value(Value::Object(json)).unwrap();
        assert_eq!(back, provider);
    }

    /// Regression: `config.credentials[]` left `api_key` out for a keyless
    /// credential, which `PUT /providers/{name}` reads as "keep the key
    /// stored here" — the object did not round-trip.
    #[test]
    fn a_keyless_credential_spells_out_its_missing_key() {
        let mut provider = ProviderConfig::new("p", ProviderKind::OpenaiCompat);
        provider.credentials = vec![
            CredentialConfig {
                api_key: "sk-a…wxyz".into(),
                label: "with key".into(),
                ..CredentialConfig::default()
            },
            CredentialConfig {
                label: "keyless".into(),
                proxy: "http://127.0.0.1:3128".into(),
                ..CredentialConfig::default()
            },
            CredentialConfig::default(),
        ];
        let json = provider_config_json(&provider);
        assert_eq!(
            json["credentials"],
            json!([
                {"api_key": "sk-a…wxyz", "label": "with key"},
                {"api_key": null, "label": "keyless", "proxy": "http://127.0.0.1:3128"},
                {"api_key": null},
            ])
        );
        // The key comes first, as in an entry that has one.
        let keys: Vec<&str> = json["credentials"][1]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["api_key", "label", "proxy"]);
    }

    /// Patterns match ignoring case, so a pattern that differs from an
    /// earlier one only in case is a repeat; the first spelling stays.
    #[test]
    fn model_patterns_repeat_ignoring_case() {
        assert_eq!(
            clean_models(&["GPT-*", " gpt-* ", "Claude-*", "claude-*", "", "mock-ECHO"]),
            ["GPT-*", "Claude-*", "mock-ECHO"]
        );
        assert_eq!(clean_models(&["ΣIGMA", "σigma"]), ["ΣIGMA"]);
        assert!(clean_models::<&str>(&[]).is_empty());
    }

    #[test]
    fn credential_sources_follow_the_schedulers_order() {
        let mut provider = ProviderConfig::new("p", ProviderKind::Openai);
        provider.api_keys = vec!["a".into(), "  ".into(), "b".into()];
        provider.credentials = vec![CredentialConfig::default(), CredentialConfig::default()];
        assert_eq!(
            credential_sources(&provider),
            [
                CredentialSource::ApiKeys(0),
                CredentialSource::ApiKeys(2),
                CredentialSource::Credentials(0),
                CredentialSource::Credentials(1),
            ]
        );
        assert_eq!(
            credential_sources(&provider).len(),
            provider.all_credentials().len()
        );

        // Kinds that need no secret get one implicit credential.
        let mock = ProviderConfig::new("m", ProviderKind::Mock);
        assert_eq!(credential_sources(&mock), [CredentialSource::Implicit]);
        // Kinds that need one get none.
        let empty = ProviderConfig::new("o", ProviderKind::Openai);
        assert!(credential_sources(&empty).is_empty());
    }

    #[test]
    fn a_credential_without_runtime_state_shows_its_configuration() {
        let mut provider = ProviderConfig::new("p", ProviderKind::Openai);
        provider.priority = 3;
        provider.credentials = vec![CredentialConfig {
            api_key: "sk-a…wxyz".into(),
            label: "team".into(),
            disabled: true,
            weight: Some(4),
            ..CredentialConfig::default()
        }];
        let view = credential_view(&provider, CredentialSource::Credentials(0), None);
        assert_eq!(view["id"], Value::Null);
        assert_eq!(view["label"], "team");
        assert_eq!(view["masked_key"], "sk-a…wxyz");
        assert_eq!(view["source"], "credentials");
        assert_eq!(view["index"], 0);
        assert_eq!(view["disabled"], true);
        assert_eq!(view["weight"], 4);
        assert_eq!(view["priority"], 3);
        assert_eq!(view["status"], "unknown");
        assert_eq!(view["usable"], false);
    }

    #[test]
    fn references_are_shown_as_written_and_literals_masked() {
        assert_eq!(mask_value(" env:OPENAI_KEY "), "env:OPENAI_KEY");
        assert_eq!(mask_value("${OPENAI_KEY}"), "${OPENAI_KEY}");
        let masked = mask_value("sk-abcdefghijklmnopqrstuvwxyz0123456789");
        assert!(masked.contains('…'));
        assert!(!masked.contains("ghijklmnop"));
    }

    #[test]
    fn section_views_spell_out_defaults() {
        let aliases = [AliasConfig {
            name: "fast".into(),
            targets: vec!["mock-echo".into()],
            hide_targets: false,
        }];
        assert_eq!(
            aliases_view(&aliases),
            json!([{"name": "fast", "targets": ["mock-echo"], "hide_targets": false}])
        );
        assert_eq!(
            payload_view(&PayloadConfig::default()),
            json!({"default": [], "override": [], "filter": []})
        );
        let prices = [PriceConfig {
            model: "gpt-*".into(),
            input: 1.5,
            output: 6.0,
            cache_read: None,
            cache_write: Some(2.0),
        }];
        assert_eq!(
            pricing_view(&prices),
            json!([{"model": "gpt-*", "input": 1.5, "output": 6.0, "cache_read": null, "cache_write": 2.0}])
        );
    }
}
