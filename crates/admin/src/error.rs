//! The one error shape of the admin API:
//! `{"error":{"message":"…","issues":[{"path":"…","message":"…"}]?}}`.

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::header::{HeaderValue, RETRY_AFTER, WWW_AUTHENTICATE};
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use serde_json::{Map, Value, json};
use switchyard_config_store::ConfigStoreError;
use switchyard_core::config::ConfigIssue;
use switchyard_core::{ApiError, ErrorKind};

/// An admin API failure: a status, a sentence for the operator and, for
/// validation failures, the fields at fault.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ApiFailure {
    pub status: StatusCode,
    pub message: String,
    pub issues: Vec<ConfigIssue>,
    /// Rendered as a `Retry-After` header (seconds).
    pub retry_after_secs: Option<u64>,
}

impl ApiFailure {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiFailure {
            status,
            message: message.into(),
            issues: Vec::new(),
            retry_after_secs: None,
        }
    }

    /// 400: the request itself is malformed.
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::BAD_REQUEST, message)
    }

    /// 400 naming the offending field of the request body.
    pub fn bad_field(path: impl Into<String>, message: impl Into<String>) -> Self {
        let issue = ConfigIssue {
            path: path.into(),
            message: message.into(),
        };
        ApiFailure {
            status: StatusCode::BAD_REQUEST,
            message: format!("invalid request: {issue}"),
            issues: vec![issue],
            retry_after_secs: None,
        }
    }

    /// 400 for a query parameter that is repeated or holds a value the route
    /// refuses. The message names the parameter, and so does the issue's
    /// path (`since`), the way a body field is named.
    pub fn bad_query(issue: ConfigIssue) -> Self {
        let message = if issue.path.is_empty() {
            format!("invalid query string: {}", issue.message)
        } else {
            format!(
                "invalid query parameter `{}`: {}",
                issue.path, issue.message
            )
        };
        ApiFailure {
            status: StatusCode::BAD_REQUEST,
            message,
            issues: vec![issue],
            retry_after_secs: None,
        }
    }

    /// 400 naming several fields of the request body. The message leads
    /// with `lead` and quotes the first issues, so it says what is wrong on
    /// its own.
    pub fn bad_fields(lead: &str, issues: Vec<ConfigIssue>) -> Self {
        ApiFailure {
            status: StatusCode::BAD_REQUEST,
            message: summary(lead, &issues),
            issues,
            retry_after_secs: None,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::CONFLICT, message)
    }

    /// 409 for a value of the request body that is already taken: `message`
    /// is the sentence, and the one issue names the field (`path`) so a
    /// form can put `said` next to it without reading the sentence.
    pub fn conflict_on(
        path: impl Into<String>,
        said: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        ApiFailure {
            issues: vec![ConfigIssue {
                path: path.into(),
                message: said.into(),
            }],
            ..ApiFailure::conflict(message)
        }
    }

    /// 409: the edit is fine, but the configuration file on disk holds
    /// something that is not valid — a manual edit in progress — which
    /// saving the edit would overwrite. `issues` are the file's, by their
    /// place in the file.
    pub fn disk_invalid(issues: Vec<ConfigIssue>) -> Self {
        ApiFailure {
            status: StatusCode::CONFLICT,
            message: summary(
                "the configuration file on disk is not valid, so the change was not saved \
                 (the file was left as it is); fix or restore the file, or replace it as a \
                 whole on the raw tab (PUT /config/raw). What is wrong with the file",
                &issues,
            ),
            issues,
            retry_after_secs: None,
        }
    }

    /// Makes the issue paths of a 422 relative to the part of the
    /// configuration a request is about: with `scope` `providers[3]`, the
    /// path `providers[3].headers.X-Team` becomes `headers.X-Team`, the
    /// field's place in the request body. Issues about anything else keep
    /// their place in the whole configuration, and failures other than a
    /// 422 (whose issues, if any, are already about the body, or about the
    /// file) are returned as they are.
    pub fn relative_to(mut self, scope: &str) -> Self {
        if self.status != StatusCode::UNPROCESSABLE_ENTITY || scope.is_empty() {
            return self;
        }
        for issue in &mut self.issues {
            let Some(rest) = issue.path.strip_prefix(scope) else {
                continue;
            };
            // `providers[3]` is no prefix of `providers[30]`.
            if rest.is_empty() || rest.starts_with('[') {
                issue.path = rest.to_string();
            } else if let Some(rest) = rest.strip_prefix('.') {
                issue.path = rest.to_string();
            }
        }
        self.message = summary("the configuration is not valid", &self.issues);
        self
    }

    /// 422: the request was understood but the configuration it would
    /// produce is not valid.
    pub fn invalid_config(issues: Vec<ConfigIssue>) -> Self {
        ApiFailure {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: summary("the configuration is not valid", &issues),
            issues,
            retry_after_secs: None,
        }
    }

    pub fn too_many_attempts(retry_after_secs: u64) -> Self {
        ApiFailure {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!(
                "too many failed sign-in attempts from this address; try again in {}",
                human_wait(retry_after_secs)
            ),
            issues: Vec::new(),
            retry_after_secs: Some(retry_after_secs.max(1)),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// The JSON body.
    pub fn body(&self) -> Value {
        let mut error = Map::new();
        error.insert("message".to_string(), Value::String(self.message.clone()));
        if !self.issues.is_empty() {
            error.insert("issues".to_string(), json!(self.issues));
        }
        json!({ "error": error })
    }
}

/// `"<lead>: <first issues>"`, so the message alone says what is wrong.
fn summary(lead: &str, issues: &[ConfigIssue]) -> String {
    const SHOWN: usize = 3;
    let mut text = lead.to_string();
    for (index, issue) in issues.iter().take(SHOWN).enumerate() {
        text.push_str(if index == 0 { ": " } else { "; " });
        // An issue about the request body as a whole has no path.
        if issue.path.is_empty() {
            text.push_str(&issue.message);
        } else {
            text.push_str(&issue.to_string());
        }
    }
    if issues.len() > SHOWN {
        text.push_str(&format!(" (and {} more)", issues.len() - SHOWN));
    }
    text
}

/// "29 minutes", "45 seconds".
fn human_wait(secs: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if secs >= 120 {
        plural(secs.div_ceil(60), "minute")
    } else {
        plural(secs.max(1), "second")
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(secs) = self.retry_after_secs {
            headers.insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        if self.status == StatusCode::UNAUTHORIZED {
            headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        (self.status, headers, Json(self.body())).into_response()
    }
}

impl From<ConfigStoreError> for ApiFailure {
    fn from(error: ConfigStoreError) -> Self {
        match error {
            ConfigStoreError::Invalid(issues) => ApiFailure::invalid_config(issues),
            // The message names the file and the operating system's reason,
            // never file content.
            ConfigStoreError::Io { .. } => ApiFailure::internal(format!(
                "the configuration file could not be read or written: {error}"
            )),
            ConfigStoreError::Edit(message) => {
                ApiFailure::internal(format!("the configuration could not be written: {message}"))
            }
            // The edit is fine; the file on disk holds a broken manual edit
            // that it would overwrite. The issues say what is wrong with
            // the file (fields and rules, never values).
            ConfigStoreError::DiskInvalid(issues) => ApiFailure::disk_invalid(issues),
        }
    }
}

impl ApiFailure {
    /// An upstream that the admin API asked on the operator's behalf did
    /// not deliver: 502, or 504 when it did not answer in time.
    ///
    /// The upstream's own status is deliberately not passed on. Inside the
    /// admin API those statuses mean something else — a 429 with
    /// `Retry-After` is the sign-in lockout, a 404 an unknown provider, a
    /// 422 a configuration with `issues`, a 400 a malformed admin request —
    /// and the operator's request was none of these. What the upstream said
    /// (already stripped of credentials by the gateway) goes into the
    /// message after `what`, a sentence fragment saying what was asked of it;
    /// a wait it asked for is mentioned there too, not sent as a
    /// `Retry-After` header.
    pub fn upstream(what: &str, error: &ApiError) -> Self {
        let status = if error.kind == ErrorKind::Timeout {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_GATEWAY
        };
        let mut message = what.to_string();
        if let Some(secs) = error.retry_after_secs {
            message.push_str(&format!(" (it asks to wait {})", human_wait(secs)));
        }
        let said = error.message.trim();
        if !said.is_empty() {
            message.push_str(": ");
            message.push_str(said);
        }
        ApiFailure::new(status, message)
    }
}

/// What every handler returns.
pub(crate) type ApiResult<T = Response> = Result<T, ApiFailure>;

/// A 200 with a JSON body.
pub(crate) fn ok_json<T: Serialize>(value: &T) -> ApiResult {
    json_with_status(StatusCode::OK, value)
}

/// A JSON body with the given status.
pub(crate) fn json_with_status<T: Serialize>(status: StatusCode, value: &T) -> ApiResult {
    match serde_json::to_value(value) {
        Ok(body) => Ok((status, Json(body)).into_response()),
        Err(error) => {
            tracing::error!(%error, "an admin API response could not be serialised");
            Err(ApiFailure::internal("the response could not be serialised"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn issue(path: &str, message: &str) -> ConfigIssue {
        ConfigIssue {
            path: path.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn body_has_issues_only_when_there_are_some() {
        assert_eq!(
            ApiFailure::not_found("no such provider").body(),
            json!({"error": {"message": "no such provider"}})
        );
        let failure = ApiFailure::invalid_config(vec![issue("server.port", "must be 1-65535")]);
        assert_eq!(failure.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            failure.body(),
            json!({"error": {
                "message": "the configuration is not valid: server.port: must be 1-65535",
                "issues": [{"path": "server.port", "message": "must be 1-65535"}],
            }})
        );
    }

    #[test]
    fn long_issue_lists_are_summarised() {
        let issues: Vec<ConfigIssue> = (0..5).map(|i| issue(&format!("f{i}"), "bad")).collect();
        let failure = ApiFailure::invalid_config(issues);
        assert_eq!(
            failure.message,
            "the configuration is not valid: f0: bad; f1: bad; f2: bad (and 2 more)"
        );
        assert_eq!(failure.issues.len(), 5);
    }

    #[test]
    fn lockout_carries_retry_after() {
        let response = ApiFailure::too_many_attempts(1800).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "1800");
        assert_eq!(human_wait(1800), "30 minutes");
        assert_eq!(human_wait(119), "119 seconds");
        assert_eq!(human_wait(1), "1 second");
        assert_eq!(human_wait(0), "1 second");
    }

    #[test]
    fn unauthorized_names_the_scheme() {
        let response = ApiFailure::unauthorized("nope").into_response();
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
    }

    #[test]
    fn store_and_gateway_errors_map_to_statuses() {
        let invalid: ApiFailure = ConfigStoreError::Invalid(vec![issue("a", "b")]).into();
        assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
        let edit: ApiFailure = ConfigStoreError::Edit("boom".into()).into();
        assert_eq!(edit.status, StatusCode::INTERNAL_SERVER_ERROR);

        // A broken file on disk: a conflict that says what to do about it
        // and what is wrong with the file.
        let disk: ApiFailure = ConfigStoreError::DiskInvalid(vec![
            issue("line 3, column 9", "invalid number"),
            issue(
                "providers[1].base_url",
                "must start with http:// or https://",
            ),
        ])
        .into();
        assert_eq!(disk.status, StatusCode::CONFLICT);
        assert_eq!(disk.issues.len(), 2);
        assert_eq!(disk.issues[0].path, "line 3, column 9");
        for part in [
            "on disk is not valid",
            "was not saved",
            "fix or restore the file",
            "PUT /config/raw",
            "line 3, column 9: invalid number",
        ] {
            assert!(disk.message.contains(part), "{part}: {}", disk.message);
        }
        // Those are the file's issues: they are never made relative to a
        // request body.
        assert_eq!(disk.clone().relative_to("providers[1]"), disk);
    }

    #[test]
    fn a_taken_value_names_its_field() {
        let failure = ApiFailure::conflict_on(
            "name",
            "is already used by another provider",
            "a provider named `mock` already exists",
        );
        assert_eq!(
            failure.body(),
            json!({"error": {
                "message": "a provider named `mock` already exists",
                "issues": [{"path": "name", "message": "is already used by another provider"}],
            }})
        );
        assert_eq!(failure.status, StatusCode::CONFLICT);
    }

    #[test]
    fn issue_paths_become_relative_to_the_edited_part() {
        let failure = ApiFailure::invalid_config(vec![
            issue("providers[3].headers.X Team", "bad name"),
            issue("providers[3].credentials[1].api_key", "empty reference"),
            issue("providers[30].name", "duplicate"),
            issue("server.port", "must be between 1 and 65535"),
        ])
        .relative_to("providers[3]");
        let paths: Vec<&str> = failure.issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "headers.X Team",
                "credentials[1].api_key",
                "providers[30].name",
                "server.port"
            ]
        );
        assert_eq!(
            failure.message,
            "the configuration is not valid: headers.X Team: bad name; \
             credentials[1].api_key: empty reference; providers[30].name: duplicate (and 1 more)"
        );

        // A list is the body: its entries are `[i]…`.
        let aliases = ApiFailure::invalid_config(vec![
            issue("aliases[0].targets[1]", "must not be empty"),
            issue("aliases", "too many"),
        ])
        .relative_to("aliases");
        assert_eq!(aliases.issues[0].path, "[0].targets[1]");
        assert_eq!(aliases.issues[1].path, "");
        assert_eq!(
            aliases.message,
            "the configuration is not valid: [0].targets[1]: must not be empty; too many"
        );

        // Only a 422 is about the configuration.
        let bad = ApiFailure::bad_field("providers[3].name", "is required");
        assert_eq!(bad.clone().relative_to("providers[3]"), bad);
        let invalid = ApiFailure::invalid_config(vec![issue("a.b", "c")]);
        assert_eq!(invalid.clone().relative_to(""), invalid);
    }

    #[test]
    fn upstream_failures_never_borrow_an_admin_status() {
        // Whatever class the gateway gave the upstream's failure, the admin
        // API answers 502 — or 504 for a timeout — and never with the
        // lockout's `Retry-After`.
        let cases = [
            (ApiError::rate_limit("slow down"), StatusCode::BAD_GATEWAY),
            (ApiError::not_found("no /models"), StatusCode::BAD_GATEWAY),
            (
                ApiError::invalid_request("bad").with_status(422),
                StatusCode::BAD_GATEWAY,
            ),
            (ApiError::invalid_request("bad"), StatusCode::BAD_GATEWAY),
            (ApiError::unavailable("overloaded"), StatusCode::BAD_GATEWAY),
            (ApiError::upstream("boom"), StatusCode::BAD_GATEWAY),
            (ApiError::timeout("too slow"), StatusCode::GATEWAY_TIMEOUT),
        ];
        for (error, expected) in cases {
            let error = error.with_retry_after(std::time::Duration::from_secs(17));
            let failure = ApiFailure::upstream("the upstream did not list its models", &error);
            assert_eq!(failure.status, expected, "{error:?}");
            assert_eq!(failure.retry_after_secs, None, "{error:?}");
            assert!(failure.issues.is_empty());
            assert_eq!(
                failure.message,
                format!(
                    "the upstream did not list its models (it asks to wait 17 seconds): {}",
                    error.message
                )
            );
            let response = failure.into_response();
            assert!(!response.headers().contains_key(RETRY_AFTER));
        }

        // Nothing said, no wait asked for: the lead alone.
        let silent = ApiFailure::upstream("the upstream failed", &ApiError::upstream("  "));
        assert_eq!(silent.message, "the upstream failed");
    }
}
