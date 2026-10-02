//! Anthropic Messages codec. See `docs/DESIGN.md` section 3.
//!
//! [`AnthropicCodec`] maps the Messages API (`POST /v1/messages`, its SSE
//! stream, `POST /v1/messages/count_tokens`, `GET /v1/models`) to and from
//! the canonical model in [`switchyard_core::ir`]:
//!
//! * client side — [`Codec::decode_request`], [`Codec::encode_response`],
//!   [`Codec::stream_encoder`], [`Codec::encode_error`], model listings;
//! * upstream side — [`Codec::encode_request`], [`Codec::decode_response`],
//!   [`Codec::stream_decoder`], [`Codec::decode_error`], token counting;
//! * raw-body helpers for same-protocol passthrough.
//!
//! Opaque blobs (thinking signatures, redacted thinking payloads) are tagged
//! with their origin: blobs read from a client go through
//! [`switchyard_core::sig::decode_from_client`], blobs written to a client
//! through [`switchyard_core::sig::encode_for_client`], and a request sent
//! upstream only ever carries blobs Anthropic issued.
//!
//! A request translated from another protocol is fitted to the rules of the
//! Claude generation it is addressed to (read off the model id): no assistant
//! prefill from Claude 4.6 on, no forced tool use on Opus 5.5 / Sonnet 5.5 /
//! Fable 5.1 / Mythos 5.1 and their successors. A forced tool the body does
//! not offer becomes `tool_choice: none`. A Messages client's own request is
//! replayed as written.

mod blocks;
mod error;
mod models;
mod passthrough;
mod reasoning;
mod redact;
mod request_dec;
mod request_enc;
mod response;
mod schema;
mod stream_dec;
mod stream_enc;
mod util;

use serde_json::{Value, json};
use switchyard_core::reasoning::{Fitted, ReasoningConfig};
use switchyard_core::{
    ApiError, ClientCtx, Codec, CodecError, ModelInfo, Protocol, Request, RequestMeta, RequestPath,
    Response, StreamDecoder, StreamEncoder, UpstreamCtx, UpstreamErrorInfo,
};

/// The Anthropic Messages protocol.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicCodec;

impl Codec for AnthropicCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    /// `stream` is true only when the body's `stream` is the JSON literal
    /// `true`.
    fn request_meta(
        &self,
        body: &Value,
        path: &RequestPath<'_>,
    ) -> Result<RequestMeta, CodecError> {
        request_dec::request_meta(body, path)
    }

    fn set_request_model(&self, body: &mut Value, model: &str) {
        if let Some(object) = body.as_object_mut() {
            object.insert("model".to_string(), Value::String(model.to_string()));
        }
    }

    fn read_reasoning(&self, body: &Value) -> ReasoningConfig {
        reasoning::read_reasoning(body)
    }

    fn write_reasoning(&self, body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
        reasoning::write_reasoning(body, depth, ctx);
    }

    /// Adjustments to a body forwarded verbatim. Nothing the client wrote is
    /// reinterpreted; only omissions and content the API is known to refuse
    /// are repaired:
    ///
    /// * `max_tokens` is mandatory: when absent and the model's output limit
    ///   is known, that limit is written;
    /// * `stream` is made to agree with the transport the gateway chose;
    /// * empty `allowed_domains` / `blocked_domains` lists on web tools are
    ///   removed (the API calls an empty list ambiguous and refuses it);
    /// * when the model is a Claude model (its id contains `claude`),
    ///   assistant history that Anthropic did not issue is made replayable:
    ///   `thinking` blocks without a signature, `redacted_thinking` blocks
    ///   without data and whitespace-only text blocks are removed (and a
    ///   message left without content with them), web citations without an
    ///   `encrypted_index` are removed, and `tool_use` ids that are not
    ///   `[a-zA-Z0-9_-]+` or not unique are replaced, together with the
    ///   `tool_use_id` of their results. The gateway hands such blocks to
    ///   Messages clients while another vendor serves them (reasoning without
    ///   a signature is rendered with `"signature": ""`), clients replay
    ///   them, and Anthropic answers each with a 400. Other models served
    ///   over this protocol may issue and expect exactly such blocks, so
    ///   their bodies are not touched;
    /// * for the same models, `thinking` of type `enabled` is removed when
    ///   the assistant turn in progress (a tool loop, a prefill) does not
    ///   open with a thinking block, which the API refuses.
    ///
    /// A body with a native history comes out unchanged. The model id is
    /// read from the body, so the gateway must have written the upstream
    /// model id ([`Codec::set_request_model`]) before calling this.
    fn prepare_passthrough(&self, body: &mut Value, stream: bool, ctx: &UpstreamCtx<'_>) {
        passthrough::prepare_passthrough(body, stream, ctx);
    }

    /// Rewrites `model` of a complete message and `message.model` of a
    /// `message_start` stream event.
    fn rewrite_response_model(&self, payload: &mut Value, model: &str) {
        let Some(object) = payload.as_object_mut() else {
            return;
        };
        if object.get("model").is_some_and(Value::is_string) {
            object.insert("model".to_string(), Value::String(model.to_string()));
        }
        if let Some(Value::Object(message)) = object.get_mut("message")
            && message.get("model").is_some_and(Value::is_string)
        {
            message.insert("model".to_string(), Value::String(model.to_string()));
        }
    }

    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
        request_dec::decode_request(body, path)
    }

    fn encode_response(&self, response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
        response::encode_response(response, ctx)
    }

    fn stream_encoder(&self, ctx: &ClientCtx) -> Box<dyn StreamEncoder> {
        Box::new(stream_enc::Encoder::new(ctx))
    }

    fn encode_error(&self, error: &ApiError) -> Value {
        error::encode_error(error)
    }

    fn encode_models(&self, models: &[ModelInfo]) -> Value {
        models::encode_models(models)
    }

    fn encode_model(&self, model: &ModelInfo) -> Value {
        models::encode_model(model)
    }

    /// `{"input_tokens": N}`.
    fn encode_count_response(&self, input_tokens: u64) -> Option<Value> {
        Some(json!({"input_tokens": input_tokens}))
    }

    fn encode_request(
        &self,
        request: &Request,
        ctx: &UpstreamCtx<'_>,
    ) -> Result<Value, CodecError> {
        request_enc::encode_request(request, ctx)
    }

    fn decode_response(&self, body: &Value) -> Result<Response, CodecError> {
        response::decode_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(stream_dec::Decoder::new())
    }

    fn decode_error(&self, status: u16, body: &[u8]) -> UpstreamErrorInfo {
        error::decode_error(status, body)
    }

    /// The body of `POST /v1/messages/count_tokens`.
    fn encode_count_request(&self, request: &Request, ctx: &UpstreamCtx<'_>) -> Option<Value> {
        request_enc::encode_count_request(request, ctx)
    }

    /// Reads `input_tokens` from a counting response.
    fn decode_count_response(&self, body: &Value) -> Option<u64> {
        switchyard_core::util::u64_field(body, "input_tokens")
    }
}
