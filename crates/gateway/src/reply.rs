//! Building replies: error envelopes, response headers.

use crate::failover::FinalError;
use crate::types::FullReply;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use switchyard_core::{ApiError, Codec, ErrorKind};
use switchyard_translate::translate_error;

/// Media type of every JSON reply.
pub(crate) const JSON: &str = "application/json";

/// Status recorded for a request the client abandoned (nginx's "client
/// closed request"). The reply carrying it is never read by anyone.
pub(crate) const CLIENT_CLOSED_REQUEST: u16 = 499;

/// An upstream error body that is forwarded to the client unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verbatim {
    pub status: u16,
    pub body: String,
}

/// `text` when it is a JSON object, which is what every vendor's error
/// envelope is. Anything else (an HTML error page, a truncated body, a bare
/// string) is not shown to clients as is.
pub(crate) fn json_object_text(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .filter(Value::is_object)
        .map(|_| trimmed)
}

/// The error for a request whose client went away.
pub(crate) fn client_closed() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidRequest,
        "the client closed the request before it was answered",
    )
    .with_status(CLIENT_CLOSED_REQUEST)
    .with_code("client_closed_request")
}

/// Upstream response headers that are safe and useful to show a client:
/// rate-limit state, processing time, and the upstream's request id (under
/// a name that cannot be confused with the gateway's own).
pub(crate) fn passthrough_headers(upstream: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut upstream_request_id: Option<String> = None;
    for (name, value) in upstream {
        let name = name.as_str();
        let Ok(value) = value.to_str() else {
            continue;
        };
        if name == "request-id" || name == "x-request-id" {
            // `request-id` (Anthropic) wins over a generic `x-request-id`.
            if upstream_request_id.is_none() || name == "request-id" {
                upstream_request_id = Some(value.to_string());
            }
        } else if name.starts_with("x-ratelimit-")
            || name.starts_with("anthropic-ratelimit-")
            || name == "retry-after"
            || name == "openai-processing-ms"
        {
            out.push((name.to_string(), value.to_string()));
        }
    }
    if let Some(id) = upstream_request_id {
        out.push(("x-upstream-request-id".to_string(), id));
    }
    out
}

/// What is known about how a reply was produced, for its headers.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Served<'a> {
    pub provider: Option<&'a str>,
    pub upstream_model: Option<&'a str>,
    /// The upstream's response headers, when they are to be passed through.
    pub upstream_headers: Option<&'a HeaderMap>,
}

/// The headers every reply carries: the request id, then the debugging and
/// passthrough headers that apply.
pub(crate) fn reply_headers(request_id: &str, served: Served<'_>) -> Vec<(String, String)> {
    let mut headers = vec![("x-request-id".to_string(), request_id.to_string())];
    if let Some(provider) = served.provider {
        headers.push(("x-switchyard-provider".to_string(), provider.to_string()));
    }
    if let Some(model) = served.upstream_model {
        headers.push(("x-switchyard-model".to_string(), header_safe(model)));
    }
    if let Some(upstream) = served.upstream_headers {
        headers.extend(passthrough_headers(upstream));
    }
    headers
}

/// A value that can go into an HTTP header: visible ASCII only.
fn header_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Renders `error` for a client speaking `codec`'s protocol.
pub(crate) fn error_reply(
    codec: &dyn Codec,
    error: &ApiError,
    request_id: &str,
    served: Served<'_>,
) -> FullReply {
    let (status, body) = translate_error(codec, error);
    let mut headers = reply_headers(request_id, served);
    if let Some(secs) = error.retry_after_secs {
        set_retry_after(&mut headers, secs);
    }
    FullReply {
        status,
        headers,
        content_type: JSON.to_string(),
        body: Bytes::from(body.to_string()),
        request_id: request_id.to_string(),
    }
}

/// Renders the error an attempt loop ended with: the upstream's own body
/// when it may be forwarded, the gateway's rendering otherwise.
pub(crate) fn final_error_reply(
    codec: &dyn Codec,
    error: &FinalError,
    request_id: &str,
    served: Served<'_>,
) -> FullReply {
    let Some(verbatim) = &error.verbatim else {
        return error_reply(codec, &error.api, request_id, served);
    };
    let mut headers = reply_headers(request_id, served);
    if let Some(secs) = error.api.retry_after_secs {
        set_retry_after(&mut headers, secs);
    }
    FullReply {
        status: verbatim.status,
        headers,
        content_type: JSON.to_string(),
        body: Bytes::from(verbatim.body.clone()),
        request_id: request_id.to_string(),
    }
}

/// Sets `retry-after`, replacing one passed through from the upstream: the
/// gateway's own figure accounts for cooldowns the upstream knows nothing
/// about.
fn set_retry_after(headers: &mut Vec<(String, String)>, secs: u64) {
    headers.retain(|(name, _)| name != "retry-after");
    headers.push(("retry-after".to_string(), secs.max(1).to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use switchyard_core::Protocol;

    #[test]
    fn only_json_objects_are_forwardable() {
        assert_eq!(
            json_object_text(" {\"error\":{\"message\":\"x\"}} "),
            Some("{\"error\":{\"message\":\"x\"}}")
        );
        for not in ["", "oops", "<html>", "[1]", "\"text\"", "{\"cut\": "] {
            assert_eq!(json_object_text(not), None, "{not}");
        }
    }

    #[test]
    fn passthrough_keeps_rate_limit_headers_and_renames_the_request_id() {
        let mut upstream = HeaderMap::new();
        for (name, value) in [
            ("x-ratelimit-remaining-requests", "41"),
            ("anthropic-ratelimit-tokens-reset", "2026-01-01T00:00:00Z"),
            ("retry-after", "3"),
            ("openai-processing-ms", "120"),
            ("x-request-id", "req_generic"),
            ("request-id", "req_anthropic"),
            ("content-type", "application/json"),
            ("server", "cloudflare"),
            ("x-custom", "nope"),
        ] {
            upstream.insert(name, HeaderValue::from_static(value));
        }
        let out = passthrough_headers(&upstream);
        let get = |name: &str| {
            out.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("x-ratelimit-remaining-requests"), Some("41"));
        assert_eq!(
            get("anthropic-ratelimit-tokens-reset"),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(get("retry-after"), Some("3"));
        assert_eq!(get("openai-processing-ms"), Some("120"));
        assert_eq!(get("x-upstream-request-id"), Some("req_anthropic"));
        assert_eq!(get("x-request-id"), None);
        assert_eq!(get("server"), None);
        assert_eq!(get("x-custom"), None);
        assert_eq!(get("content-type"), None);
    }

    #[test]
    fn error_replies_carry_request_id_and_retry_after() {
        let codec = switchyard_codecs::codec(Protocol::OpenaiChat);
        let error =
            ApiError::rate_limit("slow down").with_retry_after(std::time::Duration::from_secs(7));
        let reply = error_reply(codec, &error, "req-1", Served::default());
        assert_eq!(reply.status, 429);
        assert_eq!(reply.header("x-request-id"), Some("req-1"));
        assert_eq!(reply.header("retry-after"), Some("7"));
        assert_eq!(reply.content_type, "application/json");
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["error"]["message"], "slow down");
    }

    #[test]
    fn the_gateways_retry_after_replaces_the_upstreams() {
        let codec = switchyard_codecs::codec(Protocol::Anthropic);
        let mut upstream = HeaderMap::new();
        upstream.insert("retry-after", HeaderValue::from_static("1"));
        let error =
            ApiError::rate_limit("slow down").with_retry_after(std::time::Duration::from_secs(30));
        let reply = error_reply(
            codec,
            &error,
            "req-1",
            Served {
                provider: Some("p"),
                upstream_model: Some("m\u{e9}"),
                upstream_headers: Some(&upstream),
            },
        );
        let retry: Vec<&str> = reply
            .headers
            .iter()
            .filter(|(name, _)| name == "retry-after")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(retry, vec!["30"]);
        assert_eq!(reply.header("x-switchyard-provider"), Some("p"));
        assert_eq!(reply.header("x-switchyard-model"), Some("m_"));
    }

    #[test]
    fn verbatim_errors_keep_the_upstreams_body_and_status() {
        let codec = switchyard_codecs::codec(Protocol::OpenaiChat);
        let error = FinalError {
            api: ApiError::rate_limit("converted")
                .with_retry_after(std::time::Duration::from_secs(4)),
            upstream_status: Some(429),
            verbatim: Some(Verbatim {
                status: 429,
                body: "{\"error\":{\"message\":\"upstream words\"}}".into(),
            }),
            detail: None,
        };
        let reply = final_error_reply(codec, &error, "req-2", Served::default());
        assert_eq!(reply.status, 429);
        assert_eq!(
            reply.body,
            Bytes::from_static(b"{\"error\":{\"message\":\"upstream words\"}}")
        );
        assert_eq!(reply.header("retry-after"), Some("4"));
    }

    #[test]
    fn the_cancellation_error_is_a_499() {
        let codec = switchyard_codecs::codec(Protocol::Gemini);
        let reply = error_reply(codec, &client_closed(), "r", Served::default());
        assert_eq!(reply.status, 499);
    }
}
