//! Raw proxying of OpenAI-style side endpoints (embeddings, image
//! generation, speech, moderations): the body is forwarded as it is to an
//! `openai` / `openai-compat` provider that serves the request's model.
//!
//! These endpoints are optional: an upstream that serves chat completions
//! need not have `/embeddings` or `/moderations`, and a key may be allowed
//! one and not the other. A refusal therefore only counts against the
//! credential when it would hold for generation traffic too (see
//! `failover`): the client is told what the upstream said, the next
//! credential is tried, and the model stays in rotation.

use crate::failover::{Failover, Failure, Limits, Next};
use crate::gateway::Inner;
use crate::generate::{
    abandoned_attempt_record, attempt_record, check_identity, elapsed_ms, failed_attempt_record,
    no_route, timeouts,
};
use crate::recorder::Recorder;
use crate::reply::{Served, reply_headers};
use crate::target::{ReadError, body_limit, offered_headers, read_limited, too_large};
use crate::types::{FullReply, RawRequest, Reply};
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use switchyard_core::config::ProviderKind;
use switchyard_core::util::{now_unix_ms, u64_field};
use switchyard_core::{ApiError, Protocol, Usage};
use switchyard_scheduler::{Lease, Outcome};
use switchyard_telemetry::{Mode, Transport};
use switchyard_upstream::Operation;

/// The error envelope and failover bookkeeping of raw requests are those of
/// the OpenAI API, whose side endpoints these are.
const RAW_PROTOCOL: Protocol = Protocol::OpenaiChat;

/// Response bodies up to this size are looked at for a `usage` object.
const USAGE_SCAN_LIMIT: usize = 8 * 1024 * 1024;

/// Whether a media type is JSON (`application/json`, `…+json`).
fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        let media = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        media == "application/json" || media.ends_with("+json")
    })
}

/// Whether a request body is to be treated as JSON: by its media type, or —
/// when the client sent none — by looking like a JSON object.
fn body_is_json(content_type: Option<&str>, body: &[u8]) -> bool {
    match content_type {
        Some(_) => is_json(content_type),
        None => body.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{'),
    }
}

/// The body to send upstream: the client's, with the `model` field of a
/// JSON body replaced by the upstream id. Re-serialization also ensures that
/// duplicate JSON fields cannot make an upstream select a different model.
fn with_upstream_model(body: &Bytes, json: bool, upstream_model: &str) -> Bytes {
    if !json {
        return body.clone();
    }
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    match object.get("model") {
        Some(Value::String(_)) => {}
        _ => return body.clone(),
    }
    object.insert(
        "model".to_string(),
        Value::String(upstream_model.to_string()),
    );
    serde_json::to_vec(&Value::Object(object))
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

/// Token usage reported by an OpenAI-style JSON response, if any
/// (embeddings report `prompt_tokens`; image models `input_tokens` /
/// `output_tokens`).
fn usage_of(body: &[u8], json: bool) -> Usage {
    if !json || body.len() > USAGE_SCAN_LIMIT {
        return Usage::default();
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Usage::default();
    };
    let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) else {
        return Usage::default();
    };
    let field = |names: [&str; 2]| names.iter().find_map(|name| u64_field(usage, name));
    Usage {
        input_tokens: field(["prompt_tokens", "input_tokens"]).unwrap_or(0),
        output_tokens: field(["completion_tokens", "output_tokens"]).unwrap_or(0),
        ..Usage::default()
    }
}

/// How one raw attempt ended.
enum Forwarded {
    Done {
        status: u16,
        body: Bytes,
        content_type: String,
        headers: HeaderMap,
    },
    Failed(Failure),
    Fatal(ApiError),
    Cancelled,
}

impl Inner {
    /// See [`crate::Gateway::raw`].
    pub(crate) async fn raw(self: &Arc<Self>, request: RawRequest) -> Reply {
        let config = self.store.current();
        let client = switchyard_codecs::codec(RAW_PROTOCOL);
        let mut recorder = self.begin_record(
            &config,
            RAW_PROTOCOL,
            &request.endpoint,
            Transport::Http,
            &request.identity,
            request.client_ip.clone(),
            &request.headers,
            None,
        );
        recorder.capture_client_request(&request.headers, &request.body);
        recorder
            .builder()
            .set_requested_model(request.model.clone())
            .set_mode(Mode::Raw);
        recorder.announce();

        if request.model.trim().is_empty() {
            let error = ApiError::invalid_request("the request names no model").with_param("model");
            return self.reject(recorder, client, &error);
        }
        let resolution = self.scheduler.resolve(&request.model);
        if let Err(error) = check_identity(
            &self.keys.load(),
            &request.identity,
            &request.model,
            resolution.as_ref().ok(),
            serde_json::from_slice::<Value>(&request.body).ok().as_ref(),
        ) {
            return self.reject(recorder, client, &error);
        }
        let mut resolved = match resolution {
            Ok(resolved) => resolved,
            Err(error) => return self.reject(recorder, client, &ApiError::from(error)),
        };
        resolved.retain_routes(|route| {
            matches!(
                route.kind,
                ProviderKind::Openai | ProviderKind::OpenaiCompat
            )
        });
        if !resolved.has_routes() {
            let error = no_route(&resolved.base, "an OpenAI-compatible provider");
            return self.reject(recorder, client, &error);
        }
        recorder.builder().set_client_model(resolved.base.clone());

        let mut failover = Failover::new(
            self,
            &resolved,
            None,
            RAW_PROTOCOL,
            &request.cancel,
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
            recorder.begin_attempt(&lease, lease.upstream_protocol);
            match self.attempt_raw(&request, &lease, &mut recorder).await {
                Forwarded::Done {
                    status,
                    body,
                    content_type,
                    headers,
                } => {
                    self.report(
                        &lease,
                        Outcome::Success {
                            latency_ms: elapsed_ms(started),
                        },
                    );
                    recorder
                        .builder()
                        .push_attempt(
                            attempt_record(&lease, lease.upstream_protocol, started)
                                .with_status(status),
                        )
                        .clear_error()
                        .mark_first_byte(now_unix_ms());
                    let json = is_json(Some(&content_type));
                    recorder.set_usage(usage_of(&body, json));
                    if json {
                        // Binary payloads (audio, images) are not captured.
                        recorder.capture_client_response(&body);
                    }
                    let reply = FullReply {
                        status,
                        headers: reply_headers(
                            recorder.id(),
                            Served {
                                provider: Some(&lease.credential.provider),
                                upstream_model: Some(&lease.upstream_model),
                                upstream_headers: config
                                    .upstream
                                    .passthrough_headers
                                    .then_some(&headers),
                            },
                        ),
                        content_type,
                        body,
                        request_id: recorder.id().to_string(),
                    };
                    recorder.finish(status);
                    return Reply::Full(reply);
                }
                Forwarded::Failed(failure) => {
                    recorder
                        .builder()
                        .push_attempt(failed_attempt_record(&lease, &failure, started));
                    recorder.capture_upstream_failure(&failure.error);
                    failover.failed_optional(&lease, failure);
                    last = Some(lease);
                }
                Forwarded::Fatal(error) => {
                    recorder.builder().push_attempt(
                        attempt_record(&lease, lease.upstream_protocol, started)
                            .failed(error.status, &error.message),
                    );
                    return self.reject(recorder, client, &error);
                }
                Forwarded::Cancelled => {
                    recorder.builder().push_attempt(abandoned_attempt_record(
                        &lease,
                        lease.upstream_protocol,
                        started,
                    ));
                    return self.cancelled(recorder, client);
                }
            }
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

    /// One raw attempt with `lease`.
    async fn attempt_raw(
        &self,
        request: &RawRequest,
        lease: &Lease,
        recorder: &mut Recorder,
    ) -> Forwarded {
        let config = self.store.current();
        let target = match self
            .target_for(
                &config,
                &lease.provider_config,
                &lease.credential,
                lease.upstream_protocol,
                &lease.upstream_model,
            )
            .await
        {
            Ok(target) => target,
            Err(error) => return Forwarded::Failed(Failure::local(error, RAW_PROTOCOL)),
        };
        let json = body_is_json(request.content_type.as_deref(), &request.body);
        let body = with_upstream_model(&request.body, json, &lease.upstream_model);
        if json {
            // Binary and multipart payloads are not captured.
            recorder.capture_upstream_request(None, &body);
        }
        // The transport takes the body's media type from the client's
        // headers; make sure the one the server determined is there.
        let mut headers = offered_headers(request.headers.clone());
        if let Some(value) = request
            .content_type
            .as_deref()
            .and_then(|value| HeaderValue::from_str(value).ok())
        {
            headers.insert(CONTENT_TYPE, value);
        }
        let op = Operation::Raw {
            method: request.method.clone(),
            path: request.path.clone(),
            query: request.query.clone(),
        };
        let call = self
            .upstream
            .send_unbuffered(&target, &op, body, &headers, timeouts(&config));
        let sent = tokio::select! {
            biased;
            _ = request.cancel.cancelled() => return Forwarded::Cancelled,
            sent = call => sent,
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) => return Forwarded::Failed(Failure::upstream(error, RAW_PROTOCOL)),
        };
        let status = response.status;
        let content_type = response
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let headers = response.headers;
        match read_limited(response.body, body_limit(&config), &request.cancel).await {
            Ok(body) => Forwarded::Done {
                status,
                body,
                content_type,
                headers,
            },
            Err(ReadError::Cancelled) => Forwarded::Cancelled,
            Err(ReadError::TooLarge) => Forwarded::Fatal(too_large(&config)),
            Err(ReadError::Upstream(error)) => {
                Forwarded::Failed(Failure::upstream(error, RAW_PROTOCOL))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_media_types() {
        assert!(is_json(Some("application/json")));
        assert!(is_json(Some("Application/JSON; charset=utf-8")));
        assert!(is_json(Some("application/vnd.api+json")));
        assert!(!is_json(Some("multipart/form-data; boundary=x")));
        assert!(!is_json(Some("text/plain")));
        assert!(!is_json(None));
    }

    #[test]
    fn a_body_without_a_media_type_is_sniffed() {
        assert!(body_is_json(None, b"  {\"model\": \"m\"}"));
        assert!(!body_is_json(
            None,
            b"--boundary
"
        ));
        assert!(!body_is_json(None, b""));
        // A stated media type is believed.
        assert!(!body_is_json(
            Some("multipart/form-data"),
            b"{\"model\": \"m\"}"
        ));
        assert!(body_is_json(Some("application/json"), b"not json at all"));
    }

    #[test]
    fn only_the_model_field_is_replaced() {
        let body = Bytes::from_static(
            b"{\"input\": [\"a\", \"b\"], \"model\": \"alias\", \"dimensions\": 256, \"x\": 1.50}",
        );
        let out = with_upstream_model(&body, true, "text-embedding-3-small");
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "{\"input\":[\"a\",\"b\"],\"model\":\"text-embedding-3-small\",\"dimensions\":256,\"x\":1.5}"
        );
    }

    #[test]
    fn json_models_are_normalized_and_other_bodies_keep_their_bytes() {
        let same = Bytes::from_static(b"{ \"model\" : \"m\" ,  \"input\":\"x\" }");
        assert_eq!(
            with_upstream_model(&same, true, "m"),
            Bytes::from_static(b"{\"model\":\"m\",\"input\":\"x\"}")
        );
        let repeated = Bytes::from_static(b"{\"model\":\"first\",\"model\":\"m\",\"input\":\"x\"}");
        assert_eq!(
            with_upstream_model(&repeated, true, "m"),
            Bytes::from_static(b"{\"model\":\"m\",\"input\":\"x\"}")
        );
        let no_model = Bytes::from_static(b"{ \"input\":\"x\" }");
        assert_eq!(with_upstream_model(&no_model, true, "m"), no_model);
        let not_object = Bytes::from_static(b"[1, 2]");
        assert_eq!(with_upstream_model(&not_object, true, "m"), not_object);
        let broken = Bytes::from_static(b"{\"model\": ");
        assert_eq!(with_upstream_model(&broken, true, "m"), broken);
        // Not JSON by media type: never parsed.
        let form = Bytes::from_static(b"{\"model\":\"alias\"}");
        assert_eq!(with_upstream_model(&form, false, "m"), form);
    }

    #[test]
    fn usage_is_read_from_openai_style_bodies() {
        let embeddings = json!({"data": [], "usage": {"prompt_tokens": 8, "total_tokens": 8}});
        assert_eq!(
            usage_of(embeddings.to_string().as_bytes(), true),
            Usage {
                input_tokens: 8,
                ..Usage::default()
            }
        );
        let images = json!({"usage": {"input_tokens": 50, "output_tokens": 1000}});
        let usage = usage_of(images.to_string().as_bytes(), true);
        assert_eq!((usage.input_tokens, usage.output_tokens), (50, 1000));
        assert!(usage_of(b"{\"data\": []}", true).is_empty());
        assert!(usage_of(b"binary", false).is_empty());
        assert!(usage_of(embeddings.to_string().as_bytes(), false).is_empty());
    }
}
