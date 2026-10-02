//! The codec contract: everything a wire protocol must provide so the gateway
//! can accept it from clients, speak it to upstreams, and translate between it
//! and every other protocol.
//!
//! A codec has two faces:
//!
//! * the **client side** — the gateway acts as a server of the protocol:
//!   [`Codec::decode_request`], [`Codec::encode_response`],
//!   [`Codec::stream_encoder`], [`Codec::encode_error`], model listings;
//! * the **upstream side** — the gateway acts as a client of the protocol:
//!   [`Codec::encode_request`], [`Codec::decode_response`],
//!   [`Codec::stream_decoder`], [`Codec::decode_error`].
//!
//! When client and upstream speak the same protocol the gateway forwards
//! bodies verbatim and only uses the small *inspect / patch* methods
//! ([`Codec::request_meta`], [`Codec::set_request_model`],
//! [`Codec::write_reasoning`], [`Codec::rewrite_response_model`]) plus a stream
//! decoder running on the side for usage accounting.
//!
//! Codecs are pure: no I/O, no clocks beyond [`crate::util`], no global state.

use crate::error::{ApiError, CodecError, UpstreamErrorInfo};
use crate::ir::{Request, Response};
use crate::model::ModelInfo;
use crate::protocol::Protocol;
use crate::reasoning::{Fitted, ModelThinking, ReasoningConfig};
use crate::sse::SseEvent;
use crate::stream::StreamEvent;
use serde_json::Value;
use std::sync::Arc;

/// Request facts carried by the URL rather than the body. Gemini puts the
/// model and the stream/non-stream choice in the path
/// (`/v1beta/models/{model}:streamGenerateContent`); other protocols leave
/// both `None`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestPath<'a> {
    pub model: Option<&'a str>,
    pub stream: Option<bool>,
}

/// The two facts the gateway needs before it can route a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMeta {
    /// Model name exactly as the client wrote it (may carry a prefix and a
    /// reasoning suffix).
    pub model: String,
    pub stream: bool,
}

/// Context for rendering output to a client.
#[derive(Clone, Debug)]
pub struct ClientCtx {
    /// Model name to report back — the name the client asked for, not the
    /// upstream's id.
    pub model: String,
    /// The client's original request body in the client's protocol, for
    /// protocols whose responses echo request fields (OpenAI Responses) or
    /// whose output depends on request options (`stream_options.include_usage`).
    /// `Value::Null` when unavailable.
    pub request: Arc<Value>,
}

impl ClientCtx {
    pub fn new(model: impl Into<String>) -> Self {
        ClientCtx {
            model: model.into(),
            request: Arc::new(Value::Null),
        }
    }

    pub fn with_request(mut self, request: Arc<Value>) -> Self {
        self.request = request;
        self
    }
}

/// Which field carries the output-token limit in OpenAI Chat Completions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaxTokensField {
    /// `max_completion_tokens` — required by OpenAI reasoning models.
    #[default]
    MaxCompletionTokens,
    /// Legacy `max_tokens` — what most OpenAI-compatible servers understand.
    MaxTokens,
}

/// Behavioural differences between upstreams that speak "the same" protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quirks {
    /// Chat Completions: name of the output-token limit field.
    pub max_tokens_field: MaxTokensField,
    /// Chat Completions: whether `stream_options: {"include_usage": true}` may
    /// be sent on streaming requests.
    pub stream_usage: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Quirks {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            stream_usage: true,
        }
    }
}

/// Context for building a request to an upstream.
#[derive(Clone, Copy, Debug)]
pub struct UpstreamCtx<'a> {
    /// What the target model accepts as reasoning settings.
    pub thinking: ModelThinking<'a>,
    /// The target model's maximum output tokens, when known. Protocols that
    /// require an explicit limit (Anthropic `max_tokens`) use it as the
    /// default when the client supplied none.
    pub max_output_tokens: Option<u64>,
    pub quirks: Quirks,
}

impl Default for UpstreamCtx<'_> {
    fn default() -> Self {
        UpstreamCtx {
            thinking: ModelThinking::Unknown,
            max_output_tokens: None,
            quirks: Quirks::default(),
        }
    }
}

/// A wire protocol implementation.
pub trait Codec: Send + Sync + 'static {
    fn protocol(&self) -> Protocol;

    // ------------------------------------------------------------------
    // Inspect / patch raw bodies (same-protocol passthrough)
    // ------------------------------------------------------------------

    /// Extracts the model name and stream flag from a client request without
    /// decoding the whole body.
    fn request_meta(&self, body: &Value, path: &RequestPath<'_>)
    -> Result<RequestMeta, CodecError>;

    /// Overwrites the model name in a request body. A no-op for protocols that
    /// carry the model in the URL.
    fn set_request_model(&self, body: &mut Value, model: &str);

    /// Reads the reasoning settings out of a request body in this protocol.
    /// Returns an empty config when the body says nothing about reasoning.
    fn read_reasoning(&self, body: &Value) -> ReasoningConfig;

    /// Writes a fitted reasoning depth into a request body in this protocol,
    /// replacing whatever depth fields were there. [`Fitted::Strip`] removes
    /// them. Must also keep the body valid for the protocol (for example
    /// Anthropic's `budget_tokens < max_tokens` rule).
    fn write_reasoning(&self, body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>);

    /// Last-minute adjustments to a body that is forwarded verbatim to an
    /// upstream of this protocol (for example asking for usage in streams).
    /// The default does nothing.
    fn prepare_passthrough(&self, _body: &mut Value, _stream: bool, _ctx: &UpstreamCtx<'_>) {}

    /// Replaces the model name inside a response payload — a complete response
    /// body or the JSON of a single stream event — with `model`. Used so
    /// clients see the alias they asked for. Payloads without a model field
    /// are left untouched.
    fn rewrite_response_model(&self, payload: &mut Value, model: &str);

    // ------------------------------------------------------------------
    // Client side
    // ------------------------------------------------------------------

    /// Decodes a client request into the canonical model.
    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError>;

    /// Renders a complete response for a client.
    fn encode_response(&self, response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError>;

    /// Creates a stateful encoder that renders canonical stream events as this
    /// protocol's stream.
    fn stream_encoder(&self, ctx: &ClientCtx) -> Box<dyn StreamEncoder>;

    /// Renders an error in this protocol's envelope. The HTTP status is
    /// [`ApiError::status`].
    fn encode_error(&self, error: &ApiError) -> Value;

    /// Renders a model listing (`GET /v1/models`, `GET /v1beta/models`).
    fn encode_models(&self, models: &[ModelInfo]) -> Value;

    /// Renders a single model (`GET /v1/models/{id}`).
    fn encode_model(&self, model: &ModelInfo) -> Value;

    /// Renders a token-count result for protocols with a counting endpoint.
    fn encode_count_response(&self, _input_tokens: u64) -> Option<Value> {
        None
    }

    // ------------------------------------------------------------------
    // Upstream side
    // ------------------------------------------------------------------

    /// Encodes a canonical request as a body for an upstream of this protocol.
    /// `request.model` is already the upstream model id and `request.reasoning`
    /// has already been fitted to the target model. For protocols that carry
    /// the model or the stream flag in the URL, those are simply omitted.
    fn encode_request(&self, request: &Request, ctx: &UpstreamCtx<'_>)
    -> Result<Value, CodecError>;

    /// Decodes a complete upstream response.
    fn decode_response(&self, body: &Value) -> Result<Response, CodecError>;

    /// Creates a stateful decoder for an upstream stream of this protocol.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// Extracts what it can from an upstream error body. Must never fail: an
    /// unparsable body yields its (truncated) text as the message.
    fn decode_error(&self, status: u16, body: &[u8]) -> UpstreamErrorInfo;

    /// Encodes the body of a token-counting call, for protocols that have a
    /// counting endpoint.
    fn encode_count_request(&self, _request: &Request, _ctx: &UpstreamCtx<'_>) -> Option<Value> {
        None
    }

    /// Reads the input token count out of a counting endpoint's response.
    fn decode_count_response(&self, _body: &Value) -> Option<u64> {
        None
    }
}

/// Turns an upstream's wire events into canonical stream events.
///
/// Implementations must uphold the sequence contract documented in
/// [`crate::stream`], whatever the upstream sends.
pub trait StreamDecoder: Send {
    /// Consumes one wire event. Events the decoder does not understand are
    /// skipped (returning an empty vector), never an error; an `Err` means the
    /// stream is unusable.
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError>;

    /// Called once when the upstream closes the stream. Returns whatever is
    /// needed to complete the sequence: closes an open block and, if no
    /// terminal event was produced, emits one.
    fn finish(&mut self) -> Vec<StreamEvent>;
}

/// Turns canonical stream events into a client's wire events.
pub trait StreamEncoder: Send {
    /// Renders one canonical event as zero or more wire events.
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent>;

    /// Called once after the last canonical event. Returns protocol
    /// terminators such as OpenAI's `data: [DONE]`. If the sequence ended
    /// without a terminal event the encoder closes the stream as gracefully
    /// as its protocol allows.
    fn finish(&mut self) -> Vec<SseEvent>;
}
