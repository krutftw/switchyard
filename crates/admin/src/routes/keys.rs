//! Client keys: the keys applications present to use the gateway.

use super::{JsonBody, PathParam};
use crate::Shared;
use crate::error::{ApiFailure, ApiResult, json_with_status, ok_json};
use crate::state::blocking;
use crate::views::{clean_models, key_id, key_view};
use axum::extract::State;
use http::StatusCode;
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Deserializer};
use serde_json::json;
use std::sync::Arc;
use switchyard_core::config::{ClientKey, is_secret_reference, resolve_secret};
use switchyard_core::util::now_unix_ms;

/// Prefix of generated keys.
const KEY_PREFIX: &str = "sy-";
/// Random characters of a generated key (about 238 bits).
const KEY_RANDOM_CHARS: usize = 40;
/// Longest key name accepted.
const MAX_NAME_CHARS: usize = 100;

/// A fresh client key: `sy-` and 40 random letters and digits. Letters and
/// digits only, so the key survives URLs, shells and a double-click.
fn generate_key() -> String {
    let mut key = String::with_capacity(KEY_PREFIX.len() + KEY_RANDOM_CHARS);
    key.push_str(KEY_PREFIX);
    Alphanumeric.append_string(&mut rand::rng(), &mut key, KEY_RANDOM_CHARS);
    key
}

/// The views of `keys`, in order, each with the usage of the last 30 days.
///
/// Usage is looked up by the key's id — what the request records carry —
/// and not by its name, which the usage store's own per-key breakdown goes
/// by: a renamed key keeps its numbers, and a key that is given a name
/// another key had does not inherit that key's traffic (see
/// [`crate::key_usage`]). Blocking work: the usage files are read.
async fn views_of(
    state: &Shared,
    keys: Vec<ClientKey>,
) -> Result<Vec<serde_json::Value>, ApiFailure> {
    let state = Arc::clone(state);
    blocking(move || {
        let usage = state
            .key_usage
            .usage(state.gateway.telemetry(), now_unix_ms());
        keys.iter()
            .map(|key| {
                let used = usage.get(&key_id(key)).copied().unwrap_or_default();
                key_view(key, &used)
            })
            .collect()
    })
    .await
}

/// `GET /keys`.
pub(crate) async fn list(State(state): State<Shared>) -> ApiResult {
    let keys = state.gateway.config().auth.keys.clone();
    ok_json(&views_of(&state, keys).await?)
}

fn unknown_key(id: &str) -> ApiFailure {
    ApiFailure::not_found(format!("there is no client key with the id `{id}`"))
}

/// A usable name: trimmed, not empty, not absurdly long.
fn clean_name(name: &str) -> Result<String, ApiFailure> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiFailure::bad_field("name", "must not be empty"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiFailure::bad_field(
            "name",
            format!("must be at most {MAX_NAME_CHARS} characters"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ApiFailure::bad_field(
            "name",
            "must not contain control characters",
        ));
    }
    Ok(name.to_string())
}

/// 409 when another key (any but `except`) already goes by `name`. The
/// usage statistics (`/usage/summary`, `/usage/timeseries`) and the request
/// list label requests with the key's name, so names have to tell the keys
/// of a configuration apart.
fn check_name_free(
    keys: &[ClientKey],
    name: &str,
    except: Option<usize>,
) -> Result<(), ApiFailure> {
    let taken = keys
        .iter()
        .enumerate()
        .any(|(index, key)| Some(index) != except && key.name.trim().eq_ignore_ascii_case(name));
    if taken {
        return Err(ApiFailure::conflict_on(
            "name",
            "is the name of another client key",
            format!("a client key named `{name}` already exists"),
        ));
    }
    Ok(())
}

/// A requests-per-minute limit as a request body gives it.
///
/// Read through a visitor of its own so that a value of the wrong kind is
/// answered with the range a limit has ("a whole number from 1 to
/// 4294967295") rather than the range of the integer type behind it. `0`
/// is let through: the configuration's own rule refuses it, with a message
/// that says how "no limit" is written.
struct RateLimit(u32);

impl<'de> Deserialize<'de> for RateLimit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Limit;

        impl serde::de::Visitor<'_> for Limit {
            type Value = RateLimit;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a whole number from 1 to 4294967295")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<RateLimit, E> {
                u32::try_from(value)
                    .map(RateLimit)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Unsigned(value), &self))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<RateLimit, E> {
                u32::try_from(value)
                    .map(RateLimit)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(value), &self))
            }
        }

        deserializer.deserialize_u64(Limit)
    }
}

/// An optional limit: absent or `null` is none.
fn rate_limit<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<RateLimit>::deserialize(deserializer)?.map(|limit| limit.0))
}

/// A limit in a patch: absent is `None` (keep), `null` is `Some(None)`
/// (remove the limit).
fn rate_limit_patch<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<u32>>, D::Error> {
    Ok(nullable::<D, RateLimit>(deserializer)?.map(|limit| limit.map(|limit| limit.0)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateKey {
    name: String,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default, deserialize_with = "rate_limit")]
    rate_limit_rpm: Option<u32>,
    #[serde(default)]
    key: Option<String>,
}

/// `POST /keys`: adds a client key, generating one unless the body brings
/// its own. The answer is the only time the full key is shown, apart from
/// `POST /keys/{id}/reveal`.
pub(crate) async fn create(
    State(state): State<Shared>,
    JsonBody(body): JsonBody<CreateKey>,
) -> ApiResult {
    let name = clean_name(&body.name)?;
    let key = match body.key.as_deref().map(str::trim) {
        Some(key) if !key.is_empty() => {
            if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(ApiFailure::bad_field(
                    "key",
                    "must not contain spaces or control characters",
                ));
            }
            key.to_string()
        }
        _ => generate_key(),
    };
    let entry = ClientKey {
        key,
        name,
        enabled: true,
        models: clean_models(&body.models.unwrap_or_default()),
        rate_limit_rpm: body.rate_limit_rpm,
    };
    let created = entry.clone();
    state
        .edit_config(move |config, scope| {
            scope.set(format!("auth.keys[{}]", config.auth.keys.len()));
            check_name_free(&config.auth.keys, &entry.name, None)?;
            let resolved = resolve_secret(&entry.key).ok();
            let duplicate = config.auth.keys.iter().any(|existing| {
                existing.key.trim() == entry.key
                    || (resolved.is_some() && resolve_secret(&existing.key).ok() == resolved)
            });
            if duplicate {
                return Err(ApiFailure::conflict_on(
                    "key",
                    "is already a client key",
                    "this client key already exists",
                ));
            }
            config.auth.keys.push(entry);
            Ok(())
        })
        .await?;
    json_with_status(
        StatusCode::CREATED,
        &json!({
            "id": key_id(&created),
            "key": created.key,
            "is_reference": is_secret_reference(&created.key),
        }),
    )
}

/// Tells an absent field from an explicit `null`: absent is `None`, `null`
/// is `Some(None)`.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateKey {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    models: Option<Vec<String>>,
    /// `null` removes the limit.
    #[serde(default, deserialize_with = "rate_limit_patch")]
    rate_limit_rpm: Option<Option<u32>>,
}

/// `PATCH /keys/{id}`: changes what is given, keeps the rest. The key
/// itself cannot be changed: create a new one instead.
pub(crate) async fn update(
    State(state): State<Shared>,
    PathParam(id): PathParam,
    JsonBody(body): JsonBody<UpdateKey>,
) -> ApiResult {
    let name = body.name.as_deref().map(clean_name).transpose()?;
    let (_, updated) = state
        .edit_config(move |config, scope| {
            let keys = &mut config.auth.keys;
            let Some(index) = keys.iter().position(|key| key_id(key) == id) else {
                return Err(unknown_key(&id));
            };
            scope.set(format!("auth.keys[{index}]"));
            if let Some(name) = name {
                check_name_free(keys, &name, Some(index))?;
                keys[index].name = name;
            }
            let key = &mut keys[index];
            if let Some(enabled) = body.enabled {
                key.enabled = enabled;
            }
            if let Some(models) = body.models {
                key.models = clean_models(&models);
            }
            if let Some(limit) = body.rate_limit_rpm {
                key.rate_limit_rpm = limit;
            }
            Ok(key.clone())
        })
        .await?;
    let view = views_of(&state, vec![updated]).await?.pop();
    ok_json(&view)
}

/// `DELETE /keys/{id}`.
pub(crate) async fn remove(State(state): State<Shared>, PathParam(id): PathParam) -> ApiResult {
    state
        .edit_config(move |config, _| {
            let keys = &mut config.auth.keys;
            let Some(index) = keys.iter().position(|key| key_id(key) == id) else {
                return Err(unknown_key(&id));
            };
            keys.remove(index);
            Ok(())
        })
        .await?;
    ok_json(&json!({ "ok": true }))
}

/// `POST /keys/{id}/reveal`: the key as the configuration holds it. For a
/// reference that is the reference (`env:NAME`), not the variable's value.
pub(crate) async fn reveal(State(state): State<Shared>, PathParam(id): PathParam) -> ApiResult {
    let config = state.gateway.config();
    let key = config
        .auth
        .keys
        .iter()
        .find(|key| key_id(key) == id)
        .ok_or_else(|| unknown_key(&id))?;
    ok_json(&json!({
        "key": key.key.trim(),
        "is_reference": is_secret_reference(&key.key),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn generated_keys_are_long_random_and_plain() {
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a.starts_with("sy-"));
        assert!(a[3..].bytes().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn names_are_cleaned_and_checked() {
        assert_eq!(clean_name("  laptop ").unwrap(), "laptop");
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(clean_name("line\nbreak").is_err());
        assert_eq!(
            clean_models(&[" gpt-* ", "", "claude-*", "gpt-*", "  "]),
            ["gpt-*", "claude-*"]
        );
    }

    #[test]
    fn names_must_be_unique_ignoring_case() {
        let key = |name: &str| ClientKey {
            key: format!("sy-{name}"),
            name: name.to_string(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        };
        let keys = [key("laptop"), key("ci")];
        assert_eq!(
            check_name_free(&keys, "LAPTOP", None).unwrap_err().status,
            StatusCode::CONFLICT
        );
        // Renaming a key to its own name is not a conflict.
        assert!(check_name_free(&keys, "laptop", Some(0)).is_ok());
        assert!(check_name_free(&keys, "phone", None).is_ok());
    }

    #[test]
    fn null_and_absent_limits_are_told_apart() {
        let absent: UpdateKey = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.rate_limit_rpm, None);
        let cleared: UpdateKey = serde_json::from_str(r#"{"rate_limit_rpm": null}"#).unwrap();
        assert_eq!(cleared.rate_limit_rpm, Some(None));
        let set: UpdateKey = serde_json::from_str(r#"{"rate_limit_rpm": 60}"#).unwrap();
        assert_eq!(set.rate_limit_rpm, Some(Some(60)));
        assert!(serde_json::from_str::<UpdateKey>(r#"{"key": "x"}"#).is_err());
    }

    #[test]
    fn a_limit_is_read_with_its_own_range() {
        let created =
            |body: &str| serde_json::from_str::<CreateKey>(body).map(|key| key.rate_limit_rpm);
        assert_eq!(created(r#"{"name": "a"}"#).unwrap(), None);
        assert_eq!(
            created(r#"{"name": "a", "rate_limit_rpm": null}"#).unwrap(),
            None
        );
        assert_eq!(
            created(r#"{"name": "a", "rate_limit_rpm": 4294967295}"#).unwrap(),
            Some(u32::MAX)
        );
        // 0 is read; the configuration's rule refuses it with a message
        // that says how "no limit" is written.
        assert_eq!(
            created(r#"{"name": "a", "rate_limit_rpm": 0}"#).unwrap(),
            Some(0)
        );
        for wrong in ["-5", "4294967296", "1.5", "\"many\"", "true", "[60]"] {
            let error = created(&format!(r#"{{"name": "a", "rate_limit_rpm": {wrong}}}"#))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("expected a whole number from 1 to 4294967295"),
                "{wrong}: {error}"
            );
        }
        let patched = |body: &str| {
            serde_json::from_str::<UpdateKey>(body)
                .map(|key| key.rate_limit_rpm)
                .map_err(|error| error.to_string())
        };
        assert!(
            patched(r#"{"rate_limit_rpm": -1}"#)
                .unwrap_err()
                .contains("expected a whole number from 1 to 4294967295")
        );
    }
}
