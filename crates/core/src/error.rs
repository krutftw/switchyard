//! Error types shared across the gateway.
//!
//! * [`ApiError`] is what a *client* is told. Each codec renders it in its own
//!   protocol's error envelope.
//! * [`CodecError`] is a failure to decode or encode a payload.
//! * [`UpstreamError`] describes a failed upstream call in enough detail for
//!   the scheduler to decide whether to retry, switch credential or give up.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Coarse classification of a client-visible error. Codecs map it to their
/// protocol's error `type` / `status` strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Malformed or semantically invalid request (400).
    InvalidRequest,
    /// Missing or invalid client credentials (401).
    Authentication,
    /// Authenticated but not allowed (403).
    Permission,
    /// Unknown model, route or resource (404).
    NotFound,
    /// Request body too large (413).
    TooLarge,
    /// Rate limited or out of quota (429).
    RateLimit,
    /// The upstream answered with an error that is not the client's fault (502).
    Upstream,
    /// No usable upstream right now: all credentials cooling down, provider
    /// overloaded (503).
    Unavailable,
    /// Upstream or gateway timeout (504).
    Timeout,
    /// A bug or unexpected condition inside the gateway (500).
    Internal,
}

impl ErrorKind {
    /// Default HTTP status for this kind.
    pub const fn status(self) -> u16 {
        match self {
            ErrorKind::InvalidRequest => 400,
            ErrorKind::Authentication => 401,
            ErrorKind::Permission => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::TooLarge => 413,
            ErrorKind::RateLimit => 429,
            ErrorKind::Internal => 500,
            ErrorKind::Upstream => 502,
            ErrorKind::Unavailable => 503,
            ErrorKind::Timeout => 504,
        }
    }

    /// Best-effort classification of an HTTP status.
    pub const fn from_status(status: u16) -> ErrorKind {
        match status {
            400 | 422 => ErrorKind::InvalidRequest,
            401 => ErrorKind::Authentication,
            402 | 403 => ErrorKind::Permission,
            404 => ErrorKind::NotFound,
            408 | 504 => ErrorKind::Timeout,
            413 => ErrorKind::TooLarge,
            429 => ErrorKind::RateLimit,
            500 => ErrorKind::Internal,
            503 | 529 => ErrorKind::Unavailable,
            s if s >= 500 => ErrorKind::Upstream,
            s if s >= 400 => ErrorKind::InvalidRequest,
            _ => ErrorKind::Internal,
        }
    }
}

/// An error to be reported to the client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    /// HTTP status to answer with.
    pub status: u16,
    pub kind: ErrorKind,
    /// Human readable description. Never contains credentials.
    pub message: String,
    /// Machine readable code (OpenAI `error.code`), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Offending parameter (OpenAI `error.param`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Seconds the client should wait before retrying; rendered as a
    /// `Retry-After` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        ApiError {
            status: kind.status(),
            kind,
            message: message.into(),
            code: None,
            param: None,
            retry_after_secs: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn with_retry_after(mut self, wait: Duration) -> Self {
        self.retry_after_secs = Some(wait.as_secs().max(1));
        self
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::InvalidRequest, message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Authentication, message)
    }

    pub fn permission(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Permission, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::NotFound, message)
    }

    pub fn rate_limit(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::RateLimit, message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Upstream, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Unavailable, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Timeout, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::new(ErrorKind::Internal, message)
    }

    /// `model_not_found`-style error for an unroutable model name.
    pub fn unknown_model(model: &str) -> Self {
        ApiError::new(
            ErrorKind::NotFound,
            format!("unknown model `{model}`: no configured provider serves it"),
        )
        .with_code("model_not_found")
        .with_param("model")
    }
}

impl From<CodecError> for ApiError {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::InvalidRequest { message, param } => {
                let mut err = ApiError::invalid_request(message);
                err.param = param;
                err
            }
            CodecError::Unsupported(message) => ApiError::invalid_request(message),
            CodecError::InvalidUpstream(message) => ApiError::upstream(format!(
                "upstream returned an unexpected payload: {message}"
            )),
        }
    }
}

/// A payload could not be decoded or encoded.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CodecError {
    /// The client's request is malformed for its protocol.
    #[error("{message}")]
    InvalidRequest {
        message: String,
        /// JSON path of the offending field, when known.
        param: Option<String>,
    },
    /// The request uses a feature that cannot be expressed in the target
    /// protocol and cannot be safely dropped.
    #[error("{0}")]
    Unsupported(String),
    /// An upstream response did not look like its protocol says it should.
    #[error("{0}")]
    InvalidUpstream(String),
}

impl CodecError {
    pub fn invalid(message: impl Into<String>) -> Self {
        CodecError::InvalidRequest {
            message: message.into(),
            param: None,
        }
    }

    pub fn invalid_param(param: impl Into<String>, message: impl Into<String>) -> Self {
        CodecError::InvalidRequest {
            message: message.into(),
            param: Some(param.into()),
        }
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        CodecError::InvalidUpstream(message.into())
    }
}

/// What a codec could extract from an upstream error response body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpstreamErrorInfo {
    /// Provider's message, or a trimmed excerpt of the raw body.
    pub message: String,
    /// Provider error type/status string (`rate_limit_error`,
    /// `RESOURCE_EXHAUSTED`, `insufficient_quota`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// Provider error code, when distinct from the type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Retry delay stated *inside the body* (Gemini `RetryInfo.retryDelay`,
    /// "try again in 3.2s" messages), in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// How the scheduler should treat a failed upstream call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The request itself is bad (400, 413, 422): retrying anywhere is
    /// pointless and the credential is not at fault.
    Request,
    /// The credential is rejected (401, 403 auth failures): take it out of
    /// rotation for a long time.
    Auth,
    /// The account is out of money or quota (402, quota-type 429).
    Quota,
    /// Short-term rate limit (429).
    RateLimit,
    /// The model is not available on this credential (404).
    ModelNotFound,
    /// Server-side fault (5xx, 529 overloaded).
    Server,
    /// Connect/read failure or timeout before a response arrived.
    Transport,
}

impl FailureClass {
    /// Whether trying a different credential could succeed.
    pub const fn should_failover(self) -> bool {
        !matches!(self, FailureClass::Request)
    }

    /// Default classification from an HTTP status alone.
    pub const fn from_status(status: u16) -> FailureClass {
        match status {
            401 => FailureClass::Auth,
            402 => FailureClass::Quota,
            403 => FailureClass::Auth,
            404 => FailureClass::ModelNotFound,
            408 => FailureClass::Transport,
            429 => FailureClass::RateLimit,
            s if s >= 500 => FailureClass::Server,
            _ => FailureClass::Request,
        }
    }
}

/// A failed upstream call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("upstream error (status {status}): {}", .info.message)]
pub struct UpstreamError {
    /// HTTP status, or `0` when no response was received.
    pub status: u16,
    pub class: FailureClass,
    pub info: UpstreamErrorInfo,
    /// Wait requested by the upstream (`Retry-After` header or body hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Raw response body (possibly truncated), for same-protocol passthrough
    /// of the error to the client and for request logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `Content-Type` of the raw body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl UpstreamError {
    /// A transport-level failure (no HTTP response).
    pub fn transport(message: impl Into<String>) -> Self {
        UpstreamError {
            status: 0,
            class: FailureClass::Transport,
            info: UpstreamErrorInfo {
                message: message.into(),
                ..UpstreamErrorInfo::default()
            },
            retry_after_ms: None,
            body: None,
            content_type: None,
        }
    }

    /// Converts to the error shown to the client when no retry is possible.
    pub fn to_api_error(&self) -> ApiError {
        let (kind, status) = match self.class {
            FailureClass::Request => (ErrorKind::InvalidRequest, self.status.max(400)),
            FailureClass::Auth => (ErrorKind::Upstream, 502),
            FailureClass::Quota | FailureClass::RateLimit => (ErrorKind::RateLimit, 429),
            FailureClass::ModelNotFound => (ErrorKind::NotFound, 404),
            FailureClass::Server => {
                if self.status == 503 || self.status == 529 {
                    (ErrorKind::Unavailable, 503)
                } else {
                    (ErrorKind::Upstream, 502)
                }
            }
            FailureClass::Transport => {
                if self.status == 408 {
                    (ErrorKind::Timeout, 504)
                } else {
                    (ErrorKind::Upstream, 502)
                }
            }
        };
        let mut err = ApiError::new(kind, self.info.message.clone()).with_status(status);
        err.code = self
            .info
            .code
            .clone()
            .or_else(|| self.info.error_type.clone());
        if let Some(ms) = self.retry_after_ms {
            err.retry_after_secs = Some(ms.div_ceil(1000).max(1));
        }
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_status_mapping() {
        assert_eq!(ErrorKind::RateLimit.status(), 429);
        assert_eq!(ErrorKind::from_status(429), ErrorKind::RateLimit);
        assert_eq!(ErrorKind::from_status(529), ErrorKind::Unavailable);
        assert_eq!(ErrorKind::from_status(418), ErrorKind::InvalidRequest);
        assert_eq!(ErrorKind::from_status(502), ErrorKind::Upstream);
    }

    #[test]
    fn failure_class_from_status() {
        assert_eq!(FailureClass::from_status(400), FailureClass::Request);
        assert_eq!(FailureClass::from_status(401), FailureClass::Auth);
        assert_eq!(FailureClass::from_status(429), FailureClass::RateLimit);
        assert_eq!(FailureClass::from_status(529), FailureClass::Server);
        assert!(!FailureClass::Request.should_failover());
        assert!(FailureClass::Server.should_failover());
    }

    #[test]
    fn upstream_error_to_api_error() {
        let mut e = UpstreamError::transport("connection reset");
        assert_eq!(e.to_api_error().status, 502);
        e.status = 429;
        e.class = FailureClass::RateLimit;
        e.retry_after_ms = Some(1500);
        let api = e.to_api_error();
        assert_eq!(api.status, 429);
        assert_eq!(api.retry_after_secs, Some(2));
        // An upstream auth failure is the gateway's problem, not the client's.
        e.status = 401;
        e.class = FailureClass::Auth;
        assert_eq!(e.to_api_error().status, 502);
    }

    #[test]
    fn codec_error_becomes_400() {
        let api: ApiError = CodecError::invalid_param("messages", "must not be empty").into();
        assert_eq!(api.status, 400);
        assert_eq!(api.param.as_deref(), Some("messages"));
    }
}
