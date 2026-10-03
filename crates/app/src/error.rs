use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use switchyard_agent::AgentError;
use switchyard_agent_adapters::AdapterError;

/// A safe local-app error; credentials and raw provider responses are never added here.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub(crate) fn local(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "local_io", message)
    }
}

impl From<AgentError> for AppError {
    fn from(error: AgentError) -> Self {
        let status = match &error {
            AgentError::Invalid(_) => StatusCode::BAD_REQUEST,
            AgentError::NotFound(_) => StatusCode::NOT_FOUND,
            AgentError::Conflict(_) | AgentError::Interrupted => StatusCode::CONFLICT,
            AgentError::Permission(_) => StatusCode::FORBIDDEN,
            AgentError::Gateway(_) => StatusCode::BAD_GATEWAY,
            AgentError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AgentError::Limit(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };
        let message = if matches!(&error, AgentError::Storage(_)) {
            "Local session storage is unavailable; another host may be using this data directory."
                .to_owned()
        } else {
            error.to_string()
        };
        Self::new(status, error.kind(), message)
    }
}

impl From<AdapterError> for AppError {
    fn from(error: AdapterError) -> Self {
        let status = match &error {
            AdapterError::Invalid(_) => StatusCode::BAD_REQUEST,
            AdapterError::NotFound(_) => StatusCode::NOT_FOUND,
            AdapterError::Conflict(_) => StatusCode::CONFLICT,
            AdapterError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AdapterError::Protocol(_) => StatusCode::BAD_GATEWAY,
            AdapterError::Limit(_) => StatusCode::TOO_MANY_REQUESTS,
        };
        Self::new(status, error.kind(), error.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}
