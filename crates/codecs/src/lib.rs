//! Registry of the four protocol codecs.
//!
//! The gateway never names a concrete codec; it asks this crate for the codec
//! of a [`Protocol`]. The individual codec crates are re-exported for the few
//! protocol-specific helpers that live outside the [`Codec`] trait (the
//! Responses WebSocket transcript helpers, the legacy completions shim, the
//! Vertex body adapter).

use switchyard_core::{Codec, Protocol};

pub use switchyard_codec_anthropic as anthropic;
pub use switchyard_codec_chat as chat;
pub use switchyard_codec_gemini as gemini;
pub use switchyard_codec_responses as responses;

pub use switchyard_codec_anthropic::AnthropicCodec;
pub use switchyard_codec_chat::ChatCodec;
pub use switchyard_codec_gemini::GeminiCodec;
pub use switchyard_codec_responses::ResponsesCodec;

static CHAT: ChatCodec = ChatCodec;
static RESPONSES: ResponsesCodec = ResponsesCodec;
static ANTHROPIC: AnthropicCodec = AnthropicCodec;
static GEMINI: GeminiCodec = GeminiCodec;

/// The codec for `protocol`.
pub fn codec(protocol: Protocol) -> &'static dyn Codec {
    match protocol {
        Protocol::OpenaiChat => &CHAT,
        Protocol::OpenaiResponses => &RESPONSES,
        Protocol::Anthropic => &ANTHROPIC,
        Protocol::Gemini => &GEMINI,
    }
}

/// All codecs, in [`Protocol::ALL`] order.
pub fn all() -> [&'static dyn Codec; 4] {
    [&CHAT, &RESPONSES, &ANTHROPIC, &GEMINI]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_protocol_has_its_own_codec() {
        for p in Protocol::ALL {
            assert_eq!(codec(p).protocol(), p);
        }
        let protocols: Vec<Protocol> = all().iter().map(|c| c.protocol()).collect();
        assert_eq!(protocols, Protocol::ALL.to_vec());
    }
}
