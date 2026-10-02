//! Operations for the admin API: model discovery and provider tests.

use crate::failover::rests_whole_credential;
use crate::gateway::{Inner, wants_discovery};
use crate::prepare::adapt_for_vertex;
use crate::target::{
    ReadError, bad_gateway, body_limit, declared_failure, failed_response, protocol_for,
    read_limited, too_large,
};
use crate::types::{DiscoveryState, DiscoveryStatus, ProviderTest};
use bytes::Bytes;
use http::HeaderMap;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use switchyard_core::codec::UpstreamCtx;
use switchyard_core::config::{Config, ProviderConfig, ProviderKind};
use switchyard_core::ir::{Message, Request};
use switchyard_core::reasoning::ModelThinking;
use switchyard_core::util::{now_unix_ms, truncate_chars};
use switchyard_core::{ApiError, FailureClass, ModelInfo, Protocol, UpstreamError};
use switchyard_scheduler::Outcome;
use switchyard_telemetry::redact_text;
use switchyard_upstream::{Operation, Target, Timeouts, mock_models, mock_response};
use tokio_util::sync::CancellationToken;

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
/// rejected the credential. What the upstream said is shown without key
/// material: the transport removed the credential the call was made with,
/// and anything else shaped like a key, a token or a password (a relay
/// quoting another account's key, say) is masked like in the log.
fn admin_error(error: &UpstreamError) -> ApiError {
    let mut api = error.to_api_error();
    if error.class == FailureClass::Auth {
        api.message = format!(
            "the upstream rejected the gateway's credential: {}",
            error.info.message
        );
    }
    api.message = redact_text(&api.message);
    api
}

/// What a 2xx answer to the test request amounts to: `Ok` when it is a
/// response a request could be served with, otherwise the failure a
/// generation request answered the same way is charged with — the
/// generation the upstream itself reports as failed (a Responses body with
/// `status: "failed"`, classified by its error code: out of quota, rate
/// limited, …), or a body that is no response at all (the HTML of a web
/// page behind a mistyped `base_url` arrives with a `200` too).
fn answered(protocol: Protocol, body: &[u8], target: &Target) -> Result<(), UpstreamError> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Err(bad_gateway(
            "the upstream answered with a body that is not valid JSON",
        ));
    };
    if let Some(failure) = declared_failure(protocol, &value, Some(target)) {
        return Err(failure);
    }
    match switchyard_codecs::codec(protocol).decode_response(&value) {
        Ok(_) => Ok(()),
        Err(error) => Err(
            failed_response(protocol, &value, Some(target)).unwrap_or_else(|| {
                bad_gateway(target.redact(&format!(
                    "the upstream response could not be understood: {error}"
                )))
            }),
        ),
    }
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

/// Longest failure text kept in a discovery state.
const DISCOVERY_ERROR_CHARS: usize = 300;

/// One background listing that [`Discoveries::plan`] asked for: the
/// provider and the number of the run, by which a listing that a later one
/// has overtaken is recognised.
pub(crate) type DiscoveryRun = (String, u64);

/// What is remembered about one provider's discovery.
struct Discovery {
    state: DiscoveryState,
    /// The number of the listing whose outcome counts for the provider (0:
    /// none was started). A background listing that finishes under another
    /// number was started for settings that have changed since, and is
    /// dropped.
    run: u64,
}

/// The discovery states and the number of the last listing started.
#[derive(Default)]
struct DiscoveryBook {
    by_provider: HashMap<String, Discovery>,
    /// Numbers listings across all providers, for as long as the gateway
    /// runs. A number is never handed out twice, so the listing of a
    /// provider that was removed cannot be taken for one of a provider
    /// created later under the same name (a count kept per provider would
    /// start again with the new entry).
    runs: u64,
}

/// Where the discovery of each provider's model list stands (see
/// [`crate::Gateway::discovery_states`]).
#[derive(Default)]
pub(crate) struct Discoveries {
    book: Mutex<DiscoveryBook>,
}

/// Whether a provider's model list has to be asked for again after its
/// entry changed from `before` to `after`: anything that decides which
/// upstream is asked, as whom and by which route.
fn discovery_settings_changed(before: &ProviderConfig, after: &ProviderConfig) -> bool {
    before.kind != after.kind
        || before.effective_base_url() != after.effective_base_url()
        || before.api_keys != after.api_keys
        || before.credentials != after.credentials
        || before.proxy != after.proxy
        || before.headers != after.headers
        || before.project != after.project
        || before.location != after.location
}

/// A failure text as a discovery state shows it: one line of bounded
/// length, without key material. The transport has removed the credential
/// the listing was asked with; anything else an upstream quotes that is
/// shaped like a key, a token or a password is masked here, the way the
/// log line of the same failure is.
fn one_line(message: &str) -> String {
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&redact_text(&line), DISCOVERY_ERROR_CHARS)
}

impl Discoveries {
    /// The state of every provider, by name.
    pub(crate) fn states(&self) -> HashMap<String, DiscoveryState> {
        self.book
            .lock()
            .by_provider
            .iter()
            .map(|(name, discovery)| (name.clone(), discovery.state.clone()))
            .collect()
    }

    /// Brings the states in line with `config`, which replaces `previous`
    /// (`None` at start), and returns the listings to run for it.
    ///
    /// Providers that are gone are forgotten, providers that do not want
    /// discovery are `off`. Of those that want it, a listing is started —
    /// and the state becomes `pending` — for every one at start, for the
    /// ones whose discovery-relevant settings changed, and for all of them
    /// when the configuration did not change at all: a reload of the same
    /// content is the operator asking for everything to be done again.
    /// Every other provider keeps its state, and its upstream is not asked.
    ///
    /// `remembered` says how many models the list the scheduler has in use
    /// for a provider holds, under `config`
    /// (`Scheduler::discovered_models`, asked after the rebuild): what a
    /// `pending` state — and a `failed` one after it — counts as `models`.
    pub(crate) fn plan(
        &self,
        previous: Option<&Config>,
        config: &Config,
        remembered: impl Fn(&str) -> usize,
    ) -> Vec<DiscoveryRun> {
        let now = now_unix_ms();
        let everything = previous.is_none_or(|previous| {
            previous == config || previous.upstream.proxy != config.upstream.proxy
        });
        // Looked up by name through maps: with hundreds of providers a scan
        // per provider would make every applied configuration quadratic.
        let by_name = |config: &'_ Config| -> HashMap<String, usize> {
            config
                .providers
                .iter()
                .enumerate()
                .map(|(index, provider)| (provider.name.clone(), index))
                .collect()
        };
        let present = by_name(config);
        let earlier = previous.map(by_name).unwrap_or_default();
        let mut runs = Vec::new();
        let mut book = self.book.lock();
        let DiscoveryBook {
            by_provider,
            runs: started,
        } = &mut *book;
        // The number of a listing that starts now — or, for a provider that
        // stops wanting discovery, a number nothing under way carries.
        let mut next_run = || {
            *started += 1;
            *started
        };
        by_provider.retain(|name, _| present.contains_key(name));
        for provider in &config.providers {
            let discovery = by_provider
                .entry(provider.name.clone())
                .or_insert_with(|| Discovery {
                    state: DiscoveryState::OFF,
                    run: 0,
                });
            if !wants_discovery(provider) {
                // Whatever is still on its way is no longer wanted.
                discovery.run = next_run();
                discovery.state = DiscoveryState::OFF;
                continue;
            }
            let before = previous
                .zip(earlier.get(&provider.name))
                .and_then(|(previous, &index)| previous.providers.get(index))
                .filter(|before| wants_discovery(before));
            let unchanged = before.is_some_and(|before| {
                !discovery_settings_changed(before, provider)
                    && discovery.state.state != DiscoveryStatus::Off
            });
            if unchanged && !everything {
                continue;
            }
            discovery.run = next_run();
            discovery.state = DiscoveryState {
                state: DiscoveryStatus::Pending,
                at: Some(now),
                error: None,
                // The list of the last success stays in use while the
                // endpoint is the same — also across the provider being
                // switched off and on again; otherwise the scheduler has
                // dropped it. The scheduler is the one that knows.
                models: remembered(&provider.name),
            };
            runs.push((provider.name.clone(), discovery.run));
        }
        runs
    }

    /// Records how a listing for `provider` ended. `run` is the number of a
    /// background listing, `None` for one the operator asked for. Returns
    /// whether the outcome counts: `false` for a background listing that a
    /// later one, a change of settings or the removal of the provider has
    /// overtaken — its list must not reach the scheduler either.
    ///
    /// A provider whose state is `off` stays `off`: its upstream can still
    /// be asked on request, but nothing is waiting for the answer.
    fn finish(
        &self,
        provider: &str,
        run: Option<u64>,
        outcome: &Result<Vec<ModelInfo>, UpstreamError>,
    ) -> bool {
        let mut book = self.book.lock();
        let Some(discovery) = book.by_provider.get_mut(provider) else {
            return run.is_none();
        };
        if run.is_some_and(|run| run != discovery.run) {
            return false;
        }
        if discovery.state.state == DiscoveryStatus::Off {
            return true;
        }
        let at = Some(now_unix_ms());
        discovery.state = match outcome {
            Ok(models) => DiscoveryState {
                state: DiscoveryStatus::Ok,
                at,
                error: None,
                models: models.len(),
            },
            Err(error) => DiscoveryState {
                state: DiscoveryStatus::Failed,
                at,
                error: Some(one_line(&error.info.message)),
                models: discovery.state.models,
            },
        };
        true
    }
}

impl Inner {
    /// Asks the providers of `runs` for their model lists and hands each
    /// list to the scheduler. Failures are logged and recorded in the
    /// provider's discovery state; a provider that cannot be listed keeps
    /// the models it had.
    pub(crate) async fn discover_all(&self, config: &Config, runs: &[DiscoveryRun]) {
        futures::future::join_all(runs.iter().map(|(name, run)| async move {
            let Some(provider) = config.provider(name) else {
                return;
            };
            let listed = self.list_models(config, provider).await;
            if self.discoveries.finish(name, Some(*run), &listed)
                && let Ok(models) = listed
            {
                self.scheduler.set_discovered(name, models);
            }
        }))
        .await;
    }

    /// Lists the models of one provider through its first usable
    /// credential. Changes nothing: the caller decides what the list, or
    /// the failure, means.
    async fn list_models(
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
        // A key file repaired since the configuration was applied counts.
        self.check_service_accounts(&config).await;
        if self.scheduler.credentials(provider).is_empty() {
            return Err(ApiError::unavailable(format!(
                "provider `{provider}` has no usable credential"
            )));
        }
        let listed = self.list_models(&config, &entry).await;
        self.discoveries.finish(provider, None, &listed);
        match listed {
            Ok(models) => {
                self.scheduler.set_discovered(provider, models.clone());
                Ok(models)
            }
            Err(error) => Err(admin_error(&error)),
        }
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
        // A key file repaired since the configuration was applied counts.
        self.check_service_accounts(&config).await;
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
                Ok(target) => {
                    let sent = self
                        .upstream
                        .send_unbuffered(&target, &op, body, &HeaderMap::new(), limits)
                        .await;
                    match sent {
                        Ok(response) => {
                            // A 2xx status alone proves little: the answer
                            // has to be one a request could be served with.
                            let status = response.status;
                            let never = CancellationToken::new();
                            match read_limited(response.body, body_limit(&config), &never).await {
                                Ok(bytes) => answered(protocol, &bytes, &target).map(|()| status),
                                Err(ReadError::Upstream(error)) => Err(error),
                                Err(ReadError::TooLarge) => {
                                    Err(bad_gateway(too_large(&config).message))
                                }
                                // Unreachable: nothing cancels the token.
                                Err(ReadError::Cancelled) => {
                                    Err(bad_gateway("the provider test was abandoned"))
                                }
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
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
                        // Like a discovery error: nothing key-shaped the
                        // upstream may have quoted.
                        redact_text(&error.info.message)
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
