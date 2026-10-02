//! Reading and writing `generationConfig.thinkingConfig` on raw bodies.
//!
//! Gemini expresses reasoning depth either as a token budget
//! (`thinkingBudget`, 2.5 models; `-1` dynamic, `0` off) or as a level
//! (`thinkingLevel`, 3.x models). The two must never be sent together.
//! `includeThoughts` controls whether thought summaries are returned and is
//! independent of the depth.

use crate::util::{num_i64, pick, pick_str};
use serde_json::{Map, Value};
use switchyard_core::UpstreamCtx;
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, ThinkingSupport,
    budget_to_effort, depth_from_budget, depth_from_effort_str,
};

const GENERATION_CONFIG: [&str; 2] = ["generationConfig", "generation_config"];
const THINKING_CONFIG: [&str; 2] = ["thinkingConfig", "thinking_config"];
const DEPTH_KEYS: [&str; 4] = [
    "thinkingBudget",
    "thinking_budget",
    "thinkingLevel",
    "thinking_level",
];
const INCLUDE_KEYS: [&str; 2] = ["includeThoughts", "include_thoughts"];

/// Reads depth and summary intent from a Gemini request body. Both the REST
/// (`camelCase`) and the SDK (`snake_case`) spellings are understood; a level
/// wins over a budget when a body carries both.
pub(crate) fn read_reasoning(body: &Value) -> ReasoningConfig {
    let Some(thinking) = pick(body, &GENERATION_CONFIG).and_then(|gc| pick(gc, &THINKING_CONFIG))
    else {
        return ReasoningConfig::default();
    };
    let level =
        pick_str(thinking, &["thinkingLevel", "thinking_level"]).and_then(depth_from_effort_str);
    let depth = level.or_else(|| {
        pick(thinking, &["thinkingBudget", "thinking_budget"])
            .and_then(num_i64)
            .and_then(depth_from_budget)
    });
    let summary = pick(thinking, &INCLUDE_KEYS)
        .and_then(Value::as_bool)
        .map(|on| if on { Summary::Auto } else { Summary::Off });
    ReasoningConfig { depth, summary }
}

/// Gemini's spelling of a level. It has no tier above `high`.
fn level_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High | Effort::Xhigh | Effort::Max => "high",
    }
}

fn clamp_budget(caps: &ThinkingSupport, budget: u32) -> u32 {
    let mut budget = budget;
    if caps.min > 0 && budget < caps.min {
        budget = caps.min;
    }
    if caps.max > 0 && budget > caps.max {
        budget = caps.max;
    }
    budget
}

/// The single depth field to write for `depth`.
///
/// The depth normally arrives already fitted to the model; the conversions
/// here only guard against a level reaching a budget-only model (or a budget
/// reaching a level-only one), which Gemini answers with a 400.
fn render(depth: Depth, thinking: ModelThinking<'_>) -> (&'static str, Value) {
    let caps = match thinking {
        ModelThinking::Supported(caps) => Some(caps),
        ModelThinking::Unknown | ModelThinking::Unsupported => None,
    };
    match depth {
        Depth::Off => ("thinkingBudget", Value::from(0)),
        Depth::Auto => ("thinkingBudget", Value::from(-1)),
        Depth::Budget(budget) => match caps {
            Some(caps) if caps.has_levels() && !caps.has_range() => {
                let level = caps.clamp_level(budget_to_effort(budget));
                ("thinkingLevel", Value::from(level_name(level)))
            }
            _ => ("thinkingBudget", Value::from(budget)),
        },
        Depth::Level(effort) => match caps {
            Some(caps) if caps.has_range() && !caps.has_levels() => (
                "thinkingBudget",
                Value::from(clamp_budget(caps, effort.budget())),
            ),
            _ => ("thinkingLevel", Value::from(level_name(effort))),
        },
    }
}

/// Removes the thinking config (either spelling) from the body and returns
/// its entries together with the generation-config key that held it.
fn take_thinking(root: &mut Map<String, Value>) -> (Map<String, Value>, &'static str) {
    let key = GENERATION_CONFIG
        .into_iter()
        .find(|key| root.get(*key).is_some_and(Value::is_object))
        .unwrap_or("generationConfig");
    let mut thinking = Map::new();
    if let Some(Value::Object(config)) = root.get_mut(key) {
        for spelling in THINKING_CONFIG {
            if let Some(Value::Object(existing)) = config.shift_remove(spelling) {
                for (k, v) in existing {
                    thinking.entry(k).or_insert(v);
                }
            }
        }
    }
    (thinking, key)
}

fn put_thinking(root: &mut Map<String, Value>, key: &str, thinking: Map<String, Value>) {
    if thinking.is_empty() {
        // Do not leave an empty `generationConfig` behind.
        if root
            .get(key)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            root.shift_remove(key);
        }
        return;
    }
    if !root.get(key).is_some_and(Value::is_object) {
        root.insert(key.to_string(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(config)) = root.get_mut(key) {
        config.insert("thinkingConfig".to_string(), Value::Object(thinking));
    }
}

/// Writes a fitted depth into a Gemini request body.
///
/// Exactly one of `thinkingBudget` / `thinkingLevel` is left behind (none for
/// [`Fitted::Strip`]); snake-case duplicates are removed and an explicit
/// `includeThoughts` boolean is kept under its camel-case name. When the
/// model is known not to think the whole `thinkingConfig` goes, because
/// `includeThoughts` alone is rejected by such models.
pub(crate) fn write_reasoning(body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    let (mut thinking, key) = take_thinking(root);
    let include = INCLUDE_KEYS
        .iter()
        .find_map(|k| thinking.get(*k).and_then(Value::as_bool));
    for stale in DEPTH_KEYS.iter().chain(INCLUDE_KEYS.iter()) {
        thinking.shift_remove(*stale);
    }
    let unsupported = matches!(ctx.thinking, ModelThinking::Unsupported);
    match depth {
        Fitted::Strip if unsupported => thinking.clear(),
        Fitted::Strip => {}
        Fitted::Use(depth) => {
            let (field, value) = render(depth, ctx.thinking);
            thinking.insert(field.to_string(), value);
        }
    }
    if let Some(include) = include
        && !(unsupported && depth == Fitted::Strip)
    {
        thinking.insert("includeThoughts".to_string(), Value::Bool(include));
    }
    put_thinking(root, key, thinking);
}

/// Sets `generationConfig.thinkingConfig.includeThoughts`.
pub(crate) fn set_include_thoughts(body: &mut Value, include: bool) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    let (mut thinking, key) = take_thinking(root);
    for stale in INCLUDE_KEYS {
        thinking.shift_remove(stale);
    }
    thinking.insert("includeThoughts".to_string(), Value::Bool(include));
    put_thinking(root, key, thinking);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn render_guards_against_the_wrong_representation() {
        let budget_only = ThinkingSupport::budget(128, 32768);
        let level_only = ThinkingSupport::levels(&[Effort::Low, Effort::High]);
        assert_eq!(
            render(
                Depth::Level(Effort::Max),
                ModelThinking::Supported(&budget_only)
            ),
            ("thinkingBudget", json!(32768))
        );
        assert_eq!(
            render(
                Depth::Level(Effort::Minimal),
                ModelThinking::Supported(&ThinkingSupport::budget(1024, 0))
            ),
            ("thinkingBudget", json!(1024))
        );
        assert_eq!(
            render(Depth::Budget(9000), ModelThinking::Supported(&level_only)),
            ("thinkingLevel", json!("high"))
        );
        assert_eq!(
            render(Depth::Budget(600), ModelThinking::Supported(&level_only)),
            ("thinkingLevel", json!("low"))
        );
        assert_eq!(
            render(Depth::Level(Effort::Xhigh), ModelThinking::Unknown),
            ("thinkingLevel", json!("high"))
        );
    }
}
