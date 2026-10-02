//! Tool names across vendor alphabets.
//!
//! Every vendor restricts what a function tool may be called, and the
//! restrictions differ:
//!
//! | target protocol | accepted names |
//! |---|---|
//! | OpenAI Chat, OpenAI Responses, Anthropic | `^[a-zA-Z0-9_-]{1,64}$` |
//! | Gemini | a letter or `_`, then `[a-zA-Z0-9_.:-]`, at most 64 characters |
//!
//! A client of one protocol may therefore declare a tool the upstream of
//! another protocol refuses (`mcp.server:get-data` is fine for Gemini and a
//! 400 for OpenAI; `1password_lookup` is the reverse). On the translation
//! path the gateway rewrites such names before encoding the upstream request
//! and puts the client's own names back into whatever the upstream answers:
//!
//! 1. [`sanitize_tool_names`] rewrites every name in an IR request that is
//!    invalid for the target protocol — tool declarations, a forced tool
//!    choice, tool calls and tool-result names in the history — to a valid,
//!    collision-free name, and returns the [`ToolNames`] map;
//! 2. [`ToolNames::restore_response`] (complete responses) and
//!    [`ToolNames::restore_event`] (streams; the
//!    [`crate::transcode::Transcoder`] calls it when given the map) put the
//!    original names back.
//!
//! Names that are valid for the target are never touched, so a request whose
//! names are all acceptable passes through unchanged and yields an empty map.
//!
//! # The rewrite
//!
//! Characters outside the target's alphabet become `_`; for Gemini a name
//! that does not start with a letter or `_` gets a leading `_`; the result
//! is cut to 64 characters. When two names would end up the same — an
//! invalid name whose rewrite equals another tool's name, or two invalid
//! names with the same rewrite — each of the rewritten ones gets a suffix
//! derived from a hash of its original name (`_` plus eight hex digits), so
//! the mapping stays one-to-one and does not depend on the order in which
//! the tools are declared. The rewrite of a name is therefore the same on
//! every turn of a conversation, which keeps provider prompt caches valid.
//!
//! Empty names are left alone: there is nothing to derive a name from, and
//! the codecs decide what an anonymous tool means for their protocol.

use std::collections::{HashMap, HashSet};
use switchyard_core::ir::{Part, Request, Response, Tool, ToolChoice};
use switchyard_core::protocol::{Family, Protocol};
use switchyard_core::stream::{BlockStart, StreamEvent};

/// Longest tool name every supported vendor accepts.
pub const MAX_TOOL_NAME_CHARS: usize = 64;

/// Length of the disambiguating suffix: `_` plus eight hex digits.
const SUFFIX_CHARS: usize = 9;

/// Whether `c` may appear in a tool name of the target's vendor (anywhere
/// but, for Gemini, the first position).
fn is_name_char(c: char, family: Family) -> bool {
    match family {
        Family::Openai | Family::Anthropic => c.is_ascii_alphanumeric() || c == '_' || c == '-',
        Family::Google => c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'),
    }
}

/// Whether `c` may start a tool name of the target's vendor.
fn is_first_char(c: char, family: Family) -> bool {
    match family {
        Family::Openai | Family::Anthropic => is_name_char(c, family),
        Family::Google => c.is_ascii_alphabetic() || c == '_',
    }
}

/// Whether an upstream speaking `target` accepts `name` as a function name.
pub fn is_valid_tool_name(name: &str, target: Protocol) -> bool {
    let family = target.family();
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_first_char(first, family)
        && chars.all(|c| is_name_char(c, family))
        && name.chars().count() <= MAX_TOOL_NAME_CHARS
}

/// The plain rewrite of `name` for `target`: invalid characters replaced,
/// the first character fixed, cut to 64 characters. Not yet collision-free.
fn rewrite(name: &str, family: Family) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if is_name_char(c, family) { c } else { '_' })
        .collect();
    if !out.chars().next().is_some_and(|c| is_first_char(c, family)) {
        out.insert(0, '_');
    }
    // Only ASCII is left at this point, so bytes are characters.
    out.truncate(MAX_TOOL_NAME_CHARS);
    out
}

/// 64-bit FNV-1a. Stable across processes and platforms, which the standard
/// library's hasher is not; the rewritten names end up in upstream prompt
/// caches.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `base` with a suffix derived from `original` (and `salt`, to step past
/// the vanishingly rare suffix that is taken as well), within 64 characters.
fn with_suffix(base: &str, original: &str, salt: u32) -> String {
    let mut seed = original.as_bytes().to_vec();
    if salt > 0 {
        seed.extend_from_slice(&salt.to_le_bytes());
    }
    // Folding the two halves keeps all 64 bits involved in the 32 shown.
    let hash = fnv1a64(&seed);
    let folded = (hash >> 32) as u32 ^ hash as u32;
    let mut out = base.to_string();
    out.truncate(MAX_TOOL_NAME_CHARS - SUFFIX_CHARS);
    out.push('_');
    out.push_str(&format!("{folded:08x}"));
    out
}

/// The names a request was rewritten with, for putting the client's own
/// names back into the upstream's answer. See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolNames {
    /// Rewritten name → the name the client used.
    originals: HashMap<String, String>,
}

impl ToolNames {
    /// A map that restores nothing.
    pub fn new() -> Self {
        ToolNames::default()
    }

    /// True when no name was rewritten.
    pub fn is_empty(&self) -> bool {
        self.originals.is_empty()
    }

    /// Number of rewritten names.
    pub fn len(&self) -> usize {
        self.originals.len()
    }

    /// The client's name for a rewritten name, if `name` is one.
    pub fn original(&self, name: &str) -> Option<&str> {
        self.originals.get(name).map(String::as_str)
    }

    /// The `(rewritten, original)` pairs, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.originals
            .iter()
            .map(|(rewritten, original)| (rewritten.as_str(), original.as_str()))
    }

    /// Replaces `name` by the client's name when it is a rewritten one.
    /// Returns whether it was.
    pub fn restore_name(&self, name: &mut String) -> bool {
        match self.originals.get(name.as_str()) {
            Some(original) => {
                name.clone_from(original);
                true
            }
            None => false,
        }
    }

    fn restore_part(&self, part: &mut Part) -> usize {
        match part {
            Part::ToolCall(call) => usize::from(self.restore_name(&mut call.name)),
            Part::ToolResult(result) => {
                let own = result
                    .name
                    .as_mut()
                    .map_or(0, |name| usize::from(self.restore_name(name)));
                own + result
                    .content
                    .iter_mut()
                    .map(|inner| self.restore_part(inner))
                    .sum::<usize>()
            }
            _ => 0,
        }
    }

    /// Puts the client's names back into the tool calls of a decoded
    /// upstream response. Returns the number of names restored.
    pub fn restore_response(&self, response: &mut Response) -> usize {
        if self.is_empty() {
            return 0;
        }
        response
            .parts
            .iter_mut()
            .map(|part| self.restore_part(part))
            .sum()
    }

    /// Puts the client's name back into a canonical stream event that opens
    /// a tool call ([`BlockStart::ToolCall`], or a [`BlockStart::Whole`]
    /// carrying a tool call). Every other event is left alone. Returns
    /// whether a name was restored.
    pub fn restore_event(&self, event: &mut StreamEvent) -> bool {
        if self.is_empty() {
            return false;
        }
        match event {
            StreamEvent::BlockStart {
                block: BlockStart::ToolCall { name, .. },
                ..
            } => self.restore_name(name),
            StreamEvent::BlockStart {
                block: BlockStart::Whole { part },
                ..
            } => self.restore_part(part) > 0,
            _ => false,
        }
    }
}

/// Visits every tool name of a request: declarations, the forced tool
/// choice, tool calls and tool-result names in the history.
fn for_each_name(request: &mut Request, visit: &mut dyn FnMut(&mut String)) {
    fn visit_parts(parts: &mut [Part], visit: &mut dyn FnMut(&mut String)) {
        for part in parts {
            match part {
                Part::ToolCall(call) => visit(&mut call.name),
                Part::ToolResult(result) => {
                    if let Some(name) = result.name.as_mut() {
                        visit(name);
                    }
                    visit_parts(&mut result.content, visit);
                }
                _ => {}
            }
        }
    }

    for tool in &mut request.tools {
        match tool {
            Tool::Function(function) => visit(&mut function.name),
            Tool::Custom(custom) => visit(&mut custom.name),
            // Provider-executed tools are named by the vendor, not the client.
            Tool::Builtin(_) => {}
        }
    }
    if let Some(ToolChoice::Tool { name }) = request.tool_choice.as_mut() {
        visit(name);
    }
    for message in &mut request.messages {
        visit_parts(&mut message.parts, visit);
    }
}

/// Rewrites every tool name of `request` that an upstream speaking `target`
/// would refuse and returns the map that restores them. See the module docs
/// for what is rewritten and how.
///
/// Call it on the request that is about to be encoded with the target
/// codec (`Codec::encode_request`); names valid for `target` and empty
/// names are not touched.
pub fn sanitize_tool_names(request: &mut Request, target: Protocol) -> ToolNames {
    let family = target.family();

    // Pass 1: which names are there, and which of them stay as they are.
    let mut valid: HashSet<String> = HashSet::new();
    let mut invalid: Vec<String> = Vec::new();
    let mut seen_invalid: HashSet<String> = HashSet::new();
    for_each_name(request, &mut |name: &mut String| {
        if name.is_empty() {
            return;
        }
        if is_valid_tool_name(name, target) {
            valid.insert(name.clone());
        } else if seen_invalid.insert(name.clone()) {
            invalid.push(name.clone());
        }
    });
    if invalid.is_empty() {
        return ToolNames::default();
    }

    // Pass 2: the plain rewrite of each invalid name, and how many names
    // want each rewrite.
    let plain: Vec<String> = invalid.iter().map(|name| rewrite(name, family)).collect();
    let mut claims: HashMap<&str, usize> = HashMap::new();
    for candidate in &plain {
        *claims.entry(candidate.as_str()).or_insert(0) += 1;
    }

    // Pass 3: assign. Uncontested rewrites are taken first so that a
    // suffixed name can never displace one of them.
    let mut taken: HashSet<String> = valid;
    let mut renamed: HashMap<String, String> = HashMap::with_capacity(invalid.len());
    let mut contested: Vec<usize> = Vec::new();
    for (index, candidate) in plain.iter().enumerate() {
        let alone = claims.get(candidate.as_str()).copied().unwrap_or(0) == 1;
        if alone && !taken.contains(candidate) {
            taken.insert(candidate.clone());
            renamed.insert(invalid[index].clone(), candidate.clone());
        } else {
            contested.push(index);
        }
    }
    // Sorted by original name so the outcome does not depend on the order
    // in which the client declared its tools.
    contested.sort_by(|a, b| invalid[*a].cmp(&invalid[*b]));
    for index in contested {
        let original = &invalid[index];
        let mut salt = 0u32;
        let unique = loop {
            let candidate = with_suffix(&plain[index], original, salt);
            if !taken.contains(&candidate) {
                break candidate;
            }
            salt = salt.wrapping_add(1);
        };
        taken.insert(unique.clone());
        renamed.insert(original.clone(), unique);
    }

    // Pass 4: apply.
    for_each_name(request, &mut |name: &mut String| {
        if let Some(new) = renamed.get(name.as_str()) {
            name.clone_from(new);
        }
    });

    ToolNames {
        originals: renamed
            .into_iter()
            .map(|(original, rewritten)| (rewritten, original))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::ir::{
        BuiltinKind, BuiltinTool, CustomTool, FunctionTool, Message, Role, ToolCallKind, ToolResult,
    };

    fn function(name: &str) -> Tool {
        Tool::Function(FunctionTool {
            name: name.to_string(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        })
    }

    fn request_with(names: &[&str]) -> Request {
        let mut request = Request::new("m", Protocol::OpenaiChat);
        request.tools = names.iter().map(|name| function(name)).collect();
        request
    }

    fn declared(request: &Request) -> Vec<&str> {
        request.tools.iter().filter_map(Tool::name).collect()
    }

    // ----- validity --------------------------------------------------------

    #[test]
    fn validity_per_vendor() {
        let openai_like = [
            Protocol::OpenaiChat,
            Protocol::OpenaiResponses,
            Protocol::Anthropic,
        ];
        for target in openai_like {
            for name in [
                "get_weather",
                "a",
                "A-b_9",
                "9lives",
                "-dash",
                &"x".repeat(64),
            ] {
                assert!(is_valid_tool_name(name, target), "{name} for {target}");
            }
            for name in ["", "a.b", "ns:tool", "has space", "naïve", &"x".repeat(65)] {
                assert!(!is_valid_tool_name(name, target), "{name} for {target}");
            }
        }
        for name in ["get_weather", "_x", "mcp.server:get-data", &"x".repeat(64)] {
            assert!(is_valid_tool_name(name, Protocol::Gemini), "{name}");
        }
        for name in [
            "",
            "9lives",
            "-dash",
            ".dot",
            "has space",
            "a/b",
            &"x".repeat(65),
        ] {
            assert!(!is_valid_tool_name(name, Protocol::Gemini), "{name}");
        }
    }

    // ----- no-op -----------------------------------------------------------

    #[test]
    fn valid_names_are_untouched_and_the_map_is_empty() {
        for target in Protocol::ALL {
            let mut request = request_with(&["get_weather", "lookup", "Search_2"]);
            request.tool_choice = Some(ToolChoice::Tool {
                name: "lookup".into(),
            });
            request.messages = vec![
                Message::new(Role::Assistant, vec![Part::tool_call("c1", "lookup", "{}")]),
                Message::new(Role::User, vec![Part::tool_result_text("c1", "ok")]),
            ];
            let before = request.clone();
            let names = sanitize_tool_names(&mut request, target);
            assert!(names.is_empty(), "{target}");
            assert_eq!(names.len(), 0);
            assert_eq!(request, before, "{target}");
        }
    }

    #[test]
    fn a_request_without_tools_is_untouched() {
        let mut request = Request::new("m", Protocol::Anthropic);
        request.messages.push(Message::user_text("hi"));
        let before = request.clone();
        assert!(sanitize_tool_names(&mut request, Protocol::Gemini).is_empty());
        assert_eq!(request, before);
    }

    // ----- rewriting -------------------------------------------------------

    #[test]
    fn gemini_names_are_rewritten_for_openai() {
        let mut request = request_with(&["mcp.server:get-data", "plain"]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        assert_eq!(declared(&request), vec!["mcp_server_get-data", "plain"]);
        assert_eq!(names.len(), 1);
        assert_eq!(
            names.original("mcp_server_get-data"),
            Some("mcp.server:get-data")
        );
        assert_eq!(names.original("plain"), None);
        for (rewritten, _) in names.iter() {
            assert!(is_valid_tool_name(rewritten, Protocol::OpenaiChat));
        }
    }

    #[test]
    fn openai_names_are_rewritten_for_gemini() {
        let mut request = request_with(&["1password_lookup", "-dash", "ok_name"]);
        let names = sanitize_tool_names(&mut request, Protocol::Gemini);
        assert_eq!(
            declared(&request),
            vec!["_1password_lookup", "_-dash", "ok_name"]
        );
        assert_eq!(
            names.original("_1password_lookup"),
            Some("1password_lookup")
        );
        assert_eq!(names.original("_-dash"), Some("-dash"));
    }

    #[test]
    fn long_and_non_ascii_names_become_valid() {
        let long = "n".repeat(100);
        let digits = "7".repeat(64);
        for target in Protocol::ALL {
            let mut request = request_with(&[&long, "名前 tool", &digits, "a b"]);
            let names = sanitize_tool_names(&mut request, target);
            for name in declared(&request) {
                assert!(is_valid_tool_name(name, target), "{name} for {target}");
            }
            // Every rewritten name maps back to exactly what the client wrote.
            let restored: HashSet<&str> = names.iter().map(|(_, original)| original).collect();
            assert!(restored.contains(long.as_str()), "{target}");
            assert!(restored.contains("名前 tool"), "{target}");
            assert!(restored.contains("a b"), "{target}");
        }
    }

    #[test]
    fn every_position_of_a_name_is_rewritten_consistently() {
        let mut request = request_with(&["ns.tool"]);
        request.tools.push(Tool::Custom(CustomTool {
            name: "free:form".into(),
            description: None,
            format: None,
        }));
        request.tools.push(Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: Protocol::Gemini,
            raw: json!({"googleSearch": {}}),
        }));
        request.tool_choice = Some(ToolChoice::Tool {
            name: "ns.tool".into(),
        });
        request.messages = vec![
            Message::user_text("go"),
            Message::new(
                Role::Assistant,
                vec![
                    Part::text("calling"),
                    Part::tool_call("c1", "ns.tool", "{\"a\":1}"),
                    Part::tool_call("c2", "free:form", "raw"),
                ],
            ),
            Message::new(
                Role::User,
                vec![
                    Part::ToolResult(ToolResult {
                        call_id: "c1".into(),
                        name: Some("ns.tool".into()),
                        content: vec![Part::text("1")],
                        is_error: false,
                        cache_control: None,
                    }),
                    Part::tool_result_text("c2", "2"),
                ],
            ),
        ];
        let names = sanitize_tool_names(&mut request, Protocol::Anthropic);
        assert_eq!(names.len(), 2);
        assert_eq!(declared(&request), vec!["ns_tool", "free_form"]);
        assert_eq!(
            request.tool_choice,
            Some(ToolChoice::Tool {
                name: "ns_tool".into()
            })
        );
        let calls: Vec<&str> = request.messages[1]
            .tool_calls()
            .map(|call| call.name.as_str())
            .collect();
        assert_eq!(calls, vec!["ns_tool", "free_form"]);
        let results: Vec<Option<&str>> = request.messages[2]
            .tool_results()
            .map(|result| result.name.as_deref())
            .collect();
        assert_eq!(results, vec![Some("ns_tool"), None]);
        // Arguments, ids and text are not names.
        assert_eq!(request.messages[1].parts[0].as_text(), Some("calling"));
        assert_eq!(request.messages[1].tool_calls().next().unwrap().id, "c1");
    }

    #[test]
    fn names_only_in_the_history_are_rewritten_too() {
        // The tool was declared on an earlier turn and is no longer offered.
        let mut request = Request::new("m", Protocol::Gemini);
        request.messages = vec![Message::new(
            Role::Assistant,
            vec![Part::tool_call("c1", "old.tool", "{}")],
        )];
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiResponses);
        assert_eq!(
            request.messages[0].tool_calls().next().unwrap().name,
            "old_tool"
        );
        assert_eq!(names.original("old_tool"), Some("old.tool"));
    }

    #[test]
    fn empty_names_are_left_alone() {
        let mut request = request_with(&["", "a.b"]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        assert_eq!(declared(&request), vec!["", "a_b"]);
        assert_eq!(names.len(), 1);
    }

    // ----- collisions ------------------------------------------------------

    #[test]
    fn a_rewrite_never_collides_with_an_existing_valid_name() {
        let mut request = request_with(&["a.b", "a_b"]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        let after = declared(&request);
        assert_eq!(after[1], "a_b", "the valid name is untouched");
        assert_ne!(after[0], "a_b");
        assert!(after[0].starts_with("a_b_"), "{}", after[0]);
        assert!(is_valid_tool_name(after[0], Protocol::OpenaiChat));
        assert_eq!(names.original(after[0]), Some("a.b"));
        assert_eq!(names.original("a_b"), None);
    }

    #[test]
    fn two_invalid_names_with_the_same_rewrite_stay_distinct() {
        let mut request = request_with(&["a.b", "a:b", "a b"]);
        let names = sanitize_tool_names(&mut request, Protocol::Anthropic);
        let after: Vec<String> = declared(&request).iter().map(|s| s.to_string()).collect();
        let distinct: HashSet<&String> = after.iter().collect();
        assert_eq!(distinct.len(), 3, "{after:?}");
        for name in &after {
            assert!(is_valid_tool_name(name, Protocol::Anthropic), "{name}");
            assert!(name.starts_with("a_b_"), "{name}");
        }
        assert_eq!(names.original(&after[0]), Some("a.b"));
        assert_eq!(names.original(&after[1]), Some("a:b"));
        assert_eq!(names.original(&after[2]), Some("a b"));
    }

    #[test]
    fn collisions_among_truncated_names_are_resolved_within_the_limit() {
        let a = format!("{}-first", "p".repeat(70));
        let b = format!("{}-second", "p".repeat(70));
        let mut request = request_with(&[&a, &b]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        let after = declared(&request);
        assert_ne!(after[0], after[1]);
        for name in &after {
            assert_eq!(name.chars().count(), MAX_TOOL_NAME_CHARS);
            assert!(is_valid_tool_name(name, Protocol::OpenaiChat));
        }
        assert_eq!(names.original(after[0]), Some(a.as_str()));
        assert_eq!(names.original(after[1]), Some(b.as_str()));
    }

    #[test]
    fn the_mapping_is_deterministic_and_independent_of_declaration_order() {
        let forward = {
            let mut request = request_with(&["a.b", "a:b", "x y", "a_b"]);
            sanitize_tool_names(&mut request, Protocol::OpenaiChat)
        };
        let again = {
            let mut request = request_with(&["a.b", "a:b", "x y", "a_b"]);
            sanitize_tool_names(&mut request, Protocol::OpenaiChat)
        };
        let reversed = {
            let mut request = request_with(&["a_b", "x y", "a:b", "a.b"]);
            sanitize_tool_names(&mut request, Protocol::OpenaiChat)
        };
        assert_eq!(forward, again);
        assert_eq!(forward, reversed);
        assert_eq!(forward.len(), 3);
    }

    #[test]
    fn a_suffixed_name_does_not_displace_an_uncontested_rewrite() {
        // "a.b" and "a:b" both want "a_b" and are suffixed; whatever the
        // suffixes are, they must not equal the plain rewrite of a third
        // name that nobody else claims.
        let mut request = request_with(&["a.b", "a:b"]);
        let first = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        let suffixed = declared(&request)[0].to_string();
        // Now add an invalid name whose plain rewrite is exactly that
        // suffixed name with one character changed back.
        let rival = suffixed.replacen("a_b_", "a.b_", 1);
        let mut request = request_with(&["a.b", "a:b", &rival]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        let after: HashSet<&str> = declared(&request).into_iter().collect();
        assert_eq!(after.len(), 3, "{after:?}");
        assert_eq!(names.len(), 3);
        assert!(!first.is_empty());
    }

    // ----- restoring -------------------------------------------------------

    #[test]
    fn response_names_are_restored() {
        let mut request = request_with(&["mcp.server:get-data", "plain"]);
        let names = sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        let mut response = Response::new("r", "m");
        response.parts = vec![
            Part::text("let me look"),
            Part::tool_call("c1", "mcp_server_get-data", "{}"),
            Part::tool_call("c2", "plain", "{}"),
            Part::tool_call("c3", "unknown_tool", "{}"),
        ];
        assert_eq!(names.restore_response(&mut response), 1);
        let called: Vec<&str> = response.tool_calls().map(|c| c.name.as_str()).collect();
        assert_eq!(called, vec!["mcp.server:get-data", "plain", "unknown_tool"]);
    }

    #[test]
    fn stream_events_are_restored() {
        let mut request = request_with(&["mcp.server:get-data"]);
        let names = sanitize_tool_names(&mut request, Protocol::Anthropic);

        let mut start = StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::ToolCall {
                id: "c1".into(),
                name: "mcp_server_get-data".into(),
                kind: ToolCallKind::Function,
                signature: None,
            },
        };
        assert!(names.restore_event(&mut start));
        match &start {
            StreamEvent::BlockStart {
                block: BlockStart::ToolCall { name, id, .. },
                index,
            } => {
                assert_eq!(name, "mcp.server:get-data");
                assert_eq!(id, "c1");
                assert_eq!(*index, 1);
            }
            other => panic!("unexpected event {other:?}"),
        }

        let mut whole = StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Whole {
                part: Part::tool_call("c2", "mcp_server_get-data", "{}"),
            },
        };
        assert!(names.restore_event(&mut whole));
        match &whole {
            StreamEvent::BlockStart {
                block:
                    BlockStart::Whole {
                        part: Part::ToolCall(call),
                    },
                ..
            } => assert_eq!(call.name, "mcp.server:get-data"),
            other => panic!("unexpected event {other:?}"),
        }

        // Events that carry no tool name, and names that were not rewritten.
        let mut untouched = vec![
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "mcp_server_get-data".into(),
            },
            StreamEvent::ToolArgsDelta {
                index: 1,
                fragment: "{\"name\":\"mcp_server_get-data\"}".into(),
            },
            StreamEvent::BlockStart {
                index: 2,
                block: BlockStart::ToolCall {
                    id: "c3".into(),
                    name: "other".into(),
                    kind: ToolCallKind::Function,
                    signature: None,
                },
            },
        ];
        let before = untouched.clone();
        for event in &mut untouched {
            assert!(!names.restore_event(event));
        }
        assert_eq!(untouched, before);
    }

    #[test]
    fn an_empty_map_restores_nothing() {
        let names = ToolNames::new();
        let mut response = Response::new("r", "m");
        response.parts = vec![Part::tool_call("c1", "a_b", "{}")];
        assert_eq!(names.restore_response(&mut response), 0);
        let mut name = "a_b".to_string();
        assert!(!names.restore_name(&mut name));
        assert_eq!(name, "a_b");
    }

    #[test]
    fn round_trip_through_a_request_and_its_response() {
        // What the gateway does: sanitise, "call the model", restore.
        for target in Protocol::ALL {
            let originals = ["1st.tool", "second tool", "third:tool", "fine_tool"];
            let mut request = request_with(&originals);
            let names = sanitize_tool_names(&mut request, target);
            let mut response = Response::new("r", "m");
            response.parts = declared(&request)
                .into_iter()
                .enumerate()
                .map(|(i, name)| Part::tool_call(format!("c{i}"), name, "{}"))
                .collect();
            names.restore_response(&mut response);
            let called: Vec<&str> = response.tool_calls().map(|c| c.name.as_str()).collect();
            assert_eq!(called, originals.to_vec(), "{target}");
        }
    }
}
