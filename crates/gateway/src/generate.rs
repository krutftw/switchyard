//! The generation pipeline (`docs/DESIGN.md` section 8): one client request
//! from parsing to the reply, with every upstream attempt in between.

use crate::auth::ClientIdentity;
use crate::failover::{Failover, Failure, FinalError, Limits, Next};
use crate::gateway::Inner;
use crate::prepare::{Job, PrepareError, Prepared};
use crate::recorder::{CaptureBuf, Recorder};
use crate::reply::{
    CLIENT_CLOSED_REQUEST, JSON, Served, client_closed, error_reply, final_error_reply,
    reply_headers,
};
use crate::session::session_key;
use crate::stream::{
    Boot, CanonicalDecoder, Feed, Pump, STREAM_CHANNEL_CAPACITY, asked_for_usage_chunk, bootstrap,
    idle_limit, idle_timeout, is_usage_only_chunk,
};
use crate::target::{ReadError, bad_gateway, body_limit, offered_headers, read_limited, too_large};
use crate::types::{ClientRequest, FullReply, Reply, StreamReply};
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use switchyard_core::codec::{RequestMeta, RequestPath};
use switchyard_core::config::{Config, ProviderKind};
use switchyard_core::reasoning::parse_model_suffix;
use switchyard_core::util::now_unix_ms;
use switchyard_core::{ApiError, Codec, Protocol, SseEvent, Usage};
use switchyard_scheduler::{Lease, Outcome, Resolved};
use switchyard_telemetry::{Attempt, ClientInfo, Mode, RequestStart, Transport, new_request_id};
use switchyard_translate::{
    CodecRef, ReasoningInputs, Transcoder, apply_to_request, plan_with_label, rewrite_model_text,
};
use switchyard_upstream::{
    Operation, Target, Timeouts, UpstreamBody, build_request, mock_error, mock_response,
    mock_stream,
};
use tokio::sync::mpsc;

/// How one attempt ended.
pub(crate) enum Attempted {
    /// A complete response for the client.
    Full(FullDone),
    /// A stream that produced its first event.
    Stream(Box<Committed>),
    /// The upstream failed: report, and maybe try another credential.
    Failed(Failure),
    /// The request cannot be served whatever the credential: answer with
    /// this error. The scheduler is not told anything.
    Fatal(ApiError),
    /// The client went away.
    Cancelled,
}

/// A successful non-streaming attempt.
pub(crate) struct FullDone {
    pub body: Bytes,
    pub usage: Usage,
    pub upstream_headers: Option<HeaderMap>,
}

/// A stream past its bootstrap, ready to be pumped.
pub(crate) struct Committed {
    pub feed: Feed,
    pub transcoder: Transcoder,
    pub pending: Vec<SseEvent>,
    pub ended: bool,
    pub upstream_headers: Option<HeaderMap>,
    pub target: Option<Target>,
    pub upstream_capture: Option<CaptureBuf>,
}

/// Milliseconds since `since`.
pub(crate) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The record entry of an attempt made with `lease`.
pub(crate) fn attempt_record(lease: &Lease, protocol: Protocol, started: Instant) -> Attempt {
    Attempt::new(
        lease.credential.provider.clone(),
        lease.upstream_model.clone(),
        protocol,
    )
    .with_credential(
        lease.credential.id.clone(),
        Some(lease.credential.label.clone()),
    )
    .with_duration_ms(elapsed_ms(started))
}

/// The record entry of a failed attempt.
pub(crate) fn failed_attempt_record(lease: &Lease, failure: &Failure, started: Instant) -> Attempt {
    attempt_record(lease, failure.protocol, started)
        .failed(failure.error.status, &failure.error.info.message)
}

/// The record entry of an attempt that was in progress when the client went
/// away. The upstream was called (and may bill for it), so the record says
/// with which credential; nobody is blamed.
pub(crate) fn abandoned_attempt_record(
    lease: &Lease,
    protocol: Protocol,
    started: Instant,
) -> Attempt {
    attempt_record(lease, protocol, started).failed(
        CLIENT_CLOSED_REQUEST,
        "the client closed the request before the upstream answered",
    )
}

/// Parses a client body and reads its model and stream flag.
pub(crate) fn parse_request(
    codec: &dyn Codec,
    body: &[u8],
    path_model: Option<&str>,
    path_stream: Option<bool>,
) -> Result<(Value, RequestMeta), ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::invalid_request("the request body is empty"));
    }
    let json: Value = serde_json::from_slice(body).map_err(|error| {
        ApiError::invalid_request(format!("the request body is not valid JSON: {error}"))
    })?;
    let meta = codec.request_meta(
        &json,
        &RequestPath {
            model: path_model,
            stream: path_stream,
        },
    )?;
    Ok((json, meta))
}

/// The client key's checks: the model allow-list, then the rate limit (so
/// a request for a model the key may not use does not count against it).
///
/// The allow-list is matched against the name the client wrote (without its
/// reasoning suffix) and against the registered name it resolved to, so a
/// pattern works whichever spelling it uses.
pub(crate) fn check_identity(
    identity: &ClientIdentity,
    requested: &str,
    resolved: Option<&Resolved>,
) -> Result<(), ApiError> {
    let written = parse_model_suffix(requested.trim()).base;
    let allowed = identity.allows_model(written)
        || resolved.is_some_and(|resolved| identity.allows_model(&resolved.base));
    if !allowed {
        return Err(ApiError::permission(format!(
            "this API key is not allowed to use model `{written}`"
        ))
        .with_code("model_not_allowed")
        .with_param("model"));
    }
    identity.check_rate(Instant::now())
}

/// The largest time limits of an upstream call for this configuration.
pub(crate) fn timeouts(config: &Config) -> Timeouts {
    Timeouts::from_config(&config.upstream)
}

impl Inner {
    /// Starts the record of a request.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_record(
        &self,
        config: &Arc<Config>,
        protocol: Protocol,
        endpoint: &str,
        transport: Transport,
        identity: &ClientIdentity,
        client_ip: Option<String>,
        headers: &HeaderMap,
        request_id: Option<String>,
    ) -> Recorder {
        let id = request_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(new_request_id);
        let user_agent = headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let start = RequestStart::new(protocol, endpoint, "", now_unix_ms())
            .with_id(id)
            .with_client(ClientInfo {
                key_id: identity.key_id.clone(),
                key_name: identity.key_name.clone(),
                ip: client_ip,
                user_agent,
            })
            .with_transport(transport);
        Recorder::begin(&self.telemetry, Arc::clone(config), start)
    }

    /// Ends a request with an error that did not come out of an attempt
    /// loop.
    pub(crate) fn reject(
        &self,
        mut recorder: Recorder,
        codec: &dyn Codec,
        error: &ApiError,
    ) -> Reply {
        recorder.fail_with(error, None);
        let reply = error_reply(codec, error, recorder.id(), Served::default());
        recorder.capture_client_response(&reply.body);
        recorder.finish(reply.status);
        Reply::Full(reply)
    }

    /// Ends a request whose client went away. Nothing negative is reported
    /// to the scheduler: the credential is not at fault.
    pub(crate) fn cancelled(&self, mut recorder: Recorder, codec: &dyn Codec) -> Reply {
        let error = client_closed();
        recorder
            .builder()
            .set_error(switchyard_telemetry::RecordError::new(
                "client_disconnect",
                &error.message,
            ));
        let reply = error_reply(codec, &error, recorder.id(), Served::default());
        recorder.finish(CLIENT_CLOSED_REQUEST);
        Reply::Full(reply)
    }

    /// Ends a request with what an attempt loop gave up on.
    pub(crate) fn give_up(
        &self,
        mut recorder: Recorder,
        codec: &dyn Codec,
        error: FinalError,
        served: Served<'_>,
    ) -> Reply {
        recorder.fail_finally(&error);
        let reply = final_error_reply(codec, &error, recorder.id(), served);
        recorder.capture_client_response(&reply.body);
        recorder.finish(reply.status);
        Reply::Full(reply)
    }

    /// See [`crate::Gateway::generate`].
    pub(crate) async fn generate(self: &Arc<Self>, request: ClientRequest) -> Reply {
        let ClientRequest {
            protocol,
            endpoint,
            body,
            path_model,
            path_stream,
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

        // 1. Parse; model and stream flag.
        let (json, meta) = match parse_request(client, &body, path_model.as_deref(), path_stream) {
            Ok(parsed) => parsed,
            Err(error) => return self.reject(recorder, client, &error),
        };
        recorder
            .builder()
            .set_requested_model(meta.model.clone())
            .set_stream(meta.stream);
        recorder.announce();

        // 2-4. Suffix split and resolution, with the client key's checks in
        // between: a key that may not use a model is told so whether or not
        // the model exists.
        let resolution = self.scheduler.resolve(&meta.model);
        if let Err(error) = check_identity(&identity, &meta.model, resolution.as_ref().ok()) {
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
            path_stream,
            headers: offered_headers(headers),
            scope,
            decoded: None,
            cancel: cancel.clone(),
        };

        // 5. The attempt loop.
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
            let protocol_u = crate::target::upstream_protocol(&lease);
            recorder.begin_attempt(&lease, protocol_u);
            match self
                .attempt(&mut job, &resolved, &lease, &mut recorder)
                .await
            {
                Attempted::Full(done) => {
                    self.report(
                        &lease,
                        Outcome::Success {
                            latency_ms: elapsed_ms(started),
                        },
                    );
                    recorder
                        .builder()
                        .push_attempt(attempt_record(&lease, protocol_u, started))
                        .clear_error()
                        .mark_first_byte(now_unix_ms());
                    recorder.set_usage(done.usage);
                    recorder.capture_client_response(&done.body);
                    let reply = FullReply {
                        status: 200,
                        headers: reply_headers(
                            recorder.id(),
                            served(&lease, done.upstream_headers.as_ref()),
                        ),
                        content_type: JSON.to_string(),
                        body: done.body,
                        request_id: recorder.id().to_string(),
                    };
                    recorder.finish(200);
                    return Reply::Full(reply);
                }
                Attempted::Stream(committed) => {
                    let latency_ms = elapsed_ms(started);
                    recorder
                        .builder()
                        .push_attempt(attempt_record(&lease, protocol_u, started))
                        .clear_error()
                        .mark_first_byte(now_unix_ms());
                    let reply_headers = reply_headers(
                        recorder.id(),
                        served(&lease, committed.upstream_headers.as_ref()),
                    );
                    let request_id = recorder.id().to_string();
                    let (tx, events) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
                    let Committed {
                        feed,
                        transcoder,
                        pending,
                        ended,
                        target,
                        upstream_capture,
                        ..
                    } = *committed;
                    Pump {
                        inner: Arc::clone(self),
                        client_capture: recorder.capture_buf(),
                        stream_gauge: self.telemetry.track_stream(),
                        recorder,
                        lease: *lease,
                        scope: job.scope,
                        feed,
                        transcoder,
                        pending,
                        ended,
                        tx,
                        cancel: cancel.clone(),
                        idle: idle_limit(config.streaming.idle_timeout_secs),
                        latency_ms,
                        target,
                        upstream_capture,
                        delivered: false,
                    }
                    .spawn();
                    return Reply::Stream(StreamReply {
                        headers: reply_headers,
                        protocol,
                        request_id,
                        events,
                    });
                }
                Attempted::Failed(failure) => {
                    recorder
                        .builder()
                        .push_attempt(failed_attempt_record(&lease, &failure, started));
                    recorder.capture_upstream_failure(&failure.error);
                    failover.failed(&lease, failure);
                    last = Some(lease);
                }
                Attempted::Fatal(error) => {
                    recorder.builder().push_attempt(
                        attempt_record(&lease, protocol_u, started)
                            .failed(error.status, &error.message),
                    );
                    return self.reject(recorder, client, &error);
                }
                Attempted::Cancelled => {
                    recorder
                        .builder()
                        .push_attempt(abandoned_attempt_record(&lease, protocol_u, started));
                    return self.cancelled(recorder, client);
                }
            }
        }

        // 6. Nothing worked.
        let attempts = failover.attempts();
        let error = failover.into_error();
        tracing::debug!(
            request = %recorder.id(),
            attempts,
            status = error.api.status,
            "request failed: {}",
            error.api.message
        );
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

    /// One upstream attempt with `lease`.
    async fn attempt(
        &self,
        job: &mut Job,
        resolved: &Resolved,
        lease: &Lease,
        recorder: &mut Recorder,
    ) -> Attempted {
        if lease.credential.kind == ProviderKind::Mock {
            return self.attempt_mock(job, resolved, lease, recorder).await;
        }
        let prepared = match self.prepare_generate(job, resolved, lease).await {
            Ok(prepared) => prepared,
            Err(PrepareError::Client(error)) => return Attempted::Fatal(error),
            Err(PrepareError::Upstream(error)) => {
                return Attempted::Failed(Failure::local(
                    error,
                    crate::target::upstream_protocol(lease),
                ));
            }
        };
        let Prepared {
            target,
            protocol,
            mode,
            body,
            names,
            reasoning,
        } = prepared;
        let upstream = switchyard_codecs::codec(protocol);
        let stream = job.meta.stream;
        let op = Operation::Generate { stream };
        recorder.builder().set_mode(mode);
        recorder.builder().record_mut().reasoning = reasoning.map(str::to_string);
        if recorder.capturing() {
            let sent_headers = build_request(&target, &op, &body, &job.headers)
                .ok()
                .map(|built| built.headers);
            recorder.capture_upstream_request(sent_headers.as_ref(), &body);
        }

        let limits = timeouts(&job.config);
        let idle = idle_limit(job.config.streaming.idle_timeout_secs);
        let call = self
            .upstream
            .send_unbuffered(&target, &op, body, &job.headers, limits);
        let sent = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => return Attempted::Cancelled,
            sent = within(call, if stream { idle } else { None }) => sent,
        };
        let response = match sent {
            Some(Ok(response)) => response,
            Some(Err(error)) => return Attempted::Failed(Failure::upstream(error, protocol)),
            None => {
                return Attempted::Failed(Failure::local(
                    idle_timeout(idle.unwrap_or_default()),
                    protocol,
                ));
            }
        };
        let passthrough_headers = job.config.upstream.passthrough_headers;
        let upstream_headers = passthrough_headers.then(|| response.headers.clone());
        // The name the client used; responses carry it instead of the
        // upstream's id.
        let client_model = job.meta.model.clone();
        let renamed = client_model != lease.upstream_model;

        if stream {
            let UpstreamBody::Stream(bytes) = response.body else {
                return Attempted::Failed(Failure::local(
                    bad_gateway("the upstream did not answer with a stream"),
                    protocol,
                ));
            };
            let mut feed = Feed::bytes(bytes);
            let transcoder = if mode == Mode::Passthrough {
                let forwarder = Transcoder::passthrough(
                    upstream.stream_decoder(),
                    CodecRef::Static(upstream),
                    renamed.then_some(client_model),
                )
                // A stream cut short by the upstream must not look complete.
                .report_truncation(true);
                if protocol == Protocol::OpenaiChat && !asked_for_usage_chunk(&job.body) {
                    // The upstream was asked for usage on the gateway's own
                    // account; a client that did not ask is not sent the
                    // extra chunk.
                    forwarder.with_passthrough_filter(|event| !is_usage_only_chunk(event))
                } else {
                    forwarder
                }
            } else {
                Transcoder::translate(
                    upstream.stream_decoder(),
                    job.client.stream_encoder(&job.client_ctx()),
                )
                .with_tool_names(names)
            };
            // What the upstream says about a failure goes to the client;
            // the credential it may quote does not.
            let scrubber = target.clone();
            let mut transcoder = transcoder.with_scrubber(move |text| scrubber.redact(text));
            let mut upstream_capture = recorder.capture_buf();
            return match bootstrap(
                &mut feed,
                &mut transcoder,
                &job.cancel,
                idle,
                &mut upstream_capture,
                Some(&target),
            )
            .await
            {
                Boot::Committed { pending, ended } => Attempted::Stream(Box::new(Committed {
                    feed,
                    transcoder,
                    pending,
                    ended,
                    upstream_headers,
                    target: Some(target),
                    upstream_capture,
                })),
                Boot::Failed(error) => {
                    if let Some(text) = upstream_capture.and_then(CaptureBuf::into_text) {
                        recorder.capture_upstream_response(target.redact(&text).as_bytes());
                    }
                    Attempted::Failed(Failure::local(error, protocol).after_start())
                }
                Boot::Cancelled => Attempted::Cancelled,
            };
        }

        let bytes = match read_limited(response.body, body_limit(&job.config), &job.cancel).await {
            Ok(bytes) => bytes,
            Err(ReadError::Cancelled) => return Attempted::Cancelled,
            Err(ReadError::TooLarge) => return Attempted::Fatal(too_large(&job.config)),
            Err(ReadError::Upstream(error)) => {
                return Attempted::Failed(Failure::upstream(error, protocol));
            }
        };
        recorder.capture_upstream_response(&bytes);
        let not_json = || {
            Attempted::Failed(Failure::local(
                bad_gateway("the upstream answered with a body that is not valid JSON"),
                protocol,
            ))
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return not_json();
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return not_json();
        };

        if mode == Mode::Passthrough {
            // Decoded on the side for accounting only: whatever the decoder
            // makes of the body, the client gets the upstream's own bytes.
            let mut usage = Usage::default();
            if let Ok(decoded) = upstream.decode_response(&value) {
                usage = decoded.usage;
                self.reasoning.remember(&decoded, &job.scope);
            }
            let body = if renamed {
                rewrite_model_text(upstream, text, &client_model)
                    .map(Bytes::from)
                    .unwrap_or(bytes)
            } else {
                bytes
            };
            return Attempted::Full(FullDone {
                body,
                usage,
                upstream_headers,
            });
        }

        let mut decoded = match upstream.decode_response(&value) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Attempted::Failed(Failure::local(
                    bad_gateway(format!(
                        "the upstream response could not be understood: {error}"
                    )),
                    protocol,
                ));
            }
        };
        names.restore_response(&mut decoded);
        self.reasoning.remember(&decoded, &job.scope);
        let rendered = job
            .client
            .encode_response(&decoded, &job.client_ctx())
            .map_err(ApiError::from)
            .and_then(|value| {
                serde_json::to_vec(&value).map_err(|error| {
                    ApiError::internal(format!("the response could not be serialised: {error}"))
                })
            });
        match rendered {
            Ok(body) => Attempted::Full(FullDone {
                body: Bytes::from(body),
                usage: decoded.usage,
                upstream_headers,
            }),
            Err(error) => Attempted::Fatal(error),
        }
    }

    /// An attempt served by the built-in mock provider: no network, always
    /// through the canonical model.
    async fn attempt_mock(
        &self,
        job: &mut Job,
        resolved: &Resolved,
        lease: &Lease,
        recorder: &mut Recorder,
    ) -> Attempted {
        let protocol = lease.upstream_protocol;
        let stream = job.meta.stream;
        let mut request = match job.decoded() {
            Ok(request) => request.clone(),
            Err(error) => return Attempted::Fatal(error),
        };
        request.model = lease.upstream_model.clone();
        request.stream = stream;
        let asked = request.reasoning.clone().unwrap_or_default();
        let (plan, label) = plan_with_label(&ReasoningInputs {
            client: &asked,
            suffix: resolved.depth_for(lease),
            model: lease.info.thinking_caps(),
            target: protocol,
            passthrough: false,
        });
        apply_to_request(&mut request, plan);
        recorder.builder().set_mode(Mode::Mock);
        recorder.builder().record_mut().reasoning = label.map(str::to_string);
        if recorder.capturing()
            && let Ok(canonical) = serde_json::to_vec(&request)
        {
            recorder.capture_upstream_request(None, &canonical);
        }

        // Asked first, so a failing mock model is a failed *attempt* (with
        // failover and cooldowns) rather than an error event in a stream.
        // The failure is scripted: it may rest the failing model, never the
        // mock credential that the working mock models share.
        if let Some(error) = mock_error(&request) {
            return Attempted::Failed(Failure::scripted(error, protocol));
        }

        if stream {
            let mut feed = Feed::Canonical(mock_stream(&request, protocol));
            let mut transcoder = Transcoder::translate(
                Box::new(CanonicalDecoder),
                job.client.stream_encoder(&job.client_ctx()),
            );
            let idle = idle_limit(job.config.streaming.idle_timeout_secs);
            let mut no_capture = None;
            return match bootstrap(
                &mut feed,
                &mut transcoder,
                &job.cancel,
                idle,
                &mut no_capture,
                None,
            )
            .await
            {
                Boot::Committed { pending, ended } => Attempted::Stream(Box::new(Committed {
                    feed,
                    transcoder,
                    pending,
                    ended,
                    upstream_headers: None,
                    target: None,
                    upstream_capture: None,
                })),
                Boot::Failed(error) => {
                    Attempted::Failed(Failure::scripted(error, protocol).after_start())
                }
                Boot::Cancelled => Attempted::Cancelled,
            };
        }

        let answered = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => return Attempted::Cancelled,
            answered = mock_response(&request, protocol) => answered,
        };
        let response = match answered {
            Ok(response) => response,
            Err(error) => return Attempted::Failed(Failure::scripted(error, protocol)),
        };
        self.reasoning.remember(&response, &job.scope);
        let rendered = job
            .client
            .encode_response(&response, &job.client_ctx())
            .map_err(ApiError::from)
            .and_then(|value| {
                serde_json::to_vec(&value).map_err(|error| {
                    ApiError::internal(format!("the response could not be serialised: {error}"))
                })
            });
        match rendered {
            Ok(body) => Attempted::Full(FullDone {
                body: Bytes::from(body),
                usage: response.usage,
                upstream_headers: None,
            }),
            Err(error) => Attempted::Fatal(error),
        }
    }
}

/// `future` under an optional time limit; `None` when the limit expired.
pub(crate) async fn within<F: Future>(future: F, limit: Option<Duration>) -> Option<F::Output> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, future).await.ok(),
        None => Some(future.await),
    }
}

/// The reply-header facts of a request served with `lease`.
fn served<'a>(lease: &'a Lease, upstream_headers: Option<&'a HeaderMap>) -> Served<'a> {
    Served {
        provider: Some(&lease.credential.provider),
        upstream_model: Some(&lease.upstream_model),
        upstream_headers,
    }
}

/// The error for a model that no provider of the required kind serves.
pub(crate) fn no_route(model: &str, what: &str) -> ApiError {
    ApiError::not_found(format!("model `{model}` is not served by {what}"))
        .with_code("model_not_found")
        .with_param("model")
}
