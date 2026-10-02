//! The REST handlers, one module per area, and the extractors they share.
//!
//! Every extractor rejects with the admin error envelope, so a malformed
//! request never produces a framework-flavoured plain-text answer.

pub(crate) mod config;
pub(crate) mod keys;
pub(crate) mod models;
pub(crate) mod playground;
pub(crate) mod providers;
pub(crate) mod status;
pub(crate) mod usage;

use crate::error::ApiFailure;
use crate::shape;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use bytes::Bytes;
use http::StatusCode;
use http::request::Parts;
use serde::de::DeserializeOwned;
use switchyard_core::config::ConfigIssue;

/// Largest request body of the ordinary admin routes. A configuration file
/// is a few kilobytes; the playground, which may carry images, has its own
/// limit (`server.body_limit_mb`).
pub(crate) const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Reads a request body of at most `limit` bytes.
pub(crate) async fn read_body(request: Request, limit: usize) -> Result<Bytes, ApiFailure> {
    axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|error| {
            let too_large = std::error::Error::source(&error)
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>());
            if too_large {
                ApiFailure::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("the request body is larger than {limit} bytes"),
                )
            } else {
                ApiFailure::bad_request("the request body could not be read")
            }
        })
}

/// Parses a JSON document, naming the field a schema error is about.
pub(crate) fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiFailure> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiFailure::bad_request(
            "the request body is empty; a JSON document is expected",
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(schema_error)?;
    deserializer.end().map_err(|error| not_json(&error))?;
    Ok(value)
}

/// Converts a JSON value that was already parsed, naming the field a schema
/// error is about. `root` prefixes the path (`"routing"`).
pub(crate) fn from_json_value<T: DeserializeOwned>(
    value: serde_json::Value,
    root: &str,
) -> Result<T, ConfigIssue> {
    serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error.path().to_string();
        let mut issue = shape::describe(&path, &error.into_inner().to_string());
        issue.path = match (root.is_empty(), issue.path.is_empty()) {
            (_, true) => root.to_string(),
            (true, false) => issue.path,
            (false, false) => format!("{root}.{}", issue.path),
        };
        issue
    })
}

/// The 400 for a body that is not JSON at all.
fn not_json(error: &serde_json::Error) -> ApiFailure {
    ApiFailure::bad_request(format!(
        "the request body is not valid JSON: {}",
        shape::syntax(error)
    ))
}

/// The 400 for a body of the wrong shape: the field when one can be named,
/// and what is expected of it (see [`shape::describe`]).
pub(crate) fn shape_failure(issue: ConfigIssue) -> ApiFailure {
    if issue.path.is_empty() {
        ApiFailure::bad_request(format!("invalid request body: {}", issue.message))
    } else {
        ApiFailure::bad_field(issue.path, issue.message)
    }
}

fn schema_error(error: serde_path_to_error::Error<serde_json::Error>) -> ApiFailure {
    let path = error.path().to_string();
    let inner = error.into_inner();
    if inner.is_syntax() || inner.is_eof() {
        return not_json(&inner);
    }
    shape_failure(shape::describe(&path, &inner.to_string()))
}

/// A JSON request body. The `Content-Type` header is not required: the
/// admin API speaks nothing but JSON.
pub(crate) struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiFailure;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(request, BODY_LIMIT).await?;
        parse_json(&bytes).map(JsonBody)
    }
}

/// A JSON request body that may be left out entirely (`POST …/test`).
pub(crate) struct OptionalJson<T>(pub T);

impl<S, T> FromRequest<S> for OptionalJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Default,
{
    type Rejection = ApiFailure;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_body(request, BODY_LIMIT).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(OptionalJson(T::default()));
        }
        parse_json(&bytes).map(OptionalJson)
    }
}

/// The query string, deserialised.
pub(crate) struct Params<T>(pub T);

impl<S, T> FromRequestParts<S> for Params<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiFailure;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::try_from_uri(&parts.uri)
            .map(|Query(value)| Params(value))
            .map_err(|error| {
                // The rejection wraps the same kind of serde message a body
                // error has; its own lead-in says nothing an operator needs.
                let text = error.body_text();
                let said = text
                    .strip_prefix("Failed to deserialize query string: ")
                    .unwrap_or(&text);
                ApiFailure::bad_request(format!(
                    "invalid query string: {}",
                    shape::describe("", said).message
                ))
            })
    }
}

/// The one path parameter of a route (`{name}`, `{id}`), percent-decoded.
pub(crate) struct PathParam(pub String);

impl<S> FromRequestParts<S> for PathParam
where
    S: Send + Sync,
{
    type Rejection = ApiFailure;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<String>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| PathParam(value))
            .map_err(|_| ApiFailure::bad_request("the path is not valid"))
    }
}

/// Any path under `/admin/api` that is not a route.
pub(crate) async fn unknown_route() -> ApiFailure {
    ApiFailure::not_found("no such admin API route")
}

/// A route that exists, asked with a method it does not have.
pub(crate) async fn method_not_allowed() -> ApiFailure {
    ApiFailure::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "this admin API route does not support the request method",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
        #[serde(default)]
        nested: Nested,
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Nested {
        #[serde(default)]
        count: u8,
    }

    #[test]
    fn parse_errors_say_what_and_where() {
        let ok: Sample = parse_json(br#"{"name":"a","nested":{"count":3}}"#).unwrap();
        assert_eq!(ok.nested.count, 3);

        let empty = parse_json::<Sample>(b"  \n").unwrap_err();
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);
        assert!(empty.message.contains("empty"), "{}", empty.message);

        let syntax = parse_json::<Sample>(b"{\"name\": ").unwrap_err();
        assert!(
            syntax.message.contains("not valid JSON"),
            "{}",
            syntax.message
        );
        assert!(syntax.issues.is_empty());

        let trailing = parse_json::<Sample>(br#"{"name":"a"} x"#).unwrap_err();
        assert!(
            trailing.message.contains("not valid JSON"),
            "{}",
            trailing.message
        );

        // No parser position in the serde style, no Rust type name.
        for failure in [&syntax, &trailing] {
            assert!(
                !failure.message.contains(" at line "),
                "{}",
                failure.message
            );
            assert!(
                failure.message.contains("line 1, column "),
                "{}",
                failure.message
            );
        }

        let wrong = parse_json::<Sample>(br#"{"name":"a","nested":{"count":"x"}}"#).unwrap_err();
        assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
        assert_eq!(wrong.issues.len(), 1);
        assert_eq!(wrong.issues[0].path, "nested.count");
        assert_eq!(
            wrong.message,
            "invalid request: nested.count: expected a whole number from 0 to 255, got a string"
        );

        let missing = parse_json::<Sample>(br#"{}"#).unwrap_err();
        assert_eq!(missing.message, "invalid request: name: is required");
        assert_eq!(missing.issues[0].path, "name");

        let unknown = parse_json::<Sample>(br#"{"name":"a","extra":1}"#).unwrap_err();
        assert_eq!(
            unknown.message,
            "invalid request: extra: unknown field `extra`"
        );
        assert_eq!(unknown.issues[0].path, "extra");

        // The body as a whole has no field to name.
        let whole = parse_json::<Sample>(br#""just text""#).unwrap_err();
        assert_eq!(
            whole.message,
            "invalid request body: expected an object, got a string"
        );
        assert!(whole.issues.is_empty());
    }

    #[test]
    fn value_conversion_prefixes_the_section() {
        let issue = from_json_value::<Nested>(json!({"count": -1}), "routing").unwrap_err();
        assert_eq!(issue.path, "routing.count");
        assert_eq!(issue.message, "must be a whole number from 0 to 255");
        let issue = from_json_value::<Nested>(json!("fast"), "routing").unwrap_err();
        assert_eq!(issue.path, "routing");
        assert_eq!(issue.message, "expected an object, got a string");
        let issue = from_json_value::<Nested>(json!({"count": 999}), "").unwrap_err();
        assert_eq!(issue.path, "count");
        let issue = from_json_value::<Nested>(json!({"cuont": 1}), "routing").unwrap_err();
        assert_eq!(
            (issue.path.as_str(), issue.message.as_str()),
            ("routing.cuont", "unknown field `cuont`")
        );
    }
}
