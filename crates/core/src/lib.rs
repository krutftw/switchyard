//! Shared foundation of the Switchyard gateway.
//!
//! * [`ir`] — the canonical request/response model every protocol is
//!   translated through;
//! * [`stream`] — the canonical streaming model;
//! * [`codec`] — the trait a wire protocol implements;
//! * [`reasoning`] — reasoning-depth normalisation across vendors;
//! * [`config`] — the configuration file schema;
//! * [`error`], [`usage`], [`model`], [`sse`], [`sig`], [`util`] — supporting
//!   types.

pub mod codec;
pub mod config;
pub mod error;
pub mod ir;
pub mod model;
pub mod protocol;
pub mod reasoning;
pub mod sig;
pub mod sse;
pub mod stream;
pub mod usage;
pub mod util;

pub use codec::{
    ClientCtx, Codec, MaxTokensField, Quirks, RequestMeta, RequestPath, StreamDecoder,
    StreamEncoder, UpstreamCtx,
};
pub use config::Config;
pub use error::{ApiError, CodecError, ErrorKind, FailureClass, UpstreamError, UpstreamErrorInfo};
pub use ir::{
    FinishReason, MediaPart, MediaSource, Message, Part, Reasoning, Request, Response, Role,
    Signature, TextPart, Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
pub use model::ModelInfo;
pub use protocol::{Family, Protocol};
pub use reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
};
pub use sse::{SseEvent, SseParser};
pub use stream::{Accumulator, BlockStart, StreamEvent};
pub use usage::Usage;
