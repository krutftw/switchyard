//! Providers and their credentials.

use super::{JsonBody, OptionalJson, PathParam};
use crate::Shared;
use crate::error::{ApiFailure, ApiResult, json_with_status, ok_json};
use crate::views::{
    CredentialSource, credential_sources, provider_view_by_name, provider_views, scheduled_config,
};
use axum::extract::State;
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use switchyard_config_store::unmask_into;
use switchyard_core::config::{ConfigIssue, CredentialConfig, ProviderConfig, resolve_secret};
use switchyard_core::{ApiError, Config, ErrorKind};
use switchyard_scheduler::Scheduler;

/// `GET /providers`.
pub(crate) async fn list(State(state): State<Shared>) -> ApiResult {
    ok_json(&provider_views(&state, &scheduled_config(&state)))
}

/// `GET /providers/{name}`.
pub(crate) async fn get_one(State(state): State<Shared>, PathParam(name): PathParam) -> ApiResult {
    let view = provider_view_by_name(&state, &scheduled_config(&state), &name)
        .ok_or_else(|| unknown_provider(&name))?;
    ok_json(&view)
}

fn unknown_provider(name: &str) -> ApiFailure {
    ApiFailure::not_found(format!("there is no provider named `{name}`"))
}

/// The view of a provider after an edit. The provider can only be missing
/// when another edit removed it in between.
fn view_after_edit(
    state: &Shared,
    config: &Config,
    name: &str,
) -> Result<serde_json::Value, ApiFailure> {
    provider_view_by_name(state, config, name).ok_or_else(|| unknown_provider(name))
}

/// Tidies a provider entry that arrived from the dashboard: the name is
/// trimmed and blank `api_keys` rows (an empty line in the form) are
/// dropped. This happens before secrets are restored, so an emptied row is
/// a removed key, never "the key that used to be at this position".
fn normalise(mut provider: ProviderConfig) -> ProviderConfig {
    provider.name = provider.name.trim().to_string();
    provider.api_keys.retain(|key| !key.trim().is_empty());
    provider
}

/// Puts the stored secrets back into an incoming provider entry whose
/// secrets are masked or empty, as `unmask_into` defines it. `index` is the
/// position the entry takes in `current` (its length for a new provider).
///
/// Only issues about this provider are reported: the rest of the
/// configuration is not part of the request.
fn restore_secrets(
    current: &Config,
    index: usize,
    incoming: ProviderConfig,
) -> Result<ProviderConfig, ApiFailure> {
    let mut probe = current.clone();
    if index < probe.providers.len() {
        probe.providers[index] = incoming;
    } else {
        probe.providers.push(incoming);
    }
    if let Err(issues) = unmask_into(&mut probe, current) {
        let prefix = format!("providers[{index}]");
        let mine: Vec<ConfigIssue> = issues
            .into_iter()
            .filter(|issue| {
                issue
                    .path
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            })
            .collect();
        if !mine.is_empty() {
            return Err(ApiFailure::invalid_config(mine));
        }
    }
    let index = index.min(probe.providers.len().saturating_sub(1));
    Ok(probe.providers.swap_remove(index))
}

/// `POST /providers`: adds a provider. Its name must be new.
pub(crate) async fn create(
    State(state): State<Shared>,
    JsonBody(body): JsonBody<ProviderConfig>,
) -> ApiResult {
    let body = normalise(body);
    let name = body.name.clone();
    let (config, ()) = state
        .edit_config(move |config| {
            if config.provider(&body.name).is_some() {
                return Err(ApiFailure::conflict(format!(
                    "a provider named `{}` already exists",
                    body.name
                )));
            }
            // A new provider inherits nothing: a masked secret in it cannot
            // be resolved and is reported.
            let provider = restore_secrets(config, config.providers.len(), body)?;
            config.providers.push(provider);
            Ok(())
        })
        .await?;
    json_with_status(
        StatusCode::CREATED,
        &view_after_edit(&state, &config, &name)?,
    )
}

/// `PUT /providers/{name}`: replaces a provider. A different `name` in the
/// body renames it.
pub(crate) async fn replace(
    State(state): State<Shared>,
    PathParam(name): PathParam,
    JsonBody(body): JsonBody<ProviderConfig>,
) -> ApiResult {
    let body = normalise(body);
    let new_name = body.name.clone();
    let (config, ()) = state
        .edit_config(move |config| {
            let Some(index) = config.providers.iter().position(|p| p.name == name) else {
                return Err(unknown_provider(&name));
            };
            let renamed = body.name != name;
            if renamed && config.provider(&body.name).is_some() {
                return Err(ApiFailure::conflict(format!(
                    "a provider named `{}` already exists",
                    body.name
                )));
            }
            let provider = restore_secrets(config, index, body)?;
            if renamed && !provider.name.is_empty() {
                // Payload rules name the provider they apply to.
                let payload = &mut config.payload;
                for rule in payload
                    .default
                    .iter_mut()
                    .chain(payload.overrides.iter_mut())
                    .chain(payload.filter.iter_mut())
                {
                    if rule.provider.trim() == name {
                        rule.provider = provider.name.clone();
                    }
                }
            }
            config.providers[index] = provider;
            Ok(())
        })
        .await?;
    ok_json(&view_after_edit(&state, &config, &new_name)?)
}

/// `DELETE /providers/{name}`.
pub(crate) async fn remove(State(state): State<Shared>, PathParam(name): PathParam) -> ApiResult {
    state
        .edit_config(move |config| {
            let before = config.providers.len();
            config.providers.retain(|provider| provider.name != name);
            if config.providers.len() == before {
                return Err(unknown_provider(&name));
            }
            Ok(())
        })
        .await?;
    ok_json(&json!({ "ok": true }))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestBody {
    #[serde(default)]
    model: Option<String>,
}

/// `POST /providers/{name}/test`: one tiny request through the provider.
/// Always 200 for a provider that exists; the outcome is in the body.
pub(crate) async fn test(
    State(state): State<Shared>,
    PathParam(name): PathParam,
    OptionalJson(body): OptionalJson<TestBody>,
) -> ApiResult {
    if state.gateway.config().provider(&name).is_none() {
        return Err(unknown_provider(&name));
    }
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let outcome = state.gateway.test_provider(&name, model).await;
    ok_json(&outcome)
}

/// `POST /providers/{name}/discover`: ask the upstream for its models now.
pub(crate) async fn discover(State(state): State<Shared>, PathParam(name): PathParam) -> ApiResult {
    if state.gateway.config().provider(&name).is_none() {
        return Err(unknown_provider(&name));
    }
    match state.gateway.discover(&name).await {
        Ok(models) => ok_json(&json!({ "models": models })),
        Err(error) => Err(discover_failure(&state, &name, &error)),
    }
}

/// Why a model listing could not be had, as an admin API failure.
///
/// The gateway reports two conditions of its own — it does not know the
/// provider (404), the provider has no credential to ask with (503) — and
/// otherwise whatever the upstream answered, classified the way a client
/// request would be: 429 with a wait for a rate limit, 404 for an address
/// without a model listing, 400 or 422 for a request the upstream did not
/// like. Those upstream classes must not reach the dashboard as admin
/// statuses (see [`ApiFailure::upstream`]), so the two cases are told apart
/// here, by asking the scheduler what the gateway asked it.
fn discover_failure(state: &Shared, name: &str, error: &ApiError) -> ApiFailure {
    let scheduler = state.gateway.scheduler();
    if scheduler.provider_config(name).is_none() {
        // Removed while the request was on its way.
        return unknown_provider(name);
    }
    if scheduler.credentials(name).is_empty() {
        return ApiFailure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("provider `{name}` has no usable credential to ask its upstream with"),
        );
    }
    let what = match error.kind {
        ErrorKind::RateLimit => {
            "the upstream refused to list its models: it is rate limiting this credential \
             or the credential is out of quota"
        }
        ErrorKind::NotFound => "the upstream has no model listing at this address",
        ErrorKind::Timeout => "the upstream did not answer the model listing request in time",
        ErrorKind::Unavailable => "the upstream is unavailable and did not list its models",
        ErrorKind::InvalidRequest
        | ErrorKind::TooLarge
        | ErrorKind::Authentication
        | ErrorKind::Permission => "the upstream rejected the model listing request",
        ErrorKind::Upstream | ErrorKind::Internal => "the upstream did not list its models",
    };
    ApiFailure::upstream(what, error)
}

fn unknown_credential(id: &str) -> ApiFailure {
    ApiFailure::not_found(format!("there is no credential with the id `{id}`"))
}

/// `POST /credentials/{id}/reset`: clear cooldowns and failure streaks.
pub(crate) async fn reset_credential(
    State(state): State<Shared>,
    PathParam(id): PathParam,
) -> ApiResult {
    if !state.gateway.scheduler().reset_cooldowns(&id) {
        return Err(unknown_credential(&id));
    }
    ok_json(&json!({ "ok": true }))
}

/// `POST /credentials/{id}/disable`.
pub(crate) async fn disable_credential(
    State(state): State<Shared>,
    PathParam(id): PathParam,
) -> ApiResult {
    set_credential_disabled(state, id, true).await
}

/// `POST /credentials/{id}/enable`.
pub(crate) async fn enable_credential(
    State(state): State<Shared>,
    PathParam(id): PathParam,
) -> ApiResult {
    set_credential_disabled(state, id, false).await
}

/// Where the credential `id` sits in `config`: provider index and source.
///
/// Credential ids are derived by the scheduler, so the mapping is taken
/// from a scheduler built for exactly this configuration — the one being
/// edited under the store's lock, which may be newer than the one the
/// running scheduler was built from.
fn locate_credential(config: &Config, id: &str) -> Option<(usize, CredentialSource)> {
    let scheduler = Scheduler::new(config, &resolve_secret);
    for (provider_index, snapshot) in scheduler.snapshot().iter().enumerate() {
        let Some(position) = snapshot.credentials.iter().position(|c| c.id == id) else {
            continue;
        };
        let provider = config.providers.get(provider_index)?;
        let source = credential_sources(provider).get(position).copied()?;
        return Some((provider_index, source));
    }
    None
}

/// Writes the `disabled` flag of a credential into the configuration, so
/// the choice survives a restart.
///
/// A `credentials[]` entry has the flag. An `api_keys[]` entry is a bare
/// string and has nowhere to keep it, so disabling one turns it into a
/// `credentials[]` entry (`{ api_key, disabled = true }`); the credential's
/// id — a hash of provider and key — and with it its runtime state stay the
/// same. The same goes for the implicit keyless credential of a provider
/// that needs no secret.
fn write_disabled(
    provider: &mut ProviderConfig,
    source: CredentialSource,
    disabled: bool,
) -> Result<(), ApiFailure> {
    let gone = || ApiFailure::conflict("the credential changed while it was being edited");
    match source {
        CredentialSource::Credentials(index) => {
            provider
                .credentials
                .get_mut(index)
                .ok_or_else(gone)?
                .disabled = disabled;
        }
        // Shorthand keys and the implicit credential are enabled by
        // definition: only disabling needs to write anything.
        CredentialSource::ApiKeys(index) if disabled => {
            if index >= provider.api_keys.len() {
                return Err(gone());
            }
            let api_key = provider.api_keys.remove(index);
            provider.credentials.push(CredentialConfig {
                api_key,
                disabled: true,
                ..CredentialConfig::default()
            });
        }
        CredentialSource::Implicit if disabled => {
            provider.credentials.push(CredentialConfig {
                disabled: true,
                ..CredentialConfig::default()
            });
        }
        CredentialSource::ApiKeys(_) | CredentialSource::Implicit => {}
    }
    Ok(())
}

async fn set_credential_disabled(state: Shared, id: String, disabled: bool) -> ApiResult {
    let lookup = id.clone();
    let (config, provider_name) = state
        .edit_config(move |config| {
            let (provider_index, source) =
                locate_credential(config, &lookup).ok_or_else(|| unknown_credential(&lookup))?;
            let provider = &mut config.providers[provider_index];
            write_disabled(provider, source, disabled)?;
            Ok(provider.name.clone())
        })
        .await?;
    // The configuration is now the one place the choice lives; a switch
    // flipped at runtime earlier must not contradict it.
    state.gateway.scheduler().set_runtime_disabled(&id, false);
    ok_json(&view_after_edit(&state, &config, &provider_name)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_config_store::mask_config;
    use switchyard_core::config::ProviderKind;

    const KEY_A: &str = "sk-provider-aaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY_B: &str = "sk-provider-bbbbbbbbbbbbbbbbbbbbbbbb";
    const KEY_C: &str = "sk-provider-cccccccccccccccccccccccc";

    fn config() -> Config {
        let mut first = ProviderConfig::new("first", ProviderKind::Openai);
        first.api_keys = vec![KEY_A.into(), KEY_B.into()];
        let mut second = ProviderConfig::new("second", ProviderKind::Anthropic);
        second.credentials = vec![CredentialConfig {
            api_key: KEY_C.into(),
            label: "team".into(),
            ..CredentialConfig::default()
        }];
        Config {
            providers: vec![first, second],
            ..Config::default()
        }
    }

    #[test]
    fn masked_and_dropped_rows_resolve_to_the_right_keys() {
        let stored = config();
        let masked = mask_config(&stored);

        // Unchanged masks keep their keys.
        let back = restore_secrets(&stored, 0, normalise(masked.providers[0].clone())).unwrap();
        assert_eq!(back.api_keys, [KEY_A, KEY_B]);

        // First key deleted, a blank row left behind, a new key added: the
        // blank row is gone and nothing slides into the wrong slot.
        let mut edited = masked.providers[0].clone();
        edited.api_keys = vec![
            String::new(),
            edited.api_keys[1].clone(),
            "sk-new-key".into(),
        ];
        let back = restore_secrets(&stored, 0, normalise(edited)).unwrap();
        assert_eq!(back.api_keys, [KEY_B, "sk-new-key"]);

        // A blank row alone removes the keys; it does not resurrect one.
        let mut emptied = masked.providers[0].clone();
        emptied.api_keys = vec!["  ".into()];
        let back = restore_secrets(&stored, 0, normalise(emptied)).unwrap();
        assert!(back.api_keys.is_empty());
    }

    #[test]
    fn a_new_provider_inherits_nothing() {
        let stored = config();
        let masked = mask_config(&stored);
        let mut fresh = ProviderConfig::new("third", ProviderKind::Openai);
        fresh.api_keys = vec![masked.providers[0].api_keys[0].clone()];
        let error = restore_secrets(&stored, stored.providers.len(), fresh).unwrap_err();
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.issues[0].path, "providers[2].api_keys[0]");

        let mut literal = ProviderConfig::new("third", ProviderKind::Openai);
        literal.api_keys = vec!["sk-literal".into(), "env:THIRD_KEY".into()];
        let back = restore_secrets(&stored, stored.providers.len(), literal).unwrap();
        assert_eq!(back.api_keys, ["sk-literal", "env:THIRD_KEY"]);
    }

    #[test]
    fn a_rename_keeps_the_secrets() {
        let stored = config();
        let mut renamed = mask_config(&stored).providers[1].clone();
        renamed.name = "claude".into();
        let back = restore_secrets(&stored, 1, renamed).unwrap();
        assert_eq!(back.name, "claude");
        assert_eq!(back.credentials[0].api_key, KEY_C);
    }

    #[test]
    fn credentials_are_located_in_the_configuration_being_edited() {
        let stored = config();
        let scheduler = Scheduler::new(&stored, &resolve_secret);
        let snapshot = scheduler.snapshot();
        let id_b = &snapshot[0].credentials[1].id;
        let id_c = &snapshot[1].credentials[0].id;
        assert_eq!(
            locate_credential(&stored, id_b),
            Some((0, CredentialSource::ApiKeys(1)))
        );
        assert_eq!(
            locate_credential(&stored, id_c),
            Some((1, CredentialSource::Credentials(0)))
        );
        assert_eq!(locate_credential(&stored, "first:nope"), None);
    }

    #[test]
    fn disabling_a_shorthand_key_turns_it_into_a_credential_with_the_same_id() {
        let mut stored = config();
        let before = Scheduler::new(&stored, &resolve_secret).snapshot();
        let id_a = before[0].credentials[0].id.clone();

        write_disabled(&mut stored.providers[0], CredentialSource::ApiKeys(0), true).unwrap();
        assert_eq!(stored.providers[0].api_keys, [KEY_B]);
        assert_eq!(
            stored.providers[0].credentials,
            [CredentialConfig {
                api_key: KEY_A.into(),
                disabled: true,
                ..CredentialConfig::default()
            }]
        );
        assert!(stored.validate().is_empty());

        let after = Scheduler::new(&stored, &resolve_secret).snapshot();
        let moved = after[0].credentials.iter().find(|c| c.id == id_a).unwrap();
        assert!(moved.disabled);

        // Enabling it again flips the flag of the entry it has become.
        let (index, source) = locate_credential(&stored, &id_a).unwrap();
        assert_eq!((index, source), (0, CredentialSource::Credentials(0)));
        write_disabled(&mut stored.providers[0], source, false).unwrap();
        assert!(!stored.providers[0].credentials[0].disabled);

        // Enabling a shorthand key changes nothing.
        let unchanged = stored.clone();
        write_disabled(
            &mut stored.providers[0],
            CredentialSource::ApiKeys(0),
            false,
        )
        .unwrap();
        assert_eq!(stored, unchanged);
    }

    #[test]
    fn the_implicit_credential_of_a_keyless_provider_can_be_disabled() {
        let mut mock = ProviderConfig::new("mock", ProviderKind::Mock);
        write_disabled(&mut mock, CredentialSource::Implicit, true).unwrap();
        assert_eq!(mock.credentials.len(), 1);
        assert!(mock.credentials[0].disabled);
        let config = Config {
            providers: vec![mock],
            ..Config::default()
        };
        assert!(config.validate().is_empty());
        let snapshot = Scheduler::new(&config, &resolve_secret).snapshot();
        assert_eq!(snapshot[0].credentials.len(), 1);
        assert!(snapshot[0].credentials[0].disabled);
    }
}
