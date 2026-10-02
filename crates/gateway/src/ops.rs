//! Operations for the admin API: model discovery and provider tests.

use crate::failover::rests_whole_credential;
use crate::gateway::{Inner, wants_discovery};
use crate::prepare::adapt_for_vertex;
use crate::target::protocol_for;
use crate::types::ProviderTest;
use bytes::Bytes;
use http::HeaderMap;
use std::time::{Duration, Instant};
use switchyard_core::codec::UpstreamCtx;
use switchyard_core::config::{Config, ProviderConfig, ProviderKind};
use switchyard_core::ir::{Message, Request};
use switchyard_core::reasoning::ModelThinking;
use switchyard_core::{ApiError, FailureClass, ModelInfo, Protocol, UpstreamError};
use switchyard_scheduler::Outcome;
use switchyard_upstream::{Operation, Timeouts, mock_models, mock_response};

/// Limits of one model-listing page: discovery must never hold anything up
/// for long.
const DISCOVERY_CONNECT: Duration = Duration::from_secs(10);
const DISCOVERY_REQUEST: Duration = Duration::from_secs(20);

/// Longest a provider test waits for its answer.
const TEST_REQUEST: Duration = Duration::from_secs(60);

/// What the test request asks.
const TEST_PROMPT: &str = "ping";

/// Output-token cap of the test request: it only has to prove the upstream
/// answers.
const TEST_MAX_OUTPUT_TOKENS: u64 = 16;

/// The error shown for an upstream failure in an admin operation. Unlike a
/// client, the operator is told what the upstream said — including that it
/// rejected the credential.
fn admin_error(error: &UpstreamError) -> ApiError {
    let mut api = error.to_api_error();
    if error.class == FailureClass::Auth {
        api.message = format!(
            "the upstream rejected the gateway's credential: {}",
            error.info.message
        );
    }
    api
}

/// Fragments of model ids that name something other than a text generation
/// model. A discovered listing mixes all kinds (OpenAI's starts,
/// alphabetically, with `babbage-002`), and "ping" sent to an embedding or
/// speech model fails although the provider is perfectly healthy.
const NOT_FOR_GENERATION: [&str; 12] = [
    "embed",
    "moderation",
    "whisper",
    "transcribe",
    "tts",
    "audio",
    "realtime",
    "dall-e",
    "image",
    "rerank",
    "babbage",
    "davinci",
];

/// The model a provider test uses when the operator named none and the
/// provider's models were discovered rather than configured: the first one
/// the gateway has metadata for (the catalog lists generation models),
/// else the first whose id does not look like an embedding, speech, image
/// or legacy completion model, else the first. `candidates` are (known,
/// upstream id) in listing order.
fn default_test_model(candidates: &[(bool, String)]) -> Option<String> {
    let plausible = |id: &str| {
        let id = id.to_ascii_lowercase();
        !NOT_FOR_GENERATION
            .iter()
            .any(|fragment| id.contains(fragment))
    };
    candidates
        .iter()
        .find(|(known, id)| *known && plausible(id))
        .or_else(|| candidates.iter().find(|(_, id)| plausible(id)))
        .or_else(|| candidates.first())
        .map(|(_, id)| id.clone())
}

impl Inner {
    /// Asks every provider that wants discovery for its model list.
    /// Failures are logged and otherwise ignored.
    pub(crate) async fn discover_all(&self, config: &Config) {
        let providers = config.providers.iter().filter(|p| wants_discovery(p));
        futures::future::join_all(providers.map(|provider| async move {
            // Logged inside; a provider that cannot be listed keeps the
            // models it had.
            let _ = self.discover_provider(config, provider).await;
        }))
        .await;
    }

    /// Lists the models of one provider through its first usable credential
    /// and hands the list to the scheduler.
    async fn discover_provider(
        &self,
        config: &Config,
        provider: &ProviderConfig,
    ) -> Result<Vec<ModelInfo>, UpstreamError> {
        let name = provider.name.as_str();
        let Some(credential) = self.scheduler.credentials(name).into_iter().next() else {
            tracing::warn!(
                provider = name,
                "model discovery skipped: no usable credential"
            );
            return Err(UpstreamError::transport(format!(
                "provider `{name}` has no usable credential"
            )));
        };
        let protocol = provider
            .protocols()
            .first()
            .copied()
            .unwrap_or(Protocol::OpenaiChat);
        let limits = Timeouts {
            connect: DISCOVERY_CONNECT.min(Duration::from_secs(
                config.upstream.connect_timeout_secs.max(1),
            )),
            request: DISCOVERY_REQUEST,
        };
        let listed = match self
            .target_for(config, provider, &credential, protocol, "")
            .await
        {
            Ok(target) => self.upstream.list_models_with(&target, limits).await,
            Err(error) => Err(error),
        };
        match listed {
            Ok(models) => {
                tracing::info!(provider = name, models = models.len(), "models discovered");
                self.scheduler.set_discovered(name, models.clone());
                Ok(models)
            }
            Err(error) => {
                tracing::warn!(
                    provider = name,
                    status = error.status,
                    "model discovery failed: {}",
                    error.info.message
                );
                Err(error)
            }
        }
    }

    /// See [`crate::Gateway::discover`].
    pub(crate) async fn discover(&self, provider: &str) -> Result<Vec<ModelInfo>, ApiError> {
        let config = self.store.current();
        let Some(entry) = self.scheduler.provider_config(provider) else {
            return Err(ApiError::not_found(format!(
                "unknown provider `{provider}`"
            )));
        };
        if entry.kind == ProviderKind::Mock {
            let models = mock_models();
            self.scheduler.set_discovered(provider, models.clone());
            return Ok(models);
        }
        if self.scheduler.credentials(provider).is_empty() {
            return Err(ApiError::unavailable(format!(
                "provider `{provider}` has no usable credential"
            )));
        }
        self.discover_provider(&config, &entry)
            .await
            .map_err(|error| admin_error(&error))
    }

    /// The upstream model id a provider test uses: `wanted` (a client-facing
    /// name of one of the provider's models is turned into its upstream id;
    /// anything else is taken as an upstream id), else the provider's first
    /// configured model, else — for a provider whose models were discovered
    /// — the most plausible generation model among those the scheduler
    /// routes to it ([`default_test_model`]).
    fn test_model(&self, provider: &ProviderConfig, wanted: Option<&str>) -> Option<String> {
        // (the gateway has metadata for the model, its upstream id), for
        // every model routed to this provider — all of them, or the one a
        // client knows as `name`.
        let routed = |name: Option<&str>| -> Vec<(bool, String)> {
            self.scheduler
                .models()
                .into_iter()
                .filter(|entry| entry.alias_targets.is_none())
                .filter(|entry| name.is_none_or(|name| entry.name == name))
                .flat_map(|entry| {
                    let known = entry.info.known;
                    entry
                        .routes
                        .into_iter()
                        .filter(|route| route.provider == provider.name)
                        .map(move |route| (known, route.upstream_model))
                })
                .collect()
        };
        match wanted.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => provider
                .models
                .iter()
                .find(|model| model.client_name() == name || model.id.trim() == name)
                .map(|model| model.id.trim().to_string())
                .or_else(|| routed(Some(name)).into_iter().next().map(|(_, id)| id))
                .or_else(|| Some(name.to_string())),
            None => provider
                .models
                .first()
                .map(|model| model.id.trim().to_string())
                .or_else(|| default_test_model(&routed(None))),
        }
    }

    /// See [`crate::Gateway::test_provider`].
    pub(crate) async fn test_provider(&self, provider: &str, model: Option<&str>) -> ProviderTest {
        let started = Instant::now();
        let failed =
            |status: u16, model: Option<String>, credential: Option<String>, error: String| {
                ProviderTest {
                    ok: false,
                    status,
                    latency_ms: crate::generate::elapsed_ms(started),
                    model,
                    credential,
                    error: Some(error),
                }
            };
        let config = self.store.current();
        let Some(entry) = self.scheduler.provider_config(provider) else {
            return failed(0, None, None, format!("unknown provider `{provider}`"));
        };
        let Some(credential) = self.scheduler.credentials(provider).into_iter().next() else {
            return failed(
                0,
                None,
                None,
                "the provider has no usable credential".to_string(),
            );
        };
        let label = Some(credential.label.clone());
        let Some(model) = self.test_model(&entry, model) else {
            return failed(
                0,
                None,
                label,
                "the provider has no model to test with".to_string(),
            );
        };
        let preferred = entry
            .protocols()
            .first()
            .copied()
            .unwrap_or(Protocol::OpenaiChat);
        let protocol = protocol_for(entry.kind, preferred, &model);

        let mut request = Request::new(model.clone(), protocol);
        request.messages.push(Message::user_text(TEST_PROMPT));
        request.max_output_tokens = Some(TEST_MAX_OUTPUT_TOKENS);

        let started = Instant::now();
        let outcome: Result<u16, UpstreamError> = if entry.kind == ProviderKind::Mock {
            mock_response(&request, protocol).await.map(|_| 200)
        } else {
            let op = Operation::Generate { stream: false };
            let ctx = UpstreamCtx {
                thinking: ModelThinking::Unknown,
                max_output_tokens: None,
                quirks: entry.quirks(),
            };
            let encoded = switchyard_codecs::codec(protocol)
                .encode_request(&request, &ctx)
                .map_err(|error| error.to_string())
                .and_then(|mut body| {
                    if entry.kind == ProviderKind::Vertex {
                        adapt_for_vertex(&mut body, protocol, &op);
                    }
                    serde_json::to_vec(&body).map_err(|error| error.to_string())
                });
            let body = match encoded {
                Ok(body) => Bytes::from(body),
                Err(error) => {
                    return failed(
                        0,
                        Some(model),
                        label,
                        format!("the test request could not be built: {error}"),
                    );
                }
            };
            let limits = Timeouts {
                connect: Duration::from_secs(config.upstream.connect_timeout_secs.max(1)),
                request: match config.upstream.request_timeout_secs {
                    0 => TEST_REQUEST,
                    secs => TEST_REQUEST.min(Duration::from_secs(secs)),
                },
            };
            match self
                .target_for(&config, &entry, &credential, protocol, &model)
                .await
            {
                Ok(target) => self
                    .upstream
                    .send(&target, &op, body, &HeaderMap::new(), limits)
                    .await
                    .map(|response| response.status),
                Err(error) => Err(error),
            }
        };

        let latency_ms = crate::generate::elapsed_ms(started);
        let now = self.scheduler.now();
        match outcome {
            Ok(status) => {
                self.scheduler.report_credential(
                    &credential.id,
                    &model,
                    Outcome::Success { latency_ms },
                    now,
                );
                self.publish_credential(provider, &credential.id);
                ProviderTest {
                    ok: true,
                    status,
                    latency_ms,
                    model: Some(model),
                    credential: label,
                    error: None,
                }
            }
            Err(error) => {
                // A mock model's scripted failure is not held against the
                // mock credential as a whole (see `failover`): testing the
                // provider with `mock-error-401` must not switch off the
                // mock models that work.
                let scripted = entry.kind == ProviderKind::Mock;
                if !(scripted && rests_whole_credential(error.class)) {
                    self.scheduler.report_credential(
                        &credential.id,
                        &model,
                        Outcome::Failure(&error),
                        now,
                    );
                    self.publish_credential(provider, &credential.id);
                }
                ProviderTest {
                    ok: false,
                    status: error.status,
                    latency_ms,
                    model: Some(model),
                    credential: label,
                    error: Some(if error.info.message.trim().is_empty() {
                        format!("the upstream answered with status {}", error.status)
                    } else {
                        error.info.message.clone()
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(candidates: &[(bool, &str)]) -> Option<String> {
        let owned: Vec<(bool, String)> = candidates
            .iter()
            .map(|(known, id)| (*known, id.to_string()))
            .collect();
        default_test_model(&owned)
    }

    #[test]
    fn the_default_test_model_is_one_that_generates_text() {
        // OpenAI's listing, alphabetically.
        assert_eq!(
            pick(&[
                (false, "babbage-002"),
                (false, "dall-e-3"),
                (false, "gpt-4o-mini-tts"),
                (true, "gpt-5"),
                (false, "text-embedding-3-small"),
            ])
            .as_deref(),
            Some("gpt-5")
        );
        // Nothing the catalog knows: the first plausible id.
        assert_eq!(
            pick(&[
                (false, "bge-reranker-v2"),
                (false, "nomic-embed-text"),
                (false, "qwen3:8b"),
                (false, "zephyr"),
            ])
            .as_deref(),
            Some("qwen3:8b")
        );
        // A known id that is not a generation model does not win.
        assert_eq!(
            pick(&[(true, "text-embedding-3-large"), (false, "my-model")]).as_deref(),
            Some("my-model")
        );
        // Nothing plausible: still test something rather than nothing.
        assert_eq!(
            pick(&[(false, "whisper-1"), (false, "tts-1")]).as_deref(),
            Some("whisper-1")
        );
        assert_eq!(pick(&[]), None);
    }
}
