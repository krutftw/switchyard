//! One-shot conversions between two protocols through the canonical model.
//!
//! These are the non-streaming halves of the translation path described in
//! `docs/DESIGN.md` section 2:
//!
//! ```text
//! client body ──C.decode_request──► ir::Request ──(mutate)──► U.encode_request ──► upstream body
//! upstream body ──U.decode_response──► ir::Response ──C.encode_response──► client body
//! ```
//!
//! Both helpers hand back the intermediate IR value as well, because the
//! gateway needs it afterwards (usage accounting, the reasoning store, request
//! records). The streaming counterpart is [`crate::transcode::Transcoder`].

use serde_json::Value;
use switchyard_core::codec::{ClientCtx, Codec, RequestPath, UpstreamCtx};
use switchyard_core::error::{ApiError, CodecError};
use switchyard_core::ir::{Request, Response};

/// Translates a client request body into an upstream request body.
///
/// 1. `from.decode_request(body, path)` — the client's protocol;
/// 2. `mutate(&mut request)` — the gateway's hook between the two codecs. This
///    is where the routed upstream model id is written to `request.model`,
///    the reasoning plan is applied
///    ([`crate::thinking::apply_to_request`]) and remembered reasoning is put
///    back ([`crate::reasoning_store::ReasoningStore::restore`]);
/// 3. `to.encode_request(&request, upstream_ctx)` — the upstream's protocol.
///
/// Returns the upstream body together with the request as it was encoded
/// (after `mutate`). Payload rules are *not* applied here; they run on the
/// returned body ([`crate::payload::apply_payload_rules`]).
///
/// Errors are the codecs' own: a [`CodecError::InvalidRequest`] when the
/// client body cannot be understood, a [`CodecError::Unsupported`] when the
/// request cannot be expressed for the upstream. `mutate` is not called when
/// decoding fails.
pub fn translate_request(
    from: &dyn Codec,
    to: &dyn Codec,
    body: &Value,
    path: &RequestPath<'_>,
    upstream_ctx: &UpstreamCtx<'_>,
    mutate: impl FnOnce(&mut Request),
) -> Result<(Value, Request), CodecError> {
    let mut request = from.decode_request(body, path)?;
    mutate(&mut request);
    let upstream_body = to.encode_request(&request, upstream_ctx)?;
    Ok((upstream_body, request))
}

/// Translates a complete upstream response body into the client's protocol.
///
/// Decodes with `from_upstream`, encodes with `to_client` using `client_ctx`
/// (which carries the model name the client asked for, so the upstream's own
/// model id never leaks through an alias). Returns the client body together
/// with the decoded response, whose `usage`, `finish` and parts the gateway
/// records and feeds to the reasoning store.
///
/// A body the upstream codec cannot decode yields
/// [`CodecError::InvalidUpstream`], which converts into a 502 [`ApiError`].
pub fn translate_response(
    from_upstream: &dyn Codec,
    to_client: &dyn Codec,
    body: &Value,
    client_ctx: &ClientCtx,
) -> Result<(Value, Response), CodecError> {
    let response = from_upstream.decode_response(body)?;
    let client_body = to_client.encode_response(&response, client_ctx)?;
    Ok((client_body, response))
}

/// Renders an error for a client: the HTTP status to answer with and the body
/// in the client protocol's error envelope.
///
/// The status is [`ApiError::status`]; a value that is not an HTTP error
/// status (anything outside `400..=599`, which only a hand-built error can
/// carry) falls back to the default status of the error's kind so a failure
/// is never reported with a success code.
pub fn translate_error(to_client: &dyn Codec, err: &ApiError) -> (u16, Value) {
    let status = if (400..=599).contains(&err.status) {
        err.status
    } else {
        err.kind.status()
    };
    (status, to_client.encode_error(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeCodec;
    use crate::thinking::{ReasoningPlan, apply_to_request};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use switchyard_core::error::ErrorKind;
    use switchyard_core::ir::{FinishReason, Part, Role};
    use switchyard_core::protocol::Protocol;
    use switchyard_core::reasoning::{Depth, Effort, Fitted};
    use switchyard_core::usage::Usage;

    fn client() -> FakeCodec {
        FakeCodec::new(Protocol::OpenaiChat, "a")
    }

    fn upstream() -> FakeCodec {
        FakeCodec::new(Protocol::Anthropic, "b")
    }

    fn no_path() -> RequestPath<'static> {
        RequestPath::default()
    }

    // ----- translate_request -----------------------------------------------

    #[test]
    fn request_is_decoded_mutated_and_encoded() {
        let body = json!({
            "fmt": "a",
            "model": "alias(high)",
            "stream": true,
            "messages": [
                {"role": "system", "text": "be brief"},
                {"role": "user", "text": "hello"},
                {"role": "assistant", "text": "hi"}
            ]
        });
        let (out, request) = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |req| req.model = "real-model".to_string(),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({
                "fmt": "b",
                "model": "real-model",
                "stream": true,
                "messages": [
                    {"role": "system", "text": "be brief"},
                    {"role": "user", "text": "hello"},
                    {"role": "assistant", "text": "hi"}
                ]
            })
        );
        // The returned request is the one that was encoded.
        assert_eq!(request.model, "real-model");
        assert_eq!(request.source, Protocol::OpenaiChat);
        assert!(request.stream);
        assert_eq!(request.messages.len(), 3);
        assert_eq!(request.messages[2].role, Role::Assistant);
    }

    #[test]
    fn mutate_can_apply_a_reasoning_plan() {
        let body =
            json!({"model": "m", "effort": "low", "messages": [{"role": "user", "text": "q"}]});
        let (out, request) = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |req| {
                apply_to_request(
                    req,
                    ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High))),
                );
            },
        )
        .unwrap();
        assert_eq!(out["effort"], "high");
        assert_eq!(
            request.reasoning.and_then(|r| r.depth),
            Some(Depth::Level(Effort::High))
        );
    }

    #[test]
    fn mutate_can_strip_reasoning() {
        let body =
            json!({"model": "m", "effort": "low", "messages": [{"role": "user", "text": "q"}]});
        let (out, _) = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |req| apply_to_request(req, ReasoningPlan::Write(Fitted::Strip)),
        )
        .unwrap();
        assert_eq!(out.get("effort"), None);
    }

    #[test]
    fn identity_mutation_keeps_the_request() {
        let body = json!({"model": "m", "messages": [{"role": "user", "text": "q"}]});
        let (out, request) = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(out["model"], "m");
        assert_eq!(out["stream"], false);
        assert_eq!(request.reasoning, None);
    }

    #[test]
    fn path_supplies_model_and_stream() {
        // Gemini-style: the model and the stream flag live in the URL.
        let body = json!({"messages": [{"role": "user", "text": "q"}]});
        let path = RequestPath {
            model: Some("from-path"),
            stream: Some(true),
        };
        let (out, request) = translate_request(
            &client(),
            &upstream(),
            &body,
            &path,
            &UpstreamCtx::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(request.model, "from-path");
        assert_eq!(out["model"], "from-path");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn decode_failure_is_returned_and_mutate_is_not_called() {
        let body = json!({"model": "m"});
        let mut called = false;
        let err = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |_| called = true,
        )
        .unwrap_err();
        assert!(!called);
        assert_eq!(
            err,
            CodecError::invalid_param("messages", "messages is required")
        );
        // Rendered to the client as a 400 naming the field.
        let api: ApiError = err.into();
        assert_eq!(api.status, 400);
        assert_eq!(api.param.as_deref(), Some("messages"));
    }

    #[test]
    fn missing_model_is_a_decode_failure() {
        let body = json!({"messages": []});
        let err = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, CodecError::InvalidRequest { param: Some(p), .. } if p == "model"));
    }

    #[test]
    fn encode_failure_is_returned() {
        // The fake upstream protocol cannot express an empty conversation.
        let body = json!({"model": "m", "messages": []});
        let err = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, CodecError::Unsupported(_)));
    }

    #[test]
    fn mutate_runs_before_encoding() {
        // A mutation that makes the request inexpressible must surface as the
        // encoder's error, proving the encoder saw the mutated request.
        let body = json!({"model": "m", "messages": [{"role": "user", "text": "q"}]});
        let err = translate_request(
            &client(),
            &upstream(),
            &body,
            &no_path(),
            &UpstreamCtx::default(),
            |req| req.messages.clear(),
        )
        .unwrap_err();
        assert!(matches!(err, CodecError::Unsupported(_)));
    }

    // ----- translate_response ----------------------------------------------

    fn upstream_response() -> Value {
        json!({
            "fmt": "b",
            "model": "real-model",
            "response": {
                "id": "resp_1",
                "model": "real-model",
                "created": 1_700_000_000,
                "parts": [
                    {"type": "text", "text": "Hello"},
                    {"type": "tool_call", "id": "c1", "name": "lookup", "arguments": "{\"q\":1}"}
                ],
                "finish": "tool_calls",
                "usage": {"input_tokens": 12, "output_tokens": 7}
            }
        })
    }

    #[test]
    fn response_is_decoded_and_encoded_for_the_client() {
        let ctx = ClientCtx::new("alias");
        let (out, response) =
            translate_response(&upstream(), &client(), &upstream_response(), &ctx).unwrap();

        // The IR response is the upstream's, untouched.
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "real-model");
        assert_eq!(response.finish, FinishReason::ToolCalls);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 7,
                ..Usage::default()
            }
        );
        assert_eq!(response.parts[0], Part::text("Hello"));
        assert_eq!(response.tool_calls().count(), 1);

        // The client body is in the client's format and names the alias.
        assert_eq!(out["fmt"], "a");
        assert_eq!(out["model"], "alias");
        assert_eq!(out["response"]["model"], "alias");
        assert_eq!(out["response"]["parts"][0]["text"], "Hello");
        assert_eq!(out["response"]["finish"], "tool_calls");
    }

    #[test]
    fn client_ctx_request_is_passed_through() {
        let ctx = ClientCtx::new("alias").with_request(Arc::new(json!({"model": "alias"})));
        let (out, _) =
            translate_response(&upstream(), &client(), &upstream_response(), &ctx).unwrap();
        assert_eq!(out["model"], "alias");
    }

    #[test]
    fn undecodable_upstream_response_is_an_upstream_error() {
        let ctx = ClientCtx::new("alias");
        for bad in [
            json!({"fmt": "other", "response": {}}),
            json!({"fmt": "b"}),
            json!({"fmt": "b", "response": {"id": 5}}),
            json!("<html>bad gateway</html>"),
            Value::Null,
        ] {
            let err = translate_response(&upstream(), &client(), &bad, &ctx).unwrap_err();
            assert!(matches!(err, CodecError::InvalidUpstream(_)), "{bad}");
            let api: ApiError = err.into();
            assert_eq!(api.status, 502);
            assert_eq!(api.kind, ErrorKind::Upstream);
        }
    }

    #[test]
    fn response_round_trips_between_two_protocols() {
        let ctx = ClientCtx::new("real-model");
        let (as_a, first) =
            translate_response(&upstream(), &client(), &upstream_response(), &ctx).unwrap();
        // Feed the client-format body back through the other direction.
        let (as_b, second) = translate_response(&client(), &upstream(), &as_a, &ctx).unwrap();
        assert_eq!(first, second);
        assert_eq!(as_b["fmt"], "b");
        assert_eq!(as_b["response"]["parts"], as_a["response"]["parts"]);
    }

    // ----- translate_error -------------------------------------------------

    #[test]
    fn error_is_rendered_in_the_client_envelope() {
        let err = ApiError::rate_limit("slow down");
        let (status, body) = translate_error(&client(), &err);
        assert_eq!(status, 429);
        assert_eq!(
            body,
            json!({"fmt": "a", "error": {"message": "slow down", "status": 429}})
        );
        let (_, body) = translate_error(&upstream(), &err);
        assert_eq!(body["fmt"], "b");
    }

    #[test]
    fn error_status_override_is_respected() {
        let err = ApiError::upstream("overloaded").with_status(529);
        assert_eq!(translate_error(&client(), &err).0, 529);
        let err = ApiError::invalid_request("nope").with_status(422);
        assert_eq!(translate_error(&client(), &err).0, 422);
    }

    #[test]
    fn default_statuses_per_kind() {
        for (err, status) in [
            (ApiError::invalid_request("x"), 400),
            (ApiError::authentication("x"), 401),
            (ApiError::permission("x"), 403),
            (ApiError::unknown_model("gpt-x"), 404),
            (ApiError::rate_limit("x"), 429),
            (ApiError::internal("x"), 500),
            (ApiError::upstream("x"), 502),
            (ApiError::unavailable("x"), 503),
            (ApiError::timeout("x"), 504),
        ] {
            assert_eq!(translate_error(&client(), &err).0, status);
        }
    }

    #[test]
    fn a_non_error_status_falls_back_to_the_kind() {
        for bogus in [0, 200, 302, 399, 600, 999] {
            let err = ApiError::upstream("x").with_status(bogus);
            assert_eq!(translate_error(&client(), &err).0, 502, "{bogus}");
        }
        let err = ApiError::rate_limit("x").with_status(200);
        assert_eq!(translate_error(&client(), &err).0, 429);
    }

    #[test]
    fn codec_errors_convert_and_render() {
        let api: ApiError = CodecError::invalid_param("messages", "must be an array").into();
        let (status, body) = translate_error(&client(), &api);
        assert_eq!(status, 400);
        assert_eq!(body["error"]["message"], "must be an array");
    }
}
