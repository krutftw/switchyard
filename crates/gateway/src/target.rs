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

/// Classifies an error that arrived *inside* a stream — an upstream error
/// event, or the error the transcoder made up for a broken stream — the way
/// an HTTP error of the same meaning would be classified, so the scheduler
/// rests the credential appropriately. Credentials the upstream may have
/// quoted are removed from the message.
pub(crate) fn stream_failure(error: &ApiError, target: Option<&Target>) -> UpstreamError {
    let code = error.code.as_deref().unwrap_or("").to_ascii_lowercase();
    let request_fault = REQUEST_FAULT_STREAM_CODES.contains(&code.as_str());
    let class = match error.kind {
        _ if request_fault => FailureClass::Request,
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
        // the unknown code for a server failure.
        status: if request_fault && !(400..500).contains(&error.status) {
            400
        } else {
            error.status
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
    async fn service_account(&self, file: &str) -> Result<Arc<ServiceAccount>, UpstreamError> {
        let path: PathBuf = self.store.resolve_path(file);
        if let Some(account) = self.service_accounts.lock().get(&path) {
            return Ok(Arc::clone(account));
        }
        let shown = PathBuf::from(file)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string());
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
        self.service_accounts
            .lock()
            .insert(path, Arc::clone(&account));
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
