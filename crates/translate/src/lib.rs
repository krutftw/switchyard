//! Codec-agnostic pipeline pieces. See `docs/DESIGN.md` section 4.
//!
//! Everything here works on [`switchyard_core`] types and `&dyn Codec`; the
//! crate knows nothing about any particular wire protocol. The gateway
//! combines the pieces per upstream attempt:
//!
//! | step | passthrough (client protocol = upstream protocol) | translation |
//! |---|---|---|
//! | request | forward the client's JSON, model replaced | [`convert::translate_request`] |
//! | reasoning depth | [`thinking::plan_reasoning`] + [`thinking::apply_to_body`] | [`thinking::plan_reasoning`] + [`thinking::apply_to_request`] |
//! | tool names | — (the client speaks the upstream's dialect) | [`toolnames::sanitize_tool_names`], restored through [`toolnames::ToolNames`] |
//! | lost reasoning blobs | — (the client's own blobs are forwarded) | [`reasoning_store::ReasoningStore::restore`] |
//! | operator patches | [`payload::apply_payload_rules`] | [`payload::apply_payload_rules`] |
//! | response | forward, model rewritten ([`transcode::rewrite_model_text`]) | [`convert::translate_response`] |
//! | stream | [`transcode::Transcoder::passthrough`] | [`transcode::Transcoder::translate`] |
//! | errors | upstream body forwarded | [`convert::translate_error`] |
//! | afterwards | [`reasoning_store::ReasoningStore::remember`] | [`reasoning_store::ReasoningStore::remember`] |
//!
//! [`jsonpath`] is the path language of payload rules and [`estimate`] the
//! local token estimate used when no upstream counting endpoint is available.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod convert;
pub mod estimate;
pub mod jsonpath;
pub mod payload;
pub mod reasoning_store;
mod splice;
pub mod thinking;
pub mod toolnames;
pub mod transcode;

#[cfg(test)]
mod testing;

pub use convert::{translate_error, translate_request, translate_response};
pub use estimate::estimate_tokens;
pub use payload::{PayloadCtx, apply_payload_rules};
pub use reasoning_store::{Clock, ManualClock, ReasoningStore, Restored, SystemClock};
pub use thinking::{
    ReasoningInputs, ReasoningPlan, apply_to_body, apply_to_request, effective_label,
    plan_reasoning, plan_with_label,
};
pub use toolnames::{ToolNames, is_valid_tool_name, sanitize_tool_names};
pub use transcode::{CodecRef, Transcoder, rewrite_model_text};
