//! Reasoning ("thinking") configuration: one canonical representation, the
//! conversions between effort levels and token budgets, model capability
//! clamping, and the `model(effort)` name suffix.
//!
//! Vendors expose reasoning depth differently — OpenAI as an effort level,
//! Anthropic as a token budget or an adaptive effort, Gemini as a budget or a
//! level depending on the model generation. Codecs read whatever the client
//! sent into a [`ReasoningConfig`], [`normalize_depth`] fits it to what the
//! target model accepts, and the target codec writes it back out.

use crate::protocol::Protocol;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Canonical reasoning settings of a request. Both fields are optional because
/// "the client said nothing" must stay distinguishable from "off": an absent
/// field leaves the upstream default untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// How hard the model should think.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<Depth>,
    /// Whether (and how) reasoning text should be returned to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
}

impl ReasoningConfig {
    pub fn with_depth(depth: Depth) -> Self {
        ReasoningConfig {
            depth: Some(depth),
            summary: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.depth.is_none() && self.summary.is_none()
    }
}

/// Requested reasoning depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum Depth {
    /// Reasoning disabled.
    Off,
    /// The provider decides (Anthropic adaptive without effort, Gemini `-1`).
    Auto,
    /// A discrete effort level.
    Level(Effort),
    /// An explicit thinking-token budget (> 0).
    Budget(u32),
}

impl Depth {
    /// Short lower-case label for logs and usage records: `none`, `auto`, or
    /// an effort name (budgets are bucketed with [`budget_to_effort`]).
    pub fn label(&self) -> &'static str {
        match self {
            Depth::Off => "none",
            Depth::Auto => "auto",
            Depth::Level(e) => e.as_str(),
            Depth::Budget(b) => budget_to_effort(*b).as_str(),
        }
    }
}

/// Discrete effort levels, ordered from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 6] = [
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    const fn rank(self) -> i32 {
        self as i32
    }

    /// The token budget conventionally equivalent to this level.
    pub const fn budget(self) -> u32 {
        match self {
            Effort::Minimal => 512,
            Effort::Low => 1024,
            Effort::Medium => 8192,
            Effort::High => 24576,
            Effort::Xhigh => 32768,
            Effort::Max => 128_000,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Ok(Effort::Minimal),
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::Xhigh),
            "max" => Ok(Effort::Max),
            _ => Err(()),
        }
    }
}

/// Maps a token budget onto the nearest effort bucket. Never returns
/// [`Effort::Max`]: a budget alone cannot express "everything you have".
pub const fn budget_to_effort(budget: u32) -> Effort {
    match budget {
        0..=512 => Effort::Minimal,
        513..=1024 => Effort::Low,
        1025..=8192 => Effort::Medium,
        8193..=24576 => Effort::High,
        _ => Effort::Xhigh,
    }
}

/// Parses a wire effort string (`none`, `auto`, a level) into a [`Depth`].
/// Unknown strings yield `None`.
pub fn depth_from_effort_str(s: &str) -> Option<Depth> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" => Some(Depth::Off),
        "auto" | "dynamic" => Some(Depth::Auto),
        other => other.parse::<Effort>().ok().map(Depth::Level),
    }
}

/// Parses a wire budget (`0` off, `-1` dynamic, positive budget) into a [`Depth`].
pub fn depth_from_budget(budget: i64) -> Option<Depth> {
    match budget {
        0 => Some(Depth::Off),
        -1 => Some(Depth::Auto),
        b if b > 0 => Some(Depth::Budget(b.min(u32::MAX as i64) as u32)),
        _ => None,
    }
}

/// Whether reasoning text should be surfaced to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Summary {
    /// Do not return reasoning text.
    Off,
    /// Return it, provider chooses the detail.
    Auto,
    Concise,
    Detailed,
}

impl Summary {
    pub const fn is_on(self) -> bool {
        !matches!(self, Summary::Off)
    }

    /// OpenAI Responses `reasoning.summary` value for an enabled summary.
    pub const fn as_openai(self) -> Option<&'static str> {
        match self {
            Summary::Off => None,
            Summary::Auto => Some("auto"),
            Summary::Concise => Some("concise"),
            Summary::Detailed => Some("detailed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Model capabilities
// ---------------------------------------------------------------------------

/// What a model accepts as a reasoning setting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingSupport {
    /// Smallest accepted budget (0 = no budget range).
    #[serde(default)]
    pub min: u32,
    /// Largest accepted budget (0 = no budget range).
    #[serde(default)]
    pub max: u32,
    /// Reasoning can be switched off entirely.
    #[serde(default)]
    pub zero_allowed: bool,
    /// The model accepts "let the provider decide".
    #[serde(default)]
    pub dynamic_allowed: bool,
    /// Discrete effort levels the model accepts, in the order the vendor lists
    /// them. Empty for budget-only models.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<Effort>,
}

impl ThinkingSupport {
    pub fn levels(levels: &[Effort]) -> Self {
        ThinkingSupport {
            levels: levels.to_vec(),
            ..ThinkingSupport::default()
        }
    }

    pub fn budget(min: u32, max: u32) -> Self {
        ThinkingSupport {
            min,
            max,
            ..ThinkingSupport::default()
        }
    }

    pub const fn has_range(&self) -> bool {
        self.min > 0 || self.max > 0
    }

    pub fn has_levels(&self) -> bool {
        !self.levels.is_empty()
    }

    fn clamp_budget(&self, budget: u32) -> u32 {
        if !self.has_range() {
            return budget;
        }
        let mut b = budget;
        if self.min > 0 && b < self.min {
            b = self.min;
        }
        if self.max > 0 && b > self.max {
            b = self.max;
        }
        b
    }

    /// Picks the supported level closest to `level`; ties go to the lower one.
    /// "Top effort" is spelled `xhigh` by some vendors and `max` by others, so
    /// those two are tried for each other before falling back to distance.
    pub fn clamp_level(&self, level: Effort) -> Effort {
        if self.levels.is_empty() || self.levels.contains(&level) {
            return level;
        }
        let preferred: &[Effort] = match level {
            Effort::Xhigh => &[Effort::Max, Effort::High],
            Effort::Max => &[Effort::Xhigh, Effort::High],
            _ => &[],
        };
        for p in preferred {
            if self.levels.contains(p) {
                return *p;
            }
        }
        let mut best = self.levels[0];
        let mut best_dist = i32::MAX;
        for &cand in &self.levels {
            let dist = (cand.rank() - level.rank()).abs();
            if dist < best_dist || (dist == best_dist && cand < best) {
                best = cand;
                best_dist = dist;
            }
        }
        best
    }
}

/// What is known about the target model's reasoning support.
#[derive(Clone, Copy, Debug)]
pub enum ModelThinking<'a> {
    /// Nothing is known about the model: pass the request through with only
    /// the representation changes the target protocol forces.
    Unknown,
    /// The model is known not to reason: reasoning settings must be removed.
    Unsupported,
    Supported(&'a ThinkingSupport),
}

/// Result of fitting a requested [`Depth`] to a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fitted {
    /// Send this depth.
    Use(Depth),
    /// Remove every reasoning-depth field from the upstream request.
    Strip,
}

/// Fits a requested depth to what `model` accepts when reached over `target`.
///
/// The gateway never rejects a reasoning setting: values a model cannot take
/// are moved to the nearest one it can, so a client written for one vendor
/// keeps working against another.
pub fn normalize_depth(depth: Depth, model: ModelThinking<'_>, target: Protocol) -> Fitted {
    let caps = match model {
        ModelThinking::Unsupported => return Fitted::Strip,
        ModelThinking::Unknown => {
            // Gemini has no way to express a level on budget-only models and we
            // cannot tell which kind this is; budgets are accepted by both.
            return Fitted::Use(match (depth, target) {
                (Depth::Level(e), Protocol::Gemini) => Depth::Budget(e.budget()),
                (d, _) => d,
            });
        }
        ModelThinking::Supported(caps) => caps,
    };

    let has_range = caps.has_range();
    let has_levels = caps.has_levels();

    // 1. Representation: move to the form the model understands.
    let mut depth = match depth {
        Depth::Level(e) if has_range && !has_levels => Depth::Budget(e.budget()),
        Depth::Budget(b) if has_levels && !has_range => Depth::Level(budget_to_effort(b)),
        d => d,
    };

    // 2. "Let the provider decide". Each vendor spells that differently:
    //    on OpenAI's APIs it is simply the absence of an effort; on Anthropic
    //    it is adaptive thinking, which every model that takes effort levels
    //    has; on Gemini it is the dynamic budget, which the model must allow.
    //    Only a model with no such mode needs an explicit value instead.
    let auto_is_native = match target.family() {
        crate::protocol::Family::Openai => true,
        crate::protocol::Family::Anthropic => has_levels || caps.dynamic_allowed,
        crate::protocol::Family::Google => caps.dynamic_allowed,
    };
    if depth == Depth::Auto && !auto_is_native {
        depth = if has_levels && !has_range {
            Depth::Level(Effort::Medium)
        } else {
            let mid = (caps.min + caps.max) / 2;
            if mid > 0 {
                Depth::Budget(mid)
            } else if caps.zero_allowed {
                Depth::Off
            } else {
                Depth::Budget(caps.min)
            }
        };
    }

    // 3. Clamp into the supported set / range.
    depth = match depth {
        Depth::Level(e) if has_levels => Depth::Level(caps.clamp_level(e)),
        Depth::Budget(b) => Depth::Budget(caps.clamp_budget(b)),
        d => d,
    };

    // 4. "Off" on a model that cannot be switched off becomes its floor.
    //    Anthropic can always express `disabled`, whatever the catalog says.
    if depth == Depth::Off && !caps.zero_allowed && target != Protocol::Anthropic {
        depth = if has_levels {
            Depth::Level(caps.levels[0])
        } else if caps.min > 0 {
            Depth::Budget(caps.min)
        } else {
            Depth::Off
        };
    }

    Fitted::Use(depth)
}

// ---------------------------------------------------------------------------
// Model-name suffix
// ---------------------------------------------------------------------------

/// A model name split into its base and an optional reasoning suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSuffix<'a> {
    /// The model name without the suffix.
    pub base: &'a str,
    /// Raw text between the parentheses, when a suffix is present.
    pub raw: Option<&'a str>,
    /// The depth the suffix asks for. `None` when there is no suffix or its
    /// content is not a recognised effort/budget.
    pub depth: Option<Depth>,
}

/// Splits `name(suffix)` into base name and reasoning directive.
///
/// The suffix is the text between the **last** `(` and a `)` that ends the
/// string: `gpt-5(high)`, `claude-sonnet-4-5(16000)`, `gemini-2.5-pro(none)`,
/// `model(auto)`. A recognised suffix overrides whatever the request body says
/// about reasoning depth. An unrecognised one (`model(ultra)`) is still removed
/// from the upstream model name but carries no directive.
pub fn parse_model_suffix(name: &str) -> ModelSuffix<'_> {
    let no_suffix = ModelSuffix {
        base: name,
        raw: None,
        depth: None,
    };
    if !name.ends_with(')') {
        return no_suffix;
    }
    let Some(open) = name.rfind('(') else {
        return no_suffix;
    };
    if open == 0 {
        return no_suffix;
    }
    let base = &name[..open];
    let raw = &name[open + 1..name.len() - 1];
    let depth = depth_from_effort_str(raw).or_else(|| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.parse::<i64>() {
            Ok(n) => depth_from_budget(n),
            Err(_) => None,
        }
    });
    ModelSuffix {
        base,
        raw: Some(raw),
        depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effort::*;

    fn caps(min: u32, max: u32, zero: bool, dynamic: bool, levels: &[Effort]) -> ThinkingSupport {
        ThinkingSupport {
            min,
            max,
            zero_allowed: zero,
            dynamic_allowed: dynamic,
            levels: levels.to_vec(),
        }
    }

    fn fit(depth: Depth, c: &ThinkingSupport, target: Protocol) -> Depth {
        match normalize_depth(depth, ModelThinking::Supported(c), target) {
            Fitted::Use(d) => d,
            Fitted::Strip => panic!("unexpected strip"),
        }
    }

    #[test]
    fn budget_buckets() {
        assert_eq!(budget_to_effort(1), Minimal);
        assert_eq!(budget_to_effort(512), Minimal);
        assert_eq!(budget_to_effort(513), Low);
        assert_eq!(budget_to_effort(1024), Low);
        assert_eq!(budget_to_effort(8192), Medium);
        assert_eq!(budget_to_effort(8193), High);
        assert_eq!(budget_to_effort(24576), High);
        assert_eq!(budget_to_effort(24577), Xhigh);
        assert_eq!(budget_to_effort(1_000_000), Xhigh);
    }

    #[test]
    fn clamp_level_prefers_top_effort_alias_then_nearest_lower() {
        let c = ThinkingSupport::levels(&[Low, Medium, High, Max]);
        assert_eq!(c.clamp_level(Xhigh), Max);
        let c = ThinkingSupport::levels(&[Low, Medium, High]);
        assert_eq!(c.clamp_level(Max), High);
        assert_eq!(c.clamp_level(Xhigh), High);
        let c = ThinkingSupport::levels(&[Low, High]);
        assert_eq!(c.clamp_level(Medium), Low);
        assert_eq!(c.clamp_level(Minimal), Low);
        let c = ThinkingSupport::levels(&[Minimal, High]);
        assert_eq!(c.clamp_level(Low), Minimal);
        assert_eq!(c.clamp_level(Medium), High);
    }

    #[test]
    fn level_only_model() {
        // OpenAI-style reasoning model.
        let c = caps(0, 0, false, false, &[Minimal, Low, Medium, High]);
        let t = Protocol::OpenaiResponses;
        assert_eq!(fit(Depth::Off, &c, t), Depth::Level(Minimal));
        assert_eq!(fit(Depth::Budget(64000), &c, t), Depth::Level(High));
        assert_eq!(fit(Depth::Level(Xhigh), &c, t), Depth::Level(High));
        let c = caps(0, 0, false, false, &[Low, High]);
        assert_eq!(fit(Depth::Budget(8192), &c, t), Depth::Level(Low));
    }

    #[test]
    fn auto_is_whatever_the_vendor_calls_provider_decides() {
        // OpenAI: omitting the effort is "auto", whatever the model lists.
        let levels_only = caps(0, 0, false, false, &[Low, Medium, High]);
        assert_eq!(
            fit(Depth::Auto, &levels_only, Protocol::OpenaiResponses),
            Depth::Auto
        );
        assert_eq!(
            fit(Depth::Auto, &levels_only, Protocol::OpenaiChat),
            Depth::Auto
        );
        // Anthropic: a model that takes effort levels has adaptive thinking.
        let claude_hybrid = caps(1024, 128_000, true, false, &[Low, Medium, High, Max]);
        assert_eq!(
            fit(Depth::Auto, &claude_hybrid, Protocol::Anthropic),
            Depth::Auto
        );
        // …a budget-only Claude does not, and gets the middle of its range.
        let claude_manual = caps(1024, 128_000, true, false, &[]);
        assert_eq!(
            fit(Depth::Auto, &claude_manual, Protocol::Anthropic),
            Depth::Budget(64512)
        );
        // Gemini: only when the model allows the dynamic budget.
        let gemini_dynamic = caps(128, 32768, false, true, &[Low, High]);
        assert_eq!(
            fit(Depth::Auto, &gemini_dynamic, Protocol::Gemini),
            Depth::Auto
        );
        let gemini_fixed = caps(0, 0, false, false, &[Low, High]);
        assert_eq!(
            fit(Depth::Auto, &gemini_fixed, Protocol::Gemini),
            Depth::Level(Low)
        );
        let gemini_fixed = caps(0, 0, false, false, &[Minimal, Low, Medium, High]);
        assert_eq!(
            fit(Depth::Auto, &gemini_fixed, Protocol::Gemini),
            Depth::Level(Medium)
        );
    }

    #[test]
    fn budget_only_model() {
        // Gemini 2.5 pro style: cannot disable, dynamic allowed.
        let c = caps(128, 20000, false, true, &[]);
        let t = Protocol::Gemini;
        assert_eq!(fit(Depth::Level(Medium), &c, t), Depth::Budget(8192));
        assert_eq!(fit(Depth::Level(Xhigh), &c, t), Depth::Budget(20000));
        assert_eq!(fit(Depth::Off, &c, t), Depth::Budget(128));
        assert_eq!(fit(Depth::Auto, &c, t), Depth::Auto);
        assert_eq!(fit(Depth::Budget(64000), &c, t), Depth::Budget(20000));
        assert_eq!(fit(Depth::Budget(5), &c, t), Depth::Budget(128));
    }

    #[test]
    fn hybrid_model_keeps_callers_form() {
        let c = caps(128, 32768, false, true, &[Low, High]);
        let t = Protocol::Gemini;
        assert_eq!(fit(Depth::Level(Xhigh), &c, t), Depth::Level(High));
        assert_eq!(fit(Depth::Off, &c, t), Depth::Level(Low));
        assert_eq!(fit(Depth::Budget(8192), &c, t), Depth::Budget(8192));
        assert_eq!(fit(Depth::Budget(64000), &c, t), Depth::Budget(32768));
    }

    #[test]
    fn claude_manual_model() {
        let c = caps(1024, 128_000, true, false, &[]);
        let t = Protocol::Anthropic;
        assert_eq!(fit(Depth::Level(Medium), &c, t), Depth::Budget(8192));
        assert_eq!(fit(Depth::Level(Xhigh), &c, t), Depth::Budget(32768));
        assert_eq!(fit(Depth::Off, &c, t), Depth::Off);
        assert_eq!(fit(Depth::Auto, &c, t), Depth::Budget(64512));
        assert_eq!(fit(Depth::Budget(200_000), &c, t), Depth::Budget(128_000));
        // Anthropic can always disable, even when the catalog says otherwise.
        let c = caps(1024, 128_000, false, false, &[]);
        assert_eq!(fit(Depth::Off, &c, t), Depth::Off);
    }

    #[test]
    fn unknown_and_unsupported_models() {
        assert_eq!(
            normalize_depth(
                Depth::Level(High),
                ModelThinking::Unsupported,
                Protocol::OpenaiChat
            ),
            Fitted::Strip
        );
        assert_eq!(
            normalize_depth(Depth::Level(High), ModelThinking::Unknown, Protocol::Gemini),
            Fitted::Use(Depth::Budget(24576))
        );
        assert_eq!(
            normalize_depth(
                Depth::Level(High),
                ModelThinking::Unknown,
                Protocol::Anthropic
            ),
            Fitted::Use(Depth::Level(High))
        );
        assert_eq!(
            normalize_depth(
                Depth::Budget(5000),
                ModelThinking::Unknown,
                Protocol::OpenaiChat
            ),
            Fitted::Use(Depth::Budget(5000))
        );
    }

    #[test]
    fn suffix_parsing() {
        let s = parse_model_suffix("gpt-5(high)");
        assert_eq!((s.base, s.depth), ("gpt-5", Some(Depth::Level(High))));
        let s = parse_model_suffix("claude(16000)");
        assert_eq!((s.base, s.depth), ("claude", Some(Depth::Budget(16000))));
        let s = parse_model_suffix("m(08192)");
        assert_eq!(s.depth, Some(Depth::Budget(8192)));
        let s = parse_model_suffix("m(none)");
        assert_eq!(s.depth, Some(Depth::Off));
        let s = parse_model_suffix("m(0)");
        assert_eq!(s.depth, Some(Depth::Off));
        let s = parse_model_suffix("m(auto)");
        assert_eq!(s.depth, Some(Depth::Auto));
        let s = parse_model_suffix("m(-1)");
        assert_eq!(s.depth, Some(Depth::Auto));
        let s = parse_model_suffix("m(ultra)");
        assert_eq!((s.base, s.raw, s.depth), ("m", Some("ultra"), None));
        let s = parse_model_suffix("m()");
        assert_eq!((s.base, s.raw, s.depth), ("m", Some(""), None));
        let s = parse_model_suffix("plain-model");
        assert_eq!((s.base, s.raw, s.depth), ("plain-model", None, None));
        let s = parse_model_suffix("a(b)c");
        assert_eq!((s.base, s.raw), ("a(b)c", None));
        let s = parse_model_suffix("org/model(x)(low)");
        assert_eq!((s.base, s.depth), ("org/model(x)", Some(Depth::Level(Low))));
        let s = parse_model_suffix("(high)");
        assert_eq!((s.base, s.raw), ("(high)", None));
    }

    #[test]
    fn depth_labels() {
        assert_eq!(Depth::Off.label(), "none");
        assert_eq!(Depth::Budget(9000).label(), "high");
        assert_eq!(Depth::Level(Max).label(), "max");
    }
}
