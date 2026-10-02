//! Carrying opaque provider signatures through a foreign protocol.
//!
//! When a client speaks protocol A and the upstream speaks protocol B, blobs
//! issued by B (thinking signatures, encrypted reasoning, Gemini thought
//! signatures) have to be handed to the client in A's signature slot and come
//! back on the next turn. To keep them from ever being replayed to the wrong
//! vendor they are wrapped with a tag naming their origin:
//!
//! ```text
//! sy1.<tag>.<original blob>
//! ```
//!
//! where `<tag>` is [`Protocol::tag`]. Blobs whose origin family matches the
//! client protocol are delivered unwrapped, so native clients see native data.
//! A blob that arrives *without* a tag is assumed to be native to the protocol
//! it arrived in.

use crate::ir::Signature;
use crate::protocol::Protocol;

/// Prefix of a wrapped signature.
pub const PREFIX: &str = "sy1.";

/// Renders `sig` for delivery to a client speaking `client`.
pub fn encode_for_client(sig: &Signature, client: Protocol) -> String {
    if sig.origin.family() == client.family() {
        sig.data.clone()
    } else {
        format!("{PREFIX}{}.{}", sig.origin.tag(), sig.data)
    }
}

/// Parses a signature string received from a client speaking `client`.
pub fn decode_from_client(raw: &str, client: Protocol) -> Signature {
    if let Some(rest) = raw.strip_prefix(PREFIX) {
        let mut chars = rest.chars();
        if let (Some(tag), Some('.')) = (chars.next(), chars.next())
            && let Some(origin) = Protocol::from_tag(tag)
        {
            return Signature::new(origin, chars.as_str());
        }
    }
    Signature::new(client, raw)
}

/// Cheap test for "this request body contains a wrapped signature". The
/// gateway uses it to decide that a same-protocol request cannot be forwarded
/// verbatim and must be re-encoded so foreign blobs are stripped.
pub fn contains_wrapped(body: &[u8]) -> bool {
    let needle = PREFIX.as_bytes();
    body.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_signatures_are_not_wrapped() {
        let sig = Signature::new(Protocol::Anthropic, "abc==");
        assert_eq!(encode_for_client(&sig, Protocol::Anthropic), "abc==");
        assert_eq!(decode_from_client("abc==", Protocol::Anthropic), sig);
    }

    #[test]
    fn foreign_signatures_round_trip() {
        let sig = Signature::new(Protocol::Gemini, "Cg.x/y+z==");
        let wire = encode_for_client(&sig, Protocol::Anthropic);
        assert_eq!(wire, "sy1.g.Cg.x/y+z==");
        assert_eq!(decode_from_client(&wire, Protocol::Anthropic), sig);
        assert!(contains_wrapped(wire.as_bytes()));
    }

    #[test]
    fn same_family_is_native() {
        let sig = Signature::new(Protocol::OpenaiResponses, "enc");
        assert_eq!(encode_for_client(&sig, Protocol::OpenaiChat), "enc");
    }

    #[test]
    fn malformed_tag_is_treated_as_native() {
        let s = decode_from_client("sy1.zz", Protocol::Gemini);
        assert_eq!(s.origin, Protocol::Gemini);
        assert_eq!(s.data, "sy1.zz");
    }
}
