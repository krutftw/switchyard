//! Token counting: the upstream's own counting endpoint when it has one,
//! a local estimate otherwise.

use crate::failover::{Failover, Failure, Limits, Next, blames_credential};
use crate::gateway::Inner;
use crate::generate::{
    abandoned_attempt_record, attempt_record, check_identity, elapsed_ms, failed_attempt_record,
    parse_request, timeouts,
};
use crate::prepare::{Job, adapt_for_vertex, upstream_ctx};
use crate::recorder::Recorder;
use crate::reply::{JSON, Served, reply_headers};
use crate::session::session_key;
use crate::summary::strip_summary;
use crate::target::{
    ReadError, bad_gateway, body_limit, offered_headers, read_limited, too_large, upstream_protocol,
};
use crate::types::{ClientRequest, FullReply, Reply};
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use switchyard_core::codec::UpstreamCtx;
use switchyard_core::config::ProviderKind;
use switchyard_core::util::now_unix_ms;
use switchyard_core::{ApiError, Codec, Protocol, UpstreamError};
use switchyard_scheduler::{Lease, Outcome, Resolved};
use switchyard_telemetry::Mode;
use switchyard_translate::{
    ReasoningInputs, apply_to_request, estimate_tokens, plan_reasoning, sanitize_tool_names,
};
use switchyard_upstream::Operation;

/// Upstream statuses that mean "this upstream has no counting endpoint",
/// not "this credential is in trouble".
const NO_ENDPOINT_STATUSES: [u16; 3] = [404, 405, 501];

/// Whether a failed counting call is answered with a local estimate instead
/// of an error: the upstream has no counting endpoint, or refused the call
/// for a reason that says nothing about the credential's ability to
/// generate (see [`blames_credential`]). Counting is optional; the client
/// still deserves a number, and the credential stays in rotation.
fn estimate_instead(error: &UpstreamError) -> bool {
    NO_ENDPOINT_STATUSES.contains(&error.status) || !blames_credential(error)
}

/// The client's own counting body, made fit to forward to an upstream of
/// its protocol: the model replaced by the upstream's id and the history
/// repaired exactly as it is for generation (`Codec::prepare_passthrough`:
/// signatures of other vendors, blocks the vendor refuses to see again), so
/// a conversation the generation endpoint accepts is not refused by the
/// counting one.
///
/// A counting body is not a generation body, though: whatever top-level
/// field the preparation *adds* — Anthropic's mandatory `max_tokens`, a
/// `stream` flag — is taken out again, because the counting endpoints
/// reject fields they do not know.
fn passthrough_count_body(
    upstream: &dyn Codec,
    client_body: &Value,
    upstream_model: &str,
    ctx: &UpstreamCtx<'_>,
) -> Value {
    let mut body = client_body.clone();
    upstream.set_request_model(&mut body, upstream_model);
    let before: Option<Vec<String>> = body
        .as_object()
        .map(|object| object.keys().cloned().collect());
    upstream.prepare_passthrough(&mut body, false, ctx);
    if let (Some(before), Some(object)) = (before, body.as_object_mut()) {
        object.retain(|key, _| before.iter().any(|known| known == key));
    }
    body
}

/// How one counting attempt ended.
enum Counted {
    /// The upstream counted. In passthrough mode its own body is kept: it
    /// is already in the client's shape.
    Tokens(u64, Option<Bytes>),
    /// The upstream cannot count: estimate locally. Carries the upstream's
    /// refusal when it was asked and said no, for the request record.
    Estimate(Option<Failure>),
    Failed(Failure),
    Fatal(ApiError),
    Cancelled,
}

impl Inner {
    /// See [`crate::Gateway::count_tokens`].
    pub(crate) async fn count_tokens(self: &Arc<Self>, request: ClientRequest) -> Reply {
        let ClientRequest {
            protocol,
            endpoint,
            body,
            path_model,
            path_stream: _,
            headers,
            identity,
            client_ip,
            transport,
            request_id,
            session,
            cancel,
        } = request;
        let config = self.store.current();
        let client = switchyard_codecs::codec(protocol);
        let mut recorder = self.begin_record(
            &config, protocol, &endpoint, transport, &identity, client_ip, &headers, request_id,
        );
        recorder.capture_client_request(&headers, &body);

        if client.encode_count_response(0).is_none() {
            let error = ApiError::not_found(format!(
                "the {} protocol has no token counting endpoint",
                protocol.display_name()
            ));
            return self.reject(recorder, client, &error);
        }

        // A counting request never streams, whatever the URL or body say.
        let (json, mut meta) =
            match parse_request(client, &body, path_model.as_deref(), Some(false)) {
                Ok(parsed) => parsed,
                Err(error) => return self.reject(recorder, client, &error),
            };
        meta.stream = false;
        recorder.builder().set_requested_model(meta.model.clone());
        recorder.announce();

        let resolution = self.scheduler.resolve(&meta.model);
        if let Err(error) = check_identity(
            &self.keys.load(),
            &identity,
            &meta.model,
            resolution.as_ref().ok(),
            Some(&json),
        ) {
            return self.reject(recorder, client, &error);
        }
        let resolved = match resolution {
            Ok(resolved) => resolved,
            Err(error) => return self.reject(recorder, client, &ApiError::from(error)),
        };
        recorder.builder().set_client_model(resolved.base.clone());

        let scope = identity.scope().to_string();
        let session = if config.routing.session_affinity {
            session_key(session.as_deref(), &headers, &json, &scope)
        } else {
            None
        };
        let mut job = Job {
            config: Arc::clone(&config),
            client,
            wrapped: switchyard_core::sig::contains_wrapped(&body),
            body: Arc::new(json),
            meta,
            path_model,
            path_stream: Some(false),
            headers: offered_headers(headers),
            scope,
            decoded: None,
            cancel: cancel.clone(),
            summary_refused_by: Vec::new(),
        };

        let mut failover = Failover::new(
            self,
            &resolved,
            session.as_deref(),
            protocol,
            &cancel,
            Limits::from_config(&config),
        );
        let mut last: Option<Box<Lease>> = None;
        loop {
            let lease = match failover.next().await {
                Next::Lease(lease) => lease,
                Next::Stop => break,
                Next::Cancelled => return self.cancelled(recorder, client),
            };
            let started = Instant::now();
            let protocol_u = upstream_protocol(&lease);
            recorder.begin_attempt(&lease, protocol_u);
            let counted = self
                .attempt_count(&mut job, &resolved, &lease, &mut recorder)
                .await;
            let (tokens, upstream_body) = match counted {
                Counted::Tokens(tokens, body) => {
                    self.report(
                        &lease,
                        Outcome::Success {
                            latency_ms: elapsed_ms(started),
                        },
                    );
                    recorder
                        .builder()
                        .push_attempt(attempt_record(&lease, protocol_u, started));
                    (tokens, body)
                }
                Counted::Estimate(refused) => {
                    // Nothing is reported: the credential was either not
                    // called at all or lacks an optional endpoint, which
                    // says nothing about its health. A call that was made
                    // is on the record all the same.
                    if let Some(failure) = refused {
                        recorder
                            .builder()
                            .push_attempt(failed_attempt_record(&lease, &failure, started));
                    }
                    let tokens = match job.decoded() {
                        Ok(decoded) => estimate_tokens(decoded),
                        Err(error) => return self.reject(recorder, client, &error),
                    };
                    (tokens, None)
                }
                Counted::Failed(failure) => {
                    recorder
                        .builder()
                        .push_attempt(failed_attempt_record(&lease, &failure, started));
                    recorder.capture_upstream_failure(&failure.error);
                    failover.failed(&lease, failure);
                    last = Some(lease);
                    continue;
                }
                Counted::Fatal(error) => return self.reject(recorder, client, &error),
                Counted::Cancelled => {
                    recorder
                        .builder()
                        .push_attempt(abandoned_attempt_record(&lease, protocol_u, started));
                    return self.cancelled(recorder, client);
                }
            };
            let body = match upstream_body {
                Some(body) => body,
                None => match client.encode_count_response(tokens) {
                    Some(value) => Bytes::from(value.to_string()),
                    None => {
                        let error = ApiError::internal("the token count could not be rendered");
                        return self.reject(recorder, client, &error);
                    }
                },
            };
            recorder
                .builder()
                .clear_error()
                .mark_first_byte(now_unix_ms());
            recorder.capture_client_response(&body);
            let reply = FullReply {
                status: 200,
                headers: reply_headers(
                    recorder.id(),
                    Served {
                        provider: Some(&lease.credential.provider),
                        upstream_model: Some(&lease.upstream_model),
                        upstream_headers: None,
                    },
                ),
                content_type: JSON.to_string(),
                body,
                request_id: recorder.id().to_string(),
            };
            recorder.finish(200);
            return Reply::Full(reply);
        }

        let error = failover.into_error();
        let served = match &last {
            Some(lease) => Served {
                provider: Some(&lease.credential.provider),
                upstream_model: Some(&lease.upstream_model),
                upstream_headers: None,
            },
            None => Served::default(),
        };
        self.give_up(recorder, client, error, served)
    }

    /// One counting attempt with `lease`.
    async fn attempt_count(
        &self,
        job: &mut Job,
        resolved: &Resolved,
        lease: &Lease,
        recorder: &mut Recorder,
    ) -> Counted {
        if lease.credential.kind == ProviderKind::Mock {
            recorder.builder().set_mode(Mode::Mock);
            return Counted::Estimate(None);
        }
        let protocol = upstream_protocol(lease);
        let upstream = switchyard_codecs::codec(protocol);
        let ctx = upstream_ctx(lease);
        let op = Operation::CountTokens;
        let passthrough = job.client.protocol() == protocol && !job.wrapped;
        recorder.builder().set_mode(if passthrough {
            Mode::Passthrough
        } else {
            Mode::Translated
        });

        let mut body = if passthrough {
            passthrough_count_body(upstream, &job.body, &lease.upstream_model, &ctx)
        } else {
            let mut request = match job.decoded() {
                Ok(request) => request.clone(),
                Err(error) => return Counted::Fatal(error),
            };
            request.model = lease.upstream_model.clone();
            request.stream = false;
            let asked = request.reasoning.clone().unwrap_or_default();
            let plan = plan_reasoning(&ReasoningInputs {
                client: &asked,
                suffix: resolved.depth_for(lease),
                model: ctx.thinking,
                target: protocol,
                passthrough: false,
            });
            apply_to_request(&mut request, plan);
            sanitize_tool_names(&mut request, protocol);
            match upstream.encode_count_request(&request, &ctx) {
                Some(mut body) => {
                    if protocol == Protocol::OpenaiResponses {
                        // Whether reasoning is summarised has no bearing on
                        // the size of the input, and the field is one that
                        // organisations OpenAI has not verified are refused
                        // (see `crate::summary`).
                        strip_summary(&mut body);
                    }
                    body
                }
                // The upstream's protocol has no counting endpoint.
                None => return Counted::Estimate(None),
            }
        };
        if lease.credential.kind == ProviderKind::Vertex {
            adapt_for_vertex(&mut body, protocol, &op);
        }
        let target = match self
            .target_for(
                &job.config,
                &lease.provider_config,
                &lease.credential,
                protocol,
                &lease.upstream_model,
            )
            .await
        {
            Ok(target) => target,
            Err(error) => return Counted::Failed(Failure::local(error, protocol)),
        };
        let Ok(bytes) = serde_json::to_vec(&body).map(Bytes::from) else {
            return Counted::Fatal(ApiError::internal(
                "the upstream request could not be serialised",
            ));
        };
        recorder.capture_upstream_request(None, &bytes);

        let call =
            self.upstream
                .send_unbuffered(&target, &op, bytes, &job.headers, timeouts(&job.config));
        let sent = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => return Counted::Cancelled,
            sent = call => sent,
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) if estimate_instead(&error) => {
                tracing::debug!(
                    provider = %lease.credential.provider,
                    status = error.status,
                    "the upstream's token counting endpoint is not available; estimating locally"
                );
                recorder.capture_upstream_failure(&error);
                return Counted::Estimate(Some(Failure::upstream(error, protocol)));
            }
            Err(error) => return Counted::Failed(Failure::upstream(error, protocol)),
        };
        let bytes = match read_limited(response.body, body_limit(&job.config), &job.cancel).await {
            Ok(bytes) => bytes,
            Err(ReadError::Cancelled) => return Counted::Cancelled,
            Err(ReadError::TooLarge) => return Counted::Fatal(too_large(&job.config)),
            Err(ReadError::Upstream(error)) => {
                return Counted::Failed(Failure::upstream(error, protocol));
            }
        };
        recorder.capture_upstream_response(&bytes);
        let counted = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| upstream.decode_count_response(&value));
        match counted {
            Some(tokens) => Counted::Tokens(tokens, passthrough.then_some(bytes)),
            // A 200 that is not a count: this upstream cannot be relied on
            // for counting, but the client still deserves a number.
            None => Counted::Estimate(Some(Failure::local(
                bad_gateway("the upstream's answer is not a token count"),
                protocol,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::local_failure;
    use serde_json::json;
    use switchyard_core::{FailureClass, Protocol};

    #[test]
    fn a_forwarded_counting_body_is_repaired_but_stays_a_counting_body() {
        let codec = switchyard_codecs::codec(Protocol::Anthropic);
        let ctx = UpstreamCtx {
            max_output_tokens: Some(64_000),
            ..UpstreamCtx::default()
        };
        let client = json!({
            "model": "alias",
            "system": "be brief",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "pondering", "signature": ""},
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "user", "content": "again"}
            ]
        });
        let body = passthrough_count_body(codec, &client, "claude-opus-5", &ctx);
        assert_eq!(body["model"], "claude-opus-5");
        // Repaired like a generation body …
        assert!(!body.to_string().contains("pondering"), "{body}");
        assert_eq!(body["messages"][1]["content"][0]["text"], "hello");
        // … without the fields only generation knows.
        assert_eq!(body.get("max_tokens"), None);
        assert_eq!(body.get("stream"), None);
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["model", "system", "messages"]);
    }

    #[test]
    fn a_native_counting_body_is_forwarded_as_written() {
        for protocol in [Protocol::Anthropic, Protocol::OpenaiResponses] {
            let codec = switchyard_codecs::codec(protocol);
            let client = match protocol {
                Protocol::Anthropic => json!({
                    "model": "claude-opus-5",
                    "messages": [{"role": "user", "content": "hi"}],
                    "tools": [{"name": "f", "input_schema": {"type": "object"}}]
                }),
                _ => json!({"model": "claude-opus-5", "input": "hi", "x-unknown": 1.50}),
            };
            let body =
                passthrough_count_body(codec, &client, "claude-opus-5", &UpstreamCtx::default());
            assert_eq!(body, client, "{protocol}");
        }
    }

    #[test]
    fn counting_falls_back_to_an_estimate_unless_the_credential_is_in_trouble() {
        let failed = |status, class| local_failure(class, status, "x");
        for (status, class) in [
            (404, FailureClass::ModelNotFound),
            (404, FailureClass::Request),
            (405, FailureClass::Server),
            (501, FailureClass::Server),
            (401, FailureClass::Auth),
            (403, FailureClass::Auth),
        ] {
            assert!(estimate_instead(&failed(status, class)), "{status}");
        }
        for (status, class) in [
            (429, FailureClass::RateLimit),
            (500, FailureClass::Server),
            (0, FailureClass::Transport),
            (400, FailureClass::Request),
        ] {
            assert!(!estimate_instead(&failed(status, class)), "{status}");
        }
    }
}
