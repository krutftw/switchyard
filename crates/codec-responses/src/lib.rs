//! OpenAI Responses codec (`POST /v1/responses`, also spoken over WebSocket).
//!
//! [`ResponsesCodec`] implements [`switchyard_core::Codec`] for the Responses
//! protocol: it decodes client requests into the canonical model, renders
//! canonical responses and streams for clients, and does the reverse for
//! upstreams that speak Responses. See `docs/DESIGN.md` §3 for the contract.
//!
//! Besides the codec the crate exports a few pure helpers the WebSocket
//! handler needs to keep a transcript for stateless upstreams:
//! [`merge_transcript`], [`is_transcript_replacement`],
//! [`repair_tool_pairs`], [`ws_error_frame`] and [`prewarm_frames`].
//!
//! Opaque reasoning blobs (`reasoning.encrypted_content`) are tagged with the
//! protocol that issued them. Blobs of any other protocol — Chat Completions
//! included, although it is the same vendor family: its blobs are the
//! signatures of the compatible servers behind it — reach a Responses client
//! wrapped (`sy1.<tag>.<blob>`), are unwrapped again when the client replays
//! them, and are never sent to a Responses upstream.
//!
//! Tool names written for another protocol are fitted to OpenAI's rules on
//! the way to an upstream (`^[a-zA-Z0-9_-]{1,64}$`), and a tool call an
//! upstream of another protocol makes is reported to the client under the
//! name the client declared, whatever spelling that upstream was given.
//!
//! Tool choice across protocols:
//!
//! * a client's `allowed_tools` choice restricts the model to some of the
//!   declared tools. The canonical model cannot say that, so the decoder
//!   narrows the tool list to the allowed tools (and keeps the raw choice
//!   for a Responses upstream): no other upstream may be offered the tools
//!   the client excluded;
//! * a forced provider tool of another vendor that was mapped to a hosted
//!   tool (a Messages client's `web_search`) forces the hosted tool; a
//!   forced tool the body does not offer becomes `"none"`.
//!
//! Requests written for another protocol are also fitted where this API is
//! stricter: `strict` of another vendor's tool is not taken over, a JSON
//! schema format whose root is not an object schema is described in
//! `instructions` instead, `json_object` mode gets the mention of "JSON" the
//! API insists on, and a Chat Completions client's `reasoning_effort` asks
//! for reasoning summaries (it has no other way to).

mod common;
mod error;
mod models;
mod names;
mod reasoning;
mod request;
mod response;
mod schema;
mod stream_dec;
mod stream_enc;
mod ws;

pub use ws::{
    is_transcript_replacement, merge_transcript, prewarm_frames, repair_tool_pairs, ws_error_frame,
};

use serde_json::Value;
use switchyard_core::ir::{Request, Response};
use switchyard_core::reasoning::{Fitted, ReasoningConfig};
use switchyard_core::{
    ApiError, ClientCtx, Codec, CodecError, ModelInfo, Protocol, RequestMeta, RequestPath,
    StreamDecoder, StreamEncoder, UpstreamCtx, UpstreamErrorInfo,
};

/// The OpenAI Responses protocol.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponsesCodec;

impl Codec for ResponsesCodec {
    fn protocol(&self) -> Protocol {
        Protocol::OpenaiResponses
    }

    fn request_meta(
        &self,
        body: &Value,
        path: &RequestPath<'_>,
    ) -> Result<RequestMeta, CodecError> {
        request::request_meta(body, path)
    }

    fn set_request_model(&self, body: &mut Value, model: &str) {
        request::set_request_model(body, model);
    }

    /// Reads `reasoning.effort` (overridden by the last
    /// `configuration_update` input item that sets one) and the summary
    /// intent in `reasoning.summary` / `reasoning.generate_summary`.
    fn read_reasoning(&self, body: &Value) -> ReasoningConfig {
        reasoning::read_reasoning(body)
    }

    /// Writes `reasoning.effort`. Budgets are bucketed into effort levels,
    /// [`switchyard_core::Depth::Auto`] removes the field (the model default
    /// is the closest thing to "dynamic"), and [`Fitted::Strip`] removes the
    /// effort while keeping `reasoning.summary`. A `reasoning` object left
    /// empty is removed.
    fn write_reasoning(&self, body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
        reasoning::write_reasoning(body, depth, ctx);
    }

    /// Only makes the `stream` flag agree with the transport.
    fn prepare_passthrough(&self, body: &mut Value, stream: bool, _ctx: &UpstreamCtx<'_>) {
        request::prepare_passthrough(body, stream);
    }

    fn rewrite_response_model(&self, payload: &mut Value, model: &str) {
        response::rewrite_response_model(payload, model);
    }

    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
        request::decode_request(body, path)
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

    fn encode_count_response(&self, input_tokens: u64) -> Option<Value> {
        Some(response::encode_count_response(input_tokens))
    }

    /// Encodes a request for a Responses upstream. Beyond the plain mapping:
    ///
    /// * reasoning items are replayed only with a blob a Responses upstream
    ///   issued, and not at all to a model known not to reason;
    /// * tool names of a request written for another protocol are made
    ///   valid for this API (declarations, calls in the history and
    ///   `tool_choice` alike);
    /// * `store: false` (plus `include: ["reasoning.encrypted_content"]`) is
    ///   set when the model may reason and the request does not rely on
    ///   stored state, and `store: false` alone for any request from another
    ///   protocol that did not ask for storage, because only this API stores
    ///   responses by default;
    /// * `max_output_tokens` is kept between the vendor's minimum and
    ///   `ctx.max_output_tokens`;
    /// * function tool schemas that came through another protocol are
    ///   repaired where this API would reject them.
    fn encode_request(
        &self,
        request: &Request,
        ctx: &UpstreamCtx<'_>,
    ) -> Result<Value, CodecError> {
        request::encode_request(request, ctx)
    }

    /// Decodes a complete response object.
    ///
    /// A response with `status: "failed"` (or an `error` member) and no
    /// output is an upstream failure reported with HTTP 200. It yields
    /// [`CodecError::InvalidUpstream`] carrying the upstream's message and
    /// code (credentials redacted), never an empty successful response. A
    /// failed response that did produce output decodes to that output with
    /// [`switchyard_core::FinishReason::Error`].
    fn decode_response(&self, body: &Value) -> Result<Response, CodecError> {
        response::decode_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(stream_dec::Decoder::new())
    }

    /// Parses an upstream error body (JSON in any of the usual envelopes,
    /// plain text or HTML). Credentials the upstream echoed (`Bearer …`,
    /// `api_key=…`, bare `sk-…` keys) are replaced by `[REDACTED]` in
    /// everything that is extracted.
    fn decode_error(&self, status: u16, body: &[u8]) -> UpstreamErrorInfo {
        error::decode_error(status, body)
    }

    fn encode_count_request(&self, request: &Request, ctx: &UpstreamCtx<'_>) -> Option<Value> {
        request::encode_count_request(request, ctx)
    }

    fn decode_count_response(&self, body: &Value) -> Option<u64> {
        response::decode_count_response(body)
    }
}
