//! The configuration as a whole: masked view, raw text, validation, the
//! settings patch and the manual reload.

use super::{JsonBody, from_json_value};
use crate::Shared;
use crate::error::{ApiFailure, ApiResult, ok_json};
use crate::state::blocking;
use crate::views::config_view;
use axum::extract::State;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::time::UNIX_EPOCH;
use switchyard_config_store::{unmask_into, validate_text};
use switchyard_core::Config;
use switchyard_core::config::{AuthConfig, ConfigIssue};

/// Top-level sections `PATCH /settings` may touch. Providers, client keys,
/// aliases, payload rules and prices have their own endpoints, whose rules
/// for secrets and identity a generic patch cannot follow.
const SETTINGS_SECTIONS: [&str; 8] = [
    "server",
    "admin",
    "auth",
    "routing",
    "streaming",
    "upstream",
    "logging",
    "usage",
];

/// `GET /config`: the live configuration with every secret masked.
pub(crate) async fn get_config(State(state): State<Shared>) -> ApiResult {
    ok_json(&config_view(&state, &state.gateway.config()))
}

/// `GET /config/raw`: the file as it is on disk. It is the operator's own
/// file and is returned verbatim, secrets included.
pub(crate) async fn get_raw(State(state): State<Shared>) -> ApiResult {
    let store = state.gateway.config_store().clone();
    let path = store.path().display().to_string();
    let (text, modified_at) = blocking(move || (store.raw_text(), store.modified_at())).await?;
    let text = text.map_err(|error| {
        ApiFailure::internal(format!("the configuration file could not be read: {error}"))
    })?;
    let modified_at = modified_at
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
    ok_json(&json!({
        "text": text,
        "path": path,
        "modified_at": modified_at,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextBody {
    text: String,
}

/// `PUT /config/raw`: validate, write verbatim, apply.
pub(crate) async fn put_raw(
    State(state): State<Shared>,
    JsonBody(body): JsonBody<TextBody>,
) -> ApiResult {
    let config = state.replace_config_text(&body.text).await?;
    ok_json(&config_view(&state, &config))
}

/// `POST /config/validate`: always 200; the verdict is in the body.
pub(crate) async fn validate(JsonBody(body): JsonBody<TextBody>) -> ApiResult {
    let issues = validate_text(&body.text).err().unwrap_or_default();
    ok_json(&json!({
        "ok": issues.is_empty(),
        "issues": issues,
    }))
}

/// `POST /reload`: read the file again and apply it.
pub(crate) async fn reload(State(state): State<Shared>) -> ApiResult {
    let config = state.reload_config().await?;
    ok_json(&config_view(&state, &config))
}

/// `PATCH /settings`: a JSON merge patch over the scalar sections.
pub(crate) async fn patch_settings(
    State(state): State<Shared>,
    JsonBody(patch): JsonBody<Value>,
) -> ApiResult {
    let Value::Object(patch) = patch else {
        return Err(ApiFailure::bad_request(
            "the settings patch must be a JSON object",
        ));
    };
    check_sections(&patch)?;
    let (config, ()) = state
        .edit_config(move |config| apply_settings(config, &patch))
        .await?;
    ok_json(&config_view(&state, &config))
}

/// Refuses a patch that reaches outside the settings sections.
fn check_sections(patch: &Map<String, Value>) -> Result<(), ApiFailure> {
    let mut issues = Vec::new();
    let mut issue = |path: String, message: &str| {
        issues.push(ConfigIssue {
            path,
            message: message.to_string(),
        });
    };
    for (section, value) in patch {
        if !SETTINGS_SECTIONS.contains(&section.as_str()) {
            issue(
                section.clone(),
                "cannot be changed through /settings (server, admin, auth.required, routing, \
                 streaming, upstream, logging and usage can)",
            );
            continue;
        }
        let Value::Object(fields) = value else {
            issue(section.clone(), "must be an object of settings to change");
            continue;
        };
        if section == "auth" {
            for field in fields.keys().filter(|field| *field != "required") {
                issue(
                    format!("auth.{field}"),
                    "cannot be changed through /settings; client keys are managed under /keys",
                );
            }
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    Err(ApiFailure {
        issues,
        ..ApiFailure::bad_request("the settings patch touches fields it may not change")
    })
}

/// RFC 7396: objects merge key by key, `null` removes a key, anything else
/// replaces.
fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target) = target else {
        return;
    };
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// Patches one typed section: serialise, merge, deserialise. Going through
/// the type means unknown fields and wrong types are refused with a message
/// that names them, and a removed key (`null`) falls back to its default.
fn patch_section<T>(section: &mut T, patch: &Value, name: &str) -> Result<(), ConfigIssue>
where
    T: Serialize + DeserializeOwned,
{
    let mut value = serde_json::to_value(&*section).map_err(|error| ConfigIssue {
        path: name.to_string(),
        message: error.to_string(),
    })?;
    merge_patch(&mut value, patch);
    *section = from_json_value(value, name)?;
    Ok(())
}

fn apply_settings(config: &mut Config, patch: &Map<String, Value>) -> Result<(), ApiFailure> {
    let current = config.clone();
    let mut issues = Vec::new();
    for (name, value) in patch {
        let result = match name.as_str() {
            "server" => patch_section(&mut config.server, value, name),
            "admin" => patch_section(&mut config.admin, value, name),
            "routing" => patch_section(&mut config.routing, value, name),
            "streaming" => patch_section(&mut config.streaming, value, name),
            "upstream" => patch_section(&mut config.upstream, value, name),
            "logging" => patch_section(&mut config.logging, value, name),
            "usage" => patch_section(&mut config.usage, value, name),
            "auth" => patch_auth_required(&mut config.auth, value),
            // `check_sections` let nothing else through.
            _ => Ok(()),
        };
        if let Err(issue) = result {
            issues.push(issue);
        }
    }
    if !issues.is_empty() {
        return Err(ApiFailure {
            issues,
            ..ApiFailure::bad_request("the settings patch does not fit the configuration schema")
        });
    }

    // The dashboard only ever saw masks: `admin.secret` and a password in
    // `upstream.proxy` that come back masked (or empty) mean "unchanged".
    // The probe carries no providers or keys, so nothing else is touched.
    let mut probe = Config {
        admin: config.admin.clone(),
        upstream: config.upstream.clone(),
        ..Config::default()
    };
    unmask_into(&mut probe, &current).map_err(ApiFailure::invalid_config)?;
    config.admin = probe.admin;
    config.upstream = probe.upstream;
    Ok(())
}

fn patch_auth_required(auth: &mut AuthConfig, patch: &Value) -> Result<(), ConfigIssue> {
    match patch.get("required") {
        None => Ok(()),
        Some(Value::Bool(required)) => {
            auth.required = *required;
            Ok(())
        }
        Some(Value::Null) => {
            auth.required = AuthConfig::default().required;
            Ok(())
        }
        Some(_) => Err(ConfigIssue {
            path: "auth.required".to_string(),
            message: "must be true or false".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::config::{RequestLogMode, Strategy};

    fn patch(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn merge_patch_follows_rfc_7396() {
        let mut target = json!({"a": "b", "c": {"d": "e", "f": "g"}, "list": [1, 2]});
        merge_patch(
            &mut target,
            &json!({"a": "z", "c": {"f": null, "h": 1}, "list": [3], "new": {"x": null, "y": 2}}),
        );
        assert_eq!(
            target,
            json!({"a": "z", "c": {"d": "e", "h": 1}, "list": [3], "new": {"y": 2}})
        );
        let mut scalar = json!("text");
        merge_patch(&mut scalar, &json!({"k": 1}));
        assert_eq!(scalar, json!({"k": 1}));
    }

    #[test]
    fn sections_are_patched_through_their_types() {
        let mut config = Config::default();
        apply_settings(
            &mut config,
            &patch(json!({
                "routing": {"strategy": "fill-first", "cooldown": {"auth_secs": 60}},
                "logging": {"request_log": "errors"},
                "server": {"tls": {"cert": "c.pem", "key": "k.pem"}},
                "auth": {"required": false},
            })),
        )
        .unwrap();
        assert_eq!(config.routing.strategy, Strategy::FillFirst);
        assert_eq!(config.routing.cooldown.auth_secs, 60);
        // Untouched neighbours keep their values.
        assert_eq!(config.routing.cooldown.quota_secs, 3600);
        assert_eq!(config.routing.max_attempts, 3);
        assert_eq!(config.logging.request_log, RequestLogMode::Errors);
        assert_eq!(config.server.tls.as_ref().unwrap().cert, "c.pem");
        assert!(!config.auth.required);

        // `null` resets to the default.
        apply_settings(
            &mut config,
            &patch(json!({
                "routing": {"strategy": null, "cooldown": null},
                "server": {"tls": null},
                "auth": {"required": null},
            })),
        )
        .unwrap();
        assert_eq!(config.routing.strategy, Strategy::RoundRobin);
        assert_eq!(config.routing.cooldown.auth_secs, 1800);
        assert_eq!(config.server.tls, None);
        assert!(config.auth.required);
    }

    #[test]
    fn schema_errors_name_the_field() {
        let mut config = Config::default();
        let error = apply_settings(
            &mut config,
            &patch(json!({
                "routing": {"cooldown": {"auth_secs": "soon"}},
                "streaming": {"keepalive": 5},
                "auth": {"required": "yes"},
            })),
        )
        .unwrap_err();
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        let paths: Vec<&str> = error.issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths.len(), 3, "{:?}", error.issues);
        assert!(paths.contains(&"routing.cooldown.auth_secs"), "{paths:?}");
        assert!(paths.contains(&"auth.required"), "{paths:?}");
        let unknown = error
            .issues
            .iter()
            .find(|i| i.path.starts_with("streaming"))
            .unwrap();
        assert!(
            unknown.message.contains("unknown field `keepalive`"),
            "{unknown:?}"
        );
    }

    #[test]
    fn other_sections_are_refused() {
        let error = check_sections(&patch(json!({
            "providers": [],
            "auth": {"keys": [], "required": true},
            "routing": "fast",
            "usage": {"enabled": false},
        })))
        .unwrap_err();
        assert_eq!(error.status, http::StatusCode::BAD_REQUEST);
        let paths: Vec<&str> = error.issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["providers", "auth.keys", "routing"]);
        assert!(check_sections(&patch(json!({"usage": {"enabled": false}}))).is_ok());
        assert!(check_sections(&patch(json!({}))).is_ok());
    }

    #[test]
    fn the_admin_secret_follows_the_mask_rule() {
        const SECRET: &str = "admin-secret-0123456789abcdef";
        let mut stored = Config::default();
        stored.admin.secret = SECRET.to_string();
        stored.upstream.proxy = "http://user:proxy-password-123456@proxy.test:3128".to_string();
        let masked = switchyard_config_store::mask_config(&stored);

        // The mask, or nothing, keeps the stored secret.
        for unchanged in [masked.admin.secret.as_str(), ""] {
            let mut config = stored.clone();
            apply_settings(
                &mut config,
                &patch(json!({"admin": {"secret": unchanged, "allow_remote": true}})),
            )
            .unwrap();
            assert_eq!(config.admin.secret, SECRET);
            assert!(config.admin.allow_remote);
        }

        // A new literal or a reference replaces it.
        for new in ["a-brand-new-secret-value", "env:SWITCHYARD_TEST_ADMIN"] {
            let mut config = stored.clone();
            apply_settings(&mut config, &patch(json!({"admin": {"secret": new}}))).unwrap();
            assert_eq!(config.admin.secret, new);
        }

        // A mask that is not the stored secret's cannot be resolved.
        let mut config = stored.clone();
        let error = apply_settings(&mut config, &patch(json!({"admin": {"secret": "zzzz…zz"}})))
            .unwrap_err();
        assert_eq!(error.status, http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.issues[0].path, "admin.secret");

        // The proxy password comes back masked when another field of the
        // section is edited.
        let mut config = stored.clone();
        apply_settings(
            &mut config,
            &patch(
                json!({"upstream": {"proxy": masked.upstream.proxy, "connect_timeout_secs": 5}}),
            ),
        )
        .unwrap();
        assert_eq!(config.upstream.proxy, stored.upstream.proxy);
        assert_eq!(config.upstream.connect_timeout_secs, 5);
    }
}
