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
use axum::extract::{FromRequest, FromRequestParts, Path, Request};
use bytes::Bytes;
use http::StatusCode;
use http::request::Parts;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
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
///
/// Invalid values and repeated parameters are `400`s that name the
/// parameter in the message and as the path of their issue.
pub(crate) struct Params<T>(pub T);

/// API validation, separate from telemetry's lenient convenience types.
pub(crate) trait QueryParams: DeserializeOwned {
    fn validate(name: &str, value: &str) -> Result<(), ApiFailure>;
}

impl<S, T> FromRequestParts<S> for Params<T>
where
    S: Send + Sync,
    T: QueryParams,
{
    type Rejection = ApiFailure;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parse_query(parts.uri.query().unwrap_or_default()).map(Params)
    }
}

/// Rejects malformed encoding and duplicate decoded names, even for
/// parameters the route does not use. Called after authentication for
/// every API route, including routes without a `Params` extractor.
pub(crate) fn validate_query(query: &str) -> Result<(), ApiFailure> {
    query_pairs(query).map(|_| ())
}

fn query_pairs(query: &str) -> Result<Vec<(String, String)>, ApiFailure> {
    let mut names = HashSet::new();
    let mut pairs = Vec::new();
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (raw_name, raw_value) = part.split_once('=').unwrap_or((part, ""));
        let name = decode_component(raw_name).map_err(|message| query_error(raw_name, message))?;
        if name.is_empty() {
            return Err(query_error(
                "(empty)",
                "the parameter name must not be empty",
            ));
        }
        if !names.insert(name.clone()) {
            return Err(query_error(&name, "is given twice"));
        }
        let value = decode_component(raw_value).map_err(|message| query_error(&name, message))?;
        pairs.push((name, value));
    }
    Ok(pairs)
}

/// Unlike form_urlencoded's forgiving decoder, the API refuses broken
/// percent escapes and non-UTF-8 bytes instead of silently replacing them.
fn decode_component(raw: &str) -> Result<String, &'static str> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut input = raw.bytes();
    while let Some(byte) = input.next() {
        bytes.push(match byte {
            b'+' => b' ',
            b'%' => {
                let mut hex = || input.next().and_then(|b| char::from(b).to_digit(16));
                let high = hex().ok_or("must use valid percent encoding")?;
                let low = hex().ok_or("must use valid percent encoding")?;
                (high * 16 + low) as u8
            }
            byte => byte,
        });
    }
    String::from_utf8(bytes).map_err(|_| "must contain valid UTF-8 text")
}

pub(crate) fn query_error(name: &str, message: &str) -> ApiFailure {
    ApiFailure::bad_query(ConfigIssue {
        path: name.to_string(),
        message: message.to_string(),
    })
}

/// Reads a query with axum's deserialiser after validating its API values.
/// Unknown parameters remain forward-compatible, but cannot repeat.
pub(crate) fn parse_query<T: QueryParams>(query: &str) -> Result<T, ApiFailure> {
    for (name, value) in query_pairs(query)? {
        T::validate(&name, value.trim())?;
    }
    let deserializer =
        serde_urlencoded::Deserializer::new(form_urlencoded::parse(query.as_bytes()));
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let path = error.path().to_string();
        ApiFailure::bad_query(shape::describe(&path, &error.into_inner().to_string()))
    })
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

    /// Regression: a refused query parameter was answered with
    /// `invalid query string: is given twice` — which parameter, it did not
    /// say, and `issues` was missing.
    #[test]
    fn a_refused_query_parameter_is_named() {
        use switchyard_telemetry::{LogQuery, RequestQuery, UsageQuery};

        let twice = parse_query::<RequestQuery>("limit=5&status=ok&limit=6").unwrap_err();
        assert_eq!(twice.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            twice.body(),
            json!({"error": {
                "message": "invalid query parameter `limit`: is given twice",
                "issues": [{"path": "limit", "message": "is given twice"}],
            }})
        );
        for (query, param) in [
            ("range=1h&range=24h", "range"),
            ("group_by=model&bucket=hour&group_by=key", "group_by"),
        ] {
            let error = parse_query::<UsageQuery>(query).unwrap_err();
            assert_eq!(error.issues[0].path, param, "{query}");
            assert!(error.message.contains(&format!("`{param}`")), "{query}");
        }
        let error = parse_query::<LogQuery>("level=warn&level=info").unwrap_err();
        assert_eq!(error.issues[0].path, "level");

        // A value the type refuses: named, and not repeated.
        let since = parse_query::<RequestQuery>("since=yesterday&limit=2").unwrap_err();
        assert_eq!(since.issues[0].path, "since");
        assert_eq!(
            since.message,
            "invalid query parameter `since`: must be a whole number of unix milliseconds"
        );
        assert!(!since.message.contains("yesterday"));

        // Omitted values keep their defaults; single unknown parameters
        // are ignored and percent-encoding is decoded.
        let valid: RequestQuery = parse_query("x=1&client_model=mock%2Decho").unwrap();
        assert_eq!(
            valid,
            RequestQuery {
                client_model: Some("mock-echo".into()),
                ..RequestQuery::default()
            }
        );
        assert_eq!(
            parse_query::<UsageQuery>("").unwrap(),
            UsageQuery::default()
        );
    }

    #[test]
    fn every_query_checks_decoded_names_and_encoding() {
        for (query, parameter) in [
            ("unknown=one&unknown=two", "unknown"),
            ("x=one&%78=two", "x"),
            ("q=%", "q"),
            ("q=%0", "q"),
            ("q=%GG", "q"),
            ("q=%FF", "q"),
            ("q=%C3%28", "q"),
            ("bad%GG=value", "bad%GG"),
            ("=value", "(empty)"),
        ] {
            let error = validate_query(query).unwrap_err();
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(error.issues[0].path, parameter, "{query}");
            assert!(error.message.contains(parameter), "{query}");
        }
        assert_eq!(
            query_pairs("q=caf%C3%A9+%26+tea&unknown=%25&flag").unwrap(),
            vec![
                ("q".to_string(), "café & tea".to_string()),
                ("unknown".to_string(), "%".to_string()),
                ("flag".to_string(), String::new()),
            ]
        );
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
