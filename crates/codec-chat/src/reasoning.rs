//! Reasoning settings on raw Chat Completions bodies.
//!
//! OpenAI's own spelling is the top-level `reasoning_effort` string. The
//! "compatible" ecosystem added several more, all of which are *read*:
//!
//! | spelling | vendor |
//! |---|---|
//! | `reasoning_effort: "high"` | OpenAI and most compatible servers |
//! | `reasoning: {effort, max_tokens, enabled, exclude, summary}` | OpenRouter |
//! | `thinking: {type, budget_tokens}` | DeepSeek, Zhipu, Doubao (Anthropic-style) |
//! | `enable_thinking`, `thinking_budget` | Qwen / DashScope, SiliconFlow |
//! | `extra_body.google.thinking_config` | Google's OpenAI-compatible endpoint |
//! | `include_reasoning` | OpenRouter (legacy visibility switch) |
//!
//! Only `reasoning_effort` is ever *written*; the other depth spellings are
//! removed when a depth is written so the body never carries two conflicting
//! settings.

use crate::common::as_i64;
use serde_json::{Map, Value, json};
use switchyard_core::codec::UpstreamCtx;
use switchyard_core::reasoning::{
    Depth, Fitted, ModelThinking, ReasoningConfig, Summary, budget_to_effort, depth_from_budget,
    depth_from_effort_str,
};

/// Where Google's OpenAI-compatible endpoint (and clients written for it)
/// put the Gemini thinking configuration.
const GOOGLE_CONFIG_PATHS: &[&[&str]] = &[
    &["extra_body", "google", "thinking_config"],
    &["extra_body", "google", "thinkingConfig"],
    &["extra_body", "extra_body", "google", "thinking_config"],
    &["google", "thinking_config"],
    &["google", "thinkingConfig"],
];

fn at<'a>(body: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(body, |v, key| v.get(key))
}

fn either<'a>(holder: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    holder.get(snake).or_else(|| holder.get(camel))
}

/// Reads depth and summary intent from a Chat request body.
///
/// The summary intent is taken from fields that talk about visibility
/// explicitly, with one exception: `reasoning_effort: "none"` also says "no
/// summaries" (notes 12 §8.1). The other half of that rule, an effort that
/// turns reasoning *on* means "show it to me", is not applied here, because
/// it does not hold for every target: on an Anthropic upstream a Chat effort
/// controls depth and nothing else (notes 12 §8.1, the "translated" reader),
/// and the config read here cannot say which kind of statement it came from.
/// The two upstream protocols that return reasoning text only on request
/// apply it in their own encoders: Gemini (`includeThoughts`) and Responses
/// (`reasoning.summary`).
pub(crate) fn read_reasoning(body: &Value) -> ReasoningConfig {
    if !body.is_object() {
        return ReasoningConfig::default();
    }
    ReasoningConfig {
        depth: read_depth(body),
        summary: read_summary(body),
    }
}

fn effort_depth(v: Option<&Value>) -> Option<Depth> {
    v.and_then(Value::as_str).and_then(depth_from_effort_str)
}

fn budget_depth(v: Option<&Value>) -> Option<Depth> {
    v.and_then(as_i64).and_then(depth_from_budget)
}

fn read_depth(body: &Value) -> Option<Depth> {
    if let Some(depth) = effort_depth(body.get("reasoning_effort")) {
        return Some(depth);
    }
    if let Some(reasoning) = body.get("reasoning").filter(|r| r.is_object()) {
        if let Some(depth) = effort_depth(reasoning.get("effort")) {
            return Some(depth);
        }
        if let Some(depth) = budget_depth(reasoning.get("max_tokens")) {
            return Some(depth);
        }
        match reasoning.get("enabled") {
            Some(Value::Bool(false)) => return Some(Depth::Off),
            Some(Value::Bool(true)) => return Some(Depth::Auto),
            _ => {}
        }
    }
    if let Some(thinking) = body.get("thinking").filter(|t| t.is_object()) {
        let kind = thinking
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match kind.as_str() {
            "disabled" => return Some(Depth::Off),
            "enabled" => {
                return Some(budget_depth(thinking.get("budget_tokens")).unwrap_or(Depth::Auto));
            }
            "adaptive" | "auto" => return Some(Depth::Auto),
            _ => {}
        }
    }
    match body.get("enable_thinking") {
        Some(Value::Bool(false)) => return Some(Depth::Off),
        Some(Value::Bool(true)) => {
            return Some(budget_depth(body.get("thinking_budget")).unwrap_or(Depth::Auto));
        }
        _ => {}
    }
    if let Some(depth) = budget_depth(body.get("thinking_budget")) {
        return Some(depth);
    }
    for path in GOOGLE_CONFIG_PATHS {
        let Some(config) = at(body, path).filter(|c| c.is_object()) else {
            continue;
        };
        if let Some(depth) = effort_depth(either(config, "thinking_level", "thinkingLevel")) {
            return Some(depth);
        }
        if let Some(depth) = budget_depth(either(config, "thinking_budget", "thinkingBudget")) {
            return Some(depth);
        }
    }
    None
}

fn on_off(enabled: bool) -> Summary {
    if enabled { Summary::Auto } else { Summary::Off }
}

/// `reasoning.summary` / `reasoning.generate_summary`: a detail level turns
/// summaries on, `"none"` or an explicit `null` turns them off, anything else
/// is not a statement.
fn summary_field(v: Option<&Value>) -> Option<Summary> {
    match v? {
        Value::Null => Some(Summary::Off),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Summary::Auto),
            "concise" => Some(Summary::Concise),
            "detailed" => Some(Summary::Detailed),
            "none" => Some(Summary::Off),
            _ => None,
        },
        _ => None,
    }
}

fn read_summary(body: &Value) -> Option<Summary> {
    let include_thoughts = |holder: &Value| {
        either(holder, "include_thoughts", "includeThoughts").and_then(Value::as_bool)
    };
    for path in GOOGLE_CONFIG_PATHS {
        if let Some(flag) = at(body, path).and_then(include_thoughts) {
            return Some(on_off(flag));
        }
    }
    let reasoning = body.get("reasoning").filter(|r| r.is_object());
    for holder in [body.get("thinking"), reasoning].into_iter().flatten() {
        if let Some(flag) = include_thoughts(holder) {
            return Some(on_off(flag));
        }
    }
    if let Some(reasoning) = reasoning {
        for key in ["summary", "generate_summary"] {
            if let Some(summary) = summary_field(reasoning.get(key)) {
                return Some(summary);
            }
        }
        if let Some(exclude) = reasoning.get("exclude").and_then(Value::as_bool) {
            return Some(on_off(!exclude));
        }
    }
    if let Some(include) = body.get("include_reasoning").and_then(Value::as_bool) {
        return Some(on_off(include));
    }
    if let Some(enabled) = reasoning
        .and_then(|r| r.get("enabled"))
        .and_then(Value::as_bool)
    {
        return Some(on_off(enabled));
    }
    // A client that switches reasoning off does not want to be shown any,
    // which matters on models that cannot stop reasoning: they are given the
    // smallest depth they accept, and without this they would be asked for
    // the thoughts of that as well.
    let effort = body.get("reasoning_effort").and_then(Value::as_str);
    effort
        .is_some_and(|effort| effort.trim().eq_ignore_ascii_case("none"))
        .then_some(Summary::Off)
}

/// The `reasoning_effort` value for a fitted depth, or `None` when the field
/// must be absent.
///
/// * [`Depth::Auto`] has no Chat spelling: the upstream default applies.
/// * [`Depth::Budget`] is bucketed with [`budget_to_effort`].
/// * [`Depth::Off`] is `"none"`; on a model that is known not to accept it
///   the lowest level the model has is used instead.
/// * Levels are clamped to the model's supported set when that is known and
///   written verbatim otherwise.
/// * A model known not to reason gets no field at all.
pub(crate) fn effort_for(depth: Depth, thinking: ModelThinking<'_>) -> Option<&'static str> {
    let caps = match thinking {
        ModelThinking::Unsupported => return None,
        ModelThinking::Unknown => None,
        ModelThinking::Supported(caps) => Some(caps),
    };
    let level = match depth {
        Depth::Auto => return None,
        Depth::Off => {
            return Some(match caps {
                Some(caps) if !caps.zero_allowed && caps.has_levels() => caps
                    .levels
                    .iter()
                    .min()
                    .map(|e| e.as_str())
                    .unwrap_or("none"),
                _ => "none",
            });
        }
        Depth::Level(level) => level,
        Depth::Budget(budget) => budget_to_effort(budget),
    };
    Some(match caps {
        Some(caps) => caps.clamp_level(level).as_str(),
        None => level.as_str(),
    })
}

fn remove_keys(holder: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        holder.remove(*key);
    }
}

/// Removes `keys` from the object at `path` and prunes every object on the
/// way that became empty because of it.
fn strip_at(obj: &mut Map<String, Value>, path: &[&str], keys: &[&str]) {
    let Some((head, rest)) = path.split_first() else {
        remove_keys(obj, keys);
        return;
    };
    let Some(Value::Object(child)) = obj.get_mut(*head) else {
        return;
    };
    let before = child.len();
    strip_at(child, rest, keys);
    if child.is_empty() && before > 0 {
        obj.remove(*head);
    }
}

/// Removes every depth spelling other than `reasoning_effort`. Visibility
/// switches (`exclude`, `include_thoughts`, `summary`) are left alone.
fn strip_alternate_depths(obj: &mut Map<String, Value>) {
    strip_at(obj, &["reasoning"], &["effort", "max_tokens", "enabled"]);
    strip_at(obj, &["thinking"], &["type", "budget_tokens"]);
    remove_keys(obj, &["enable_thinking", "thinking_budget"]);
    for path in GOOGLE_CONFIG_PATHS {
        strip_at(
            obj,
            path,
            &[
                "thinking_level",
                "thinkingLevel",
                "thinking_budget",
                "thinkingBudget",
            ],
        );
    }
}

/// Writes a fitted depth into a Chat request body as `reasoning_effort`,
/// replacing every depth field that was there. [`Fitted::Strip`] removes
/// them all. Bodies that are not JSON objects are left untouched.
pub(crate) fn write_reasoning(body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    strip_alternate_depths(obj);
    let effort = match depth {
        Fitted::Strip => None,
        Fitted::Use(depth) => effort_for(depth, ctx.thinking),
    };
    match effort {
        Some(effort) => {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        None => {
            obj.remove("reasoning_effort");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::reasoning::{Effort, ThinkingSupport};

    fn depth(body: Value) -> Option<Depth> {
        read_reasoning(&body).depth
    }

    fn summary(body: Value) -> Option<Summary> {
        read_reasoning(&body).summary
    }

    #[test]
    fn effort_string_is_trimmed_and_case_insensitive() {
        assert_eq!(
            depth(json!({"reasoning_effort": " High "})),
            Some(Depth::Level(Effort::High))
        );
        assert_eq!(depth(json!({"reasoning_effort": "none"})), Some(Depth::Off));
        assert_eq!(
            depth(json!({"reasoning_effort": "auto"})),
            Some(Depth::Auto)
        );
        assert_eq!(depth(json!({"reasoning_effort": "ultra"})), None);
        assert_eq!(depth(json!({"reasoning_effort": null})), None);
        assert_eq!(depth(json!({"reasoning_effort": 3})), None);
    }

    #[test]
    fn top_level_effort_wins_over_other_spellings() {
        let body = json!({
            "reasoning_effort": "low",
            "reasoning": {"effort": "high"},
            "thinking": {"type": "disabled"}
        });
        assert_eq!(depth(body), Some(Depth::Level(Effort::Low)));
    }

    #[test]
    fn strip_prunes_only_what_became_empty() {
        let mut body = json!({
            "reasoning": {},
            "thinking": {"type": "enabled"},
            "extra_body": {"google": {"thinking_config": {"thinking_budget": 5}}, "other": 1}
        });
        write_reasoning(&mut body, Fitted::Strip, &UpstreamCtx::default());
        // An object that was already empty is not ours to delete.
        assert_eq!(body, json!({"reasoning": {}, "extra_body": {"other": 1}}));
    }

    #[test]
    fn off_falls_back_to_the_lowest_level_when_none_is_not_accepted() {
        let caps = ThinkingSupport::levels(&[Effort::High, Effort::Low, Effort::Medium]);
        assert_eq!(
            effort_for(Depth::Off, ModelThinking::Supported(&caps)),
            Some("low")
        );
        let mut caps = caps;
        caps.zero_allowed = true;
        assert_eq!(
            effort_for(Depth::Off, ModelThinking::Supported(&caps)),
            Some("none")
        );
        let budget_only = ThinkingSupport::budget(128, 32768);
        assert_eq!(
            effort_for(Depth::Off, ModelThinking::Supported(&budget_only)),
            Some("none")
        );
    }

    #[test]
    fn summary_precedence() {
        assert_eq!(summary(json!({"reasoning_effort": "high"})), None);
        assert_eq!(
            summary(json!({"include_reasoning": true})),
            Some(Summary::Auto)
        );
        assert_eq!(
            summary(json!({"include_reasoning": false})),
            Some(Summary::Off)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": "detailed"}, "include_reasoning": false})),
            Some(Summary::Detailed)
        );
        assert_eq!(
            summary(json!({"reasoning": {"summary": null}})),
            Some(Summary::Off)
        );
        assert_eq!(
            summary(json!({"reasoning": {"generate_summary": "concise"}})),
            Some(Summary::Concise)
        );
        assert_eq!(
            summary(json!({"reasoning": {"exclude": true}})),
            Some(Summary::Off)
        );
        assert_eq!(
            summary(json!({"reasoning": {"exclude": false}})),
            Some(Summary::Auto)
        );
        assert_eq!(
            summary(json!({"reasoning": {"enabled": true}})),
            Some(Summary::Auto)
        );
        assert_eq!(
            summary(json!({
                "extra_body": {"google": {"thinking_config": {"include_thoughts": true}}},
                "reasoning": {"exclude": true}
            })),
            Some(Summary::Auto)
        );
        assert_eq!(
            summary(json!({"thinking": {"includeThoughts": false}})),
            Some(Summary::Off)
        );
        assert_eq!(summary(json!({"include_reasoning": "true"})), None);
    }
}
