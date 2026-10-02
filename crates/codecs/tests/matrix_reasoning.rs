//! Matrix (f): reasoning depth across protocols.
//!
//! For every ordered pair `(C, U)`, every native `C` spelling of
//! none / auto / low / medium / high / xhigh / max / explicit budgets, and a
//! set of representative target models (the catalog shapes of notes 12 §1,
//! plus "unknown" and "does not reason"), the request goes
//! `C.decode_request` → `normalize_depth` → `U.encode_request` and the
//! reasoning fields of the `U` body must be exactly the ones written down
//! here.
//!
//! The same goes for the *visibility* of the reasoning text, which is a
//! separate setting (notes 12 §8): Responses `reasoning.summary`, Gemini
//! `includeThoughts`, Anthropic `thinking.display`. It is checked for every
//! depth a client asks for without saying anything about visibility (only a
//! Chat client's depth implies one), and for explicit "show" / "hide" in
//! every client's native spelling.
//!
//! The expected values are literals taken from notes 12 (§3 conversion
//! tables, §5.2 normalisation and its worked results, §6 unknown models, §7
//! rendering per provider, §8 summary visibility). Nothing here is computed
//! with the code under test.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, decode_request, fit_reasoning, short,
    upstream_model, validate_request, validate_translated_request,
};
use switchyard_codecs::codec;
use switchyard_core::reasoning::{Depth, Effort, ModelThinking, ThinkingSupport};
use switchyard_core::{Protocol, UpstreamCtx};

/// The depths a client can ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    None,
    Auto,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    /// An explicit budget of 512 tokens (below every vendor's floor but
    /// Gemini flash's).
    Budget512,
    /// 8192 tokens: the top of the `medium` bucket (notes 12 §3).
    Budget8192,
    /// 64000 tokens: in the `xhigh` bucket and above Gemini's ranges.
    Budget64000,
}

impl Ask {
    /// The canonical depth every native spelling of this ask must decode to
    /// (notes 12 §4).
    fn depth(self) -> Depth {
        match self {
            Ask::None => Depth::Off,
            Ask::Auto => Depth::Auto,
            Ask::Minimal => Depth::Level(Effort::Minimal),
            Ask::Low => Depth::Level(Effort::Low),
            Ask::Medium => Depth::Level(Effort::Medium),
            Ask::High => Depth::Level(Effort::High),
            Ask::Xhigh => Depth::Level(Effort::Xhigh),
            Ask::Max => Depth::Level(Effort::Max),
            Ask::Budget512 => Depth::Budget(512),
            Ask::Budget8192 => Depth::Budget(8192),
            Ask::Budget64000 => Depth::Budget(64000),
        }
    }
}

/// A plain request of each client protocol with reasoning asked for in that
/// protocol's native spelling. `None` when the protocol cannot spell it.
fn client_body(client: Protocol, ask: Ask) -> Option<Value> {
    let effort = |ask: Ask| match ask {
        Ask::None => Some("none"),
        Ask::Auto => Some("auto"),
        Ask::Minimal => Some("minimal"),
        Ask::Low => Some("low"),
        Ask::Medium => Some("medium"),
        Ask::High => Some("high"),
        Ask::Xhigh => Some("xhigh"),
        Ask::Max => Some("max"),
        _ => None,
    };
    let budget = |ask: Ask| match ask {
        Ask::Budget512 => Some(512),
        Ask::Budget8192 => Some(8192),
        Ask::Budget64000 => Some(64000),
        _ => None,
    };
    Some(match client {
        // `reasoning_effort` (notes 12 §4.1); budgets in the spelling of the
        // Anthropic-style compatible servers, which the Chat codec reads.
        Protocol::OpenaiChat => {
            let mut body =
                json!({"model": "gpt-5.5", "messages": [{"role": "user", "content": "Think."}]});
            match (effort(ask), budget(ask)) {
                (Some(effort), _) => body["reasoning_effort"] = json!(effort),
                (None, Some(budget)) => {
                    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget})
                }
                (None, None) => return None,
            }
            body
        }
        // `reasoning.effort` (notes 12 §4.2). Responses has no budgets.
        Protocol::OpenaiResponses => {
            json!({"model": "gpt-5.5", "input": "Think.", "reasoning": {"effort": effort(ask)?}})
        }
        // Notes 12 §4.3: `disabled`; `adaptive` with or without
        // `output_config.effort`; `enabled` with a budget. Anthropic has no
        // `minimal` effort.
        Protocol::Anthropic => {
            let mut body = json!({
                "model": "claude-sonnet-4-5", "max_tokens": 128000,
                "messages": [{"role": "user", "content": "Think."}]
            });
            match ask {
                Ask::None => body["thinking"] = json!({"type": "disabled"}),
                Ask::Auto => body["thinking"] = json!({"type": "adaptive"}),
                Ask::Minimal => return None,
                Ask::Low | Ask::Medium | Ask::High | Ask::Xhigh | Ask::Max => {
                    body["thinking"] = json!({"type": "adaptive"});
                    body["output_config"] = json!({"effort": effort(ask)?});
                }
                _ => body["thinking"] = json!({"type": "enabled", "budget_tokens": budget(ask)?}),
            }
            body
        }
        // Notes 12 §4.4: `thinkingBudget` 0 / -1 / n, or `thinkingLevel`.
        // Gemini's levels stop at `high`.
        Protocol::Gemini => {
            let config = match ask {
                Ask::None => json!({"thinkingBudget": 0}),
                Ask::Auto => json!({"thinkingBudget": -1}),
                Ask::Minimal | Ask::Low | Ask::Medium | Ask::High => {
                    json!({"thinkingLevel": effort(ask)?})
                }
                Ask::Xhigh | Ask::Max => return None,
                _ => json!({"thinkingBudget": budget(ask)?}),
            };
            json!({
                "contents": [{"role": "user", "parts": [{"text": "Think."}]}],
                "generationConfig": {"thinkingConfig": config}
            })
        }
    })
}

const ASKS: [Ask; 11] = [
    Ask::None,
    Ask::Auto,
    Ask::Minimal,
    Ask::Low,
    Ask::Medium,
    Ask::High,
    Ask::Xhigh,
    Ask::Max,
    Ask::Budget512,
    Ask::Budget8192,
    Ask::Budget64000,
];

/// A target model, by what the gateway knows about its reasoning.
struct Target {
    name: &'static str,
    /// `None`: the model is known not to reason.
    caps: Option<ThinkingSupport>,
    unknown: bool,
    /// The model's output limit (Anthropic needs it for `max_tokens`).
    max_output_tokens: Option<u64>,
    /// The expected reasoning fields for each ask.
    /// The expected reasoning fields for each ask. The flag says whether
    /// the client stated an output limit (only Anthropic clients must).
    expect: fn(Ask, bool) -> Value,
}

fn levels(levels: &[Effort]) -> ThinkingSupport {
    ThinkingSupport::levels(levels)
}

fn caps(min: u32, max: u32, zero: bool, dynamic: bool, levels: &[Effort]) -> ThinkingSupport {
    ThinkingSupport {
        min,
        max,
        zero_allowed: zero,
        dynamic_allowed: dynamic,
        levels: levels.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// OpenAI targets (Chat: `reasoning_effort`; Responses: `reasoning.effort`).
// The expectation is the effort string, or null when the field must be absent.
// ---------------------------------------------------------------------------

/// "OpenAI reasoning models: `{levels:[low,medium,high,xhigh]}`" — a
/// level-only model that cannot be switched off and has no dynamic mode.
fn openai_levels_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        // §5.2 step 6: off on a level model without `none` → `Levels[0]`.
        Ask::None => json!("low"),
        // §5.2 step 5: auto without `DynamicAllowed` on a level model → medium.
        Ask::Auto => json!("medium"),
        // §3 clampLevel: nearest supported level.
        Ask::Minimal => json!("low"),
        Ask::Low => json!("low"),
        Ask::Medium => json!("medium"),
        Ask::High => json!("high"),
        Ask::Xhigh => json!("xhigh"),
        // §3 high-intent remap: max → first supported of (max, xhigh, high).
        Ask::Max => json!("xhigh"),
        // §3 budget → level: 1..=512 minimal (→ low here), 1025..=8192
        // medium, >= 24577 xhigh.
        Ask::Budget512 => json!("low"),
        Ask::Budget8192 => json!("medium"),
        Ask::Budget64000 => json!("xhigh"),
    }
}

/// The model of the worked results in §5.2:
/// `levels [minimal, low, medium, high]`.
fn openai_minimal_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        // "`none` → effort `minimal`", "`auto` → effort `medium`".
        Ask::None => json!("minimal"),
        Ask::Auto => json!("medium"),
        Ask::Minimal => json!("minimal"),
        Ask::Low => json!("low"),
        Ask::Medium => json!("medium"),
        Ask::High => json!("high"),
        // The reference answers a same-family `xhigh` with a 400; the
        // gateway never rejects a depth and clamps instead (`docs/DESIGN.md`
        // §2), as the reference does across families ("budget 64000 → effort
        // `high` (xhigh clamped)").
        Ask::Xhigh | Ask::Max => json!("high"),
        Ask::Budget512 => json!("minimal"),
        Ask::Budget8192 => json!("medium"),
        Ask::Budget64000 => json!("high"),
    }
}

/// §6, unknown models: levels are kept, `none` is written, budgets are
/// bucketed. `auto` is not an effort the vendor accepts (notes 15 §3.1), so
/// the field is left out and the model default applies (the reference would
/// forward the literal `"auto"`).
fn openai_unknown_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => json!("none"),
        Ask::Auto => Value::Null,
        Ask::Minimal => json!("minimal"),
        Ask::Low => json!("low"),
        Ask::Medium => json!("medium"),
        Ask::High => json!("high"),
        Ask::Xhigh => json!("xhigh"),
        Ask::Max => json!("max"),
        Ask::Budget512 => json!("minimal"),
        Ask::Budget8192 => json!("medium"),
        Ask::Budget64000 => json!("xhigh"),
    }
}

/// §5 step 6: a model known not to reason gets no reasoning field.
fn stripped_expect(_: Ask, _limit: bool) -> Value {
    Value::Null
}

fn openai_targets() -> Vec<Target> {
    vec![
        Target {
            name: "levels low..xhigh",
            caps: Some(levels(&[
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
            ])),
            unknown: false,
            max_output_tokens: Some(128_000),
            expect: openai_levels_expect,
        },
        Target {
            name: "levels minimal..high",
            caps: Some(levels(&[
                Effort::Minimal,
                Effort::Low,
                Effort::Medium,
                Effort::High,
            ])),
            unknown: false,
            max_output_tokens: Some(128_000),
            expect: openai_minimal_expect,
        },
        Target {
            name: "unknown model",
            caps: None,
            unknown: true,
            max_output_tokens: None,
            expect: openai_unknown_expect,
        },
        Target {
            name: "no reasoning",
            caps: None,
            unknown: false,
            max_output_tokens: Some(16_384),
            expect: stripped_expect,
        },
    ]
}

// ---------------------------------------------------------------------------
// Anthropic targets. The expectation is `{thinking, effort}`.
// ---------------------------------------------------------------------------

fn enabled(budget: u64) -> Value {
    json!({"thinking": {"type": "enabled", "budget_tokens": budget}, "effort": null})
}

fn adaptive(effort: Option<&str>) -> Value {
    json!({"thinking": {"type": "adaptive"}, "effort": effort})
}

fn disabled() -> Value {
    json!({"thinking": {"type": "disabled"}, "effort": null})
}

/// "Claude manual-only (4.5 line): `{min:1024,max:128000,zero_allowed}`",
/// with an output limit of 128000. Worked results of §5.2: Chat `medium` /
/// `xhigh` / `none` / `auto` → `8192` / `32768` / disabled / `64512`.
fn claude_manual_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => disabled(),
        // (1024 + 128000) / 2.
        Ask::Auto => enabled(64512),
        // §3 level → budget.
        Ask::Minimal => enabled(1024), // 512, raised to the model minimum
        Ask::Low => enabled(1024),
        Ask::Medium => enabled(8192),
        Ask::High => enabled(24576),
        Ask::Xhigh => enabled(32768),
        // 128000, lowered to `max_tokens - 1` (§7.3 `budget < max_tokens`).
        Ask::Max => enabled(127_999),
        Ask::Budget512 => enabled(1024),
        Ask::Budget8192 => enabled(8192),
        Ask::Budget64000 => enabled(64000),
    }
}

/// "Claude manual + adaptive (4.6): `{min:1024,max:128000,zero_allowed,
/// levels:[low,medium,high,max]}`" — hybrid: the caller's form is kept.
fn claude_hybrid_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => disabled(),
        // No dynamic mode: the midpoint budget (§5.2 step 5).
        Ask::Auto => enabled(64512),
        // §7.3: level on an adaptive model → `adaptive` + effort; Anthropic
        // has no `minimal` (MapToClaudeEffort: minimal → low).
        Ask::Minimal => adaptive(Some("low")),
        Ask::Low => adaptive(Some("low")),
        Ask::Medium => adaptive(Some("medium")),
        Ask::High => adaptive(Some("high")),
        // High-intent remap: xhigh → first supported of (xhigh, max, high).
        Ask::Xhigh => adaptive(Some("max")),
        Ask::Max => adaptive(Some("max")),
        Ask::Budget512 => enabled(1024),
        Ask::Budget8192 => enabled(8192),
        Ask::Budget64000 => enabled(64000),
    }
}

/// "Claude adaptive-only (newest): `{zero_allowed,dynamic_allowed,
/// levels:[low,medium,high,xhigh,max]}`" — level-only.
fn claude_adaptive_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => disabled(),
        // §7.3: auto on an adaptive model → `adaptive`, upstream default effort.
        Ask::Auto => adaptive(None),
        Ask::Minimal => adaptive(Some("low")),
        Ask::Low => adaptive(Some("low")),
        Ask::Medium => adaptive(Some("medium")),
        Ask::High => adaptive(Some("high")),
        Ask::Xhigh => adaptive(Some("xhigh")),
        Ask::Max => adaptive(Some("max")),
        // §5.2 step 1, LevelOnly + Budget: thresholds, then clampLevel.
        Ask::Budget512 => adaptive(Some("low")),
        Ask::Budget8192 => adaptive(Some("medium")),
        Ask::Budget64000 => adaptive(Some("xhigh")),
    }
}

/// §6, unknown models: `disabled`; level → `adaptive` + effort; budget →
/// `enabled` + `budget_tokens`. For auto the reference writes `enabled`
/// without a budget, which the API refuses (notes 15 §5.6: `enabled` needs
/// `budget_tokens`); the codec writes `adaptive` without effort instead.
fn claude_unknown_expect(ask: Ask, limit: bool) -> Value {
    match ask {
        Ask::None => disabled(),
        Ask::Auto => adaptive(None),
        Ask::Minimal => adaptive(Some("low")),
        Ask::Low => adaptive(Some("low")),
        Ask::Medium => adaptive(Some("medium")),
        Ask::High => adaptive(Some("high")),
        Ask::Xhigh => adaptive(Some("xhigh")),
        Ask::Max => adaptive(Some("max")),
        // Below the API's minimum of 1024 (notes 15 §5.6): raised to it.
        Ask::Budget512 => enabled(1024),
        Ask::Budget8192 => enabled(8192),
        // The model's limit is unknown. An Anthropic client states its own
        // `max_tokens` (128000 here) and the budget fits under it. Other
        // clients state none, the body gets the default of 32000 (notes 06
        // §1.1) and the budget is lowered to `max_tokens - 1` (notes 12 §7.3).
        Ask::Budget64000 if limit => enabled(64000),
        Ask::Budget64000 => enabled(31_999),
    }
}

fn claude_stripped_expect(_: Ask, _limit: bool) -> Value {
    json!({"thinking": null, "effort": null})
}

fn anthropic_targets() -> Vec<Target> {
    vec![
        Target {
            name: "manual 1024..128000",
            caps: Some(caps(1024, 128_000, true, false, &[])),
            unknown: false,
            max_output_tokens: Some(128_000),
            expect: claude_manual_expect,
        },
        Target {
            name: "hybrid low..max",
            caps: Some(caps(
                1024,
                128_000,
                true,
                false,
                &[Effort::Low, Effort::Medium, Effort::High, Effort::Max],
            )),
            unknown: false,
            max_output_tokens: Some(128_000),
            expect: claude_hybrid_expect,
        },
        Target {
            name: "adaptive only",
            caps: Some(caps(
                0,
                0,
                true,
                true,
                &[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::Xhigh,
                    Effort::Max,
                ],
            )),
            unknown: false,
            max_output_tokens: Some(128_000),
            expect: claude_adaptive_expect,
        },
        Target {
            name: "unknown model",
            caps: None,
            unknown: true,
            max_output_tokens: None,
            expect: claude_unknown_expect,
        },
        Target {
            name: "no reasoning",
            caps: None,
            unknown: false,
            max_output_tokens: Some(8192),
            expect: claude_stripped_expect,
        },
    ]
}

// ---------------------------------------------------------------------------
// Gemini targets. The expectation is the depth part of `thinkingConfig`.
// ---------------------------------------------------------------------------

fn budget(n: i64) -> Value {
    json!({"thinkingBudget": n})
}

fn level(name: &str) -> Value {
    json!({"thinkingLevel": name})
}

/// "Gemini 2.5 pro: `{min:128,max:32768,dynamic_allowed}`" — budget-only,
/// cannot be disabled. §7.4: "levels as table budgets clamped to
/// `[Min, Max]` (`high` → 24576, `xhigh` → 32768, `max` → `Max`); off
/// becomes … `128` on pro".
fn gemini_pro_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => budget(128),
        Ask::Auto => budget(-1),
        Ask::Minimal => budget(512),
        Ask::Low => budget(1024),
        Ask::Medium => budget(8192),
        Ask::High => budget(24576),
        Ask::Xhigh => budget(32768),
        Ask::Max => budget(32768),
        Ask::Budget512 => budget(512),
        Ask::Budget8192 => budget(8192),
        Ask::Budget64000 => budget(32768),
    }
}

/// "Gemini 2.5 flash: `{max:24576,zero_allowed,dynamic_allowed}` (min 0)".
fn gemini_flash_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => budget(0),
        Ask::Auto => budget(-1),
        Ask::Minimal => budget(512),
        Ask::Low => budget(1024),
        Ask::Medium => budget(8192),
        Ask::High => budget(24576),
        Ask::Xhigh => budget(24576),
        Ask::Max => budget(24576),
        Ask::Budget512 => budget(512),
        Ask::Budget8192 => budget(8192),
        Ask::Budget64000 => budget(24576),
    }
}

/// "Gemini 3.x pro: `{min:128,max:32768,dynamic_allowed,levels:[low,high]}`"
/// — hybrid. Worked results of §5.2: "Chat `xhigh` / `none`; Claude budget
/// 8192 / 64000 / 0 → level `high` / `low`; budget `8192` / `32768` / level
/// `low`".
fn gemini_hybrid_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => level("low"),
        // §7.4: auto → `thinkingBudget: -1`, also on level-capable models.
        Ask::Auto => budget(-1),
        // clampLevel on [low, high]: minimal → low; medium → low (tie → lower).
        Ask::Minimal => level("low"),
        Ask::Low => level("low"),
        Ask::Medium => level("low"),
        Ask::High => level("high"),
        Ask::Xhigh => level("high"),
        Ask::Max => level("high"),
        Ask::Budget512 => budget(512),
        Ask::Budget8192 => budget(8192),
        Ask::Budget64000 => budget(32768),
    }
}

/// §6, unknown models, Gemini target: "a Level is converted to a Budget with
/// the table (so `(high)` → 24576)"; `thinkingBudget: 0` / `-1` / n.
fn gemini_unknown_expect(ask: Ask, _limit: bool) -> Value {
    match ask {
        Ask::None => budget(0),
        Ask::Auto => budget(-1),
        Ask::Minimal => budget(512),
        Ask::Low => budget(1024),
        Ask::Medium => budget(8192),
        Ask::High => budget(24576),
        Ask::Xhigh => budget(32768),
        Ask::Max => budget(128_000),
        Ask::Budget512 => budget(512),
        Ask::Budget8192 => budget(8192),
        Ask::Budget64000 => budget(64000),
    }
}

fn gemini_stripped_expect(_: Ask, _limit: bool) -> Value {
    json!({})
}

fn gemini_targets() -> Vec<Target> {
    vec![
        Target {
            name: "2.5 pro",
            caps: Some(caps(128, 32_768, false, true, &[])),
            unknown: false,
            max_output_tokens: Some(65_536),
            expect: gemini_pro_expect,
        },
        Target {
            name: "2.5 flash",
            caps: Some(caps(0, 24_576, true, true, &[])),
            unknown: false,
            max_output_tokens: Some(65_536),
            expect: gemini_flash_expect,
        },
        Target {
            name: "3.x pro",
            caps: Some(caps(128, 32_768, false, true, &[Effort::Low, Effort::High])),
            unknown: false,
            max_output_tokens: Some(65_536),
            expect: gemini_hybrid_expect,
        },
        Target {
            name: "unknown model",
            caps: None,
            unknown: true,
            max_output_tokens: None,
            expect: gemini_unknown_expect,
        },
        Target {
            name: "no reasoning",
            caps: None,
            unknown: false,
            max_output_tokens: Some(8192),
            expect: gemini_stripped_expect,
        },
    ]
}

/// The reasoning fields of an upstream body, in the shape the expectation
/// functions use.
fn reasoning_fields(upstream: Protocol, body: &Value) -> Value {
    match upstream {
        Protocol::OpenaiChat => body.get("reasoning_effort").cloned().unwrap_or(Value::Null),
        Protocol::OpenaiResponses => body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .cloned()
            .unwrap_or(Value::Null),
        Protocol::Anthropic => {
            let mut thinking = body.get("thinking").cloned().unwrap_or(Value::Null);
            if let Some(map) = thinking.as_object_mut() {
                // Visibility is a separate setting (notes 12 §8).
                map.shift_remove("display");
            }
            json!({
                "thinking": thinking,
                "effort": body.get("output_config").and_then(|c| c.get("effort")).cloned().unwrap_or(Value::Null),
            })
        }
        Protocol::Gemini => {
            let mut config = body
                .get("generationConfig")
                .and_then(|c| c.get("thinkingConfig"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            if let Some(map) = config.as_object_mut() {
                map.shift_remove("includeThoughts");
            }
            config
        }
    }
}

// ---------------------------------------------------------------------------
// Visibility of the reasoning text (notes 12 §8)
// ---------------------------------------------------------------------------

/// The field of an upstream body that says whether reasoning text is to be
/// returned: Responses `reasoning.summary`, Gemini `includeThoughts`,
/// Anthropic `thinking.display`. Chat Completions has none (§8.3: "plain
/// OpenAI Chat has no visibility field"); the fields of its relays
/// (`reasoning.exclude`, `include_reasoning`) must never be invented, so for
/// Chat this returns whichever of them is present.
fn visibility_field(upstream: Protocol, body: &Value) -> Value {
    let field = match upstream {
        Protocol::OpenaiResponses => body.pointer("/reasoning/summary"),
        Protocol::Gemini => body.pointer("/generationConfig/thinkingConfig/includeThoughts"),
        Protocol::Anthropic => body.pointer("/thinking/display"),
        Protocol::OpenaiChat => body
            .pointer("/reasoning/exclude")
            .or_else(|| body.get("include_reasoning"))
            .or_else(|| body.get("reasoning")),
    };
    field.cloned().unwrap_or(Value::Null)
}

/// What the visibility field must be when the client asked for a depth and
/// said nothing explicit about visibility. `reasons`: the target model is
/// not known to be unable to reason.
///
/// Notes 12 §8.1: only a Chat client's depth implies anything ("`openai`
/// (implicit)": `none` → Disabled, anything else → Enabled(`auto`)); the
/// Responses, Anthropic and Gemini readers take explicit fields only
/// ("Effort alone means nothing"). §8.1 "translated" reader: a Chat effort
/// says nothing to an Anthropic target. §8.3 says how an intent is written.
fn implied_visibility(client: Protocol, upstream: Protocol, ask: Ask, reasons: bool) -> Value {
    if client != CHAT || !reasons {
        // Nothing implied; and §5 step 6: a model that does not reason gets
        // no reasoning fields at all.
        return Value::Null;
    }
    match (upstream, ask) {
        // §8.3 `openai`: never invents a field.
        (Protocol::OpenaiChat, _) => Value::Null,
        // §8.1: explicit-only when the source is Chat and the target Claude.
        (Protocol::Anthropic, _) => Value::Null,
        // §8.3 `openai-response`: Disabled → the field is absent ("omission
        // is the documented off"); Enabled → `summary: "auto"`.
        (Protocol::OpenaiResponses, Ask::None) => Value::Null,
        (Protocol::OpenaiResponses, _) => json!("auto"),
        // §7.4: "Chat -> Gemini requests end up with `includeThoughts:
        // true`, and `reasoning_effort:"none"` on a model that cannot be
        // disabled ends up with the minimum budget plus `includeThoughts:
        // false`". (Where "off" can be written as `thinkingBudget: 0` the
        // reference leaves the field out; the codec writes the equivalent
        // default `false` next to the zero budget.)
        (Protocol::Gemini, Ask::None) => json!(false),
        (Protocol::Gemini, _) => json!(true),
    }
}

/// A request of each client protocol that asks for `high` depth and says
/// explicitly whether it wants to see the reasoning (§8.1, first rows).
fn explicit_visibility_body(client: Protocol, show: bool) -> Value {
    match client {
        Protocol::OpenaiChat => json!({
            "model": "gpt-5.5", "messages": [{"role": "user", "content": "Think."}],
            "reasoning_effort": "high", "include_reasoning": show
        }),
        Protocol::OpenaiResponses => json!({
            "model": "gpt-5.5", "input": "Think.",
            "reasoning": {"effort": "high", "summary": if show { json!("detailed") } else { Value::Null }}
        }),
        Protocol::Anthropic => json!({
            "model": "claude-sonnet-4-5", "max_tokens": 64000,
            "messages": [{"role": "user", "content": "Think."}],
            "thinking": {"type": "adaptive", "display": if show { "summarized" } else { "omitted" }},
            "output_config": {"effort": "high"}
        }),
        Protocol::Gemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "Think."}]}],
            "generationConfig": {"thinkingConfig": {"thinkingLevel": "high", "includeThoughts": show}}
        }),
    }
}

/// §8.3, what an explicit intent becomes on each target (a model the gateway
/// knows nothing about, so the depth is written as the client gave it).
fn explicit_visibility(client: Protocol, upstream: Protocol, show: bool) -> Value {
    match upstream {
        // Never invents a field. (A Chat client's own `include_reasoning`
        // is a top-level field the IR has no slot for; it is replayed to a
        // Chat upstream with the rest of the client's body.)
        Protocol::OpenaiChat if client == CHAT => json!(show),
        Protocol::OpenaiChat => Value::Null,
        // Enabled → the detail (`auto` unless the client named one, which
        // only a Responses client can); Disabled → absent.
        Protocol::OpenaiResponses if !show => Value::Null,
        Protocol::OpenaiResponses if client == RESPONSES => json!("detailed"),
        Protocol::OpenaiResponses => json!("auto"),
        Protocol::Gemini => json!(show),
        // Thinking is active (`adaptive` + effort): `display` is written.
        Protocol::Anthropic if show => json!("summarized"),
        Protocol::Anthropic => json!("omitted"),
    }
}

fn check_explicit_visibility(client: Protocol, upstream: Protocol, failures: &mut Failures) {
    for show in [true, false] {
        let context = format!("explicit visibility {show}");
        let body = explicit_visibility_body(client, show);
        let mut request = match decode_request(client, &body) {
            Ok(request) => request,
            Err(error) => {
                failures.push(&context, format!("decode failed: {error}"));
                continue;
            }
        };
        request.model = upstream_model(upstream).to_string();
        fit_reasoning(&mut request, ModelThinking::Unknown, upstream);
        match codec(upstream).encode_request(&request, &UpstreamCtx::default()) {
            Ok(encoded) => {
                // A Chat client's own relay field is replayed to its Chat
                // upstream; the validator models OpenAI itself, which does
                // not know it.
                if !(client == CHAT && upstream == CHAT) {
                    failures.report(&context, validate_request(upstream, &encoded));
                }
                let field = visibility_field(upstream, &encoded);
                let expected = explicit_visibility(client, upstream, show);
                failures.check(
                    &context,
                    field == expected,
                    format!("visibility field {field}, expected {expected}: {encoded}"),
                );
            }
            Err(error) => failures.push(&context, format!("encode failed: {error}")),
        }
    }
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    check_explicit_visibility(client, upstream, &mut failures);
    let targets = match upstream {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => openai_targets(),
        Protocol::Anthropic => anthropic_targets(),
        Protocol::Gemini => gemini_targets(),
    };
    let mut checked = 0;
    for ask in ASKS {
        let Some(body) = client_body(client, ask) else {
            continue;
        };
        // The client's spelling decodes to the canonical depth.
        let decoded = match decode_request(client, &body) {
            Ok(request) => request,
            Err(error) => {
                failures.push(&format!("{ask:?}"), format!("decode failed: {error}"));
                continue;
            }
        };
        let depth = decoded.reasoning.as_ref().and_then(|r| r.depth);
        failures.check(
            &format!("{ask:?}"),
            depth == Some(ask.depth()),
            format!(
                "{client} spelling decodes to {depth:?}, expected {:?}",
                ask.depth()
            ),
        );

        for target in &targets {
            let context = format!("{ask:?} -> {}", target.name);
            let thinking = match (&target.caps, target.unknown) {
                (Some(caps), _) => ModelThinking::Supported(caps),
                (None, true) => ModelThinking::Unknown,
                (None, false) => ModelThinking::Unsupported,
            };
            let ctx = UpstreamCtx {
                thinking,
                max_output_tokens: target.max_output_tokens,
                ..UpstreamCtx::default()
            };
            let mut request = decoded.clone();
            request.model = upstream_model(upstream).to_string();
            fit_reasoning(&mut request, thinking, upstream);
            let encoded = match codec(upstream).encode_request(&request, &ctx) {
                Ok(encoded) => encoded,
                Err(error) => {
                    failures.push(&context, format!("encode failed: {error}"));
                    continue;
                }
            };
            failures.report(
                &context,
                validate_translated_request(client, upstream, &encoded),
            );
            let fields = reasoning_fields(upstream, &encoded);
            let expected = (target.expect)(ask, client == ANTHROPIC);
            failures.check(
                &context,
                fields == expected,
                format!("reasoning fields {fields}, expected {expected}"),
            );
            let reasons = target.caps.is_some() || target.unknown;
            let visibility = visibility_field(upstream, &encoded);
            let implied = implied_visibility(client, upstream, ask, reasons);
            failures.check(
                &context,
                visibility == implied,
                format!("visibility field {visibility}, expected {implied}"),
            );
            checked += 1;
        }
    }
    assert!(checked >= 28, "only {checked} combinations were checked");
    failures.finish(&format!(
        "{} -> {} (reasoning)",
        short(client),
        short(upstream)
    ));
}

macro_rules! pairs {
    ($($name:ident: $client:expr => $upstream:expr;)*) => {
        $(
            #[test]
            fn $name() {
                run_pair($client, $upstream);
            }
        )*
    };
}

pairs! {
    chat_to_chat: CHAT => CHAT;
    chat_to_responses: CHAT => RESPONSES;
    chat_to_anthropic: CHAT => ANTHROPIC;
    chat_to_gemini: CHAT => GEMINI;
    responses_to_chat: RESPONSES => CHAT;
    responses_to_responses: RESPONSES => RESPONSES;
    responses_to_anthropic: RESPONSES => ANTHROPIC;
    responses_to_gemini: RESPONSES => GEMINI;
    anthropic_to_chat: ANTHROPIC => CHAT;
    anthropic_to_responses: ANTHROPIC => RESPONSES;
    anthropic_to_anthropic: ANTHROPIC => ANTHROPIC;
    anthropic_to_gemini: ANTHROPIC => GEMINI;
    gemini_to_chat: GEMINI => CHAT;
    gemini_to_responses: GEMINI => RESPONSES;
    gemini_to_anthropic: GEMINI => ANTHROPIC;
    gemini_to_gemini: GEMINI => GEMINI;
}
