//! From a scheduler lease to something the transport can call: the upstream
//! protocol of an attempt and the [`Target`] with its credential.

use crate::gateway::Inner;
use bytes::Bytes;
use futures::StreamExt;
use http::HeaderMap;
use std::path::PathBuf;
use std::sync::Arc;
use switchyard_core::config::{Config, CredentialConfig, ProviderConfig, ProviderKind};
use switchyard_core::{
    ApiError, ErrorKind, FailureClass, Protocol, UpstreamError, UpstreamErrorInfo,
};
use switchyard_scheduler::{CredentialView, Lease};
use switchyard_upstream::{Auth, ServiceAccount, Target, UpstreamBody, vertex_protocol_for_model};
use tokio_util::sync::CancellationToken;

/// The protocol an attempt speaks to the upstream: the scheduler's choice,
/// except on Vertex AI, where the model decides (Claude models are spoken
/// to in Anthropic Messages, everything else in Gemini).
pub(crate) fn upstream_protocol(lease: &Lease) -> Protocol {
    protocol_for(
        lease.credential.kind,
        lease.upstream_protocol,
        &lease.upstream_model,
    )
}

/// [`upstream_protocol`] from its parts.
pub(crate) fn protocol_for(kind: ProviderKind, preferred: Protocol, model: &str) -> Protocol {
    if kind == ProviderKind::Vertex {
        vertex_protocol_for_model(model)
    } else {
        preferred
    }
}

/// Client request headers that never go upstream, although the transport's
/// own allow-list would let them through: they name the *client's* account
/// with the vendor, and the upstream call is made with the *gateway's* key.
///
/// The official OpenAI SDKs send `OpenAI-Organization` / `OpenAI-Project`
/// by themselves whenever `OPENAI_ORG_ID` / `OPENAI_PROJECT_ID` are set —
/// which a client that used to talk to OpenAI directly still has. OpenAI
/// answers an organisation that does not own the key with a `401`, which
/// looks exactly like a revoked key: one such client would rest every
/// credential of the provider for everybody. An operator who wants these
/// headers sent sets them in the provider's `headers`.
const CLIENT_ACCOUNT_HEADERS: [&str; 2] = ["openai-organization", "openai-project"];

/// The client's request headers as they may be offered to the transport
/// (which then forwards only its allow-list): everything but the
/// [`CLIENT_ACCOUNT_HEADERS`].
pub(crate) fn offered_headers(mut client: HeaderMap) -> HeaderMap {
    for name in CLIENT_ACCOUNT_HEADERS {
        client.remove(name);
    }
    client
}

/// An upstream failure the gateway diagnosed itself.
pub(crate) fn local_failure(
    class: FailureClass,
    status: u16,
    message: impl Into<String>,
) -> UpstreamError {
    UpstreamError {
        status,
        class,
        info: UpstreamErrorInfo {
            message: message.into(),
            ..UpstreamErrorInfo::default()
        },
        retry_after_ms: None,
        body: None,
        content_type: None,
    }
}

/// The failure for an upstream that answered 2xx with something unusable.
pub(crate) fn bad_gateway(message: impl Into<String>) -> UpstreamError {
    local_failure(FailureClass::Server, 502, message)
}

/// Codes of in-stream failures that blame what the *request* carries,
/// whatever status the decoder gave them: the Responses API reports an
/// image it cannot fetch or read with `response.failed` and one of these
/// codes, which a decoder that does not know the code turns into a generic
/// upstream failure (`502`).
///
/// Treating that as the upstream's fault would let one client with a dead
/// image URL rest the model on every credential the request fails over to —
/// for everybody. No other credential would fare better, so these are
/// request faults: no failover, nothing rests, and the client is told `400`.
const REQUEST_FAULT_STREAM_CODES: [&str; 14] = [
    "invalid_image",
    "invalid_image_format",
    "invalid_base64_image",
    "invalid_image_url",
    "invalid_image_mode",
    "image_too_large",
    "image_too_small",
    "image_parse_error",
    "image_content_policy_violation",
    "image_file_too_large",
    "image_file_not_found",
    "unsupported_image_media_type",
    "empty_image_file",
    "failed_to_download_image",
];

/// Whether the transport takes `code` for "this account is out of money"
/// when it arrives as an HTTP error (`insufficient_quota`,
/// `billing_hard_limit_reached`, `usage_limit_reached`, …).
///
/// The transport's classifier is asked rather than its list copied, so the
/// two cannot drift apart: it is shown an envelope with nothing in it but
/// the code — no message, whose wording is not what is being asked about —
/// under the one status that leaves the decision to the code.
fn is_quota_code(code: &str) -> bool {
    if code.is_empty() {
        return false;
    }
    let envelope = serde_json::json!({"error": {"code": code}}).to_string();
    let verdict = switchyard_upstream::classify(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        429,
        &HeaderMap::new(),
        envelope.as_bytes(),
    );
    verdict.class == FailureClass::Quota
}

/// Classifies an error that arrived *inside* a stream — an upstream error
/// event, or the error the transcoder made up for a broken stream — the way
/// an HTTP error of the same meaning would be classified, so the scheduler
/// rests the credential appropriately. Credentials the upstream may have
/// quoted are removed from the message.
///
/// A code that says the account is out of money is exhausted quota whatever
/// status the decoder gave it — a decoder that does not know the code calls
/// the failure a generic `502`, which would rest one model for a moment
/// where the whole key is spent. Only a rejected credential outranks it, as
/// a `401` does over HTTP.
pub(crate) fn stream_failure(error: &ApiError, target: Option<&Target>) -> UpstreamError {
    let code = error.code.as_deref().unwrap_or("").to_ascii_lowercase();
    let request_fault = REQUEST_FAULT_STREAM_CODES.contains(&code.as_str());
    let out_of_quota =
        !request_fault && error.kind != ErrorKind::Authentication && is_quota_code(&code);
    let class = match error.kind {
        _ if request_fault => FailureClass::Request,
        _ if out_of_quota => FailureClass::Quota,
        ErrorKind::RateLimit if code.contains("quota") || code.contains("billing") => {
            FailureClass::Quota
        }
        ErrorKind::RateLimit => FailureClass::RateLimit,
        ErrorKind::Authentication | ErrorKind::Permission => FailureClass::Auth,
        ErrorKind::NotFound => FailureClass::ModelNotFound,
        ErrorKind::InvalidRequest | ErrorKind::TooLarge => FailureClass::Request,
        ErrorKind::Timeout => FailureClass::Transport,
        ErrorKind::Upstream | ErrorKind::Unavailable | ErrorKind::Internal => FailureClass::Server,
    };
    let message = match target {
        Some(target) => target.redact(&error.message),
        None => error.message.clone(),
    };
    UpstreamError {
        // A request fault is answered as one, also when the decoder took
        // the unknown code for a server failure; exhausted quota likewise,
        // with the status it has over HTTP.
        status: match error.status {
            status if (400..500).contains(&status) => status,
            _ if request_fault => 400,
            _ if out_of_quota => 429,
            status => status,
        },
        class,
        info: UpstreamErrorInfo {
            message,
            error_type: None,
            code: error.code.clone(),
            retry_after_ms: None,
        },
        retry_after_ms: error.retry_after_secs.map(|secs| secs.saturating_mul(1000)),
        body: None,
        content_type: None,
    }
}

/// The failure a *complete* (non-streamed) upstream answer reports about
/// itself, classified like the same failure inside a stream; `None` when
/// `body` is not such an answer.
///
/// The Responses API answers a generation that failed with HTTP `200` and a
/// response object whose `status` is `"failed"` and whose `error` says why
/// (`rate_limit_exceeded`, `insufficient_quota`, `server_error`,
/// `invalid_prompt`, …) — exactly what a stream reports with a
/// `response.failed` event. The body is therefore read by the protocol's
/// own stream decoder as that event, and the resulting error is classified
/// by [`stream_failure`]: a rate limit rests the credential and fails over,
/// exhausted quota rests the key, a request fault ends the request with a
/// `400`, and only what is left is the generic server failure.
///
/// Besides bodies that say `failed`, a body that carries an `error` without
/// saying so is recognised. That reading is only safe for bodies the
/// codec's `decode_response` has refused (nothing usable in them): use
/// [`declared_failure`] for a body that has not been decoded yet.
pub(crate) fn failed_response(
    protocol: Protocol,
    body: &serde_json::Value,
    target: Option<&Target>,
) -> Option<UpstreamError> {
    use serde_json::{Value, json};
    if protocol != Protocol::OpenaiResponses {
        return None;
    }
    let response = response_object(body);
    let failed = says_failed(response);
    // The decoder is handed an error *object* in every case, and nothing
    // else of the response (whose output may be large and has no say in
    // this), so that what it reports is the upstream's code and message and
    // nothing it would otherwise read off the event around them.
    let error = match response.get("error") {
        Some(Value::Object(error)) if !error.is_empty() => Value::Object(error.clone()),
        Some(Value::String(text)) if !text.trim().is_empty() => json!({"message": text.trim()}),
        _ if failed => json!({
            "message": "the upstream reported a failed response without saying why"
        }),
        _ => return None,
    };
    let event = json!({"type": "response.failed", "error": error});
    let mut decoder = switchyard_codecs::codec(protocol).stream_decoder();
    let api = decoder
        .decode(&switchyard_core::SseEvent::data(event.to_string()))
        .ok()?
        .into_iter()
        .find_map(|event| match event {
            switchyard_core::StreamEvent::Error(error) => Some(error),
            _ => None,
        })?;
    Some(stream_failure(&api, target))
}

/// The response object of a complete Responses body: the body itself, or —
/// some upstreams answer with the terminal stream event as the body — the
/// `response` inside it.
fn response_object(body: &serde_json::Value) -> &serde_json::Value {
    match body.get("response") {
        Some(inner) if inner.is_object() && body.get("output").is_none() => inner,
        _ => body,
    }
}

/// Whether a response object's `status` is `"failed"`.
fn says_failed(response: &serde_json::Value) -> bool {
    response
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| status.trim().eq_ignore_ascii_case("failed"))
}

/// [`failed_response`] for a complete answer that *says* its generation
/// failed (`status: "failed"`), whatever output it still carries; `None`
/// for every other body.
///
/// A generation can fail after it has produced something: a reasoning item
/// (a reasoning model's first output, which decodes to a part whenever the
/// encrypted reasoning was asked for), a hosted tool call, the first words
/// of the answer. Without streaming none of that has reached the client, so
/// the call is the failed attempt its status says it is — the credential
/// rests as the error code demands and the next one is tried — rather than
/// an empty or truncated answer handed on as a success. (A *stream* that
/// fails after its first event is past the point of retrying; this is the
/// complete-body case only.)
///
/// Checked before the body is decoded, and deliberately narrower than
/// [`failed_response`]: a body that decodes to output and does not say
/// `failed` is an answer, whatever else is in it.
pub(crate) fn declared_failure(
    protocol: Protocol,
    body: &serde_json::Value,
    target: Option<&Target>,
) -> Option<UpstreamError> {
    if protocol != Protocol::OpenaiResponses || !says_failed(response_object(body)) {
        return None;
    }
    failed_response(protocol, body, target)
}

/// Why a response body could not be read in full.
pub(crate) enum ReadError {
    /// The client went away.
    Cancelled,
    /// The body is larger than the gateway is willing to hold.
    TooLarge,
    /// The upstream connection failed.
    Upstream(UpstreamError),
}

/// Reads a response body, giving up when it exceeds `limit` bytes or the
/// client goes away.
pub(crate) async fn read_limited(
    body: UpstreamBody,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Bytes, ReadError> {
    match body {
        UpstreamBody::Full(bytes) if bytes.len() > limit => Err(ReadError::TooLarge),
        UpstreamBody::Full(bytes) => Ok(bytes),
        UpstreamBody::Stream(mut stream) => {
            let mut out = bytes::BytesMut::new();
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ReadError::Cancelled),
                    chunk = stream.next() => chunk,
                };
                match chunk {
                    None => return Ok(out.freeze()),
                    Some(Err(error)) => return Err(ReadError::Upstream(error)),
                    Some(Ok(chunk)) => {
                        if out.len().saturating_add(chunk.len()) > limit {
                            return Err(ReadError::TooLarge);
                        }
                        out.extend_from_slice(&chunk);
                    }
                }
            }
        }
    }
}

/// `server.body_limit_mb` in bytes: the most the gateway buffers of a
/// non-streamed upstream response.
pub(crate) fn body_limit(config: &Config) -> usize {
    usize::try_from(config.server.body_limit_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
}

/// The error for a response beyond [`body_limit`].
pub(crate) fn too_large(config: &Config) -> ApiError {
    ApiError::upstream(format!(
        "the upstream response is larger than the {} MiB the gateway buffers (server.body_limit_mb)",
        config.server.body_limit_mb
    ))
    .with_code("upstream_response_too_large")
}

impl Inner {
    /// The transport target for one credential of `provider`.
    ///
    /// Fails — with an `Auth`-class error, so the scheduler rests the
    /// credential — when the credential's service-account file cannot be
    /// loaded.
    pub(crate) async fn target_for(
        &self,
        config: &Config,
        provider: &ProviderConfig,
        credential: &CredentialView,
        protocol: Protocol,
        model: &str,
    ) -> Result<Target, UpstreamError> {
        let auth = if !credential.service_account_file.trim().is_empty() {
            Auth::ServiceAccount(
                self.service_account(credential.service_account_file.trim())
                    .await?,
            )
        } else if !credential.api_key.trim().is_empty() {
            Auth::ApiKey(credential.api_key.clone())
        } else {
            Auth::None
        };
        // The view's proxy is already the effective one (credential, else
        // provider, else global), so it goes in the most specific slot.
        let as_configured = CredentialConfig {
            proxy: credential.proxy.clone(),
            ..CredentialConfig::default()
        };
        Ok(Target::for_provider(
            provider,
            &as_configured,
            &config.upstream.proxy,
            protocol,
            model,
            auth,
        ))
    }

    /// The parsed service-account key file at `file` (relative to the
    /// configuration file's directory), read once and remembered until the
    /// configuration changes.
    pub(crate) async fn service_account(
        &self,
        file: &str,
    ) -> Result<Arc<ServiceAccount>, UpstreamError> {
        let path: PathBuf = self.store.resolve_path(file);
        if let Some(account) = self.service_accounts.lock().get(&path) {
            return Ok(Arc::clone(account));
        }
        let shown = PathBuf::from(file)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string());
        let read_under = self.store.current();
        let text = tokio::fs::read_to_string(&path).await.map_err(|error| {
            local_failure(
                FailureClass::Auth,
                0,
                format!("cannot read the service account file `{shown}`: {error}"),
            )
        })?;
        let account = ServiceAccount::from_json(&text).map_err(|error| {
            local_failure(
                FailureClass::Auth,
                0,
                format!("the service account file `{shown}` is not usable: {error}"),
            )
        })?;
        let account = Arc::new(account);
        // Remembered for the configuration it was read under only: a read
        // that a configuration change overtook may have seen the file as it
        // was before, and must not put that back into the cache the change
        // has just emptied. This attempt uses what it read all the same.
        // (Compared under the cache's lock: the cache is emptied after the
        // store has published the new configuration.)
        {
            let mut cache = self.service_accounts.lock();
            if Arc::ptr_eq(&read_under, &self.store.current()) {
                cache.insert(path, Arc::clone(&account));
            }
        }
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::config::ProxySetting;

    #[test]
    fn vertex_chooses_the_protocol_by_model() {
        assert_eq!(
            protocol_for(
                ProviderKind::Vertex,
                Protocol::Gemini,
                "claude-opus-4-5@20251101"
            ),
            Protocol::Anthropic
        );
        assert_eq!(
            protocol_for(ProviderKind::Vertex, Protocol::Gemini, "gemini-2.5-pro"),
            Protocol::Gemini
        );
        assert_eq!(
            protocol_for(ProviderKind::Openai, Protocol::OpenaiResponses, "claude-x"),
            Protocol::OpenaiResponses
        );
    }

    fn target(key: &str) -> Target {
        Target {
            provider: "p".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            protocol: Protocol::Anthropic,
            model: "m".into(),
            auth: Auth::ApiKey(key.into()),
            headers: Vec::new(),
            proxy: ProxySetting::Direct,
            project: String::new(),
            location: String::new(),
        }
    }

    #[test]
    fn the_clients_vendor_account_headers_are_not_offered_upstream() {
        let mut client = HeaderMap::new();
        for (name, value) in [
            ("openai-organization", "org-client"),
            ("openai-project", "proj_client"),
            ("openai-beta", "assistants=v2"),
            ("anthropic-beta", "context-1m-2025-08-07"),
            ("anthropic-version", "2023-06-01"),
            ("accept", "application/json"),
        ] {
            client.insert(name, http::HeaderValue::from_static(value));
        }
        let offered = offered_headers(client);
        let mut names: Vec<&str> = offered.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "accept",
                "anthropic-beta",
                "anthropic-version",
                "openai-beta"
            ]
        );

        // What the transport then makes of them for an OpenAI upstream.
        let mut openai = target("sk-gateway-key-0123456789");
        openai.kind = ProviderKind::Openai;
        openai.protocol = Protocol::OpenaiChat;
        openai.base_url = "https://api.openai.com/v1".into();
        let built = switchyard_upstream::build_request(
            &openai,
            &switchyard_upstream::Operation::Generate { stream: false },
            b"{}",
            &offered,
        )
        .unwrap();
        assert_eq!(built.header("openai-beta"), Some("assistants=v2"));
        assert_eq!(built.header("openai-organization"), None);
        assert_eq!(built.header("openai-project"), None);
    }

    #[test]
    fn in_stream_errors_are_classified_by_kind() {
        let cases = [
            (ApiError::rate_limit("slow"), FailureClass::RateLimit),
            (
                ApiError::rate_limit("out").with_code("insufficient_quota"),
                FailureClass::Quota,
            ),
            (ApiError::authentication("bad key"), FailureClass::Auth),
            (ApiError::permission("no"), FailureClass::Auth),
            (ApiError::not_found("no model"), FailureClass::ModelNotFound),
            (ApiError::invalid_request("bad"), FailureClass::Request),
            (ApiError::unavailable("overloaded"), FailureClass::Server),
            (ApiError::upstream("broken"), FailureClass::Server),
            (ApiError::internal("oops"), FailureClass::Server),
            (ApiError::timeout("slow"), FailureClass::Transport),
        ];
        for (api, class) in cases {
            let failure = stream_failure(&api, None);
            assert_eq!(failure.class, class, "{api:?}");
            assert_eq!(failure.status, api.status);
            assert_eq!(failure.info.message, api.message);
        }
        let hinted =
            ApiError::rate_limit("slow").with_retry_after(std::time::Duration::from_secs(3));
        assert_eq!(stream_failure(&hinted, None).retry_after_ms, Some(3_000));
    }

    #[test]
    fn in_stream_image_errors_are_the_requests_fault() {
        // What the Responses decoder makes of `response.failed` with a code
        // it has no status for.
        for code in REQUEST_FAULT_STREAM_CODES {
            let api = ApiError::upstream("The image could not be used.").with_code(code);
            assert_eq!(api.status, 502);
            let failure = stream_failure(&api, None);
            assert_eq!(failure.class, FailureClass::Request, "{code}");
            assert_eq!(failure.status, 400, "{code}");
            let told = failure.to_api_error();
            assert_eq!((told.status, told.kind), (400, ErrorKind::InvalidRequest));
            assert_eq!(told.code.as_deref(), Some(code));
        }
        // A 4xx the decoder did assign is kept.
        let api = ApiError::invalid_request("too big")
            .with_status(413)
            .with_code("image_too_large");
        assert_eq!(stream_failure(&api, None).status, 413);
        // Codes that say nothing about the request are classified by kind.
        for code in ["server_error", "vector_store_timeout", "image"] {
            let api = ApiError::upstream("broken").with_code(code);
            let failure = stream_failure(&api, None);
            assert_eq!(failure.class, FailureClass::Server, "{code}");
            assert_eq!(failure.status, 502, "{code}");
        }
    }

    #[test]
    fn in_stream_quota_codes_rest_the_key_like_their_http_errors() {
        // Codes the transport files under exhausted quota, as the Responses
        // decoder hands them on: with no status of their own (502) or as a
        // plain rate limit (429).
        for code in [
            "insufficient_quota",
            "billing_hard_limit_reached",
            "billing_not_active",
            "billing_error",
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "organization_usage_limit_exceeded",
            "enforced_spend_limit_reached",
            "usage_limit_reached",
            "insufficient_balance",
            "insufficient_user_quota",
            "Billing_Hard_Limit_Reached",
        ] {
            for api in [
                ApiError::upstream("Out of money.").with_code(code),
                ApiError::rate_limit("Out of money.").with_code(code),
            ] {
                let failure = stream_failure(&api, None);
                assert_eq!(failure.class, FailureClass::Quota, "{code}");
                assert_eq!(failure.status, 429, "{code}");
                assert_eq!(failure.info.code.as_deref(), Some(code));
            }
            // A 4xx the decoder read off the event is kept.
            let api = ApiError::permission("Out of money.").with_code(code);
            let failure = stream_failure(&api, None);
            assert_eq!((failure.class, failure.status), (FailureClass::Quota, 403));
            // A rejected credential is that first, as a 401 is over HTTP.
            let api = ApiError::authentication("Bad key.").with_code(code);
            assert_eq!(stream_failure(&api, None).class, FailureClass::Auth);
        }
        // Codes that only sound like it are classified by kind as before,
        // and a rate limit is still a rate limit.
        for (api, class) in [
            (
                ApiError::upstream("broken").with_code("quota"),
                FailureClass::Server,
            ),
            (
                ApiError::upstream("broken").with_code("billing"),
                FailureClass::Server,
            ),
            (
                ApiError::rate_limit("slow").with_code("rate_limit_exceeded"),
                FailureClass::RateLimit,
            ),
            (ApiError::rate_limit("slow"), FailureClass::RateLimit),
            (ApiError::upstream("broken"), FailureClass::Server),
        ] {
            let failure = stream_failure(&api, None);
            assert_eq!(failure.class, class, "{api:?}");
            assert_eq!(failure.status, api.status, "{api:?}");
        }
    }

    #[test]
    fn in_stream_errors_lose_quoted_credentials() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let api = ApiError::authentication(format!("invalid x-api-key: {key}"));
        let failure = stream_failure(&api, Some(&target(key)));
        assert!(
            !failure.info.message.contains(key),
            "{}",
            failure.info.message
        );
        assert!(failure.info.message.contains("invalid x-api-key"));
    }

    /// A Responses body of a generation that failed, as the API returns it
    /// with HTTP 200.
    fn failed_body(error: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "resp_1", "object": "response", "status": "failed",
            "model": "gpt-5", "output": [], "error": error
        })
    }

    #[test]
    fn a_failed_responses_body_is_classified_by_its_code() {
        use serde_json::json;
        let cases = [
            ("rate_limit_exceeded", FailureClass::RateLimit, 429),
            ("insufficient_quota", FailureClass::Quota, 429),
            ("billing_hard_limit_reached", FailureClass::Quota, 429),
            ("usage_limit_reached", FailureClass::Quota, 429),
            ("server_error", FailureClass::Server, 502),
            ("vector_store_timeout", FailureClass::Server, 502),
            ("invalid_prompt", FailureClass::Request, 400),
            ("context_length_exceeded", FailureClass::Request, 400),
            ("string_above_max_length", FailureClass::Request, 400),
            ("invalid_image", FailureClass::Request, 400),
            ("failed_to_download_image", FailureClass::Request, 400),
        ];
        for (code, class, status) in cases {
            let body = failed_body(json!({"code": code, "message": "It went wrong."}));
            let failure = failed_response(Protocol::OpenaiResponses, &body, None)
                .unwrap_or_else(|| panic!("{code}: not recognised"));
            assert_eq!(failure.class, class, "{code}");
            assert_eq!(failure.status, status, "{code}");
            assert_eq!(failure.info.message, "It went wrong.", "{code}");
            assert_eq!(failure.info.code.as_deref(), Some(code));
            assert_eq!(
                failure.class.should_failover(),
                class != FailureClass::Request
            );
        }
        // What the client of another protocol is told about a request fault.
        let body = failed_body(json!({"code": "invalid_prompt", "message": "Flagged."}));
        let told = failed_response(Protocol::OpenaiResponses, &body, None)
            .unwrap()
            .to_api_error();
        assert_eq!((told.status, told.kind), (400, ErrorKind::InvalidRequest));
        assert_eq!(told.code.as_deref(), Some("invalid_prompt"));
        assert_eq!(told.message, "Flagged.");
    }

    #[test]
    fn a_failed_responses_body_is_recognised_in_every_shape() {
        use serde_json::json;
        // The `type` stands in for a missing `code`.
        let typed = failed_body(json!({"type": "rate_limit_exceeded", "message": "Slow down."}));
        let failure = failed_response(Protocol::OpenaiResponses, &typed, None).unwrap();
        assert_eq!(failure.class, FailureClass::RateLimit);
        // A retry hint in the message is kept.
        let hinted = failed_body(json!({
            "code": "rate_limit_exceeded",
            "message": "Rate limit reached. Please try again in 3s."
        }));
        let failure = failed_response(Protocol::OpenaiResponses, &hinted, None).unwrap();
        assert_eq!(failure.retry_after_ms, Some(3_000));
        // Wrapped in the terminal stream event, as some upstreams answer.
        let wrapped = json!({
            "type": "response.failed",
            "response": failed_body(json!({"code": "insufficient_quota", "message": "Out."}))
        });
        let failure = failed_response(Protocol::OpenaiResponses, &wrapped, None).unwrap();
        assert_eq!(failure.class, FailureClass::Quota);
        // An error given as a string, and a failure without any error.
        let stringly = failed_body(json!("the model crashed"));
        let failure = failed_response(Protocol::OpenaiResponses, &stringly, None).unwrap();
        assert_eq!(failure.class, FailureClass::Server);
        assert_eq!(failure.info.message, "the model crashed");
        let silent = failed_body(serde_json::Value::Null);
        let failure = failed_response(Protocol::OpenaiResponses, &silent, None).unwrap();
        assert_eq!((failure.class, failure.status), (FailureClass::Server, 502));
        assert!(!failure.info.message.trim().is_empty());
        // An error object on a body that forgot to say `failed`.
        let unmarked = json!({"object": "response", "output": [],
            "error": {"code": "rate_limit_exceeded", "message": "Slow down."}});
        let failure = failed_response(Protocol::OpenaiResponses, &unmarked, None).unwrap();
        assert_eq!(failure.class, FailureClass::RateLimit);
        // The credential an upstream quotes does not survive.
        let key = "sk-proj-abcdefghijklmnopqrstuvwxyz012345";
        let quoting = failed_body(json!({
            "code": "server_error",
            "message": format!("internal error while serving {key}")
        }));
        let mut openai = target(key);
        openai.kind = ProviderKind::Openai;
        openai.protocol = Protocol::OpenaiResponses;
        let failure = failed_response(Protocol::OpenaiResponses, &quoting, Some(&openai)).unwrap();
        assert!(
            !failure.info.message.contains(key),
            "{}",
            failure.info.message
        );
    }

    #[test]
    fn bodies_that_report_no_failure_are_not_failed_responses() {
        use serde_json::json;
        for body in [
            json!({"object": "response", "status": "completed", "output": [], "error": null}),
            json!({"object": "response", "status": "incomplete", "output": []}),
            json!({"object": "response", "status": "completed", "output": [], "error": ""}),
            json!({"status": "in_progress"}),
            json!({}),
            json!([]),
            json!("failed"),
            json!(null),
        ] {
            assert!(
                failed_response(Protocol::OpenaiResponses, &body, None).is_none(),
                "{body}"
            );
        }
        // Only the Responses API reports failures this way.
        let failed = failed_body(json!({"code": "rate_limit_exceeded", "message": "x"}));
        for protocol in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
            assert!(
                failed_response(protocol, &failed, None).is_none(),
                "{protocol}"
            );
        }
    }

    #[test]
    fn a_declared_failure_is_one_whatever_the_body_still_carries() {
        use serde_json::json;
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [],
            "encrypted_content": "gAAAAABo-encrypted"});
        let partial = json!({"type": "message", "id": "msg_1", "role": "assistant",
            "status": "incomplete",
            "content": [{"type": "output_text", "text": "The answer is", "annotations": []}]});
        let searched = json!({"type": "file_search_call", "id": "fs_1", "status": "completed",
            "queries": ["q"]});
        for (output, code, class, status) in [
            (
                json!([reasoning]),
                "rate_limit_exceeded",
                FailureClass::RateLimit,
                429,
            ),
            (json!([partial]), "server_error", FailureClass::Server, 502),
            (
                json!([searched]),
                "vector_store_timeout",
                FailureClass::Server,
                502,
            ),
            (
                json!([reasoning, partial]),
                "context_length_exceeded",
                FailureClass::Request,
                400,
            ),
            (json!([]), "insufficient_quota", FailureClass::Quota, 429),
        ] {
            let mut body = failed_body(json!({"code": code, "message": "It went wrong."}));
            body["output"] = output;
            let failure = declared_failure(Protocol::OpenaiResponses, &body, None)
                .unwrap_or_else(|| panic!("{code}: not recognised"));
            assert_eq!((failure.class, failure.status), (class, status), "{code}");
            assert_eq!(failure.info.message, "It went wrong.", "{code}");
            assert_eq!(failure.info.code.as_deref(), Some(code));

            // Also as the terminal stream event some upstreams answer with,
            // and however the status is spelled.
            let wrapped = json!({"type": "response.failed", "response": body.clone()});
            let failure = declared_failure(Protocol::OpenaiResponses, &wrapped, None).unwrap();
            assert_eq!(failure.class, class, "{code}, wrapped");
            body["status"] = json!(" Failed ");
            assert!(declared_failure(Protocol::OpenaiResponses, &body, None).is_some());

            // Only the Responses API reports failures this way.
            for protocol in [Protocol::OpenaiChat, Protocol::Anthropic, Protocol::Gemini] {
                assert!(declared_failure(protocol, &body, None).is_none());
            }
        }
        // A failure that does not say why is still one.
        let silent = json!({"object": "response", "status": "failed", "output": [reasoning]});
        let failure = declared_failure(Protocol::OpenaiResponses, &silent, None).unwrap();
        assert_eq!((failure.class, failure.status), (FailureClass::Server, 502));
    }

    #[test]
    fn a_body_that_does_not_say_failed_declares_nothing() {
        use serde_json::json;
        let message = json!({"type": "message", "id": "msg_1", "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Done.", "annotations": []}]});
        let error = json!({"code": "rate_limit_exceeded", "message": "Slow down."});
        for body in [
            // An answer is an answer, whatever else is in the body: this is
            // decided before the body is decoded.
            json!({"object": "response", "status": "completed", "output": [message],
                   "error": error}),
            json!({"object": "response", "output": [message], "error": error}),
            json!({"object": "response", "status": "incomplete", "output": [message],
                   "incomplete_details": {"reason": "max_output_tokens"}, "error": error}),
            json!({"object": "response", "status": "cancelled", "output": [], "error": error}),
            // `failed_response` reads this one as a failure; it is for
            // bodies the decoder has refused.
            json!({"object": "response", "output": [], "error": error}),
            json!({"object": "response", "status": 500, "output": []}),
            json!({"status": ["failed"]}),
            json!("failed"),
            json!(null),
        ] {
            assert!(
                declared_failure(Protocol::OpenaiResponses, &body, None).is_none(),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn bodies_beyond_the_limit_are_refused() {
        let cancel = CancellationToken::new();
        let chunks = |n: usize| {
            UpstreamBody::Stream(
                futures::stream::iter((0..n).map(|_| Ok(Bytes::from(vec![b'x'; 1000])))).boxed(),
            )
        };
        let ok = read_limited(chunks(3), 3000, &cancel).await;
        assert!(matches!(ok, Ok(bytes) if bytes.len() == 3000));
        assert!(matches!(
            read_limited(chunks(4), 3000, &cancel).await,
            Err(ReadError::TooLarge)
        ));
        assert!(matches!(
            read_limited(UpstreamBody::Full(Bytes::from(vec![0u8; 10])), 9, &cancel).await,
            Err(ReadError::TooLarge)
        ));

        let failing = UpstreamBody::Stream(
            futures::stream::iter(vec![
                Ok(Bytes::from_static(b"ab")),
                Err(UpstreamError::transport("read: cut short")),
            ])
            .boxed(),
        );
        assert!(matches!(
            read_limited(failing, 100, &cancel).await,
            Err(ReadError::Upstream(error)) if error.info.message == "read: cut short"
        ));

        cancel.cancel();
        let never = UpstreamBody::Stream(futures::stream::pending().boxed());
        assert!(matches!(
            read_limited(never, 100, &cancel).await,
            Err(ReadError::Cancelled)
        ));
    }
}
