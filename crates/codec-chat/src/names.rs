//! Tool names and tool-call ids across the protocol seam.
//!
//! OpenAI accepts function names matching `^[a-zA-Z0-9_-]{1,64}$` and
//! tool-call ids of at most 40 characters. Anthropic allows 128-character
//! names, Gemini allows dots and colons (`mcp.files:read`), and neither
//! limits its ids that way, so a request written for one of them is renamed
//! on its way to a Chat upstream ([`UpstreamNames`], [`UpstreamIds`]). In the
//! other direction an upstream of another protocol answers with the names
//! *it* was given, and the Chat client has to see the names it declared
//! ([`ClientNames`]).

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use switchyard_core::ir::{Part, Request, Role, ToolChoice};

use crate::common::PROTOCOL;

/// Longest function name OpenAI accepts.
const MAX_NAME: usize = 64;

/// Longest tool-call id OpenAI accepts.
const MAX_CALL_ID: usize = 40;

/// Shortest wire name that may be the truncated form of a longer declared
/// one (Gemini cuts at 64 and may spend one character on a leading `_`).
const MIN_TRUNCATED: usize = 63;

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether OpenAI accepts `name` as a function name.
fn is_valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.chars().all(name_char)
}

/// `name` with every character OpenAI refuses replaced by `_`, cut to 64.
fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if name_char(c) { c } else { '_' })
        .take(MAX_NAME)
        .collect();
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

/// The names of a request as a Chat upstream is given them.
///
/// A Chat client's own names are never touched (compatible servers accept
/// more than OpenAI does, and the client wrote them for this protocol).
/// Names from any other protocol are made valid; two names that become the
/// same one are kept apart with a numeric suffix, and a name that is valid
/// as it stands always keeps its spelling.
#[derive(Debug, Default)]
pub(crate) struct UpstreamNames {
    renamed: HashMap<String, String>,
}

impl UpstreamNames {
    pub(crate) fn for_request(request: &Request) -> Self {
        if request.source == PROTOCOL {
            return Self::default();
        }
        let mut names: Vec<&str> = Vec::new();
        for tool in &request.tools {
            names.extend(tool.name());
        }
        for message in request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
        {
            for part in &message.parts {
                if let Part::ToolCall(call) = part {
                    names.push(&call.name);
                }
            }
        }
        if let Some(ToolChoice::Tool { name }) = &request.tool_choice {
            names.push(name);
        }

        let mut taken: HashSet<String> = names
            .iter()
            .filter(|name| is_valid(name))
            .map(|name| name.to_string())
            .collect();
        let mut renamed: HashMap<String, String> = HashMap::new();
        for name in names {
            if is_valid(name) || renamed.contains_key(name) {
                continue;
            }
            let base = sanitize(name);
            let mut wire = base.clone();
            let mut n = 2u32;
            while taken.contains(&wire) {
                let suffix = format!("_{n}");
                wire = base.clone();
                // Every character is ASCII after sanitising.
                wire.truncate(MAX_NAME - suffix.len());
                wire.push_str(&suffix);
                n += 1;
            }
            taken.insert(wire.clone());
            renamed.insert(name.to_string(), wire);
        }
        Self { renamed }
    }

    /// The upstream's spelling of a canonical tool name.
    pub(crate) fn wire<'a>(&'a self, name: &'a str) -> &'a str {
        self.renamed.get(name).map_or(name, String::as_str)
    }
}

/// 64-bit FNV-1a.
pub(crate) fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The tool-call ids of a request as a Chat upstream is given them.
///
/// Ids a Chat client wrote are replayed. Ids from another protocol that are
/// longer than OpenAI's 40 characters ("string too long. Expected a string
/// with maximum length 40") are replaced by a prefix of the id plus a hash
/// of all of it: the same id always maps to the same short one, so a call
/// and its result stay paired and the upstream's prompt cache keeps working.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UpstreamIds {
    shorten: bool,
}

impl UpstreamIds {
    pub(crate) fn for_request(request: &Request) -> Self {
        Self {
            shorten: request.source != PROTOCOL,
        }
    }

    pub(crate) fn wire(&self, id: &str) -> String {
        if !self.shorten || id.len() <= MAX_CALL_ID {
            return id.to_string();
        }
        let prefix: String = id
            .chars()
            .filter(|c| name_char(*c))
            .take(MAX_CALL_ID - 17)
            .collect();
        format!("{prefix}_{:016x}", fnv1a64(id))
    }
}

/// The key two spellings of one tool name share: case, punctuation and
/// leading underscores are what upstream sanitisers change.
fn loose(name: &str) -> String {
    let key: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    key.trim_start_matches('_').to_string()
}

/// The tool names a Chat client declared in its request, used to give tool
/// calls from an upstream of another protocol the client's own spelling
/// back.
///
/// A name the client declared is returned as it is. Any other name is
/// matched loosely (ignoring case, punctuation and leading underscores, and
/// allowing for a name the upstream had to cut short); only an unambiguous
/// match renames the call.
#[derive(Debug, Default)]
pub(crate) struct ClientNames {
    declared: Vec<String>,
}

impl ClientNames {
    /// Reads the declarations of a Chat request body (`tools[]` in the
    /// nested and the flat form, and the deprecated `functions[]`).
    pub(crate) fn from_request(request: Option<&Value>) -> Self {
        let mut declared: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |name: Option<&str>| {
            if let Some(name) = name.filter(|n| !n.is_empty())
                && seen.insert(name.to_string())
            {
                declared.push(name.to_string());
            }
        };
        let Some(request) = request else {
            return Self::default();
        };
        if let Some(Value::Array(tools)) = request.get("tools") {
            for tool in tools {
                let nested = ["function", "custom"]
                    .iter()
                    .find_map(|key| tool.get(key).and_then(|spec| spec.get("name")))
                    .or_else(|| tool.get("name"));
                push(nested.and_then(Value::as_str));
            }
        }
        if let Some(Value::Array(functions)) = request.get("functions") {
            for function in functions {
                push(function.get("name").and_then(Value::as_str));
            }
        }
        Self { declared }
    }

    /// The client's spelling of a tool name an upstream used.
    pub(crate) fn restore<'a>(&'a self, name: &'a str) -> &'a str {
        if self.declared.is_empty() || self.declared.iter().any(|d| d == name) {
            return name;
        }
        let key = loose(name);
        let mut found: Option<&'a str> = None;
        for declared in &self.declared {
            let candidate = loose(declared);
            let matches = candidate == key
                || (name.len() >= MIN_TRUNCATED && !key.is_empty() && candidate.starts_with(&key));
            if !matches {
                continue;
            }
            if found.is_some() {
                return name;
            }
            found = Some(declared);
        }
        found.unwrap_or(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::Protocol;
    use switchyard_core::ir::{FunctionTool, Message, Tool};

    fn function(name: &str) -> Tool {
        Tool::Function(FunctionTool {
            name: name.to_string(),
            description: None,
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
        })
    }

    #[test]
    fn foreign_names_become_valid_and_stay_distinct() {
        let long = "x".repeat(100);
        let mut request = Request::new("gpt-5.5", Protocol::Gemini);
        request.tools = vec![
            function("mcp.files:read"),
            function("mcp_files_read"),
            function("mcp files read"),
            function(&long),
            function("plain"),
        ];
        request.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "history.only", "{}")],
        ));
        let names = UpstreamNames::for_request(&request);
        // A name that is valid as it stands is never the one that moves.
        assert_eq!(names.wire("mcp_files_read"), "mcp_files_read");
        assert_eq!(names.wire("mcp.files:read"), "mcp_files_read_2");
        assert_eq!(names.wire("mcp files read"), "mcp_files_read_3");
        assert_eq!(names.wire(&long), "x".repeat(64));
        assert_eq!(names.wire("plain"), "plain");
        assert_eq!(names.wire("history.only"), "history_only");
        assert_eq!(names.wire("never seen"), "never seen");
    }

    #[test]
    fn a_chat_clients_names_are_its_own() {
        let mut request = Request::new("gpt-5.5", PROTOCOL);
        request.tools = vec![function("mcp.files:read")];
        let names = UpstreamNames::for_request(&request);
        assert_eq!(names.wire("mcp.files:read"), "mcp.files:read");
    }

    #[test]
    fn long_foreign_ids_are_shortened_deterministically() {
        let foreign = UpstreamIds { shorten: true };
        let long = "toolu_vrtx_01".to_string() + &"A".repeat(60);
        let short = foreign.wire(&long);
        assert_eq!(short.len(), 40);
        assert_eq!(short, foreign.wire(&long));
        assert_ne!(short, foreign.wire(&(long.clone() + "B")));
        assert_eq!(foreign.wire("call_1"), "call_1");
        assert_eq!(UpstreamIds { shorten: false }.wire(&long), long);
    }

    #[test]
    fn client_names_are_restored_from_sanitised_spellings() {
        let long = "y".repeat(100);
        let request = json!({
            "tools": [
                {"type": "function", "function": {"name": "mcp.files:read-file"}},
                {"type": "custom", "custom": {"name": "Apply.Patch"}},
                {"type": "function", "name": "flat.tool"},
                {"type": "function", "function": {"name": long}},
                {"type": "function", "function": {"name": "a.b"}},
                {"type": "function", "function": {"name": "a:b"}}
            ],
            "functions": [{"name": "legacy.fn"}]
        });
        let names = ClientNames::from_request(Some(&request));
        assert_eq!(names.restore("mcp_files_read-file"), "mcp.files:read-file");
        assert_eq!(names.restore("apply_patch"), "Apply.Patch");
        assert_eq!(names.restore("flat_tool"), "flat.tool");
        assert_eq!(names.restore("legacy_fn"), "legacy.fn");
        assert_eq!(names.restore(&"y".repeat(64)), long);
        assert_eq!(names.restore(&format!("_{}", "y".repeat(63))), long);
        // Ambiguous and unknown names are left alone.
        assert_eq!(names.restore("a_b"), "a_b");
        assert_eq!(names.restore("unknown"), "unknown");
        assert_eq!(names.restore("yyy"), "yyy");
        assert_eq!(ClientNames::from_request(None).restore("x_y"), "x_y");
    }

    #[test]
    fn repeated_client_declarations_keep_first_order_and_unambiguous_names() {
        let request = json!({
            "tools": [
                {"type": "function", "function": {"name": "alpha.fn"}},
                {"type": "custom", "custom": {"name": "beta.fn"}},
                {"type": "function", "function": {"name": "alpha.fn"}}
            ],
            "functions": [{"name": "beta.fn"}, {"name": "gamma.fn"}]
        });
        let names = ClientNames::from_request(Some(&request));
        assert_eq!(names.declared, ["alpha.fn", "beta.fn", "gamma.fn"]);
        assert_eq!(names.restore("alpha_fn"), "alpha.fn");
        assert_eq!(names.restore("beta_fn"), "beta.fn");
    }
}
