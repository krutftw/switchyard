//! Tool names across the protocol seam.
//!
//! OpenAI accepts function names matching `^[a-zA-Z0-9_-]{1,64}$`.
//! Anthropic allows 128 characters and Gemini allows dots and colons
//! (`mcp.files:read`), so a request written for one of them is renamed on
//! its way to a Responses upstream ([`UpstreamNames`]). In the other
//! direction an upstream of another protocol answers with the names *it*
//! was given; [`loose`] and [`may_be_truncated`] are what the tool index
//! uses to find the declaration such a name stands for.

use std::collections::{HashMap, HashSet};
use switchyard_core::ir::{Part, Request, Role, ToolChoice};

use crate::common::P;

/// Longest function name OpenAI accepts.
const MAX_NAME: usize = 64;

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

/// The names of a request as a Responses upstream is given them.
///
/// A Responses client's own names are never touched. Names from any other
/// protocol are made valid; two names that become the same one are kept
/// apart with a numeric suffix, and a name that is valid as it stands
/// always keeps its spelling.
#[derive(Debug, Default)]
pub(crate) struct UpstreamNames {
    renamed: HashMap<String, String>,
}

impl UpstreamNames {
    pub(crate) fn for_request(request: &Request) -> Self {
        if request.source == P {
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

/// The key two spellings of one tool name share: case, punctuation and
/// leading underscores are what upstream sanitisers change.
pub(crate) fn loose(name: &str) -> String {
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

/// Whether `wire` (a name an upstream used) may be `declared` cut short by
/// that upstream's length limit.
pub(crate) fn may_be_truncated(wire: &str, declared: &str) -> bool {
    let key = loose(wire);
    wire.len() >= MIN_TRUNCATED && !key.is_empty() && loose(declared).starts_with(&key)
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
        let mut request = Request::new("gpt-5.5", Protocol::Anthropic);
        request.tools = vec![
            function("mcp.files:read"),
            function("mcp_files_read"),
            function(&long),
        ];
        request.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call("call_1", "history.only", "{}")],
        ));
        let names = UpstreamNames::for_request(&request);
        assert_eq!(names.wire("mcp_files_read"), "mcp_files_read");
        assert_eq!(names.wire("mcp.files:read"), "mcp_files_read_2");
        assert_eq!(names.wire(&long), "x".repeat(64));
        assert_eq!(names.wire("history.only"), "history_only");
    }

    #[test]
    fn a_responses_clients_names_are_its_own() {
        let mut request = Request::new("gpt-5.5", P);
        request.tools = vec![function("mcp.files:read")];
        let names = UpstreamNames::for_request(&request);
        assert_eq!(names.wire("mcp.files:read"), "mcp.files:read");
    }

    #[test]
    fn loose_keys_ignore_what_sanitisers_change() {
        assert_eq!(loose("mcp.files:read-file"), loose("_MCP_files_read_file"));
        assert!(may_be_truncated(&"y".repeat(64), &"y".repeat(100)));
        assert!(!may_be_truncated("yyy", &"y".repeat(100)));
    }
}
