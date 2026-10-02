//! Wire protocols the gateway can speak, on either side of a request.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A wire protocol. The gateway accepts client requests in any of these and can
/// talk to upstreams in any of these; when the two differ the request is
/// translated through the canonical model in [`crate::ir`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// OpenAI Chat Completions (`POST /v1/chat/completions`).
    OpenaiChat,
    /// OpenAI Responses (`POST /v1/responses`, also over WebSocket).
    OpenaiResponses,
    /// Anthropic Messages (`POST /v1/messages`).
    Anthropic,
    /// Google Gemini (`POST /v1beta/models/{model}:generateContent`).
    Gemini,
}

impl Protocol {
    pub const ALL: [Protocol; 4] = [
        Protocol::OpenaiChat,
        Protocol::OpenaiResponses,
        Protocol::Anthropic,
        Protocol::Gemini,
    ];

    /// Stable identifier used in config files, logs and the admin API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenaiChat => "openai-chat",
            Protocol::OpenaiResponses => "openai-responses",
            Protocol::Anthropic => "anthropic",
            Protocol::Gemini => "gemini",
        }
    }

    /// One-letter tag used when an opaque provider signature has to be carried
    /// through a different protocol (see [`crate::sig`]).
    pub const fn tag(self) -> char {
        match self {
            Protocol::OpenaiChat => 'c',
            Protocol::OpenaiResponses => 'r',
            Protocol::Anthropic => 'a',
            Protocol::Gemini => 'g',
        }
    }

    pub const fn from_tag(tag: char) -> Option<Protocol> {
        match tag {
            'c' => Some(Protocol::OpenaiChat),
            'r' => Some(Protocol::OpenaiResponses),
            'a' => Some(Protocol::Anthropic),
            'g' => Some(Protocol::Gemini),
            _ => None,
        }
    }

    /// The vendor family. Opaque blobs (signatures, encrypted reasoning) are
    /// only ever replayed to the family that produced them.
    pub const fn family(self) -> Family {
        match self {
            Protocol::OpenaiChat | Protocol::OpenaiResponses => Family::Openai,
            Protocol::Anthropic => Family::Anthropic,
            Protocol::Gemini => Family::Google,
        }
    }

    /// Human readable name for UIs.
    pub const fn display_name(self) -> &'static str {
        match self {
            Protocol::OpenaiChat => "OpenAI Chat Completions",
            Protocol::OpenaiResponses => "OpenAI Responses",
            Protocol::Anthropic => "Anthropic Messages",
            Protocol::Gemini => "Gemini",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai-chat" | "chat" | "openai" | "chat-completions" => Ok(Protocol::OpenaiChat),
            "openai-responses" | "responses" => Ok(Protocol::OpenaiResponses),
            "anthropic" | "claude" | "messages" => Ok(Protocol::Anthropic),
            "gemini" | "google" => Ok(Protocol::Gemini),
            other => Err(format!("unknown protocol `{other}`")),
        }
    }
}

/// Vendor family of a protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    Openai,
    Anthropic,
    Google,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trip() {
        for p in Protocol::ALL {
            assert_eq!(Protocol::from_tag(p.tag()), Some(p));
            assert_eq!(p.as_str().parse::<Protocol>().unwrap(), p);
        }
    }

    #[test]
    fn serde_names_match_as_str() {
        for p in Protocol::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
        }
    }
}
