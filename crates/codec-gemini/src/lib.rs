//! Google Gemini codec (`generativelanguage` / Vertex AI `generateContent`).
//! See `docs/DESIGN.md` section 3.
//!
//! [`GeminiCodec`] implements [`switchyard_core::Codec`] for the protocol in
//! which the model name and the stream flag travel in the URL
//! (`/v1beta/models/{model}:streamGenerateContent`) rather than in the body.
//!
//! Decisions worth knowing about when reading the modules:
//!
//! * **Call ids.** Gemini pairs a `functionResponse` with its `functionCall`
//!   by position and name. Decoded requests get deterministic call ids
//!   (stable across replays of the same history); upstream requests only
//!   carry an `id` that a Gemini client supplied itself.
//!   A tool result is named after the call it is paired with in its own turn,
//!   so clients that reuse a call id from turn to turn work.
//! * **Thought signatures.** A signature is replayed only when it was issued
//!   by Gemini. The first `functionCall` of a replayed model turn that has no
//!   such signature gets the documented bypass value [`SKIP_SIGNATURE`];
//!   later calls of the same turn stay unsigned. A part that is nothing but a
//!   signature is modelled as text-less reasoning.
//! * **Foreign signatures on the Gemini wire.** `thoughtSignature` is a
//!   protobuf `bytes` field, i.e. base64, and typed Gemini SDKs fail on
//!   anything else. A blob of another vendor handed to a Gemini client is
//!   therefore tagged by `switchyard_core::sig` (`sy1.<tag>.<blob>`) *and
//!   then base64-encoded*; `decode_request` undoes both. Because the tag is
//!   no longer visible to `sig::contains_wrapped`, such a body can take the
//!   verbatim path, and `prepare_passthrough` removes foreign signatures
//!   there itself.
//! * **Tool schemas** go to `parametersJsonSchema` after [`sanitize_schema`]
//!   (bounded in size and depth); structured-output schemas go to
//!   `responseJsonSchema` unchanged.
//! * **countTokens** bodies are understood in both forms: bare `contents`
//!   and a whole request wrapped in `generateContentRequest`.
//! * **Tool results with images** are sent as a `functionResponse` part
//!   followed by `inlineData` parts in the same content, which every model
//!   accepts (the nested `functionResponse.parts` form is read but not
//!   written).
//! * **Built-in tools** of another vendor are mapped to Gemini's
//!   (`googleSearch`, `urlContext`, `codeExecution`) only when the request
//!   declares no functions, because most Gemini models reject the mix.
//! * **Function names** a client declared are what it is shown in
//!   `functionCall` parts: an upstream of another protocol that had to be
//!   given another spelling (no dots or colons on OpenAI and Anthropic)
//!   calls the function by that spelling, and the client's own is restored.
//! * **Restricted function calling.** `allowedFunctionNames` with several
//!   names (or with `VALIDATED`) limits the model to some of the declared
//!   functions. The canonical model cannot say that, so the decoder narrows
//!   the tool list to the allowed functions (and keeps the raw `toolConfig`
//!   for a Gemini upstream): no other upstream may be offered the functions
//!   the client excluded.
//! * **Output modes other than JSON** (`responseMimeType: "text/x.enum"`)
//!   are replayed to a Gemini upstream exactly as the client wrote them. For
//!   other protocols the schema is all that can be passed on, as a JSON
//!   schema format: from their upstreams such a client is answered with the
//!   JSON spelling of the value (a quoted string).
//! * **Refusals** have no part type here: the text is ordinary text and the
//!   finish reason is `SAFETY`, whether the upstream reported the refusal as
//!   a finish reason or as a refusal part of a completed answer.
//! * **Vertex AI** needs [`adapt_for_vertex`] applied to request bodies.

mod error;
mod models;
mod names;
mod parts;
mod raw;
mod reasoning;
mod redact;
mod request;
mod response;
mod schema;
mod stream_dec;
mod stream_enc;
mod util;

pub use error::CODE_DAILY_QUOTA;
pub use parts::SKIP_SIGNATURE;
pub use raw::adapt_for_vertex;
pub use schema::{sanitize_schema, sanitize_schema_legacy};
pub use util::sanitize_function_name;

use serde_json::Value;
use switchyard_core::reasoning::{Fitted, ReasoningConfig};
use switchyard_core::{
    ApiError, ClientCtx, Codec, CodecError, ModelInfo, Protocol, Request, RequestMeta, RequestPath,
    Response, StreamDecoder, StreamEncoder, UpstreamCtx, UpstreamErrorInfo,
};

/// The Gemini `generateContent` codec.
#[derive(Clone, Copy, Debug, Default)]
pub struct GeminiCodec;

impl Codec for GeminiCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Gemini
    }

    fn request_meta(
        &self,
        body: &Value,
        path: &RequestPath<'_>,
    ) -> Result<RequestMeta, CodecError> {
        raw::request_meta(body, path)
    }

    fn set_request_model(&self, body: &mut Value, model: &str) {
        raw::set_request_model(body, model);
    }

    fn read_reasoning(&self, body: &Value) -> ReasoningConfig {
        reasoning::read_reasoning(body)
    }

    fn write_reasoning(&self, body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
        reasoning::write_reasoning(body, depth, ctx);
    }

    fn prepare_passthrough(&self, body: &mut Value, _stream: bool, ctx: &UpstreamCtx<'_>) {
        raw::prepare_passthrough(body, ctx);
    }

    fn rewrite_response_model(&self, payload: &mut Value, model: &str) {
        raw::rewrite_response_model(payload, model);
    }

    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
        request::decode_request(body, path)
    }

    fn encode_response(&self, response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
        response::encode_response(response, ctx)
    }

    fn stream_encoder(&self, ctx: &ClientCtx) -> Box<dyn StreamEncoder> {
        Box::new(stream_enc::GeminiStreamEncoder::new(ctx))
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
        Some(models::encode_count_response(input_tokens))
    }

    fn encode_request(
        &self,
        request: &Request,
        ctx: &UpstreamCtx<'_>,
    ) -> Result<Value, CodecError> {
        request::encode_request(request, ctx)
    }

    fn decode_response(&self, body: &Value) -> Result<Response, CodecError> {
        response::decode_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(stream_dec::GeminiStreamDecoder::new())
    }

    fn decode_error(&self, status: u16, body: &[u8]) -> UpstreamErrorInfo {
        error::decode_error(status, body)
    }

    fn encode_count_request(&self, request: &Request, _ctx: &UpstreamCtx<'_>) -> Option<Value> {
        Some(request::encode_count_request(request))
    }

    fn decode_count_response(&self, body: &Value) -> Option<u64> {
        models::decode_count_response(body)
    }
}
