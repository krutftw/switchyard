//! The canonical ("IR") request and response model.
//!
//! Every wire protocol is decoded into these types and encoded back out of
//! them, so adding a protocol costs one codec instead of one translator per
//! protocol pair. The model is a superset shaped after the most structured of
//! the supported protocols:
//!
//! * conversation turns are [`Message`]s made of typed [`Part`]s;
//! * tool results are parts of a **user** message (as in Anthropic/Gemini),
//!   never a separate role;
//! * system/developer instructions that precede the conversation live in
//!   [`Request::system`]; ones that appear mid-conversation stay in
//!   [`Request::messages`] with [`Role::System`];
//! * opaque provider blobs (thinking signatures, encrypted reasoning, Gemini
//!   thought signatures) are carried as [`Signature`]s that remember which
//!   protocol family issued them, so they are never replayed to another vendor.
//!
//! Anything a protocol can express that the IR cannot is dropped during
//! cross-protocol translation. Same-protocol requests never go through the IR
//! on the hot path: they are forwarded verbatim (see the gateway crate).

use crate::protocol::Protocol;
use crate::reasoning::ReasoningConfig;
use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A canonical generation request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Model name as requested by the client (before alias resolution) or, once
    /// routed, the upstream model id. Encoders write this value verbatim.
    pub model: String,
    /// Whether the caller wants a streamed response.
    #[serde(default)]
    pub stream: bool,
    /// Leading system / developer instructions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<Part>,
    /// Conversation turns, oldest first.
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// `Some(false)` asks the model to emit at most one tool call per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Upper bound on generated tokens (visible output plus reasoning, with the
    /// semantics of the target protocol).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Number of candidates (`n` / `candidateCount`). Only `None`/`Some(1)` is
    /// supported across protocols; other values survive same-family encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// End-user identifier (`user`, `metadata.user_id`, `safety_identifier`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Free-form request metadata (string map in every protocol that has it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Client supplied prompt-cache routing key (OpenAI `prompt_cache_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// OpenAI Responses `previous_response_id`. Only meaningful when the
    /// upstream is itself a stateful Responses endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// OpenAI `store` flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Protocol the request was decoded from.
    pub source: Protocol,
    /// Top-level fields of the source body the IR has no slot for. An encoder
    /// may copy entries it recognises when `source` is in its own vendor
    /// family; it must ignore them otherwise.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Request {
    /// An empty request for `model`, decoded from `source`.
    pub fn new(model: impl Into<String>, source: Protocol) -> Self {
        Request {
            model: model.into(),
            stream: false,
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            seed: None,
            presence_penalty: None,
            frequency_penalty: None,
            stop: Vec::new(),
            candidate_count: None,
            reasoning: None,
            response_format: None,
            user: None,
            metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            previous_response_id: None,
            store: None,
            source,
            extra: Map::new(),
        }
    }

    /// Concatenated text of the leading system instructions.
    pub fn system_text(&self) -> String {
        join_text(&self.system, "\n\n")
    }

    /// True when any message carries a tool call or a tool result.
    pub fn has_tool_traffic(&self) -> bool {
        self.messages.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::ToolCall(_) | Part::ToolResult(_)))
        })
    }

    /// Looks up the name of the tool call with the given id anywhere in the
    /// conversation. Protocols without call ids (Gemini) need the name to
    /// address a tool result.
    pub fn tool_name_for_call(&self, call_id: &str) -> Option<&str> {
        self.messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .find_map(|p| match p {
                Part::ToolCall(c) if c.id == call_id => Some(c.name.as_str()),
                _ => None,
            })
    }
}

/// Who authored a [`Message`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A system/developer instruction that appears *inside* the conversation.
    /// Leading instructions belong in [`Request::system`] instead.
    System,
    /// The end user — including tool results being returned to the model.
    User,
    /// The model.
    Assistant,
}

/// One conversation turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    /// Optional participant name (OpenAI `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Message {
            role,
            parts,
            name: None,
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Message::new(Role::User, vec![Part::text(text)])
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message::new(Role::Assistant, vec![Part::text(text)])
    }

    /// Concatenated text parts of this message.
    pub fn text(&self) -> String {
        join_text(&self.parts, "")
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolResult(r) => Some(r),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------

/// A typed piece of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text(TextPart),
    Image(MediaPart),
    Audio(MediaPart),
    /// A document or generic file (PDF, plain text, …).
    Document(MediaPart),
    /// The model asks for a tool to be run (assistant messages only).
    ToolCall(ToolCall),
    /// The outcome of a tool call (user messages only).
    ToolResult(ToolResult),
    /// Model reasoning / thinking (assistant messages only).
    Reasoning(Reasoning),
    /// The model declined to answer.
    Refusal(RefusalPart),
    /// A provider-specific block the IR does not model (server tool activity,
    /// search results, …). Re-emitted verbatim only to the protocol family that
    /// produced it and dropped for everyone else.
    Opaque(OpaquePart),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Part {
        Part::Text(TextPart::new(text))
    }

    pub fn reasoning(text: impl Into<String>) -> Part {
        Part::Reasoning(Reasoning {
            text: text.into(),
            ..Reasoning::default()
        })
    }

    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Part {
        Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            kind: ToolCallKind::Function,
            signature: None,
            cache_control: None,
        })
    }

    pub fn tool_result_text(call_id: impl Into<String>, text: impl Into<String>) -> Part {
        Part::ToolResult(ToolResult {
            call_id: call_id.into(),
            name: None,
            content: vec![Part::text(text)],
            is_error: false,
            cache_control: None,
        })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Part::Text(t) => Some(t.text.as_str()),
            _ => None,
        }
    }

    /// The Anthropic-style cache breakpoint attached to this part, if any.
    pub fn cache_control(&self) -> Option<&Value> {
        match self {
            Part::Text(t) => t.cache_control.as_ref(),
            Part::Image(m) | Part::Audio(m) | Part::Document(m) => m.cache_control.as_ref(),
            Part::ToolCall(c) => c.cache_control.as_ref(),
            Part::ToolResult(r) => r.cache_control.as_ref(),
            Part::Reasoning(_) | Part::Refusal(_) | Part::Opaque(_) => None,
        }
    }
}

/// Plain text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    /// Anthropic `cache_control` marker, kept verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    /// Source citations attached to this text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<Citation>,
    /// Opaque signature attached to a text part (Gemini `thoughtSignature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl TextPart {
    pub fn new(text: impl Into<String>) -> Self {
        TextPart {
            text: text.into(),
            ..TextPart::default()
        }
    }
}

/// A source reference for generated text (web search, grounding, documents).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Quoted source text, when the provider supplies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
    /// Character range in the text part this citation supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

/// Binary or referenced media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaPart {
    pub source: MediaSource,
    /// IANA media type such as `image/png` or `application/pdf`. For
    /// [`MediaSource::Url`] it may be unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// OpenAI image `detail` hint (`low` / `high` / `auto`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

impl MediaPart {
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        MediaPart {
            source: MediaSource::Base64 { data: data.into() },
            media_type: Some(media_type.into()),
            filename: None,
            detail: None,
            cache_control: None,
        }
    }

    pub fn url(url: impl Into<String>) -> Self {
        MediaPart {
            source: MediaSource::Url { url: url.into() },
            media_type: None,
            filename: None,
            detail: None,
            cache_control: None,
        }
    }

    /// Renders the media as a `data:` URI when it is inline, or returns the
    /// remote URL. `None` for provider file references.
    pub fn as_url(&self) -> Option<String> {
        match &self.source {
            MediaSource::Base64 { data } => Some(format!(
                "data:{};base64,{}",
                self.media_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
                data
            )),
            MediaSource::Url { url } => Some(url.clone()),
            MediaSource::FileRef { .. } => None,
        }
    }

    /// Builds a media part from a URL, splitting `data:` URIs into media type
    /// and base64 payload.
    pub fn from_url(url: &str) -> Self {
        if let Some((media_type, data)) = parse_data_uri(url) {
            MediaPart::base64(media_type, data)
        } else {
            MediaPart::url(url)
        }
    }
}

/// Where a [`MediaPart`]'s bytes live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    /// Standard (non URL-safe) base64 without a `data:` prefix.
    Base64 { data: String },
    /// A remote `http(s)` URL.
    Url { url: String },
    /// A provider-side file handle (OpenAI `file_id`, Anthropic Files API id,
    /// Gemini `fileData.fileUri`). Only valid for the family that issued it.
    FileRef { id: String },
}

/// Splits `data:<media-type>;base64,<payload>` into its two halves.
pub fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mut pieces = meta.split(';');
    let media_type = pieces.next().unwrap_or("").trim();
    if !pieces.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    let media_type = if media_type.is_empty() {
        "application/octet-stream"
    } else {
        media_type
    };
    Some((media_type.to_string(), data.to_string()))
}

/// Flavour of a tool call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    /// JSON-argument function call.
    #[default]
    Function,
    /// Free-form ("custom") tool call whose input is raw text rather than
    /// JSON (OpenAI Responses custom tools). `arguments` holds the raw input.
    Custom,
}

/// A request from the model to run a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Call id. Decoders synthesise one (see [`crate::util::new_call_id`]) for
    /// protocols that have none.
    pub id: String,
    pub name: String,
    /// Arguments exactly as the model produced them: a JSON document for
    /// function calls (possibly empty, which means `{}`), raw text for custom
    /// tool calls.
    pub arguments: String,
    #[serde(default)]
    pub kind: ToolCallKind,
    /// Opaque signature bound to this call (Gemini `thoughtSignature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

impl ToolCall {
    /// Arguments parsed as a JSON object. Empty arguments and a literal `null`
    /// become `{}`; any other non-object or unparsable input is wrapped as
    /// `{"input": <value or raw text>}` so nothing is lost.
    pub fn arguments_value(&self) -> Value {
        let trimmed = self.arguments.trim();
        if trimmed.is_empty() {
            return Value::Object(Map::new());
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v @ Value::Object(_)) => v,
            Ok(Value::Null) => Value::Object(Map::new()),
            Ok(other) => {
                let mut m = Map::new();
                m.insert("input".to_string(), other);
                Value::Object(m)
            }
            Err(_) => {
                let mut m = Map::new();
                m.insert("input".to_string(), Value::String(self.arguments.clone()));
                Value::Object(m)
            }
        }
    }
}

/// The outcome of a tool call, returned to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the [`ToolCall`] this answers.
    pub call_id: String,
    /// Tool name when the source protocol supplies it (Gemini). Encoders that
    /// need a name and find `None` resolve it with
    /// [`Request::tool_name_for_call`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Result payload: text and, where supported, images/documents.
    pub content: Vec<Part>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

impl ToolResult {
    /// Concatenated text of the result.
    pub fn text(&self) -> String {
        join_text(&self.content, "")
    }
}

/// Model reasoning ("thinking").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    /// Provider item id (OpenAI Responses `rs_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Visible reasoning text or summary. May be empty.
    #[serde(default)]
    pub text: String,
    /// The blob that lets the provider verify/restore this reasoning on a
    /// later turn: Anthropic `signature`, Anthropic `redacted_thinking.data`,
    /// OpenAI `encrypted_content`, Gemini `thoughtSignature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    /// True when the provider withheld the text entirely (Anthropic
    /// `redacted_thinking`); `signature` then holds the encrypted payload.
    #[serde(default)]
    pub redacted: bool,
}

/// An opaque blob issued by a provider that must be replayed verbatim, and
/// only to the vendor family that issued it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub origin: Protocol,
    pub data: String,
}

impl Signature {
    pub fn new(origin: Protocol, data: impl Into<String>) -> Self {
        Signature {
            origin,
            data: data.into(),
        }
    }

    /// Whether this blob may be sent to an upstream speaking `target`.
    pub fn valid_for(&self, target: Protocol) -> bool {
        self.origin.family() == target.family()
    }
}

/// A model refusal.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RefusalPart {
    pub text: String,
}

/// A provider-specific content block carried verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpaquePart {
    pub origin: Protocol,
    /// The block exactly as it appeared on the wire.
    pub raw: Value,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A tool offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    Function(FunctionTool),
    /// Free-form tool (OpenAI Responses `custom`).
    Custom(CustomTool),
    /// A tool executed by the provider itself.
    Builtin(BuiltinTool),
}

impl Tool {
    /// Client-visible tool name, when it has one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Tool::Function(f) => Some(f.name.as_str()),
            Tool::Custom(c) => Some(c.name.as_str()),
            Tool::Builtin(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the arguments object. `Value::Null` means "no
    /// parameters"; encoders substitute `{"type":"object","properties":{}}`
    /// where a schema is mandatory.
    #[serde(default)]
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

impl FunctionTool {
    /// The parameters schema, or the canonical empty-object schema.
    pub fn parameters_or_empty(&self) -> Value {
        if self.parameters.is_null() {
            empty_object_schema()
        } else {
            self.parameters.clone()
        }
    }
}

/// `{"type":"object","properties":{}}`
pub fn empty_object_schema() -> Value {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::String("object".to_string()));
    m.insert("properties".to_string(), Value::Object(Map::new()));
    Value::Object(m)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Input grammar / format specification, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
}

/// A provider-executed tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuiltinTool {
    pub kind: BuiltinKind,
    /// Protocol the declaration came from.
    pub origin: Protocol,
    /// The declaration exactly as the client sent it. Encoders in the same
    /// family emit it verbatim; encoders of other families emit their own
    /// default declaration for [`BuiltinTool::kind`], or drop the tool when
    /// they have no equivalent.
    pub raw: Value,
}

/// Provider-executed tool categories that exist in more than one vendor's API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinKind {
    WebSearch,
    /// Fetch a specific URL (Anthropic `web_fetch`, Gemini `urlContext`).
    WebFetch,
    CodeExecution,
    /// Anything else, identified by its wire `type`.
    Other(String),
}

/// How the model may use tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// The model must not call tools.
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call this specific tool.
    Tool { name: String },
}

/// Output format constraint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    /// Any syntactically valid JSON object.
    JsonObject,
    /// JSON constrained by a schema.
    JsonSchema {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A canonical, complete (non-streamed) generation result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Upstream response id. Encoders re-prefix it if their protocol expects a
    /// particular shape (`chatcmpl-…`, `msg_…`, `resp_…`).
    pub id: String,
    /// Model that produced the response.
    pub model: String,
    /// Unix seconds.
    #[serde(default)]
    pub created: i64,
    /// Assistant output in order.
    pub parts: Vec<Part>,
    pub finish: FinishReason,
    /// The stop sequence that ended generation, when `finish` is `Stop` because
    /// of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

impl Response {
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Response {
            id: id.into(),
            model: model.into(),
            created: 0,
            parts: Vec::new(),
            finish: FinishReason::Stop,
            stop_sequence: None,
            usage: Usage::default(),
            service_tier: None,
        }
    }

    /// Concatenated visible text.
    pub fn text(&self) -> String {
        join_text(&self.parts, "")
    }

    /// Concatenated reasoning text.
    pub fn reasoning_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let Part::Reasoning(r) = p {
                out.push_str(&r.text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
    }
}

/// Why generation ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of turn or a stop sequence.
    Stop,
    /// Hit the output token limit.
    Length,
    /// The model wants tools run.
    ToolCalls,
    /// Blocked by a safety system.
    ContentFilter,
    /// The model refused.
    Refusal,
    /// The provider paused a long-running turn (Anthropic `pause_turn`); the
    /// client should send the response back to continue.
    PauseTurn,
    /// The input exceeded the model's context window.
    ContextWindow,
    /// Generation failed mid-flight.
    Error,
    /// A reason with no canonical equivalent, kept as the upstream spelled it.
    Other(String),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn join_text(parts: &[Part], sep: &str) -> String {
    let mut out = String::new();
    for p in parts {
        if let Part::Text(t) = p {
            if !out.is_empty() && !sep.is_empty() {
                out.push_str(sep);
            }
            out.push_str(&t.text);
        }
    }
    out
}

/// Normalises a conversation for protocols with strict turn structure
/// (Anthropic, Gemini):
///
/// 1. drops messages with no parts;
/// 2. merges consecutive messages of the same role into one;
/// 3. within each user message, moves tool results ahead of other content
///    (stable otherwise), because a tool result must directly follow the
///    assistant turn that requested it.
///
/// [`Role::System`] messages are left where they are and never merged; the
/// caller decides what to do with them.
pub fn normalize_turns(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.parts.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.role == msg.role && msg.role != Role::System => {
                last.parts.extend(msg.parts.iter().cloned());
            }
            _ => out.push(msg.clone()),
        }
    }
    for msg in &mut out {
        if msg.role == Role::User && msg.parts.iter().any(|p| matches!(p, Part::ToolResult(_))) {
            let (results, rest): (Vec<Part>, Vec<Part>) = std::mem::take(&mut msg.parts)
                .into_iter()
                .partition(|p| matches!(p, Part::ToolResult(_)));
            msg.parts = results;
            msg.parts.extend(rest);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn data_uri_parsing() {
        assert_eq!(
            parse_data_uri("data:image/png;base64,AAAA"),
            Some(("image/png".to_string(), "AAAA".to_string()))
        );
        assert_eq!(
            parse_data_uri("data:;base64,AAAA"),
            Some(("application/octet-stream".to_string(), "AAAA".to_string()))
        );
        assert_eq!(parse_data_uri("data:text/plain,hello"), None);
        assert_eq!(parse_data_uri("https://example.com/a.png"), None);
    }

    #[test]
    fn media_url_round_trip() {
        let m = MediaPart::from_url("data:image/jpeg;base64,Zm9v");
        assert_eq!(m.media_type.as_deref(), Some("image/jpeg"));
        assert_eq!(m.as_url().unwrap(), "data:image/jpeg;base64,Zm9v");
        let m = MediaPart::from_url("https://example.com/x.png");
        assert_eq!(m.as_url().unwrap(), "https://example.com/x.png");
    }

    #[test]
    fn arguments_value_is_always_an_object() {
        let mut call = match Part::tool_call("c1", "f", "") {
            Part::ToolCall(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(call.arguments_value(), serde_json::json!({}));
        call.arguments = "{\"a\":1}".into();
        assert_eq!(call.arguments_value(), serde_json::json!({"a":1}));
        call.arguments = "[1,2]".into();
        assert_eq!(call.arguments_value(), serde_json::json!({"input":[1,2]}));
        call.arguments = "not json".into();
        assert_eq!(
            call.arguments_value(),
            serde_json::json!({"input":"not json"})
        );
    }

    #[test]
    fn normalize_merges_and_orders_tool_results_first() {
        let msgs = vec![
            Message::user_text("hi"),
            Message::new(Role::Assistant, vec![Part::tool_call("c1", "f", "{}")]),
            Message::user_text("and also"),
            Message::new(Role::User, vec![Part::tool_result_text("c1", "42")]),
            Message::new(Role::User, vec![]),
        ];
        let out = normalize_turns(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].role, Role::User);
        assert!(matches!(out[2].parts[0], Part::ToolResult(_)));
        assert_eq!(out[2].parts[1].as_text(), Some("and also"));
    }

    #[test]
    fn signature_family_scoping() {
        let s = Signature::new(Protocol::OpenaiResponses, "x");
        assert!(s.valid_for(Protocol::OpenaiChat));
        assert!(!s.valid_for(Protocol::Anthropic));
    }

    #[test]
    fn request_serde_round_trip() {
        let mut req = Request::new("m", Protocol::Anthropic);
        req.system.push(Part::text("be brief"));
        req.messages.push(Message::user_text("hello"));
        req.tool_choice = Some(ToolChoice::Tool { name: "f".into() });
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }
}
