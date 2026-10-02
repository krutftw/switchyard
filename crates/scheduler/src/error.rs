//! Why no credential could be picked.

use std::time::Duration;
use switchyard_core::ApiError;
use switchyard_core::util::truncate_chars;

/// Longest upstream error excerpt repeated to a client.
const LAST_ERROR_MAX_CHARS: usize = 256;

/// A request could not be routed to a credential.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    /// No configured provider serves this model name.
    #[error("unknown model `{model}`: no configured provider serves it")]
    UnknownModel { model: String },
    /// The model is known but no usable credential is configured for it
    /// (none at all, or all disabled / missing their secret).
    #[error("no usable credential is configured for model `{model}`")]
    NoCredentials { model: String },
    /// Every credential that could serve the model is resting after a
    /// failure. `retry_after` is the time until the soonest one recovers.
    #[error("all credentials for model `{model}` are cooling down")]
    CoolingDown {
        model: String,
        retry_after: Duration,
        /// Short description of the most recent upstream failure among those
        /// that put these credentials to rest for this model
        /// (`"429 rate limit exceeded"`). Never an error about another model.
        last_error: Option<String>,
    },
    /// Everything that could serve the request has already been attempted
    /// for it: each candidate credential is in the tried list and has no
    /// model left that the request has not tried on it.
    #[error("every credential for model `{model}` has been tried")]
    Exhausted { model: String },
}

impl PickError {
    /// The model name the error is about.
    pub fn model(&self) -> &str {
        match self {
            PickError::UnknownModel { model }
            | PickError::NoCredentials { model }
            | PickError::CoolingDown { model, .. }
            | PickError::Exhausted { model } => model,
        }
    }

    /// The wait to advertise to the client, when the condition is temporary.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            PickError::CoolingDown { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

impl From<PickError> for ApiError {
    /// * `UnknownModel` → 404 `model_not_found`;
    /// * `NoCredentials` → 503;
    /// * `CoolingDown` → 429 with `Retry-After` (seconds, rounded up, at least 1);
    /// * `Exhausted` → 503.
    fn from(error: PickError) -> Self {
        match error {
            PickError::UnknownModel { model } => ApiError::unknown_model(&model),
            PickError::NoCredentials { model } => ApiError::unavailable(format!(
                "no usable credential is configured for model `{model}`"
            ))
            .with_code("no_credentials")
            .with_param("model"),
            PickError::CoolingDown {
                model,
                retry_after,
                last_error,
            } => {
                let secs = u64::try_from(retry_after.as_millis().div_ceil(1000))
                    .unwrap_or(u64::MAX)
                    .max(1);
                let mut message = format!(
                    "all credentials for model `{model}` are cooling down; retry in {secs}s"
                );
                if let Some(last) = last_error.filter(|l| !l.trim().is_empty()) {
                    message.push_str(" (last upstream error: ");
                    message.push_str(&truncate_chars(last.trim(), LAST_ERROR_MAX_CHARS));
                    message.push(')');
                }
                ApiError::rate_limit(message)
                    .with_code("model_cooldown")
                    .with_retry_after(Duration::from_secs(secs))
            }
            PickError::Exhausted { model } => ApiError::unavailable(format!(
                "every credential for model `{model}` failed for this request"
            ))
            .with_code("credentials_exhausted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::ErrorKind;

    #[test]
    fn unknown_model_is_404_model_not_found() {
        let api: ApiError = PickError::UnknownModel {
            model: "nope".into(),
        }
        .into();
        assert_eq!(api.status, 404);
        assert_eq!(api.kind, ErrorKind::NotFound);
        assert_eq!(api.code.as_deref(), Some("model_not_found"));
        assert_eq!(api.param.as_deref(), Some("model"));
        assert!(api.message.contains("nope"));
        assert_eq!(api.retry_after_secs, None);
    }

    #[test]
    fn no_credentials_is_503() {
        let api: ApiError = PickError::NoCredentials { model: "m".into() }.into();
        assert_eq!(api.status, 503);
        assert_eq!(api.kind, ErrorKind::Unavailable);
        assert_eq!(api.retry_after_secs, None);
    }

    #[test]
    fn cooling_down_is_429_with_retry_after_rounded_up() {
        let api: ApiError = PickError::CoolingDown {
            model: "m".into(),
            retry_after: Duration::from_millis(12_001),
            last_error: Some("429 slow down".into()),
        }
        .into();
        assert_eq!(api.status, 429);
        assert_eq!(api.kind, ErrorKind::RateLimit);
        assert_eq!(api.retry_after_secs, Some(13));
        assert_eq!(api.code.as_deref(), Some("model_cooldown"));
        assert!(api.message.contains("429 slow down"));

        let api: ApiError = PickError::CoolingDown {
            model: "m".into(),
            retry_after: Duration::ZERO,
            last_error: None,
        }
        .into();
        assert_eq!(api.retry_after_secs, Some(1));
    }

    #[test]
    fn exhausted_is_503() {
        let err = PickError::Exhausted { model: "m".into() };
        assert_eq!(err.model(), "m");
        assert_eq!(err.retry_after(), None);
        let api: ApiError = err.into();
        assert_eq!(api.status, 503);
    }

    #[test]
    fn long_upstream_errors_are_truncated() {
        let api: ApiError = PickError::CoolingDown {
            model: "m".into(),
            retry_after: Duration::from_secs(1),
            last_error: Some("x".repeat(5000)),
        }
        .into();
        assert!(api.message.chars().count() < 400);
    }
}
