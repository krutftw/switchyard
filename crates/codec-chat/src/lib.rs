//! OpenAI Chat Completions codec. See `docs/DESIGN.md` section 3.
//!
//! [`ChatCodec`] implements [`Codec`] for `POST /v1/chat/completions`, both
//! as served to clients and as spoken to upstreams. Upstreams include
//! OpenAI itself and the many servers that imitate it (DeepSeek, OpenRouter,
//! Ollama, vLLM, Google's compatibility endpoint, …), so decoding accepts
//! their extensions: `reasoning_content` / `reasoning` / `reasoning_details`,
//! vendor usage fields, non-standard finish reasons, tool calls without ids
//! or indices.
//!
//! # Mapping notes
//!
//! * Leading `system` / `developer` messages become `Request::system`; later
//!   ones stay in the conversation as `Role::System`. System text is always
//!   encoded with role `system`.
//! * `tool` messages are tool results inside a user message; consecutive
//!   ones merge into one IR message. Encoding emits one `tool` message per
//!   result followed by a user message with the remaining parts. Chat tool
//!   messages are text-only, so media returned by a tool is relayed in the
//!   following user message. A result is only encoded as a `tool` message
//!   when an earlier assistant message issued its call id and nothing
//!   answered it yet; any other result (empty id, unknown id, second answer)
//!   is shown to the model as user text, because upstreams reject a `tool`
//!   message that answers no `tool_calls` entry.
//! * Media: `image_url`, `input_audio`, `file`, plus the `video_url` and
//!   `audio_url` parts of Chat-compatible servers. The IR has no video
//!   variant, so a clip is a `Part::Document` with a `video/*` media type and
//!   is encoded as `video_url` again. `image_url.detail` is limited to
//!   `auto` / `low` / `high` on encode (`original` becomes `high`).
//! * Tool parameter schemas (and `response_format` schemas) written for
//!   another protocol are normalised for Chat upstreams: `properties: {}` is
//!   added to object schemas that lack it, boolean `true` subschemas become
//!   `{}`, and patterns with `\p{…}` / `\0` are removed. Schemas written by a
//!   Chat client are forwarded untouched.
//! * Tool names and tool-call ids written for another protocol are fitted
//!   to OpenAI's limits (`^[a-zA-Z0-9_-]{1,64}$`, ids of at most 40
//!   characters) on the way to an upstream; a Chat client's own are replayed.
//!   A tool call made by an upstream of another protocol is reported under
//!   the name the client declared. A request written for another protocol
//!   keeps at most four stop sequences, and a forced tool the body does not
//!   declare becomes `tool_choice: "none"`.
//! * Built-in tools declared by a Chat client are replayed verbatim. Other
//!   protocols' built-ins are dropped; a Responses `web_search` tool becomes
//!   `web_search_options` only when the target model id names a search model
//!   (every other Chat model rejects that option).
//! * Free-form (`custom`) tools declared by a Chat client are replayed as
//!   such. Those of another protocol's client reach the upstream as function
//!   tools taking one string `input`, their calls as function calls with
//!   arguments `{"input": "<raw text>"}` and a forced one as a forced
//!   function: Chat-compatible servers know `function` tools only.
//! * `store`, `metadata`, `prompt_cache_key`, `service_tier` and
//!   `safety_identifier` exist on OpenAI's own platform only. They are
//!   replayed for a Chat client and left out of a request translated from
//!   another protocol, whose Chat upstream is a compatible server that may
//!   refuse unknown fields.
//! * Output format of another vendor's client: a JSON schema whose root is
//!   not an object schema (legal on Gemini) cannot be a `json_schema` format
//!   and is described in a system instruction instead; `json_object` gets a
//!   system instruction when the conversation never says "JSON", which
//!   OpenAI requires for that mode.
//! * Reasoning text travels as `reasoning_content`; opaque blobs travel in
//!   `reasoning_details` (`signature` / `data`) and, for tool calls, in
//!   `extra_content.google.thought_signature`. Blobs that a Chat upstream
//!   did not issue — those of a Responses upstream included — are wrapped
//!   for clients (`sy1.<tag>.<blob>`) and never sent upstream. In streams the
//!   reasoning text is also mirrored in `reasoning_details` entries whose
//!   `index` numbers the reasoning blocks, so block boundaries survive and a
//!   client that replays `reasoning_details` returns text and signature
//!   together.
//! * Finish reasons, IR → Chat: `Stop` → `stop`, `Length` → `length`,
//!   `ToolCalls` → `tool_calls`, `ContentFilter` and `Refusal` →
//!   `content_filter` (but a `Refusal` whose text the client is sent in
//!   `message.refusal` is a completed answer and ends with `stop`, as on
//!   OpenAI itself), `PauseTurn` and `Other(_)` → `stop` (`tool_calls`
//!   when the turn carries tool calls, which clients key their tool loop
//!   on), `ContextWindow` and `Error` → `length` (the answer is incomplete).
//!   Chat → IR: `stop` →
//!   `Stop`, `length` → `Length`, `tool_calls` and the deprecated
//!   `function_call` → `ToolCalls`, `content_filter` → `ContentFilter`;
//!   spellings leaked by other vendors (`end_turn`, `max_tokens`, `safety`,
//!   …) are folded onto the same four, and anything unknown is `Other`.
//!   `stop` on a turn that carries tool calls is reported as `ToolCalls`,
//!   unless a function call's arguments are a cut-off JSON document: such a
//!   turn is `Length` (streamed or not), so no client executes a half-written
//!   call. An explicit `length` or `content_filter` always wins.
//! * Usage: `prompt_tokens` includes cached tokens and `completion_tokens`
//!   includes reasoning tokens; both are converted to the IR's disjoint
//!   buckets. Bodies that prove the other convention (`total_tokens ==
//!   prompt + completion + reasoning`, or more reasoning than completion
//!   tokens) have their reasoning tokens added to the output total.
//! * Streams: a `[DONE]` with nothing before it produces no event and the
//!   stream then ends as failed (`Finish { reason: Error }`); a usage-only
//!   chunk completes a stream without finish reason only when it trails
//!   output. An `Error` event given to an encoder that has not started is
//!   rendered as the error frame alone.
//!
//! Chat Completions has no token-counting endpoint, so the counting methods
//! keep their `None` defaults.

mod common;
pub mod completions;
mod error;
mod models;
mod names;
mod reasoning;
mod redact;
mod request;
mod response;
mod schema;
mod stream_dec;
mod stream_enc;

pub use completions::{
    chat_chunk_to_completions, chat_response_to_completions, completions_request_to_chat,
};

use serde_json::Value;
use switchyard_core::codec::{
    ClientCtx, Codec, RequestMeta, RequestPath, StreamDecoder, StreamEncoder, UpstreamCtx,
};
use switchyard_core::error::{ApiError, CodecError, UpstreamErrorInfo};
use switchyard_core::ir::{Request, Response};
use switchyard_core::model::ModelInfo;
use switchyard_core::protocol::Protocol;
use switchyard_core::reasoning::{Fitted, ReasoningConfig};

/// The OpenAI Chat Completions codec.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChatCodec;

impl Codec for ChatCodec {
    fn protocol(&self) -> Protocol {
        Protocol::OpenaiChat
    }

    /// `model` must be a non-empty string; `stream` is true only for the JSON
    /// literal `true` (so `"true"` and `1` do not stream), as OpenAI treats it.
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

    /// Reads `reasoning_effort` and the spellings compatible servers added
    /// (`reasoning`, `thinking`, `enable_thinking`, Google's `thinking_config`).
    /// The summary intent is set by fields that speak about visibility
    /// explicitly (`include_reasoning`, `reasoning.exclude`, …) and by
    /// `reasoning_effort: "none"`, which also means "no summaries". An
    /// effort that turns reasoning on leaves it unset; the encoders of the
    /// protocols that return reasoning text only on request (Gemini,
    /// Responses) read such an effort of a Chat client as asking for it.
    fn read_reasoning(&self, body: &Value) -> ReasoningConfig {
        reasoning::read_reasoning(body)
    }

    /// Writes `reasoning_effort` and removes every other depth spelling.
    ///
    /// * [`Fitted::Strip`] removes the field;
    /// * `Depth::Auto` removes it too (Chat has no "provider decides" value;
    ///   the upstream default applies);
    /// * `Depth::Off` writes `"none"` (or the model's lowest level when the
    ///   model is known not to accept `none`);
    /// * `Depth::Budget` is bucketed with `budget_to_effort`;
    /// * levels are clamped to the model's supported set when it is known.
    fn write_reasoning(&self, body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
        reasoning::write_reasoning(body, depth, ctx);
    }

    /// Adjusts a body that is forwarded verbatim:
    ///
    /// * streaming and `quirks.stream_usage`: forces
    ///   `stream_options.include_usage = true`, because the gateway needs
    ///   the usage for accounting. **The client then receives the extra
    ///   usage-only chunk (`choices: []`) even if it did not ask for it** —
    ///   passthrough forwards upstream events unchanged.
    /// * streaming without `quirks.stream_usage`: removes `stream_options`,
    ///   which such an upstream rejects.
    /// * the output-token limit is moved to the field the upstream
    ///   understands (`quirks.max_tokens_field`), never overwriting a value
    ///   already present under that name.
    fn prepare_passthrough(&self, body: &mut Value, stream: bool, ctx: &UpstreamCtx<'_>) {
        request::prepare_passthrough(body, stream, ctx);
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

    /// The usage chunk is only emitted when the client's request (in
    /// `ctx.request`) has `stream_options.include_usage: true`.
    ///
    /// A `StreamEvent::Error` is rendered as a `data: {"error":{…}}` frame
    /// and nothing else: no `[DONE]` after it, and no role chunk before it
    /// when it is the first event the encoder sees (the gateway uses a fresh
    /// encoder to append an error to a passthrough stream).
    fn stream_encoder(&self, ctx: &ClientCtx) -> Box<dyn StreamEncoder> {
        Box::new(stream_enc::ChatStreamEncoder::new(ctx))
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

    /// Buffers interleaved tool-call fragments into sequential blocks and
    /// holds `Finish` back until `[DONE]` (or the end of the stream) so a
    /// trailing usage chunk precedes it.
    ///
    /// `decode` returns nothing for a `[DONE]` that arrives before any chunk:
    /// the upstream has not answered, and `finish()` then ends the stream
    /// with `Start` + `Finish { reason: Error }`, as it does for every stream
    /// that closes without a finish reason, a trailing usage chunk or
    /// `[DONE]`.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(stream_dec::ChatStreamDecoder::new())
    }

    fn decode_error(&self, status: u16, body: &[u8]) -> UpstreamErrorInfo {
        error::decode_error(status, body)
    }
}
