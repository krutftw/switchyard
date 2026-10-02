//! The scenario library of the cross-protocol matrix.
//!
//! For each of the four protocols as a **client** protocol: native request
//! bodies, written the way that vendor's SDKs and CLIs write them, one per
//! feature the gateway has to carry across ([`requests`]). For each of the
//! four protocols as an **upstream** protocol: canned native responses, as a
//! complete JSON body and as an SSE transcript ([`responses`]).
//!
//! Every scenario states what must be found again on the other side
//! (distinctive texts, tool names, media payloads), so the matrix can check
//! meaning and not only shape.

use serde_json::Value;
use switchyard_core::ir::FinishReason;
use switchyard_core::reasoning::Depth;
use switchyard_core::{Protocol, Usage};

pub mod anthropic;
pub mod chat;
pub mod gemini;
pub mod responses;

/// A 1×1 PNG, base64.
pub const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
/// A different 1×1 PNG, returned by a tool in the tool-image scenarios.
pub const TOOL_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
/// A minimal PDF, base64.
pub const PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFtdIC9Db3VudCAwID4+CmVuZG9iagp0cmFpbGVyCjw8IC9Sb290IDEgMCBSID4+CiUlRU9GCg==";
pub const IMAGE_URL: &str = "https://example.com/images/cat.png";

/// A 100-character tool name: valid for Anthropic (128), too long for OpenAI
/// and Gemini (64).
pub const LONG_TOOL: &str = "workspace_filesystem_operations_read_the_entire_contents_of_a_text_file_from_the_given_absolute_path";
/// A name with a dot, a colon and a dash: valid for Gemini only.
pub const DOTTED_TOOL: &str = "mcp.files:read-file";

/// What a media payload in a scenario is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// An image the user attached by URL.
    ImageUrl,
    /// An image the user attached inline.
    ImageBase64,
    /// A PDF the user attached inline.
    Pdf,
    /// An image returned by a tool.
    ToolImage,
}

/// A media payload with the string that identifies it in a wire body (the
/// URL, or the base64 data).
#[derive(Clone, Copy, Debug)]
pub struct Media {
    pub kind: MediaKind,
    pub marker: &'static str,
}

/// One native client request and what the matrix expects to find again in
/// every upstream body it is translated to.
#[derive(Clone, Debug)]
pub struct ClientRequest {
    pub name: &'static str,
    pub body: Value,
    /// Texts (user, system, tool output, tool arguments) that must reach the
    /// upstream. Chosen without characters JSON escapes.
    pub texts: Vec<&'static str>,
    /// Texts of the leading system prompt. They must reach the upstream in
    /// its system slot.
    pub system: Vec<&'static str>,
    /// Function tools as the client declared them.
    pub tools: Vec<&'static str>,
    pub media: Vec<Media>,
    /// The depth the request's native reasoning fields ask for.
    pub depth: Option<Depth>,
    /// The request sets `stream`.
    pub stream: bool,
}

impl ClientRequest {
    pub fn new(name: &'static str, body: Value) -> Self {
        ClientRequest {
            name,
            body,
            texts: Vec::new(),
            system: Vec::new(),
            tools: Vec::new(),
            media: Vec::new(),
            depth: None,
            stream: false,
        }
    }

    pub fn texts(mut self, texts: &[&'static str]) -> Self {
        self.texts.extend_from_slice(texts);
        self
    }

    pub fn system(mut self, texts: &[&'static str]) -> Self {
        self.system.extend_from_slice(texts);
        self
    }

    pub fn tools(mut self, tools: &[&'static str]) -> Self {
        self.tools.extend_from_slice(tools);
        self
    }

    pub fn media(mut self, kind: MediaKind, marker: &'static str) -> Self {
        self.media.push(Media { kind, marker });
        self
    }

    pub fn depth(mut self, depth: Depth) -> Self {
        self.depth = Some(depth);
        self
    }

    pub fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }
}

/// A tool call an upstream response makes.
#[derive(Clone, Debug)]
pub struct ExpectedCall {
    pub name: &'static str,
    pub arguments: Value,
}

/// What a canned upstream response says, in canonical terms.
#[derive(Clone, Debug)]
pub struct Expect {
    /// Visible answer text, concatenated.
    pub text: &'static str,
    /// Reasoning text, concatenated.
    pub reasoning: &'static str,
    /// Refusal text, when the vendor reports one as content.
    pub refusal: &'static str,
    pub calls: Vec<ExpectedCall>,
    pub finish: FinishReason,
    /// Usage in the canonical (disjoint) buckets; `None` when the response
    /// reports none.
    pub usage: Option<Usage>,
    /// Opaque blobs the vendor issued in this response, verbatim.
    pub blobs: Vec<&'static str>,
}

impl Expect {
    pub fn text(text: &'static str) -> Self {
        Expect {
            text,
            reasoning: "",
            refusal: "",
            calls: Vec::new(),
            finish: FinishReason::Stop,
            usage: None,
            blobs: Vec::new(),
        }
    }

    pub fn reasoning(mut self, reasoning: &'static str) -> Self {
        self.reasoning = reasoning;
        self
    }

    pub fn refusal(mut self, refusal: &'static str) -> Self {
        self.refusal = refusal;
        self
    }

    pub fn call(mut self, name: &'static str, arguments: Value) -> Self {
        self.calls.push(ExpectedCall { name, arguments });
        self.finish = FinishReason::ToolCalls;
        self
    }

    pub fn finish(mut self, finish: FinishReason) -> Self {
        self.finish = finish;
        self
    }

    pub fn usage(
        mut self,
        input: u64,
        cache_read: u64,
        cache_write: u64,
        output: u64,
        reasoning: u64,
    ) -> Self {
        self.usage = Some(Usage {
            input_tokens: input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            output_tokens: output,
            reasoning_tokens: reasoning,
        });
        self
    }

    pub fn blob(mut self, blob: &'static str) -> Self {
        self.blobs.push(blob);
        self
    }
}

/// How a canned stream ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// With the vendor's terminator.
    Complete,
    /// With an in-stream error event after some output.
    Error,
    /// The connection just closes mid-answer.
    Truncated,
}

/// One canned upstream response: the same answer as a complete body and as a
/// stream transcript (either may be missing when the vendor has no such form
/// for it).
#[derive(Clone, Debug)]
pub struct UpstreamResponse {
    pub name: &'static str,
    pub json: Option<Value>,
    pub sse: Option<&'static str>,
    pub end: StreamEnd,
    pub expect: Expect,
}

impl UpstreamResponse {
    pub fn new(name: &'static str, json: Value, sse: &'static str, expect: Expect) -> Self {
        UpstreamResponse {
            name,
            json: Some(json),
            sse: Some(sse),
            end: StreamEnd::Complete,
            expect,
        }
    }

    pub fn stream_only(
        name: &'static str,
        sse: &'static str,
        end: StreamEnd,
        expect: Expect,
    ) -> Self {
        UpstreamResponse {
            name,
            json: None,
            sse: Some(sse),
            end,
            expect,
        }
    }
}

/// The request scenarios of a client protocol.
pub fn requests(client: Protocol) -> Vec<ClientRequest> {
    match client {
        Protocol::OpenaiChat => chat::requests(),
        Protocol::OpenaiResponses => responses::requests(),
        Protocol::Anthropic => anthropic::requests(),
        Protocol::Gemini => gemini::requests(),
    }
}

/// The canned responses of an upstream protocol.
pub fn responses(upstream: Protocol) -> Vec<UpstreamResponse> {
    match upstream {
        Protocol::OpenaiChat => chat::responses(),
        Protocol::OpenaiResponses => responses::responses(),
        Protocol::Anthropic => anthropic::responses(),
        Protocol::Gemini => gemini::responses(),
    }
}

/// A request of each client protocol that declares the two tools the canned
/// responses call (`get_weather`, `search_docs`) and asks for usage in
/// streams. It is the `ClientCtx::request` of the response-side matrix.
pub fn tool_request(client: Protocol) -> Value {
    match client {
        Protocol::OpenaiChat => chat::tool_request(),
        Protocol::OpenaiResponses => responses::tool_request(),
        Protocol::Anthropic => anthropic::tool_request(),
        Protocol::Gemini => gemini::tool_request(),
    }
}

/// The opening user question of the tool scenarios.
pub const WEATHER_QUESTION: &str = "What is the weather in Paris and in Tokyo right now?";
