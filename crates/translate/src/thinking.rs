//! Deciding what reasoning ("thinking") depth an upstream request carries.
//!
//! A request can ask for a reasoning depth in two places: a suffix on the
//! model name (`gpt-5(high)`, `claude-sonnet-4-5(16000)`, `model(none)`) and
//! the protocol's own body fields (`reasoning_effort`, `reasoning.effort`,
//! `thinking`, `generationConfig.thinkingConfig`). [`plan_reasoning`] turns
//! those, together with what is known about the target model, into a
//! [`ReasoningPlan`]:
//!
//! | situation | plan |
//! |---|---|
//! | recognised suffix | `Write(normalize_depth(suffix))` — the suffix beats the body |
//! | passthrough, no suffix | `LeaveAlone` — the client speaks the upstream's own dialect |
//! | passthrough, no suffix, model known not to reason, body asks for a depth | `Write(Strip)` |
//! | translation, body asks for a depth | `Write(normalize_depth(depth))` |
//! | translation, nothing requested | `LeaveAlone` — no depth is ever invented |
//!
//! The gateway never rejects a depth; [`normalize_depth`] moves it to the
//! nearest value the model accepts.
//!
//! # Applying a plan
//!
//! * **Translation.** The request has been decoded into the IR. Call
//!   [`apply_to_request`] before `Codec::encode_request`; the target codec
//!   then writes `request.reasoning` in its own shape.
//! * **Passthrough.** The client's JSON is forwarded, so the plan is applied
//!   to the raw body with the upstream codec:
//!   `Codec::write_reasoning(&mut body, fitted, &upstream_ctx)` for
//!   [`ReasoningPlan::Write`], nothing for [`ReasoningPlan::LeaveAlone`].
//!   [`apply_to_body`] does exactly that. `write_reasoning` replaces whatever
//!   depth fields the body had and keeps the body valid for the protocol.
//!
//! Either way this happens **before** payload rules run, so an operator's
//! `override` rule has the last word.

use serde_json::Value;
use switchyard_core::codec::{Codec, UpstreamCtx};
use switchyard_core::ir::Request;
use switchyard_core::protocol::Protocol;
use switchyard_core::reasoning::{Depth, Fitted, ModelThinking, ReasoningConfig, normalize_depth};

/// Everything [`plan_reasoning`] looks at.
#[derive(Clone, Copy, Debug)]
pub struct ReasoningInputs<'a> {
    /// Reasoning settings read from the client's body with the *client*
    /// codec's `read_reasoning`. Empty when the body says nothing.
    pub client: &'a ReasoningConfig,
    /// Depth requested by a recognised model-name suffix
    /// (`parse_model_suffix(..).depth`). `None` when there is no suffix or it
    /// is not a recognised effort/budget.
    pub suffix: Option<Depth>,
    /// What the target model accepts.
    pub model: ModelThinking<'a>,
    /// Protocol the upstream request is written in.
    pub target: Protocol,
    /// True when the client's body is forwarded as-is (same protocol on both
    /// sides), false when the request goes through the IR.
    pub passthrough: bool,
}

/// What to do with the reasoning depth of an upstream request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningPlan {
    /// Do not touch the reasoning fields.
    LeaveAlone,
    /// Replace the reasoning depth with this fitted value
    /// ([`Fitted::Strip`] removes it).
    Write(Fitted),
}

impl ReasoningPlan {
    /// The value to write, if the plan writes one.
    pub fn fitted(&self) -> Option<Fitted> {
        match self {
            ReasoningPlan::LeaveAlone => None,
            ReasoningPlan::Write(fitted) => Some(*fitted),
        }
    }
}

/// Computes the plan for one upstream attempt. See the module docs for the
/// rule table.
pub fn plan_reasoning(inputs: &ReasoningInputs<'_>) -> ReasoningPlan {
    if let Some(depth) = inputs.suffix {
        return ReasoningPlan::Write(normalize_depth(depth, inputs.model, inputs.target));
    }
    match (inputs.passthrough, inputs.client.depth) {
        // A model that is known not to reason rejects reasoning fields
        // outright, so this is the one case where a native body is corrected.
        (true, Some(_)) if matches!(inputs.model, ModelThinking::Unsupported) => {
            ReasoningPlan::Write(Fitted::Strip)
        }
        (true, _) => ReasoningPlan::LeaveAlone,
        (false, Some(depth)) => {
            ReasoningPlan::Write(normalize_depth(depth, inputs.model, inputs.target))
        }
        (false, None) => ReasoningPlan::LeaveAlone,
    }
}

/// The reasoning label to record in usage logs for a planned attempt: the
/// [`Depth::label`] of the depth that actually goes upstream.
///
/// * `Write(Use(depth))` → that depth's label;
/// * `Write(Strip)` → `None` (no reasoning setting is sent);
/// * `LeaveAlone` → the label of the client's own depth, if it sent one (in
///   passthrough it is forwarded untouched), otherwise `None`.
pub fn effective_label(inputs: &ReasoningInputs<'_>, plan: ReasoningPlan) -> Option<&'static str> {
    match plan {
        ReasoningPlan::Write(Fitted::Use(depth)) => Some(depth.label()),
        ReasoningPlan::Write(Fitted::Strip) => None,
        ReasoningPlan::LeaveAlone => inputs.client.depth.map(|depth| depth.label()),
    }
}

/// [`plan_reasoning`] and [`effective_label`] in one call.
pub fn plan_with_label(inputs: &ReasoningInputs<'_>) -> (ReasoningPlan, Option<&'static str>) {
    let plan = plan_reasoning(inputs);
    (plan, effective_label(inputs, plan))
}

/// Applies a plan to a decoded request (translation path).
///
/// `Write(Use(depth))` sets `request.reasoning.depth`, `Write(Strip)` clears
/// it. The summary preference is kept in both cases — whether reasoning text
/// is shown is independent of how deep the model thinks — and
/// `request.reasoning` becomes `None` only when nothing is left in it.
pub fn apply_to_request(request: &mut Request, plan: ReasoningPlan) {
    match plan {
        ReasoningPlan::LeaveAlone => {}
        ReasoningPlan::Write(Fitted::Use(depth)) => {
            request
                .reasoning
                .get_or_insert_with(ReasoningConfig::default)
                .depth = Some(depth);
        }
        ReasoningPlan::Write(Fitted::Strip) => {
            if let Some(reasoning) = &mut request.reasoning {
                reasoning.depth = None;
                if reasoning.is_empty() {
                    request.reasoning = None;
                }
            }
        }
    }
}

/// Applies a plan to a raw body in the upstream protocol (passthrough path)
/// through `codec.write_reasoning`. Returns whether the body was handed to
/// the codec, i.e. whether the plan was a [`ReasoningPlan::Write`].
pub fn apply_to_body(
    codec: &dyn Codec,
    body: &mut Value,
    plan: ReasoningPlan,
    ctx: &UpstreamCtx<'_>,
) -> bool {
    match plan {
        ReasoningPlan::LeaveAlone => false,
        ReasoningPlan::Write(fitted) => {
            codec.write_reasoning(body, fitted, ctx);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeCodec;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::reasoning::{Effort, Summary, ThinkingSupport};

    fn inputs<'a>(
        client: &'a ReasoningConfig,
        suffix: Option<Depth>,
        model: ModelThinking<'a>,
        target: Protocol,
        passthrough: bool,
    ) -> ReasoningInputs<'a> {
        ReasoningInputs {
            client,
            suffix,
            model,
            target,
            passthrough,
        }
    }

    fn levels() -> ThinkingSupport {
        ThinkingSupport::levels(&[Effort::Minimal, Effort::Low, Effort::Medium, Effort::High])
    }

    fn claude_budget() -> ThinkingSupport {
        ThinkingSupport {
            min: 1024,
            max: 128_000,
            zero_allowed: true,
            dynamic_allowed: false,
            levels: Vec::new(),
        }
    }

    const NONE: ReasoningConfig = ReasoningConfig {
        depth: None,
        summary: None,
    };

    fn with(depth: Depth) -> ReasoningConfig {
        ReasoningConfig::with_depth(depth)
    }

    // ----- suffix ----------------------------------------------------------

    #[test]
    fn suffix_wins_over_body_in_translation() {
        let caps = levels();
        let client = with(Depth::Level(Effort::Low));
        let plan = plan_reasoning(&inputs(
            &client,
            Some(Depth::Level(Effort::High)),
            ModelThinking::Supported(&caps),
            Protocol::OpenaiResponses,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High)))
        );
    }

    #[test]
    fn suffix_wins_over_body_in_passthrough() {
        let caps = levels();
        let client = with(Depth::Level(Effort::Low));
        let plan = plan_reasoning(&inputs(
            &client,
            Some(Depth::Level(Effort::High)),
            ModelThinking::Supported(&caps),
            Protocol::OpenaiChat,
            true,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High)))
        );
    }

    #[test]
    fn suffix_applies_when_body_is_silent() {
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Budget(16000)),
            ModelThinking::Unknown,
            Protocol::Anthropic,
            true,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(16000)))
        );
    }

    #[test]
    fn suffix_is_clamped_to_the_model() {
        let caps = levels();
        // xhigh is not offered: nearest supported level.
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Level(Effort::Xhigh)),
            ModelThinking::Supported(&caps),
            Protocol::OpenaiResponses,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High)))
        );
        // A level on a budget-only model becomes a budget.
        let caps = claude_budget();
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Level(Effort::Medium)),
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            false,
        ));
        assert_eq!(plan, ReasoningPlan::Write(Fitted::Use(Depth::Budget(8192))));
        // An oversized budget is clamped, never rejected.
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Budget(200_000)),
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            true,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(128_000)))
        );
    }

    #[test]
    fn suffix_none_and_auto() {
        let caps = claude_budget();
        let plan = plan_reasoning(&inputs(
            &with(Depth::Budget(4096)),
            Some(Depth::Off),
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            true,
        ));
        assert_eq!(plan, ReasoningPlan::Write(Fitted::Use(Depth::Off)));
        // No dynamic thinking on this model: the midpoint budget.
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Auto),
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(64512)))
        );
    }

    #[test]
    fn suffix_on_a_model_that_cannot_reason_strips() {
        for passthrough in [true, false] {
            let plan = plan_reasoning(&inputs(
                &with(Depth::Level(Effort::High)),
                Some(Depth::Level(Effort::High)),
                ModelThinking::Unsupported,
                Protocol::OpenaiChat,
                passthrough,
            ));
            assert_eq!(plan, ReasoningPlan::Write(Fitted::Strip));
        }
    }

    #[test]
    fn suffix_on_unknown_gemini_model_becomes_a_budget() {
        let plan = plan_reasoning(&inputs(
            &NONE,
            Some(Depth::Level(Effort::High)),
            ModelThinking::Unknown,
            Protocol::Gemini,
            true,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(24576)))
        );
    }

    // ----- passthrough without suffix --------------------------------------

    #[test]
    fn passthrough_leaves_a_native_body_alone() {
        let caps = levels();
        for model in [ModelThinking::Unknown, ModelThinking::Supported(&caps)] {
            for client in [
                NONE,
                with(Depth::Level(Effort::Xhigh)),
                with(Depth::Budget(999_999)),
                with(Depth::Off),
                with(Depth::Auto),
            ] {
                let plan = plan_reasoning(&inputs(
                    &client,
                    None,
                    model,
                    Protocol::OpenaiResponses,
                    true,
                ));
                assert_eq!(plan, ReasoningPlan::LeaveAlone, "{client:?}");
            }
        }
    }

    #[test]
    fn passthrough_strips_depth_for_a_model_that_cannot_reason() {
        for depth in [
            Depth::Level(Effort::Low),
            Depth::Budget(2048),
            Depth::Auto,
            Depth::Off,
        ] {
            let plan = plan_reasoning(&inputs(
                &with(depth),
                None,
                ModelThinking::Unsupported,
                Protocol::Anthropic,
                true,
            ));
            assert_eq!(plan, ReasoningPlan::Write(Fitted::Strip), "{depth:?}");
        }
    }

    #[test]
    fn passthrough_unsupported_model_without_depth_is_left_alone() {
        let summary_only = ReasoningConfig {
            depth: None,
            summary: Some(Summary::Auto),
        };
        for client in [NONE, summary_only] {
            let plan = plan_reasoning(&inputs(
                &client,
                None,
                ModelThinking::Unsupported,
                Protocol::OpenaiResponses,
                true,
            ));
            assert_eq!(plan, ReasoningPlan::LeaveAlone);
        }
    }

    // ----- translation without suffix --------------------------------------

    #[test]
    fn translation_fits_the_client_depth() {
        let caps = claude_budget();
        let plan = plan_reasoning(&inputs(
            &with(Depth::Level(Effort::Xhigh)),
            None,
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(32768)))
        );

        let caps = levels();
        let plan = plan_reasoning(&inputs(
            &with(Depth::Budget(64000)),
            None,
            ModelThinking::Supported(&caps),
            Protocol::OpenaiChat,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High)))
        );
    }

    #[test]
    fn translation_off_on_a_model_that_cannot_be_disabled() {
        let caps = levels();
        let plan = plan_reasoning(&inputs(
            &with(Depth::Off),
            None,
            ModelThinking::Supported(&caps),
            Protocol::OpenaiResponses,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::Minimal)))
        );
    }

    #[test]
    fn translation_unknown_model_keeps_the_depth() {
        let plan = plan_reasoning(&inputs(
            &with(Depth::Level(Effort::High)),
            None,
            ModelThinking::Unknown,
            Protocol::Anthropic,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High)))
        );
        let plan = plan_reasoning(&inputs(
            &with(Depth::Level(Effort::High)),
            None,
            ModelThinking::Unknown,
            Protocol::Gemini,
            false,
        ));
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(24576)))
        );
    }

    #[test]
    fn translation_unsupported_model_strips() {
        let plan = plan_reasoning(&inputs(
            &with(Depth::Budget(4096)),
            None,
            ModelThinking::Unsupported,
            Protocol::OpenaiChat,
            false,
        ));
        assert_eq!(plan, ReasoningPlan::Write(Fitted::Strip));
    }

    #[test]
    fn translation_with_nothing_requested_invents_nothing() {
        let caps = levels();
        let summary_only = ReasoningConfig {
            depth: None,
            summary: Some(Summary::Detailed),
        };
        for model in [
            ModelThinking::Unknown,
            ModelThinking::Unsupported,
            ModelThinking::Supported(&caps),
        ] {
            for client in [NONE, summary_only.clone()] {
                for target in Protocol::ALL {
                    let plan = plan_reasoning(&inputs(&client, None, model, target, false));
                    assert_eq!(plan, ReasoningPlan::LeaveAlone);
                }
            }
        }
    }

    // ----- labels ----------------------------------------------------------

    #[test]
    fn label_of_a_written_depth() {
        let caps = claude_budget();
        let client = with(Depth::Level(Effort::Low));
        let i = inputs(
            &client,
            Some(Depth::Level(Effort::High)),
            ModelThinking::Supported(&caps),
            Protocol::Anthropic,
            false,
        );
        let (plan, label) = plan_with_label(&i);
        assert_eq!(
            plan,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(24576)))
        );
        // Budgets are bucketed back into an effort name.
        assert_eq!(label, Some("high"));
    }

    #[test]
    fn label_none_auto_and_levels() {
        let i = inputs(
            &NONE,
            None,
            ModelThinking::Unknown,
            Protocol::OpenaiChat,
            false,
        );
        let label = |depth| effective_label(&i, ReasoningPlan::Write(Fitted::Use(depth)));
        assert_eq!(label(Depth::Off), Some("none"));
        assert_eq!(label(Depth::Auto), Some("auto"));
        assert_eq!(label(Depth::Level(Effort::Max)), Some("max"));
        assert_eq!(label(Depth::Budget(600)), Some("low"));
    }

    #[test]
    fn label_of_strip_is_absent() {
        let client = with(Depth::Level(Effort::High));
        let i = inputs(
            &client,
            None,
            ModelThinking::Unsupported,
            Protocol::OpenaiChat,
            true,
        );
        assert_eq!(
            plan_with_label(&i),
            (ReasoningPlan::Write(Fitted::Strip), None)
        );
    }

    #[test]
    fn label_of_leave_alone_is_the_clients_own_depth() {
        let client = with(Depth::Budget(10_000));
        let i = inputs(
            &client,
            None,
            ModelThinking::Unknown,
            Protocol::Anthropic,
            true,
        );
        assert_eq!(
            plan_with_label(&i),
            (ReasoningPlan::LeaveAlone, Some("high"))
        );
        let i = inputs(
            &NONE,
            None,
            ModelThinking::Unknown,
            Protocol::Anthropic,
            true,
        );
        assert_eq!(plan_with_label(&i), (ReasoningPlan::LeaveAlone, None));
    }

    // ----- applying --------------------------------------------------------

    #[test]
    fn apply_use_sets_depth_and_keeps_summary() {
        let mut req = Request::new("m", Protocol::OpenaiResponses);
        req.reasoning = Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Concise),
        });
        apply_to_request(
            &mut req,
            ReasoningPlan::Write(Fitted::Use(Depth::Budget(8192))),
        );
        assert_eq!(
            req.reasoning,
            Some(ReasoningConfig {
                depth: Some(Depth::Budget(8192)),
                summary: Some(Summary::Concise),
            })
        );
    }

    #[test]
    fn apply_use_creates_the_config_when_missing() {
        let mut req = Request::new("m", Protocol::OpenaiChat);
        apply_to_request(
            &mut req,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High))),
        );
        assert_eq!(
            req.reasoning,
            Some(ReasoningConfig::with_depth(Depth::Level(Effort::High)))
        );
    }

    #[test]
    fn apply_strip_clears_depth_but_keeps_summary() {
        let mut req = Request::new("m", Protocol::OpenaiResponses);
        req.reasoning = Some(ReasoningConfig {
            depth: Some(Depth::Level(Effort::Low)),
            summary: Some(Summary::Auto),
        });
        apply_to_request(&mut req, ReasoningPlan::Write(Fitted::Strip));
        assert_eq!(
            req.reasoning,
            Some(ReasoningConfig {
                depth: None,
                summary: Some(Summary::Auto),
            })
        );
    }

    #[test]
    fn apply_strip_drops_an_emptied_config() {
        let mut req = Request::new("m", Protocol::OpenaiChat);
        req.reasoning = Some(ReasoningConfig::with_depth(Depth::Auto));
        apply_to_request(&mut req, ReasoningPlan::Write(Fitted::Strip));
        assert_eq!(req.reasoning, None);
        // And is harmless when there was nothing.
        apply_to_request(&mut req, ReasoningPlan::Write(Fitted::Strip));
        assert_eq!(req.reasoning, None);
    }

    #[test]
    fn apply_leave_alone_changes_nothing() {
        let mut req = Request::new("m", Protocol::Gemini);
        req.reasoning = Some(ReasoningConfig::with_depth(Depth::Budget(1)));
        let before = req.clone();
        apply_to_request(&mut req, ReasoningPlan::LeaveAlone);
        assert_eq!(req, before);
    }

    #[test]
    fn apply_to_body_goes_through_the_codec() {
        let codec = FakeCodec::new(Protocol::OpenaiChat, "a");
        let ctx = UpstreamCtx::default();

        let mut body = json!({"model": "m", "effort": "low"});
        assert!(apply_to_body(
            &codec,
            &mut body,
            ReasoningPlan::Write(Fitted::Use(Depth::Level(Effort::High))),
            &ctx
        ));
        assert_eq!(body, json!({"model": "m", "effort": "high"}));

        assert!(apply_to_body(
            &codec,
            &mut body,
            ReasoningPlan::Write(Fitted::Strip),
            &ctx
        ));
        assert_eq!(body, json!({"model": "m"}));

        let mut body = json!({"model": "m", "effort": "low"});
        assert!(!apply_to_body(
            &codec,
            &mut body,
            ReasoningPlan::LeaveAlone,
            &ctx
        ));
        assert_eq!(body, json!({"model": "m", "effort": "low"}));
    }

    #[test]
    fn end_to_end_passthrough_with_suffix() {
        // What the gateway does for `model(high)` on a same-protocol route.
        let codec = FakeCodec::new(Protocol::OpenaiChat, "a");
        let mut body = json!({"model": "m", "effort": "low", "messages": []});
        let client = codec.read_reasoning(&body);
        assert_eq!(client.depth, Some(Depth::Level(Effort::Low)));
        let caps = levels();
        let i = inputs(
            &client,
            Some(Depth::Level(Effort::High)),
            ModelThinking::Supported(&caps),
            Protocol::OpenaiChat,
            true,
        );
        let (plan, label) = plan_with_label(&i);
        apply_to_body(&codec, &mut body, plan, &UpstreamCtx::default());
        assert_eq!(body["effort"], "high");
        assert_eq!(label, Some("high"));
    }

    #[test]
    fn fitted_accessor() {
        assert_eq!(ReasoningPlan::LeaveAlone.fitted(), None);
        assert_eq!(
            ReasoningPlan::Write(Fitted::Strip).fitted(),
            Some(Fitted::Strip)
        );
    }
}
