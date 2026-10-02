//! Review findings for `decode_request` (Chat Completions client body -> IR).
//!
//! Every test here asserts the behaviour the reference notes require. They
//! failed against the reviewed implementation and are kept as regression tests.

mod common;

use common::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use switchyard_core::ir::{MediaSource, Part};

// ---------------------------------------------------------------------------
// Finding: `video_url` content parts are decoded as `Part::Opaque`.
//
// Notes 07 section 1.2 (Chat -> Gemini user content): "`video_url`
// (`video_url.url`) | same as `image_url`" -> `inlineData`. An opaque part is
// only replayed to another Chat upstream; every other codec drops it, so a
// Chat client that sends a clip to a Gemini model has it removed silently.
// The IR has no video variant; the Gemini codec files video media under
// `Part::Document` with a `video/*` media type, which is what a Chat
// `video_url` has to become as well for the clip to reach the model.
// ---------------------------------------------------------------------------

#[test]
fn review_video_url_part_is_decoded_as_media_not_as_an_opaque_block() {
    let request = decode_request(json!({
        "model": "gemini-x",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what happens in this clip?"},
            {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAAIGZ0eXA="}}
        ]}]
    }));
    let parts = &request.messages[0].parts;
    assert_eq!(parts.len(), 2);
    match &parts[1] {
        Part::Document(media) | Part::Image(media) | Part::Audio(media) => {
            assert_eq!(media.media_type.as_deref(), Some("video/mp4"));
            assert_eq!(
                media.source,
                MediaSource::Base64 {
                    data: "AAAAIGZ0eXA=".into()
                }
            );
        }
        other => panic!("the clip must be a media part other codecs can forward, got {other:?}"),
    }
}
