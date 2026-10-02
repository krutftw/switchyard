//! Reading and writing reasoning ("thinking") settings on raw Messages bodies.
//!
//! The Messages API spreads reasoning over two objects: `thinking`
//! (`{"type":"enabled","budget_tokens":N}`, `{"type":"adaptive"}`,
//! `{"type":"disabled"}`, optional `display`) and `output_config.effort`.

use crate::util::{as_u64_lenient, i64_field, is_block, str_field, targets_claude};
use serde_json::{Map, Value, json};
use switchyard_core::UpstreamCtx;
use switchyard_core::reasoning::{
    Depth, Effort, Fitted, ModelThinking, ReasoningConfig, Summary, budget_to_effort,
    depth_from_budget, depth_from_effort_str,
};

/// Smallest `budget_tokens` the Messages API accepts.
const MIN_BUDGET: u32 = 1024;

type Body = Map<String, Value>;

fn effort_str(body: &Value) -> Option<String> {
    body.get("output_config")
        .and_then(|config| str_field(config, "effort"))
        .map(|effort| effort.trim().to_ascii_lowercase())
        .filter(|effort| !effort.is_empty())
}

/// Whether the body asks the model to think (`enabled`, `adaptive`, or the
/// `auto` alias some clients use). Sampling restrictions and `display` only
/// apply in that state.
pub(crate) fn thinking_active(body: &Body) -> bool {
    matches!(
        body.get("thinking").and_then(|t| str_field(t, "type")),
        Some("enabled" | "adaptive" | "auto")
    )
}

/// Reads depth and summary intent from a Messages request body.
///
/// Depth, first match wins:
/// 1. `thinking.type == "disabled"` (or `"between_tools"`, the "no extended
///    reasoning" mode of models that cannot disable thinking) → off;
/// 2. `adaptive` / `auto` → `output_config.effort` when present, else
///    [`Depth::Auto`];
/// 3. a numeric `thinking.budget_tokens` → budget (`0` off, `-1` auto);
/// 4. `enabled` without a budget → the effort when present, else auto;
/// 5. no `thinking` object but an `output_config.effort` → that level (on
///    current models thinking is on by default and effort is its only knob).
///
/// Summary intent comes from `thinking.display` and is only meaningful while
/// thinking is active: `summarized` → on, `omitted` → off.
pub(crate) fn read_reasoning(body: &Value) -> ReasoningConfig {
    let thinking = body.get("thinking").filter(|t| t.is_object());
    let kind = thinking
        .and_then(|t| str_field(t, "type"))
        .map(|t| t.trim().to_ascii_lowercase());
    let effort = effort_str(body);
    let effort_depth = |fallback: Option<Depth>| match effort.as_deref() {
        // An effort this gateway does not know still means "think".
        Some(text) => depth_from_effort_str(text).or(Some(Depth::Auto)),
        None => fallback,
    };
    let budget = thinking.and_then(|t| i64_field(t, "budget_tokens"));

    let depth = match (thinking, kind.as_deref()) {
        (None, _) => effort_depth(None),
        (Some(_), Some("disabled" | "between_tools")) => Some(Depth::Off),
        (Some(_), Some("adaptive" | "auto")) => effort_depth(Some(Depth::Auto)),
        (Some(_), other) => match budget {
            Some(budget) => depth_from_budget(budget),
            None if other == Some("enabled") => effort_depth(Some(Depth::Auto)),
            None => effort_depth(None),
        },
    };

    let active = match kind.as_deref() {
        Some("adaptive" | "auto") => true,
        Some("enabled") => budget.is_none_or(|b| b == -1 || b > 0),
        _ => false,
    };
    let summary = if active {
        match thinking.and_then(|t| str_field(t, "display")) {
            Some("summarized") => Some(Summary::Auto),
            Some("omitted") => Some(Summary::Off),
            _ => None,
        }
    } else {
        None
    };
    ReasoningConfig { depth, summary }
}

fn remove_effort(body: &mut Body) {
    let emptied = match body.get_mut("output_config").and_then(Value::as_object_mut) {
        Some(config) => {
            config.shift_remove("effort");
            config.is_empty()
        }
        None => false,
    };
    if emptied {
        body.shift_remove("output_config");
    }
}

fn set_effort(body: &mut Body, effort: &str) {
    let slot = body
        .entry("output_config")
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(config) = slot {
        config.insert("effort".to_string(), Value::String(effort.to_string()));
    }
}

/// Runs `edit` on the `thinking` object, creating it (or replacing a
/// non-object value) first. Editing in place keeps the body's key order.
fn edit_thinking(body: &mut Body, edit: impl FnOnce(&mut Body)) {
    let slot = body
        .entry("thinking")
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(thinking) = slot {
        edit(thinking);
    }
}

fn strip(body: &mut Body) {
    body.shift_remove("thinking");
    remove_effort(body);
}

fn set_disabled(body: &mut Body) {
    // `display` and `budget_tokens` are not allowed next to `disabled`, so
    // the whole object is replaced.
    body.insert("thinking".to_string(), json!({"type": "disabled"}));
    remove_effort(body);
}

fn set_adaptive(body: &mut Body, effort: Option<&str>) {
    edit_thinking(body, |thinking| {
        thinking.insert("type".to_string(), json!("adaptive"));
        thinking.shift_remove("budget_tokens");
    });
    match effort {
        Some(effort) => set_effort(body, effort),
        None => remove_effort(body),
    }
}

/// The spelling of an effort level the Messages API accepts
/// (`low | medium | high | xhigh | max`).
fn wire_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal | Effort::Low => "low",
        other => other.as_str(),
    }
}

/// Writes `enabled` + `budget_tokens`, keeping `budget_tokens < max_tokens`.
///
/// 1. the limit is the body's `max_tokens`; when the body has none and the
///    model's output limit is known, that limit is written as `max_tokens`;
/// 2. a budget at or above the limit is lowered to `limit - 1`;
/// 3. if that would fall under the minimum budget, `max_tokens` is raised to
///    the model's output limit instead (when that is larger) and the budget
///    capped below it;
/// 4. if the model's output limit is unknown or no larger, no budget the API
///    accepts fits under the limit: the thinking fields are removed. A
///    request that small is answered without extended thinking rather than
///    refused (the API would answer `budget_tokens >= max_tokens` with 400).
///
/// A budget under the minimum the API accepts is raised to it first. A body
/// without `max_tokens` for a model of unknown size cannot be checked and
/// is written as asked. `max_tokens: 0` is a cache pre-warm that generates
/// nothing: manual thinking cannot be combined with it, and the limit is
/// the client's to keep, so the thinking fields are removed.
fn set_budget(body: &mut Body, budget: u32, floor: u32, ctx: &UpstreamCtx<'_>) {
    let mut budget = u64::from(budget.max(floor));
    let floor = u64::from(floor);
    let model_max = ctx.max_output_tokens.filter(|max| *max > 0);
    let stated = body.get("max_tokens").and_then(as_u64_lenient);
    if stated == Some(0) {
        strip(body);
        return;
    }
    let mut limit = stated;
    if limit.is_none()
        && let Some(max) = model_max
    {
        body.insert("max_tokens".to_string(), json!(max));
        limit = Some(max);
    }
    if let Some(max) = limit
        && budget >= max
    {
        if max > floor {
            budget = max - 1;
        } else if let Some(model_max) = model_max.filter(|m| *m > max && *m > floor) {
            body.insert("max_tokens".to_string(), json!(model_max));
            budget = budget.min(model_max - 1);
        } else {
            strip(body);
            return;
        }
    }
    edit_thinking(body, |thinking| {
        thinking.insert("type".to_string(), json!("enabled"));
        thinking.insert("budget_tokens".to_string(), json!(budget));
    });
    remove_effort(body);
}

/// What the gateway knows about the target model, reduced to what matters
/// for choosing between the adaptive and the manual form.
enum Class {
    /// Accepts effort levels (adaptive thinking), possibly budgets too.
    Levels { budgets: bool },
    /// Manual budgets only.
    BudgetOnly,
    /// No metadata: use the modern forms without clamping.
    Unknown,
}

fn apply(body: &mut Body, depth: Depth, ctx: &UpstreamCtx<'_>) {
    let (class, caps) = match ctx.thinking {
        ModelThinking::Unsupported => {
            strip(body);
            return;
        }
        ModelThinking::Unknown => (Class::Unknown, None),
        ModelThinking::Supported(caps) => {
            let class = if caps.has_levels() {
                Class::Levels {
                    budgets: caps.has_range(),
                }
            } else if caps.has_range() {
                Class::BudgetOnly
            } else {
                Class::Unknown
            };
            (class, Some(caps))
        }
    };
    let floor = caps
        .map(|caps| caps.min)
        .filter(|min| *min > 0)
        .unwrap_or(MIN_BUDGET);

    match depth {
        Depth::Off => set_disabled(body),
        Depth::Level(level) => match class {
            Class::BudgetOnly => set_budget(body, level.budget(), floor, ctx),
            Class::Levels { .. } | Class::Unknown => {
                let level = caps.map_or(level, |caps| caps.clamp_level(level));
                set_adaptive(body, Some(wire_effort(level)));
            }
        },
        Depth::Budget(budget) => match class {
            Class::Levels { budgets: false } => {
                let level = budget_to_effort(budget);
                let level = caps.map_or(level, |caps| caps.clamp_level(level));
                set_adaptive(body, Some(wire_effort(level)));
            }
            _ => set_budget(body, budget, floor, ctx),
        },
        Depth::Auto => match class {
            // `enabled` needs an explicit budget and budget-only models
            // reject `adaptive`, so "let the provider decide" is expressed by
            // saying nothing.
            Class::BudgetOnly => strip(body),
            Class::Levels { .. } | Class::Unknown => set_adaptive(body, None),
        },
    }
}

/// The API rejects forced tool use (`any` / `tool`) together with thinking.
/// An explicit `disabled` is valid there and is kept.
pub(crate) fn drop_thinking_for_forced_tool_choice(body: &mut Body) {
    let forced = matches!(
        body.get("tool_choice").and_then(|c| str_field(c, "type")),
        Some("any" | "tool")
    );
    if forced && thinking_active(body) {
        strip(body);
    }
}

fn lower_role(message: &Value) -> String {
    str_field(message, "role")
        .unwrap_or("user")
        .trim()
        .to_ascii_lowercase()
}

fn content_blocks(message: &Value) -> &[Value] {
    message
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Whether the assistant turn that the request continues lacks the thinking
/// block manual thinking requires at its start.
///
/// The turn in progress is everything after the last user message that
/// carries no `tool_result`: a tool loop (assistant message, tool results,
/// assistant message, …) is one turn, and so is a trailing assistant message
/// (prefill). A conversation that ends with an ordinary user message starts
/// a new turn, for which there is no constraint.
fn turn_in_progress_lacks_thinking(messages: &[Value]) -> bool {
    let mut first_assistant: Option<&Value> = None;
    let mut last = true;
    for message in messages.iter().rev() {
        match lower_role(message).as_str() {
            "assistant" => first_assistant = Some(message),
            // Mid-conversation instructions belong to no turn.
            "system" | "developer" => continue,
            _ => {
                let continues = content_blocks(message)
                    .iter()
                    .any(|block| is_block(block, "tool_result"));
                if !continues {
                    if last {
                        return false;
                    }
                    break;
                }
            }
        }
        last = false;
    }
    first_assistant.is_some_and(|message| {
        !content_blocks(message).first().is_some_and(|block| {
            is_block(block, "thinking") || is_block(block, "redacted_thinking")
        })
    })
}

/// With manual thinking (`thinking.type: "enabled"`) the API requires the
/// assistant turn in progress to open with a `thinking` or
/// `redacted_thinking` block ("When `thinking` is enabled, a final
/// `assistant` message must start with a thinking block"); adaptive thinking
/// has no such rule. A tool loop whose signed thinking is not available —
/// served by another vendor, issued to a client that has no slot for it, or
/// begun with thinking off — cannot satisfy it, and the API's answer is a
/// 400 that repeats for every later request of the loop. Thinking is left
/// out for such a request instead; it comes back with the next turn.
///
/// Only `thinking` goes; `output_config.effort` is valid without it. The
/// rule is Anthropic's, so it is applied to Claude models only (see
/// [`targets_claude`]): other models served over this protocol take manual
/// thinking for any history.
pub(crate) fn drop_manual_thinking_without_turn_start(body: &mut Body) {
    let manual = body.get("thinking").and_then(|t| str_field(t, "type")) == Some("enabled");
    if !manual || !targets_claude(body) {
        return;
    }
    let lacking = body
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| turn_in_progress_lacks_thinking(messages));
    if lacking {
        body.shift_remove("thinking");
    }
}

/// Removes sampling parameters the API would reject.
///
/// * `strict` (translated requests): while thinking is active `temperature`,
///   `top_p` and `top_k` are all dropped; otherwise `top_p` is dropped when
///   `temperature` is also present.
/// * not `strict` (a native body being patched): while thinking is active only
///   the values the API actually refuses go — `temperature` other than 1,
///   `top_p` below 0.95, any `top_k`.
pub(crate) fn fix_sampling(body: &mut Body, strict: bool) {
    let number = |body: &Body, key: &str| body.get(key).and_then(Value::as_f64);
    if thinking_active(body) {
        if strict || number(body, "temperature").is_some_and(|t| t != 1.0) {
            body.shift_remove("temperature");
        }
        if strict || number(body, "top_p").is_some_and(|p| p < 0.95) {
            body.shift_remove("top_p");
        }
        body.shift_remove("top_k");
    } else if strict && number(body, "temperature").is_some() && body.contains_key("top_p") {
        body.shift_remove("top_p");
    }
}

/// Writes a fitted depth into a Messages body, replacing the depth fields
/// that were there, and keeps the body valid: `budget_tokens < max_tokens`,
/// no thinking with forced tool use, no manual thinking for a turn in
/// progress that does not open with a thinking block, no sampling values the
/// API refuses while thinking.
///
/// | depth | model with levels (or unknown) | budget-only model |
/// |---|---|---|
/// | off | `thinking: {"type":"disabled"}` | same |
/// | level | `adaptive` + `output_config.effort` | `enabled` + table budget |
/// | budget | `enabled` + `budget_tokens` (level-only: nearest effort) | `enabled` + `budget_tokens` |
/// | auto | `adaptive`, effort removed | depth fields removed |
///
/// `thinking.display` survives unless thinking ends up disabled. Where the
/// table says `enabled` and the body cannot be made valid with it (see
/// [`set_budget`] and [`drop_manual_thinking_without_turn_start`]) the
/// thinking fields are removed instead.
pub(crate) fn write_reasoning(body: &mut Value, depth: Fitted, ctx: &UpstreamCtx<'_>) {
    let Some(body) = body.as_object_mut() else {
        return;
    };
    match depth {
        Fitted::Strip => strip(body),
        Fitted::Use(depth) => apply(body, depth, ctx),
    }
    drop_thinking_for_forced_tool_choice(body);
    drop_manual_thinking_without_turn_start(body);
    fix_sampling(body, false);
}

/// Sets `thinking.display` from a summary intent. A no-op unless thinking is
/// active: `display` is invalid otherwise and the intent never turns
/// thinking on by itself.
pub(crate) fn write_summary(body: &mut Body, summary: Summary) {
    if !thinking_active(body) {
        return;
    }
    let display = if summary.is_on() {
        "summarized"
    } else {
        "omitted"
    };
    edit_thinking(body, |thinking| {
        thinking.insert("display".to_string(), json!(display));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::reasoning::ThinkingSupport;

    #[test]
    fn effort_spelling() {
        assert_eq!(wire_effort(Effort::Minimal), "low");
        assert_eq!(wire_effort(Effort::Xhigh), "xhigh");
        assert_eq!(wire_effort(Effort::Max), "max");
    }

    #[test]
    fn budget_rule_prefers_shrinking_then_raising() {
        let caps = ThinkingSupport::budget(1024, 128_000);
        let ctx = UpstreamCtx {
            thinking: ModelThinking::Supported(&caps),
            max_output_tokens: Some(64_000),
            ..UpstreamCtx::default()
        };
        // Fits: untouched.
        let mut body = json!({"max_tokens": 16000});
        write_reasoning(&mut body, Fitted::Use(Depth::Budget(8192)), &ctx);
        assert_eq!(
            body,
            json!({"max_tokens": 16000, "thinking": {"type": "enabled", "budget_tokens": 8192}})
        );
        // Too large: shrunk to max_tokens - 1.
        let mut body = json!({"max_tokens": 4096});
        write_reasoning(&mut body, Fitted::Use(Depth::Budget(8192)), &ctx);
        assert_eq!(body["thinking"]["budget_tokens"], json!(4095));
        assert_eq!(body["max_tokens"], json!(4096));
        // Shrinking would go under the minimum: max_tokens raised instead.
        let mut body = json!({"max_tokens": 1000});
        write_reasoning(&mut body, Fitted::Use(Depth::Budget(8192)), &ctx);
        assert_eq!(body["max_tokens"], json!(64000));
        assert_eq!(body["thinking"]["budget_tokens"], json!(8192));
        // No max_tokens: the model limit is written.
        let mut body = json!({});
        write_reasoning(&mut body, Fitted::Use(Depth::Budget(100_000)), &ctx);
        assert_eq!(body["max_tokens"], json!(64000));
        assert_eq!(body["thinking"]["budget_tokens"], json!(63999));
    }

    #[test]
    fn key_order_of_untouched_fields_is_preserved() {
        let mut body = json!({"model": "m", "thinking": {"type": "enabled", "budget_tokens": 2048},
                              "max_tokens": 4096, "messages": []});
        write_reasoning(&mut body, Fitted::Strip, &UpstreamCtx::default());
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "max_tokens", "messages"]);
    }
}
