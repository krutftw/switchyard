//! Reading and writing reasoning settings on raw Responses request bodies.
//!
//! Depth lives in `reasoning.effort`; summary visibility in
//! `reasoning.summary` (deprecated alias `reasoning.generate_summary`). The
//! two are independent: stripping the depth keeps the summary request and
//! vice versa.

use crate::common::type_of;
use serde_json::{Map, Value};
use switchyard_core::reasoning::{
    Depth, Fitted, ModelThinking, ReasoningConfig, Summary, budget_to_effort, depth_from_effort_str,
};
use switchyard_core::util::str_field;
use switchyard_core::{Effort, UpstreamCtx};

/// Reads depth and summary intent from a Responses request body.
///
/// * `reasoning.effort`: `none`, `auto`, or a level; unknown strings yield no
///   depth (the upstream will judge them).
/// * `input[]` items `{"type":"configuration_update","reasoning":{"effort"}}`
///   override the top-level effort; the last one wins.
/// * `reasoning.summary`, then `reasoning.generate_summary`: `auto` /
///   `concise` / `detailed` enable a summary, `"none"` and JSON `null`
///   disable it, anything else (and absence) says nothing.
pub(crate) fn read_reasoning(body: &Value) -> ReasoningConfig {
    let mut config = ReasoningConfig::default();
    if let Some(reasoning) = body.get("reasoning").filter(|v| v.is_object()) {
        config.depth = str_field(reasoning, "effort").and_then(depth_from_effort_str);
        config.summary = ["summary", "generate_summary"]
            .iter()
            .find_map(|key| reasoning.get(*key).and_then(summary_from_value));
    }
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        let update = items.iter().rev().find_map(|item| {
            if type_of(item) != "configuration_update" {
                return None;
            }
            item.get("reasoning")
                .and_then(|r| str_field(r, "effort"))
                .filter(|effort| !effort.trim().is_empty())
                .and_then(depth_from_effort_str)
        });
        if update.is_some() {
            config.depth = update;
        }
    }
    config
}

fn summary_from_value(value: &Value) -> Option<Summary> {
    match value {
        Value::Null => Some(Summary::Off),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Summary::Auto),
            "concise" => Some(Summary::Concise),
            "detailed" => Some(Summary::Detailed),
            "none" => Some(Summary::Off),
            _ => None,
        },
        _ => None,
    }
}

/// The `reasoning.effort` string for a fitted depth, or `None` when the field
/// must be absent.
///
/// * `Off` → `"none"`.
/// * `Auto` → absent: Responses has no "dynamic" effort, the model default is
///   the closest thing.
/// * `Level` → the level, clamped to the model's level set when it is known.
/// * `Budget` → bucketed into a level (Responses has no token budgets), then
///   clamped likewise.
fn effort_for(depth: Depth, thinking: ModelThinking<'_>) -> Option<&'static str> {
    let clamp = |effort: Effort| match thinking {
        ModelThinking::Supported(caps) if caps.has_levels() => caps.clamp_level(effort),
        _ => effort,
    };
    match depth {
        Depth::Off => Some("none"),
        Depth::Auto => None,
        Depth::Level(effort) => Some(clamp(effort).as_str()),
        Depth::Budget(budget) => Some(clamp(budget_to_effort(budget)).as_str()),
    }
}

/// Writes a fitted depth into a Responses request body.
///
/// [`Fitted::Strip`] (and any depth for a model known not to reason) removes
/// `reasoning.effort` while keeping `reasoning.summary`; a `reasoning` object
/// left empty is removed altogether. `configuration_update` input items are
/// kept consistent so a stale per-turn effort cannot override what was just
/// written.
pub(crate) fn write_reasoning(body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    let effort = match (depth, ctx.thinking) {
        (Fitted::Strip, _) | (_, ModelThinking::Unsupported) => None,
        (Fitted::Use(depth), thinking) => effort_for(depth, thinking),
    };

    match effort {
        Some(effort) => {
            let slot = root
                .entry("reasoning")
                .or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            if let Some(reasoning) = slot.as_object_mut() {
                reasoning.insert("effort".into(), Value::String(effort.to_string()));
            }
        }
        None => {
            let emptied = match root.get_mut("reasoning").and_then(Value::as_object_mut) {
                Some(reasoning) => {
                    reasoning.remove("effort");
                    reasoning.is_empty()
                }
                None => false,
            };
            if emptied {
                root.remove("reasoning");
            }
        }
    }

    if let Some(items) = root.get_mut("input").and_then(Value::as_array_mut) {
        for item in items.iter_mut() {
            if type_of(item) != "configuration_update" {
                continue;
            }
            let Some(reasoning) = item.get_mut("reasoning").and_then(Value::as_object_mut) else {
                continue;
            };
            if !reasoning.contains_key("effort") {
                continue;
            }
            match effort {
                Some(effort) => {
                    reasoning.insert("effort".into(), Value::String(effort.to_string()));
                }
                None => {
                    reasoning.remove("effort");
                }
            }
        }
    }
}

/// Writes summary intent the way `encode_request` needs it: an enabled
/// summary sets `reasoning.summary`, a disabled one leaves the field out
/// (omission is the documented "off"; `null` is not accepted everywhere).
pub(crate) fn write_summary(body: &mut Map<String, Value>, summary: Summary) {
    let Some(detail) = summary.as_openai() else {
        return;
    };
    let slot = body
        .entry("reasoning")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(reasoning) = slot.as_object_mut() {
        reasoning.insert("summary".into(), Value::String(detail.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use switchyard_core::ThinkingSupport;

    #[test]
    fn effort_mapping() {
        let unknown = ModelThinking::Unknown;
        assert_eq!(effort_for(Depth::Off, unknown), Some("none"));
        assert_eq!(effort_for(Depth::Auto, unknown), None);
        assert_eq!(effort_for(Depth::Level(Effort::Max), unknown), Some("max"));
        assert_eq!(effort_for(Depth::Budget(512), unknown), Some("minimal"));
        assert_eq!(effort_for(Depth::Budget(9000), unknown), Some("high"));
        let caps = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
        let known = ModelThinking::Supported(&caps);
        assert_eq!(effort_for(Depth::Level(Effort::Max), known), Some("high"));
        assert_eq!(effort_for(Depth::Budget(100), known), Some("low"));
    }

    #[test]
    fn summary_values() {
        assert_eq!(summary_from_value(&json!(null)), Some(Summary::Off));
        assert_eq!(summary_from_value(&json!("none")), Some(Summary::Off));
        assert_eq!(
            summary_from_value(&json!("Detailed")),
            Some(Summary::Detailed)
        );
        assert_eq!(summary_from_value(&json!(true)), None);
        assert_eq!(summary_from_value(&json!("verbose")), None);
    }
}
