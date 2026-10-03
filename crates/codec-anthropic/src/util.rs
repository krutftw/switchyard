//! Small helpers shared by the codec modules.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use switchyard_core::Protocol;

/// The protocol this crate implements.
pub(crate) const THIS: Protocol = Protocol::Anthropic;

/// Key in [`switchyard_core::ir::Request::extra`] under which the request
/// decoder keeps, per tool name, the tool-definition fields the IR cannot
/// hold. Only the request encoder of this crate reads it.
pub(crate) const TOOL_EXTRAS_KEY: &str = "x-switchyard-anthropic-tool-extras";

/// Longest tool name the Messages API accepts (`^[a-zA-Z0-9_-]{1,128}$`).
pub(crate) const MAX_TOOL_NAME: usize = 128;

/// A string field, `None` when absent or not a string.
pub(crate) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A string field that is present and not empty.
pub(crate) fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    str_field(value, key).filter(|s| !s.is_empty())
}

/// A non-negative integer field; integral floats (`12.0`) are accepted.
pub(crate) fn u64_field(value: &Value, key: &str) -> Option<u64> {
    switchyard_core::util::u64_field(value, key)
}

/// A JSON number as `u64`; integral and fractional non-negative floats are
/// truncated (some providers serialise counts as `12.0`).
pub(crate) fn as_u64_lenient(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
    })
}

/// A signed integer field; integral floats are accepted.
pub(crate) fn i64_field(value: &Value, key: &str) -> Option<i64> {
    let v = value.get(key)?;
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && f.fract() == 0.0)
            .map(|f| f as i64)
    })
}

/// A finite floating point field.
pub(crate) fn f64_field(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
}

/// Replaces every character outside `[a-zA-Z0-9_-]` with `_`.
pub(crate) fn sanitize_ident(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Makes a tool name acceptable to the Messages API. Names that are already
/// valid are returned unchanged, so native clients never see a difference.
pub(crate) fn sanitize_tool_name(raw: &str) -> String {
    let mut name = sanitize_ident(raw);
    // Every character is ASCII after sanitising, so byte truncation is safe.
    name.truncate(MAX_TOOL_NAME);
    if name.is_empty() {
        name.push_str("tool");
    }
    name
}

/// Whether `id` already matches `^[a-zA-Z0-9_-]+$`.
#[cfg(test)]
pub(crate) fn is_valid_ident(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether a request body is addressed to a Claude model, going by the model
/// id (`claude-sonnet-4-5`, `anthropic.claude-…`, `claude-…@20250929`).
///
/// Some of what this crate does to keep a request acceptable enforces rules
/// of Anthropic's own validator: thinking blocks must carry a signature the
/// API issued, manual thinking needs a turn that opens with a thinking
/// block. Other vendors' models served through Messages-compatible
/// endpoints do not have those rules — they issue thinking blocks with
/// empty or free-form signatures and may want them back — so these rules
/// are applied by model family and the model id is the only evidence of
/// the family a codec has. The gateway replaces the model name before it
/// encodes or patches a body, so the id seen here is the upstream's.
pub(crate) fn targets_claude(body: &serde_json::Map<String, Value>) -> bool {
    body.get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| model.to_ascii_lowercase().contains("claude"))
}

/// Family and generation of a Claude model, read off its id in the current
/// naming scheme, `claude-<family>-<major>[-<minor>]…`: `claude-sonnet-4-5`,
/// `claude-opus-5-5-20260301`, `anthropic.claude-opus-4-6-v1:0`,
/// `claude-opus-4-1@20250805`, `claude-opus-4-20250514` (minor 0).
///
/// `None` for the older scheme (`claude-3-5-sonnet-…`, whose models none of
/// the rules keyed on this apply to), for an id without a version, and for a
/// model that is not Claude. Like [`targets_claude`], this reads the model
/// id because it is the only evidence of the model a codec has.
pub(crate) fn claude_generation(model: &str) -> Option<(String, u32, u32)> {
    let lower = model.to_ascii_lowercase();
    let rest = &lower[lower.find("claude")? + "claude".len()..];
    let mut tokens = rest
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty());
    let family = tokens
        .next()
        .filter(|token| token.chars().all(|c| c.is_ascii_alphabetic()))?;
    // A version number has one or two digits; a longer run is a date.
    let version = |token: &str| {
        (token.len() <= 2 && token.chars().all(|c| c.is_ascii_digit()))
            .then(|| token.parse::<u32>().ok())
            .flatten()
    };
    let major = tokens.next().and_then(version)?;
    let minor = tokens.next().and_then(version).unwrap_or(0);
    Some((family.to_string(), major, minor))
}

/// Whether the model refuses a conversation that ends with an assistant
/// message: "This model does not support assistant message prefill. The
/// conversation must end with a user message." Claude 4.6 and everything
/// newer (notes 15 §5.2).
pub(crate) fn rejects_prefill(model: &str) -> bool {
    claude_generation(model).is_some_and(|(_, major, minor)| (major, minor) >= (4, 6))
}

/// Whether the model refuses forced tool use: `tool_choice: type "tool" and
/// "any" are not supported for this model.` Claude Opus 5.5, Sonnet 5.5,
/// Fable 5.1 and Mythos 5.1 (notes 15 §5.2), and the later generations of
/// those families.
pub(crate) fn rejects_forced_tool_choice(model: &str) -> bool {
    claude_generation(model).is_some_and(|(family, major, minor)| match family.as_str() {
        "opus" | "sonnet" => (major, minor) >= (5, 5),
        "fable" | "mythos" => (major, minor) >= (5, 1),
        _ => false,
    })
}

/// Whether a content block has the given `type`.
pub(crate) fn is_block(block: &Value, kind: &str) -> bool {
    str_field(block, "type") == Some(kind)
}

/// What to write on a `tool_result` that answers no call of the assistant
/// turn right before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unmatched {
    /// An empty `tool_use_id`, which no call can have: the request encoder
    /// turns such a result into plain user content.
    Mark,
    /// The id with invalid characters replaced and nothing more: a body that
    /// is forwarded verbatim is not restructured.
    Sanitize,
}

/// Gives the `tool_use` / `tool_result` blocks of a conversation ids the
/// Messages API accepts: each matches `^[a-zA-Z0-9_-]+$`, no two `tool_use`
/// blocks share one, and a result carries the id of the call it answers.
///
/// The conversation is fed message by message, oldest first:
///
/// * a valid id that no earlier call has is kept as it is, so native
///   histories are never touched;
/// * invalid characters become `_`; an id an earlier call already has (two
///   ids that are equal after sanitising, or one id reused by a later call,
///   as vendors that number their calls from zero in every response do) gets
///   a numbered suffix; id-less calls are numbered;
/// * a result is matched to the first not yet answered call *with the same
///   original id in the assistant turn before it*, the only place the API
///   lets its call be.
///
/// The outcome depends only on the messages seen so far, so appending to a
/// conversation never changes the ids of its earlier turns (prompt caching
/// relies on that).
#[derive(Default)]
pub(crate) struct ToolIds {
    used: HashSet<String>,
    next_suffix: HashMap<String, usize>,
    unnamed: usize,
    open: HashMap<String, VecDeque<String>>,
    after_assistant: bool,
}

impl ToolIds {
    /// Assigns ids to the `tool_use` blocks of an assistant message.
    /// Consecutive assistant messages count as one turn.
    pub(crate) fn assistant_blocks(&mut self, blocks: &mut [Value]) {
        if !self.after_assistant {
            self.open.clear();
        }
        self.after_assistant = true;
        for block in blocks.iter_mut().filter(|b| is_block(b, "tool_use")) {
            let raw = str_field(block, "id").unwrap_or("").to_string();
            let wire = self.allocate(&raw);
            if wire != raw
                && let Some(object) = block.as_object_mut()
            {
                object.insert("id".to_string(), Value::String(wire.clone()));
            }
            self.open.entry(raw).or_default().push_back(wire);
        }
    }

    /// Points the `tool_result` blocks of a user message at the calls they
    /// answer.
    pub(crate) fn user_blocks(&mut self, blocks: &mut [Value], unmatched: Unmatched) {
        self.after_assistant = false;
        for block in blocks.iter_mut().filter(|b| is_block(b, "tool_result")) {
            let raw = str_field(block, "tool_use_id").unwrap_or("").to_string();
            let call = self.open.get_mut(&raw).and_then(VecDeque::pop_front);
            let wire = match call {
                Some(wire) => wire,
                None => match unmatched {
                    Unmatched::Mark => String::new(),
                    Unmatched::Sanitize => sanitize_ident(&raw),
                },
            };
            if wire != raw
                && let Some(object) = block.as_object_mut()
            {
                object.insert("tool_use_id".to_string(), Value::String(wire));
            }
        }
    }

    fn allocate(&mut self, raw: &str) -> String {
        if raw.is_empty() {
            loop {
                self.unnamed += 1;
                let candidate = format!("toolu_unnamed_{}", self.unnamed);
                if self.used.insert(candidate.clone()) {
                    return candidate;
                }
            }
        }
        let base = sanitize_ident(raw);
        if self.used.insert(base.clone()) {
            return base;
        }
        // Each base resumes after the last suffix it tried. Reused ids
        // must not rescan all earlier collisions or rehash growing salts.
        let next = self.next_suffix.entry(base.clone()).or_insert(0);
        loop {
            *next += 1;
            let candidate = format!("{base}_{next}");
            if self.used.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

/// Formats unix seconds as an RFC 3339 UTC timestamp (`2025-09-29T00:00:00Z`).
pub(crate) fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil-from-days (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// Standard base64 of `text`.
pub(crate) fn base64_encode(text: &str) -> String {
    STANDARD.encode(text.as_bytes())
}

/// Decodes standard base64 into UTF-8 text. `None` when the payload is not
/// valid base64 or not valid UTF-8.
pub(crate) fn base64_decode_text(data: &str) -> Option<String> {
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = STANDARD.decode(compact.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// Guesses an image media type from the first characters of its base64
/// payload. Used only when a foreign protocol supplied inline image data
/// without saying what it is; the Messages API requires `media_type`.
pub(crate) fn sniff_image_type(data: &str) -> &'static str {
    if data.starts_with("/9j/") {
        "image/jpeg"
    } else if data.starts_with("R0lGOD") {
        "image/gif"
    } else if data.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_generations_are_read_off_the_model_id() {
        let generation = claude_generation;
        assert_eq!(
            generation("claude-sonnet-4-5"),
            Some(("sonnet".into(), 4, 5))
        );
        assert_eq!(
            generation("claude-opus-5-5-20260301"),
            Some(("opus".into(), 5, 5))
        );
        assert_eq!(
            generation("anthropic.claude-opus-4-6-v1:0"),
            Some(("opus".into(), 4, 6))
        );
        assert_eq!(
            generation("claude-opus-4-1@20250805"),
            Some(("opus".into(), 4, 1))
        );
        // A date is not a minor version.
        assert_eq!(
            generation("claude-opus-4-20250514"),
            Some(("opus".into(), 4, 0))
        );
        assert_eq!(generation("Claude-Fable-5.1"), Some(("fable".into(), 5, 1)));
        // The older naming scheme, ids without a version, other models.
        assert_eq!(generation("claude-3-5-sonnet-20241022"), None);
        assert_eq!(generation("claude-sonnet"), None);
        assert_eq!(generation("gpt-5.5"), None);
        assert_eq!(generation(""), None);

        assert!(!rejects_prefill("claude-sonnet-4-5"));
        assert!(rejects_prefill("claude-sonnet-4-6"));
        assert!(rejects_prefill("claude-haiku-5-0"));
        assert!(!rejects_prefill("claude-3-7-sonnet-20250219"));
        assert!(!rejects_prefill("deepseek-chat"));

        assert!(!rejects_forced_tool_choice("claude-opus-4-6"));
        assert!(!rejects_forced_tool_choice("claude-opus-5-0"));
        assert!(rejects_forced_tool_choice("claude-opus-5-5"));
        assert!(rejects_forced_tool_choice("claude-sonnet-5-5-20260601"));
        assert!(rejects_forced_tool_choice("claude-fable-5-1"));
        assert!(rejects_forced_tool_choice("claude-mythos-5-1"));
        assert!(!rejects_forced_tool_choice("claude-haiku-5-5"));
    }

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(1_759_104_000), "2025-09-29T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn tool_names_are_sanitised_and_valid_ones_untouched() {
        assert_eq!(sanitize_tool_name("get_weather"), "get_weather");
        assert_eq!(
            sanitize_tool_name("mcp.server:get_time"),
            "mcp_server_get_time"
        );
        assert_eq!(sanitize_tool_name(""), "tool");
        assert_eq!(sanitize_tool_name(&"x".repeat(200)).len(), MAX_TOOL_NAME);
        assert_eq!(sanitize_tool_name("naïve"), "na_ve");
    }

    fn call(id: &str) -> Value {
        serde_json::json!({"type": "tool_use", "id": id, "name": "f", "input": {}})
    }

    fn result(id: &str) -> Value {
        serde_json::json!({"type": "tool_result", "tool_use_id": id})
    }

    fn ids_of(blocks: &[Value], key: &str) -> Vec<String> {
        blocks
            .iter()
            .map(|block| block[key].as_str().unwrap_or("<none>").to_string())
            .collect()
    }

    #[test]
    fn tool_ids_are_consistent_and_collision_free() {
        let mut ids = ToolIds::default();
        let mut calls = vec![
            call("toolu_01A"),
            call("call.with space:1"),
            // A different id that sanitises to the same text must not collide.
            call("call.with.space.1"),
        ];
        ids.assistant_blocks(&mut calls);
        let wire = ids_of(&calls, "id");
        assert_eq!(wire[0], "toolu_01A");
        assert_eq!(wire[1], "call_with_space_1");
        assert_ne!(wire[2], "call_with_space_1");
        assert!(wire[2].starts_with("call_with_space_1_"));
        assert!(is_valid_ident(&wire[2]));

        let mut results = vec![
            result("call.with.space.1"),
            result("toolu_01A"),
            result("call.with space:1"),
        ];
        ids.user_blocks(&mut results, Unmatched::Mark);
        assert_eq!(
            ids_of(&results, "tool_use_id"),
            [wire[2].as_str(), "toolu_01A", "call_with_space_1"]
        );
    }

    #[test]
    fn a_reused_tool_id_gets_a_fresh_wire_id_and_pairs_with_its_own_result() {
        let mut ids = ToolIds::default();
        let mut first = vec![call("functions.f:0")];
        ids.assistant_blocks(&mut first);
        let mut first_results = vec![result("functions.f:0")];
        ids.user_blocks(&mut first_results, Unmatched::Mark);
        let mut second = vec![call("functions.f:0"), call("functions.f:0")];
        ids.assistant_blocks(&mut second);
        let mut second_results = vec![result("functions.f:0"), result("functions.f:0")];
        ids.user_blocks(&mut second_results, Unmatched::Mark);

        let all: Vec<String> = [ids_of(&first, "id"), ids_of(&second, "id")].concat();
        assert_eq!(all[0], "functions_f_0");
        let distinct: HashSet<&String> = all.iter().collect();
        assert_eq!(distinct.len(), 3, "{all:?}");
        assert!(all.iter().all(|id| is_valid_ident(id)));
        assert_eq!(ids_of(&first_results, "tool_use_id"), all[..1]);
        assert_eq!(ids_of(&second_results, "tool_use_id"), all[1..]);
    }

    #[test]
    fn a_valid_tool_id_that_is_reused_is_renamed_too() {
        let mut ids = ToolIds::default();
        let mut first = vec![call("call_0")];
        ids.assistant_blocks(&mut first);
        ids.user_blocks(&mut [result("call_0")], Unmatched::Mark);
        let mut second = vec![call("call_0")];
        ids.assistant_blocks(&mut second);
        assert_eq!(first[0]["id"], "call_0");
        let renamed = second[0]["id"].as_str().unwrap();
        assert!(renamed.starts_with("call_0_") && is_valid_ident(renamed));
    }

    #[test]
    fn numbered_collisions_preserve_result_order_and_skip_existing_ids() {
        let mut ids = ToolIds::default();
        let mut calls = vec![call("a_1"), call("a_2"), call("a")];
        calls.extend((0..8).map(|_| call("a")));
        ids.assistant_blocks(&mut calls);
        let wire = ids_of(&calls, "id");
        assert_eq!(&wire[..4], ["a_1", "a_2", "a", "a_3"]);
        assert_eq!(wire.iter().collect::<HashSet<_>>().len(), calls.len());
        assert!(wire.iter().all(|id| is_valid_ident(id)));

        // Equal raw ids match in call order even when distinct calls are
        // answered in a different order, and cannot be answered twice.
        let mut results: Vec<Value> = (0..9).map(|_| result("a")).collect();
        results.extend([result("a_2"), result("a_1"), result("a")]);
        ids.user_blocks(&mut results, Unmatched::Mark);
        let answered = ids_of(&results, "tool_use_id");
        assert_eq!(answered[..9], wire[2..]);
        assert_eq!(&answered[9..], ["a_2", "a_1", ""]);
    }

    #[test]
    fn a_tool_result_only_answers_a_call_of_the_turn_before_it() {
        let mut ids = ToolIds::default();
        ids.assistant_blocks(&mut [call("a")]);
        // Answered twice: the second result has no call left.
        let mut results = vec![result("a"), result("a"), result("never_called")];
        ids.user_blocks(&mut results, Unmatched::Mark);
        assert_eq!(ids_of(&results, "tool_use_id"), ["a", "", ""]);
        // The call of an older turn can no longer be answered.
        ids.assistant_blocks(&mut [call("b")]);
        let mut late = vec![result("a"), result("b:1")];
        ids.user_blocks(&mut late, Unmatched::Sanitize);
        assert_eq!(ids_of(&late, "tool_use_id"), ["a", "b_1"]);
    }

    #[test]
    fn consecutive_assistant_messages_are_one_tool_turn() {
        let mut ids = ToolIds::default();
        ids.assistant_blocks(&mut [call("a")]);
        ids.assistant_blocks(&mut [call("b")]);
        let mut results = vec![result("a"), result("b")];
        ids.user_blocks(&mut results, Unmatched::Mark);
        assert_eq!(ids_of(&results, "tool_use_id"), ["a", "b"]);
    }

    #[test]
    fn empty_tool_ids_pair_by_position() {
        let mut ids = ToolIds::default();
        let mut calls = vec![call(""), call(""), call("toolu_unnamed_3")];
        ids.assistant_blocks(&mut calls);
        let wire = ids_of(&calls, "id");
        assert_eq!(
            wire,
            ["toolu_unnamed_1", "toolu_unnamed_2", "toolu_unnamed_3"]
        );
        let mut results = vec![result(""), result("")];
        ids.user_blocks(&mut results, Unmatched::Mark);
        assert_eq!(ids_of(&results, "tool_use_id"), wire[..2]);
        // The numbering skips ids that are taken.
        let mut more = vec![call("")];
        ids.assistant_blocks(&mut more);
        assert_eq!(more[0]["id"], "toolu_unnamed_4");
    }

    #[test]
    fn base64_text_round_trip() {
        let encoded = base64_encode("héllo\nworld");
        assert_eq!(
            base64_decode_text(&encoded).as_deref(),
            Some("héllo\nworld")
        );
        assert_eq!(base64_decode_text("!!!"), None);
    }

    #[test]
    fn lenient_numbers() {
        let v = serde_json::json!({"a": 3.0, "b": -1, "c": 1.5, "d": "x"});
        assert_eq!(u64_field(&v, "a"), Some(3));
        assert_eq!(i64_field(&v, "a"), Some(3));
        assert_eq!(i64_field(&v, "b"), Some(-1));
        assert_eq!(i64_field(&v, "c"), None);
        assert_eq!(f64_field(&v, "c"), Some(1.5));
        assert_eq!(f64_field(&v, "d"), None);
    }
}
