//! A deliberately narrow setup surface. Secrets are accepted once and never returned.
use crate::api::{AppState, body, no_query};
use crate::{AppError, Result};
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use switchyard_core::config::{ProviderConfig, ProviderKind, is_secret_reference, resolve_secret};
use switchyard_gateway::{DiscoveryState, DiscoveryStatus, Gateway};

const APPLY_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

// Deliberately no Debug or Serialize on types that can contain credentials.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProvider {
    name: String,
    kind: SetupKind,
    credential: CredentialInput,
    base_url: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SetupKind {
    Openai,
    Anthropic,
    Gemini,
    Ollama,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum CredentialInput {
    ApiKey { value: String },
    Env { name: String },
    None {},
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshInput {}

#[derive(Serialize)]
struct ProviderView {
    name: String,
    kind: &'static str,
    enabled: bool,
    config_applied: bool,
    credential_source: &'static str,
    credential_present: bool,
    environment_variable: Option<String>,
    status: &'static str,
    discovery: CatalogView,
    model_count: usize,
    message: &'static str,
}

#[derive(Serialize)]
struct CatalogView {
    state: DiscoveryStatus,
    models: usize,
    at: Option<i64>,
}

fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

fn valid_environment_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && (name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn environment_name(value: &str) -> Option<&str> {
    let value = value.trim();
    value
        .strip_prefix("env:")
        .or_else(|| {
            value
                .strip_prefix("${")
                .and_then(|value| value.strip_suffix('}'))
        })
        .map(str::trim)
        .filter(|name| valid_environment_name(name))
}

fn ollama_url(value: Option<&str>) -> Result<String> {
    let value = value.unwrap_or("http://127.0.0.1:11434/v1");
    // Inspect the literal authority as well as the parsed URL. This rejects
    // numeric aliases, user info, DNS names and parser-normalized URL tricks.
    let (authority, raw_path) = value
        .strip_prefix("http://")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(|| AppError::invalid("Ollama requires an HTTP loopback URL ending in /v1."))?;
    let allowed_host = ["127.0.0.1", "[::1]", "localhost"].iter().any(|host| {
        authority == *host
            || authority.strip_prefix(host).is_some_and(|suffix| {
                suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
    });
    let mut url = url::Url::parse(value)
        .map_err(|_| AppError::invalid("Ollama requires a valid loopback URL."))?;
    if !allowed_host
        || !matches!(raw_path, "v1" | "v1/")
        || url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "/v1" | "/v1/")
        || url.port_or_known_default() == Some(0)
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(AppError::invalid(
            "Ollama requires http://127.0.0.1:<port>/v1, http://[::1]:<port>/v1 or localhost; no credentials, query or fragment.",
        ));
    }
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| AppError::invalid("Could not normalize the loopback URL."))?;
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

impl CreateProvider {
    fn into_config(self) -> Result<ProviderConfig> {
        if !valid_name(&self.name) {
            return Err(AppError::invalid(
                "Use a unique provider name of 1–48 lowercase letters, digits, hyphens or underscores, starting with a letter.",
            ));
        }
        let local = matches!(self.kind, SetupKind::Ollama);
        let kind = match self.kind {
            SetupKind::Openai => ProviderKind::Openai,
            SetupKind::Anthropic => ProviderKind::Anthropic,
            SetupKind::Gemini => ProviderKind::Gemini,
            SetupKind::Ollama => ProviderKind::OpenaiCompat,
        };
        let mut provider = ProviderConfig::new(self.name, kind);
        if local {
            provider.base_url = ollama_url(self.base_url.as_deref())?;
            if !matches!(self.credential, CredentialInput::None {}) {
                return Err(AppError::invalid(
                    "Local Ollama setup uses credential mode none.",
                ));
            }
            return Ok(provider);
        }
        if self.base_url.is_some() {
            return Err(AppError::invalid(
                "Cloud provider setup uses the provider's official endpoint; omit base_url.",
            ));
        }
        let key = match self.credential {
            CredentialInput::ApiKey { value } => {
                if value.is_empty()
                    || value.len() > 4096
                    || !value.bytes().all(|byte| byte.is_ascii_graphic())
                    || is_secret_reference(&value)
                {
                    return Err(AppError::invalid(
                        "The API key must be 1–4096 printable ASCII characters without spaces. Use environment mode for a variable reference.",
                    ));
                }
                value
            }
            CredentialInput::Env { name } => {
                if !valid_environment_name(&name) {
                    return Err(AppError::invalid(
                        "The environment variable name must be 1–128 letters, digits or underscores, starting with a letter or underscore.",
                    ));
                }
                format!("env:{name}")
            }
            CredentialInput::None {} => {
                return Err(AppError::invalid(
                    "This provider requires an API key or environment variable reference.",
                ));
            }
        };
        provider.api_keys.push(key);
        Ok(provider)
    }
}

fn provider_view(
    provider: &ProviderConfig,
    applied: bool,
    discovery: Option<&DiscoveryState>,
    model_count: usize,
) -> ProviderView {
    let credentials = provider.all_credentials();
    let keys: Vec<_> = credentials
        .iter()
        .map(|credential| credential.api_key.as_str())
        .filter(|key| !key.trim().is_empty())
        .collect();
    let refs = keys.iter().filter(|key| is_secret_reference(key)).count();
    let source = match (keys.len(), refs) {
        (0, _) => "none",
        (_, 0) => "api_key",
        (all, refs) if all == refs => "env",
        _ => "mixed",
    };
    let present = keys
        .iter()
        .any(|key| resolve_secret(key).is_ok_and(|key| !key.is_empty()));
    let usable_key = credentials.iter().any(|credential| {
        !credential.disabled && resolve_secret(&credential.api_key).is_ok_and(|key| !key.is_empty())
    });
    let missing_env = keys
        .iter()
        .any(|key| is_secret_reference(key) && resolve_secret(key).is_err());
    let environment_variable = if keys.len() == 1 {
        environment_name(keys[0]).map(str::to_owned)
    } else {
        None
    };
    let state = discovery.map_or(DiscoveryStatus::Pending, |discovery| discovery.state);
    let (status, message) = if !provider.enabled {
        ("disabled", "This provider is disabled.")
    } else if !applied {
        (
            "applying",
            "Saved. The gateway is still applying the configuration.",
        )
    } else if missing_env && !usable_key {
        (
            "env_missing",
            "Saved, but the environment variable is missing or empty in this app process. Set it and restart the app.",
        )
    } else if provider.kind.needs_credentials() && !usable_key {
        (
            "credential_missing",
            "No usable API key is configured for this provider.",
        )
    } else {
        match state {
            DiscoveryStatus::Pending => (
                "discovering",
                "Fetching the model catalog. Generation has not been tested.",
            ),
            DiscoveryStatus::Ok => (
                "catalog_ready",
                "Model catalog fetched. Generation and billing access have not been tested.",
            ),
            DiscoveryStatus::Failed => (
                "discovery_failed",
                "Could not fetch the model catalog. Check the credentials or local server, then refresh. Generation has not been tested.",
            ),
            DiscoveryStatus::Off => (
                "discovery_off",
                "Automatic catalog discovery is off. Listed models do not verify credentials or generation access.",
            ),
        }
    };
    ProviderView {
        name: provider.name.clone(),
        kind: provider.kind.as_str(),
        enabled: provider.enabled,
        config_applied: applied,
        credential_source: source,
        credential_present: present,
        environment_variable,
        status,
        discovery: CatalogView {
            state: if applied {
                state
            } else {
                DiscoveryStatus::Pending
            },
            models: if applied {
                discovery.map_or(0, |value| value.models)
            } else {
                0
            },
            at: if applied {
                discovery.and_then(|value| value.at)
            } else {
                None
            },
        },
        model_count: if applied { model_count } else { 0 },
        message,
    }
}

fn views(gateway: &Gateway) -> Vec<ProviderView> {
    let config = gateway.config_store().current();
    let applied = *gateway.scheduler().config() == *config;
    let discoveries = gateway.discovery_states();
    let snapshots = gateway.scheduler().snapshot();
    config
        .providers
        .iter()
        .map(|provider| {
            let count = snapshots
                .iter()
                .find(|view| view.name == provider.name)
                .map_or(0, |view| view.models);
            provider_view(provider, applied, discoveries.get(&provider.name), count)
        })
        .collect()
}

fn view(gateway: &Gateway, name: &str) -> Result<ProviderView> {
    views(gateway)
        .into_iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "provider_not_found",
                "This provider no longer exists.",
            )
        })
}

async fn wait_applied(gateway: &Gateway) -> bool {
    // Subscribe before the first comparison: config changes are coalesced, so
    // compare with the latest store snapshot rather than waiting for one event.
    let mut events = gateway.telemetry().bus().subscribe();
    let applied = || *gateway.scheduler().config() == *gateway.config_store().current();
    let wait = async {
        loop {
            if applied() {
                return true;
            }
            if matches!(
                events.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Closed)
            ) {
                return false;
            }
        }
    };
    tokio::time::timeout(APPLY_TIMEOUT, wait)
        .await
        .unwrap_or(false)
}

async fn save(gateway: &Gateway, input: CreateProvider) -> Result<ProviderView> {
    let provider = input.into_config()?;
    let name = provider.name.clone();
    let duplicate = Arc::new(AtomicBool::new(false));
    let found_duplicate = duplicate.clone();
    gateway.config_store().update(move |config| {
        if config.provider(&provider.name).is_some() {
            found_duplicate.store(true, Ordering::Relaxed);
            return Err("provider_name_exists".to_owned());
        }
        config.providers.push(provider);
        Ok(())
    }).await.map_err(|_| {
        if duplicate.load(Ordering::Relaxed) {
            AppError::new(StatusCode::CONFLICT, "provider_name_exists", "A provider with this name already exists. Choose another name.")
        } else {
            AppError::new(StatusCode::CONFLICT, "provider_save_failed", "The provider could not be saved. Check the gateway configuration file and its permissions; an invalid file is never overwritten.")
        }
    })?;
    wait_applied(gateway).await;
    view(gateway, &name)
}

pub(crate) async fn list(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({ "providers": views(&state.gateway) })))
}

pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: CreateProvider = body(request).await?;
    let provider = save(&state.gateway, input).await?;
    Ok((StatusCode::CREATED, Json(json!({ "provider": provider }))))
}

pub(crate) async fn refresh(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: RefreshInput = body(request).await?;
    let provider = view(&state.gateway, &name)?;
    if !provider.enabled {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "provider_disabled",
            "Enable this provider in the gateway configuration before refreshing.",
        ));
    }
    if !wait_applied(&state.gateway).await {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "config_applying",
            "Configuration is still applying. Read provider status and retry once it is applied.",
        ));
    }
    let provider = view(&state.gateway, &name)?;
    if !provider.config_applied {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "config_applying",
            "Configuration changed while refreshing. Read provider status and retry once it is applied.",
        ));
    }
    if !provider.enabled {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "provider_disabled",
            "This provider was disabled before the refresh started.",
        ));
    }
    if matches!(provider.status, "env_missing" | "credential_missing") {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "credential_missing",
            provider.message,
        ));
    }
    // A catalog GET is the only upstream action. Never invoke test_provider,
    // whose seemingly harmless connection test generates billable output.
    tokio::time::timeout(DISCOVERY_TIMEOUT, state.gateway.discover(&name))
        .await
        .map_err(|_| AppError::new(StatusCode::GATEWAY_TIMEOUT, "discovery_timeout", "The model catalog request timed out. Check the provider status and try again."))?
        .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "discovery_failed", "Could not fetch the model catalog. Check the credentials or local server, then refresh."))?;
    Ok(Json(json!({ "provider": view(&state.gateway, &name)? })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppOptions, start_host};
    use axum::Router;
    use axum::routing::get;
    use std::sync::atomic::AtomicUsize;
    use tokio_util::sync::CancellationToken;

    fn parse(value: Value) -> Result<ProviderConfig> {
        serde_json::from_value::<CreateProvider>(value)
            .map_err(|_| AppError::invalid("Invalid setup fields."))?
            .into_config()
    }

    #[test]
    fn setup_is_strict_and_keeps_secrets_out_of_readback() {
        let secret = "test-private-api-key-do-not-return";
        let provider = parse(
            json!({"name":"main","kind":"openai","credential":{"mode":"api_key","value":secret}}),
        )
        .unwrap();
        assert_eq!(provider.api_keys, [secret]);
        let discovery = DiscoveryState {
            state: DiscoveryStatus::Ok,
            at: Some(1),
            error: Some(format!("raw error {secret}")),
            models: 2,
        };
        let value =
            serde_json::to_value(provider_view(&provider, true, Some(&discovery), 4)).unwrap();
        let encoded = value.to_string();
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("raw error"));
        assert_eq!(value["status"], "catalog_ready");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("have not been tested")
        );
        for bad in [
            json!({"name":"main","kind":"openai","credential":{"mode":"api_key","value":secret,"extra":1}}),
            json!({"name":"main","kind":"openai","credential":{"mode":"api_key","value":secret},"extra":1}),
            json!({"name":"main","kind":"openai","credential":{"mode":"none"}}),
            json!({"name":"main","kind":"openai","credential":{"mode":"api_key","value":"env:API_KEY"}}),
            json!({"name":"main","kind":"openai","credential":{"mode":"api_key","value":secret},"base_url":"https://example.com"}),
            json!({"name":"Main","kind":"gemini","credential":{"mode":"env","name":"GEMINI_API_KEY"}}),
            json!({"name":"main","kind":"ollama","credential":{"mode":"api_key","value":secret}}),
            json!({"name":"main","kind":"ollama","credential":{"mode":"none","value":secret}}),
            json!({"name":"main","kind":"anthropic","credential":{"mode":"env","name":"A=B"}}),
        ] {
            assert!(parse(bad).is_err());
        }
    }

    #[test]
    fn ollama_endpoint_is_fixed_to_literal_loopback_and_v1() {
        assert_eq!(ollama_url(None).unwrap(), "http://127.0.0.1:11434/v1");
        assert_eq!(
            ollama_url(Some("http://localhost:15555/v1/")).unwrap(),
            "http://127.0.0.1:15555/v1"
        );
        assert_eq!(
            ollama_url(Some("http://[::1]:11434/v1")).unwrap(),
            "http://[::1]:11434/v1"
        );
        for invalid in [
            "https://127.0.0.1:11434/v1",
            "http://example.com/v1",
            "http://127.0.0.1.evil/v1",
            "http://127.0.0.1:11434/v1?api_key=secret",
            "http://user:secret@127.0.0.1:11434/v1",
            "http://127.0.0.1:11434/v1#secret",
            "http://2130706433:11434/v1",
            "http://0x7f000001:11434/v1",
            "http://127.0.0.1:0/v1",
            "http://localhost:11434/api",
            "http://localhost:11434/a/../v1",
        ] {
            assert!(ollama_url(Some(invalid)).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn missing_environment_is_reported_without_guessing_connectivity() {
        let name = format!("SWITCHYA_MISSING_{}", uuid::Uuid::new_v4().simple());
        assert!(std::env::var_os(&name).is_none());
        let provider = parse(json!({"name":"env-provider","kind":"anthropic","credential":{"mode":"env","name":name}})).unwrap();
        let saved = provider_view(&provider, true, None, 8);
        assert_eq!(saved.status, "env_missing");
        assert_eq!(saved.credential_source, "env");
        assert!(!saved.credential_present);
        assert_eq!(saved.environment_variable.as_deref(), Some(name.as_str()));
        let pending = provider_view(&provider, false, None, 8);
        assert_eq!(pending.status, "applying");
        assert_eq!(pending.model_count, 0);
    }

    #[tokio::test]
    async fn onboarding_persists_reloads_and_only_requests_the_model_catalog() {
        let lists = Arc::new(AtomicUsize::new(0));
        let other_requests = Arc::new(AtomicUsize::new(0));
        let observed_lists = lists.clone();
        let observed_other = other_requests.clone();
        let fixture = Router::new()
            .route("/v1/models", get(move || {
                let count = observed_lists.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    Json(json!({"object":"list","data":[{"id":"switchya-fixture-model","object":"model","owned_by":"fixture"}]}))
                }
            }))
            .fallback(move || {
                let count = observed_other.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    StatusCode::NOT_FOUND
                }
            });
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let cancel_fixture = CancellationToken::new();
        let fixture_stopped = cancel_fixture.clone();
        let fixture_task = tokio::spawn(async move {
            axum::serve(listener, fixture)
                .with_graceful_shutdown(fixture_stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("gateway.toml");
        let options = AppOptions::new(&config_path, temp.path().join("state"));
        let host = start_host(options.clone()).await.unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let base = host.base_url();
        let token = &host.launch.token;
        let input = json!({"name":"local-test","kind":"ollama","credential":{"mode":"none"},"base_url":endpoint});
        let response = client
            .post(format!("{base}/api/providers"))
            .bearer_auth(token)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let created: Value = response.json().await.unwrap();
        assert_eq!(created["provider"]["name"], "local-test");
        assert_eq!(created["provider"]["kind"], "openai-compat");
        assert_eq!(created["provider"]["config_applied"], true);
        let duplicate = client
            .post(format!("{base}/api/providers"))
            .bearer_auth(token)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let refreshed = client
            .post(format!("{base}/api/providers/local-test/refresh"))
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(refreshed.status(), StatusCode::OK);
        let refreshed: Value = refreshed.json().await.unwrap();
        assert_eq!(refreshed["provider"]["status"], "catalog_ready");
        assert_eq!(refreshed["provider"]["discovery"]["models"], 1);
        let models: Value = client
            .get(format!("{base}/api/models"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            models["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "switchya-fixture-model")
        );
        let missing = format!("SWITCHYA_MISSING_{}", uuid::Uuid::new_v4().simple());
        let env_input =
            json!({"name":"cloud-test","kind":"openai","credential":{"mode":"env","name":missing}});
        let left = client
            .post(format!("{base}/api/providers"))
            .bearer_auth(token)
            .json(&env_input)
            .send();
        let right = client
            .post(format!("{base}/api/providers"))
            .bearer_auth(token)
            .json(&env_input)
            .send();
        let (left, right) = tokio::join!(left, right);
        let left = left.unwrap();
        let right = right.unwrap();
        assert!(matches!(
            (left.status(), right.status()),
            (StatusCode::CREATED, StatusCode::CONFLICT)
                | (StatusCode::CONFLICT, StatusCode::CREATED)
        ));
        let created_env: Value = if left.status() == StatusCode::CREATED {
            left.json().await.unwrap()
        } else {
            right.json().await.unwrap()
        };
        assert_eq!(created_env["provider"]["status"], "env_missing");
        assert_eq!(created_env["provider"]["credential_present"], false);
        let invalid_refresh = client
            .post(format!("{base}/api/providers/local-test/refresh"))
            .bearer_auth(token)
            .json(&json!({"generate":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_refresh.status(), StatusCode::BAD_REQUEST);
        assert!(lists.load(Ordering::Relaxed) > 0);
        assert_eq!(
            other_requests.load(Ordering::Relaxed),
            0,
            "setup must never call a generation endpoint"
        );
        let persisted = std::fs::read_to_string(&config_path).unwrap();
        assert!(persisted.contains("local-test"));
        assert!(persisted.contains("cloud-test"));
        host.shutdown().await.unwrap();
        let restarted = start_host(options).await.unwrap();
        let readback: Value = client
            .get(format!("{}/api/providers", restarted.base_url()))
            .bearer_auth(&restarted.launch.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            readback["providers"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|provider| provider["name"] == "cloud-test")
                .count(),
            1
        );
        assert!(
            readback["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|provider| provider["name"] == "local-test")
        );
        restarted.shutdown().await.unwrap();
        cancel_fixture.cancel();
        fixture_task.await.unwrap();
    }
}
