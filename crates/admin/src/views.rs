//! The JSON the admin API answers with. Everything here is built from a
//! configuration whose secrets are already masked ([`mask_config`]) and from
//! runtime snapshots that never held one.
//!
//! Shapes are stable: a field is always present (`null`, `""`, `[]` when it
//! has no value), so the dashboard never has to guess whether a key exists.

use crate::state::AdminState;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use switchyard_config_store::{client_key_id, mask_config};
use switchyard_core::Config;
use switchyard_core::config::{
    AliasConfig, ClientKey, CredentialConfig, PayloadConfig, PayloadRule, PriceConfig,
    ProviderConfig, is_secret_reference, resolve_secret,
};
use switchyard_core::util::mask_secret;
use switchyard_scheduler::{CredentialSnapshot, ModelEntry, ProviderSnapshot};
use switchyard_telemetry::Totals;

/// `{"config", "path", "restart_required"}` — the answer of `GET /config`
/// and of every mutation that returns the whole configuration.
pub(crate) fn config_view(state: &AdminState, config: &Config) -> Value {
    let store = state.gateway.config_store();
    json!({
        "config": to_value(&mask_config(config)),
        "path": store.path().display().to_string(),
        "restart_required": store.restart_required(),
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
    let defaults: [(&str, Value); 19] = [
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
        ("websocket", json!(false)),
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
    full
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

/// One provider as `GET /providers` shows it: its configuration (secrets
/// masked), its credentials merged with their runtime state, and the
/// client-facing names of its models.
pub(crate) fn provider_view(
    masked: &ProviderConfig,
    runtime: Option<&ProviderSnapshot>,
    models: &[ModelEntry],
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
    let names: Vec<&str> = models
        .iter()
        .filter(|entry| entry.alias_targets.is_none())
        .filter(|entry| entry.routes.iter().any(|r| r.provider == masked.name))
        .map(|entry| entry.name.as_str())
        .collect();
    let model_count = runtime.map_or(names.len(), |r| r.models);

    view.insert("credentials".to_string(), Value::Array(credentials));
    view.insert("models".to_string(), json!(names));
    view.insert("model_count".to_string(), json!(model_count));
    view.insert(
        "effective_base_url".to_string(),
        json!(masked.effective_base_url()),
    );
    view.insert("protocols".to_string(), json!(masked.protocols()));
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
    masked
        .providers
        .iter()
        .map(|provider| {
            let runtime = by_name
                .get(provider.name.as_str())
                .copied()
                // A rebuild between the comparison above and the snapshot
                // shows as a different number of credentials.
                .filter(|runtime| runtime.credentials.len() == credential_sources(provider).len());
            provider_view(provider, runtime, &models)
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
    provider_views(state, config).into_iter().nth(index)
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

pub(crate) fn key_view(key: &ClientKey, usage: &KeyUsage) -> Value {
    json!({
        "id": key_id(key),
        "name": key.name,
        "masked": mask_value(&key.key),
        "is_reference": is_secret_reference(&key.key),
        "resolved": key_resolved(key),
        "enabled": key.enabled,
        "models": key.models,
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
                "websocket",
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
