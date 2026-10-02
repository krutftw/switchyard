//! Last-minute adjustments to a Messages body that is forwarded verbatim
//! ([`switchyard_core::Codec::prepare_passthrough`]).
//!
//! Nothing the client wrote is reinterpreted. What is repaired is what the
//! API is known to refuse and what a native client cannot have produced on
//! its own:
//!
//! * omissions (`max_tokens`, `stream`) and empty domain filters;
//! * for Claude models, assistant content that was not issued by Anthropic.
//!   A Messages client keeps what the gateway handed it while another vendor
//!   served the conversation — unsigned thinking, tool-call ids in that
//!   vendor's alphabet, whitespace-only text, citations without the API's
//!   `encrypted_index` — and replays it once the conversation is routed to
//!   an Anthropic upstream. Such a body carries no wrapped signature, so it
//!   takes the passthrough path, and every one of those blocks is a 400 that
//!   repeats for the rest of the conversation;
//! * for Claude models, manual thinking for a turn in progress that does
//!   not open with a thinking block (the same cause: the turn was begun by
//!   another vendor).
//!
//! A body whose history is native is not changed by any of this, and the
//! history of a model that is not a Claude model (see
//! [`targets_claude`]) is never touched.

use crate::blocks::is_sendable_text;
use crate::reasoning::drop_manual_thinking_without_turn_start;
use crate::util::{ToolIds, Unmatched, is_block, non_empty, str_field, targets_claude};
use serde_json::{Map, Value, json};
use switchyard_core::UpstreamCtx;

/// Web tool options the API rejects when they are empty lists.
const DOMAIN_FILTERS: &[&str] = &["allowed_domains", "blocked_domains"];

fn drop_empty_domain_filters(body: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for tool in tools {
        let Some(tool) = tool.as_object_mut() else {
            continue;
        };
        let is_web_tool = tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("web_search") || kind.starts_with("web_fetch"));
        if !is_web_tool {
            continue;
        }
        for key in DOMAIN_FILTERS {
            if tool
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                tool.shift_remove(*key);
            }
        }
    }
}

fn role_is(message: &Value, role: &str) -> bool {
    str_field(message, "role").is_some_and(|r| r.trim().eq_ignore_ascii_case(role))
}

/// Whether an assistant block cannot be replayed to Anthropic.
///
/// * `thinking` without a signature and `redacted_thinking` without data:
///   the API verifies both and has no value that bypasses the check;
/// * `text` that is empty or only whitespace.
fn unreplayable(block: &Value) -> bool {
    match str_field(block, "type") {
        Some("thinking") => non_empty(block, "signature").is_none_or(|s| s.trim().is_empty()),
        Some("redacted_thinking") => non_empty(block, "data").is_none_or(|d| d.trim().is_empty()),
        Some("text") => str_field(block, "text").is_some_and(|text| !is_sendable_text(text)),
        _ => false,
    }
}

/// Removes web citations that lack the `encrypted_index` the API issues with
/// every one of them: the gateway renders other vendors' citations that way
/// for display, and they cannot be replayed.
fn drop_unreplayable_citations(block: &mut Value) {
    if !is_block(block, "text") {
        return;
    }
    let Some(object) = block.as_object_mut() else {
        return;
    };
    let Some(Value::Array(citations)) = object.get_mut("citations") else {
        return;
    };
    let before = citations.len();
    citations.retain(|citation| {
        !(is_block(citation, "web_search_result_location")
            && non_empty(citation, "encrypted_index").is_none())
    });
    if citations.is_empty() && before > 0 {
        object.shift_remove("citations");
    }
}

/// Applies the history rules to `messages`; see the module documentation.
fn sanitize_history(messages: &mut Vec<Value>) {
    let mut ids = ToolIds::default();
    messages.retain_mut(|message| {
        let assistant = role_is(message, "assistant");
        let user = role_is(message, "user");
        let Some(Value::Array(blocks)) = message.get_mut("content") else {
            if user {
                ids.user_blocks(&mut [], Unmatched::Sanitize);
            }
            return true;
        };
        if assistant {
            let before = blocks.len();
            blocks.retain(|block| !unreplayable(block));
            if blocks.is_empty() && before > 0 {
                // Nothing replayable is left of this message.
                return false;
            }
            blocks.iter_mut().for_each(drop_unreplayable_citations);
            ids.assistant_blocks(blocks);
        } else if user {
            ids.user_blocks(blocks, Unmatched::Sanitize);
        }
        true
    });
}

/// See [`switchyard_core::Codec::prepare_passthrough`] and the module
/// documentation.
pub(crate) fn prepare_passthrough(body: &mut Value, stream: bool, ctx: &UpstreamCtx<'_>) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let has_limit = object
        .get("max_tokens")
        .is_some_and(|limit| !limit.is_null());
    if !has_limit && let Some(limit) = ctx.max_output_tokens.filter(|limit| *limit > 0) {
        object.insert("max_tokens".to_string(), json!(limit));
    }
    if stream {
        object.insert("stream".to_string(), Value::Bool(true));
    } else if object.contains_key("stream") {
        object.insert("stream".to_string(), Value::Bool(false));
    }
    drop_empty_domain_filters(object);
    if targets_claude(object)
        && let Some(Value::Array(messages)) = object.get_mut("messages")
    {
        sanitize_history(messages);
    }
    drop_manual_thinking_without_turn_start(object);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn unreplayable_blocks() {
        for block in [
            json!({"type": "thinking", "thinking": "x", "signature": ""}),
            json!({"type": "thinking", "thinking": "x", "signature": "  "}),
            json!({"type": "thinking", "thinking": "x"}),
            json!({"type": "thinking", "thinking": "x", "signature": null}),
            json!({"type": "redacted_thinking"}),
            json!({"type": "redacted_thinking", "data": ""}),
            json!({"type": "text", "text": ""}),
            json!({"type": "text", "text": " \n\t"}),
        ] {
            assert!(unreplayable(&block), "{block}");
        }
        for block in [
            json!({"type": "thinking", "thinking": "", "signature": "EqQB"}),
            json!({"type": "redacted_thinking", "data": "EmwK"}),
            json!({"type": "text", "text": "hi"}),
            // Not a string: the client's own mistake, left for the API.
            json!({"type": "text"}),
            json!({"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}),
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
        ] {
            assert!(!unreplayable(&block), "{block}");
        }
    }

    #[test]
    fn citations_without_an_encrypted_index_are_removed() {
        let mut block = json!({"type": "text", "text": "x", "citations": [
            {"type": "web_search_result_location", "url": "https://a", "title": "A",
             "encrypted_index": "", "cited_text": ""},
            {"type": "web_search_result_location", "url": "https://b", "title": "B",
             "encrypted_index": "Eo8B", "cited_text": "b"},
            {"type": "char_location", "cited_text": "c", "document_index": 0,
             "document_title": null, "start_char_index": 0, "end_char_index": 1}
        ]});
        drop_unreplayable_citations(&mut block);
        assert_eq!(
            block["citations"],
            json!([
                {"type": "web_search_result_location", "url": "https://b", "title": "B",
                 "encrypted_index": "Eo8B", "cited_text": "b"},
                {"type": "char_location", "cited_text": "c", "document_index": 0,
                 "document_title": null, "start_char_index": 0, "end_char_index": 1}
            ])
        );
        let mut only = json!({"type": "text", "text": "x", "citations": [
            {"type": "web_search_result_location", "url": "https://a", "title": null,
             "encrypted_index": "", "cited_text": ""}
        ]});
        drop_unreplayable_citations(&mut only);
        assert_eq!(only, json!({"type": "text", "text": "x"}));
        // An empty list the client wrote itself is not touched.
        let mut empty = json!({"type": "text", "text": "x", "citations": []});
        drop_unreplayable_citations(&mut empty);
        assert_eq!(empty, json!({"type": "text", "text": "x", "citations": []}));
    }

    #[test]
    fn claude_models_are_recognised_by_name() {
        for model in [
            "claude-sonnet-4-5",
            "Claude-Opus-5",
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "claude-opus-4-1@20250805",
        ] {
            let body = json!({"model": model});
            assert!(targets_claude(body.as_object().unwrap()), "{model}");
        }
        for body in [
            json!({"model": "glm-4.6"}),
            json!({"model": "deepseek-reasoner"}),
            json!({"model": 5}),
            json!({}),
        ] {
            assert!(!targets_claude(body.as_object().unwrap()), "{body}");
        }
    }
}
