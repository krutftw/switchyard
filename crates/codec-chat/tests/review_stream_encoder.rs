//! Review findings for the stream encoder (IR -> `chat.completion.chunk`).
//!
//! Every test here asserts the correct behaviour. They failed against the
//! reviewed implementation and are kept as regression tests.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_codec_chat::ChatCodec;
use switchyard_core::codec::{ClientCtx, Codec};
use switchyard_core::error::ApiError;
use switchyard_core::stream::StreamEvent;

// ---------------------------------------------------------------------------
// Finding: an `Error` handed to an encoder that has not seen a `Start`
// produces a role chunk before the error frame.
//
// `translate::Transcoder::fail` (idle timeout, broken upstream connection,
// shutdown) and `Transcoder::finish` with `report_truncation` render the
// in-stream error of a PASSTHROUGH stream with a throwaway encoder
// (`codec.stream_encoder(&ClientCtx::new(model))`, then `encode(Error)`,
// then `finish()`), relying on "every protocol's error event is
// self-contained". The client has already received the upstream's chunks, so
// the extra chunk arrives mid-stream with a freshly minted `id` and `created`
// and a second `role` delta, although a Chat stream carries the same id on
// every chunk (notes 15 section 3.3: "id string, same on every chunk").
// The same happens in translation mode for a stream that never started.
// The only thing on the wire must be the error frame.
// ---------------------------------------------------------------------------

#[test]
fn review_error_on_a_fresh_encoder_is_only_the_error_frame() {
    let mut encoder = ChatCodec.stream_encoder(&ClientCtx::new("alias"));
    let mut wire = encoder.encode(&StreamEvent::Error(ApiError::timeout(
        "upstream sent no data for 60 seconds",
    )));
    wire.extend(encoder.finish());
    assert_eq!(
        payloads(&wire),
        vec![json!({"error": {
            "message": "upstream sent no data for 60 seconds",
            "type": "server_error",
            "param": null,
            "code": "request_timeout"
        }})]
    );
}
