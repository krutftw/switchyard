//! Payload rules: operator-configured patches to the JSON body sent upstream.
//!
//! The `[payload]` section of the configuration ([`PayloadConfig`]) holds
//! three rule lists which are applied, in this order, to the body that is
//! about to be sent to an upstream — after translation and after reasoning
//! has been written:
//!
//! 1. **`default`** — fill in a field the client did not specify;
//! 2. **`override`** — force a field to a value;
//! 3. **`filter`** — delete fields.
//!
//! Rule paths are [`crate::jsonpath`] paths in the *upstream* protocol's body
//! layout, which is why a rule may be limited to one upstream protocol.

use crate::jsonpath::{self, Segment};
use serde_json::Value;
use std::collections::HashSet;
use switchyard_core::config::{PayloadConfig, PayloadRule};
use switchyard_core::protocol::Protocol;
use switchyard_core::util::wildcard_match;

/// The facts about one upstream attempt that payload rules are matched
/// against.
#[derive(Clone, Copy, Debug)]
pub struct PayloadCtx<'a> {
    /// Model id the upstream is called with.
    pub upstream_model: &'a str,
    /// Model name the client asked for, with any reasoning suffix already
    /// stripped (`gpt-5(high)` → `gpt-5`).
    pub requested_model: &'a str,
    /// Protocol of the upstream request body.
    pub protocol: Protocol,
    /// Name of the provider serving the attempt.
    pub provider: &'a str,
    /// True when the client speaks the same protocol as the upstream, i.e.
    /// the original client body has the same layout as the upstream body and
    /// can be consulted by `default` rules.
    pub same_protocol: bool,
}

/// Whether `rule` applies to the attempt described by `ctx`.
///
/// A rule matches when
///
/// * at least one of its `models` patterns matches the upstream model id or
///   the client-requested name (`*` wildcard, case-insensitive; blank patterns
///   and blank model names never match), and
/// * its `protocol`, when set, equals the upstream protocol, and
/// * its `provider`, when non-empty, equals the provider name exactly.
pub fn rule_matches(rule: &PayloadRule, ctx: &PayloadCtx<'_>) -> bool {
    if rule.protocol.is_some_and(|p| p != ctx.protocol) {
        return false;
    }
    let provider = rule.provider.trim();
    if !provider.is_empty() && provider != ctx.provider {
        return false;
    }
    let candidates = [ctx.upstream_model.trim(), ctx.requested_model.trim()];
    rule.models
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| {
            candidates
                .iter()
                .any(|model| !model.is_empty() && wildcard_match(pattern, model))
        })
}

/// Applies the payload rules to `upstream_body` and returns how many fields
/// were actually changed (written with a different value, or removed).
///
/// * **default** rules write a path only if all of these hold:
///   the path is absent from (or `null` in) the upstream body; when
///   [`PayloadCtx::same_protocol`] is set and the original client body is
///   available, the path is absent from that body too (a field the client
///   sent — even as `null`, even if the gateway removed it since — is the
///   client's decision and is not defaulted); and no earlier `default` rule
///   wrote the same path. The first matching rule wins per path.
///   For translated requests the client body is in another protocol's layout
///   and is not consulted.
/// * **override** rules always write; when several rules set the same path
///   the last one wins.
/// * **filter** rules delete every path listed in `remove`.
///
/// Only the field that belongs to a rule's section is read: `set` for
/// `default` / `override`, `remove` for `filter`. A write that cannot be
/// performed (see [`jsonpath::set`]: a scalar in the way, an array index out
/// of range) is skipped. Bodies that are not JSON objects are left untouched.
pub fn apply_payload_rules(
    cfg: &PayloadConfig,
    ctx: &PayloadCtx<'_>,
    original_client_body: Option<&Value>,
    upstream_body: &mut Value,
) -> usize {
    if cfg.is_empty() || !upstream_body.is_object() {
        return 0;
    }
    let mut changes = 0usize;

    let baseline = if ctx.same_protocol {
        original_client_body
    } else {
        None
    };
    let mut defaulted: HashSet<Vec<Segment>> = HashSet::new();
    for rule in cfg.default.iter().filter(|rule| rule_matches(rule, ctx)) {
        for (path, value) in &rule.set {
            let segments = jsonpath::parse(path);
            if segments.is_empty() || defaulted.contains(&segments) {
                continue;
            }
            if jsonpath::exists_at(upstream_body, &segments) {
                continue;
            }
            if baseline.is_some_and(|body| jsonpath::get_at(body, &segments).is_some()) {
                continue;
            }
            if let Some(changed) = write(upstream_body, &segments, value) {
                defaulted.insert(segments);
                changes += usize::from(changed);
            }
        }
    }

    for rule in cfg.overrides.iter().filter(|rule| rule_matches(rule, ctx)) {
        for (path, value) in &rule.set {
            let segments = jsonpath::parse(path);
            if let Some(changed) = write(upstream_body, &segments, value) {
                changes += usize::from(changed);
            }
        }
    }

    for rule in cfg.filter.iter().filter(|rule| rule_matches(rule, ctx)) {
        for path in &rule.remove {
            if jsonpath::remove(upstream_body, path) {
                changes += 1;
            }
        }
    }

    changes
}

/// Writes `value` at `segments`. `None` when the write is impossible,
/// otherwise whether the body is different afterwards.
fn write(body: &mut Value, segments: &[Segment], value: &Value) -> Option<bool> {
    let changed = jsonpath::get_at(body, segments) != Some(value);
    jsonpath::set_at(body, segments, value.clone()).then_some(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn cfg(value: Value) -> PayloadConfig {
        serde_json::from_value(value).expect("valid payload config")
    }

    fn ctx<'a>(upstream: &'a str, requested: &'a str) -> PayloadCtx<'a> {
        PayloadCtx {
            upstream_model: upstream,
            requested_model: requested,
            protocol: Protocol::OpenaiChat,
            provider: "main",
            same_protocol: false,
        }
    }

    // ----- matching --------------------------------------------------------

    fn rule(value: Value) -> PayloadRule {
        serde_json::from_value(value).expect("valid rule")
    }

    #[test]
    fn matches_upstream_model_pattern() {
        let r = rule(json!({"models": ["gpt-*"]}));
        assert!(rule_matches(&r, &ctx("gpt-5", "alias")));
        assert!(!rule_matches(&r, &ctx("claude-sonnet-4-5", "alias")));
    }

    #[test]
    fn matches_requested_model_pattern() {
        let r = rule(json!({"models": ["fast"]}));
        assert!(rule_matches(&r, &ctx("gemini-2.5-flash", "fast")));
        assert!(!rule_matches(&r, &ctx("gemini-2.5-flash", "faster")));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let r = rule(json!({"models": ["GPT-*"]}));
        assert!(rule_matches(&r, &ctx("gpt-5-mini", "x")));
        let r = rule(json!({"models": ["gemini-*"]}));
        assert!(rule_matches(&r, &ctx("x", "Gemini-2.5-Pro")));
    }

    #[test]
    fn any_pattern_is_enough() {
        let r = rule(json!({"models": ["o3*", "*-preview", "exact-name"]}));
        assert!(rule_matches(&r, &ctx("gemini-3-pro-preview", "a")));
        assert!(rule_matches(&r, &ctx("exact-name", "a")));
        assert!(!rule_matches(&r, &ctx("exact-name-2", "a")));
    }

    #[test]
    fn no_patterns_or_blank_patterns_never_match() {
        assert!(!rule_matches(&rule(json!({"models": []})), &ctx("m", "m")));
        assert!(!rule_matches(
            &rule(json!({"models": ["", "  "]})),
            &ctx("m", "m")
        ));
    }

    #[test]
    fn patterns_are_trimmed() {
        let r = rule(json!({"models": ["  gpt-*  "]}));
        assert!(rule_matches(&r, &ctx("gpt-5", "x")));
    }

    #[test]
    fn blank_model_names_are_not_candidates() {
        let r = rule(json!({"models": ["*"]}));
        assert!(!rule_matches(&r, &ctx("", "")));
        assert!(rule_matches(&r, &ctx("", "requested")));
        assert!(rule_matches(&r, &ctx("upstream", "")));
    }

    #[test]
    fn protocol_constraint() {
        let r = rule(json!({"models": ["*"], "protocol": "gemini"}));
        let mut c = ctx("m", "m");
        assert!(!rule_matches(&r, &c));
        c.protocol = Protocol::Gemini;
        assert!(rule_matches(&r, &c));
        let r = rule(json!({"models": ["*"], "protocol": "openai-responses"}));
        assert!(!rule_matches(&r, &c));
    }

    #[test]
    fn provider_constraint() {
        let r = rule(json!({"models": ["*"], "provider": "backup"}));
        let mut c = ctx("m", "m");
        assert!(!rule_matches(&r, &c));
        c.provider = "backup";
        assert!(rule_matches(&r, &c));
        // Provider names are identifiers: compared exactly.
        c.provider = "Backup";
        assert!(!rule_matches(&r, &c));
        // Blank means "any provider".
        let r = rule(json!({"models": ["*"], "provider": "  "}));
        assert!(rule_matches(&r, &c));
    }

    #[test]
    fn all_constraints_must_hold_together() {
        let r = rule(json!({"models": ["gpt-*"], "protocol": "openai-chat", "provider": "main"}));
        let c = ctx("gpt-5", "x");
        assert!(rule_matches(&r, &c));
        assert!(!rule_matches(
            &r,
            &PayloadCtx {
                provider: "other",
                ..c
            }
        ));
        assert!(!rule_matches(
            &r,
            &PayloadCtx {
                protocol: Protocol::Anthropic,
                ..c
            }
        ));
        assert!(!rule_matches(&r, &ctx("claude", "x")));
    }

    // ----- default ---------------------------------------------------------

    #[test]
    fn default_fills_absent_path() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"temperature": 0.2}}]}));
        let mut body = json!({"model": "m"});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            1
        );
        assert_eq!(body, json!({"model": "m", "temperature": 0.2}));
    }

    #[test]
    fn default_creates_nested_path() {
        let cfg = cfg(json!({"default": [{
            "models": ["gemini-*"],
            "set": {"generationConfig.thinkingConfig.thinkingBudget": 32768}
        }]}));
        let mut body = json!({"contents": []});
        apply_payload_rules(&cfg, &ctx("gemini-2.5-pro", "x"), None, &mut body);
        assert_eq!(
            body,
            json!({"contents": [], "generationConfig": {"thinkingConfig": {"thinkingBudget": 32768}}})
        );
    }

    #[test]
    fn default_does_not_touch_a_present_value() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"temperature": 0.2}}]}));
        for present in [json!(1.0), json!(0), json!(false), json!(""), json!({})] {
            let mut body = json!({"temperature": present});
            let before = body.clone();
            assert_eq!(
                apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
                0
            );
            assert_eq!(body, before);
        }
    }

    #[test]
    fn default_fills_a_null_in_the_upstream_body() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"temperature": 0.2}}]}));
        let mut body = json!({"temperature": null});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            1
        );
        assert_eq!(body, json!({"temperature": 0.2}));
    }

    #[test]
    fn default_respects_the_client_body_in_same_protocol_mode() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"reasoning_effort": "high"}}]}));
        // The client asked for an effort, the gateway stripped it for this
        // model: the default must not bring it back.
        let client = json!({"model": "m", "reasoning_effort": "low"});
        let mut body = json!({"model": "m"});
        let mut c = ctx("m", "m");
        c.same_protocol = true;
        assert_eq!(apply_payload_rules(&cfg, &c, Some(&client), &mut body), 0);
        assert_eq!(body, json!({"model": "m"}));
    }

    #[test]
    fn default_treats_client_null_as_specified() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"temperature": 0.2}}]}));
        let client = json!({"temperature": null});
        let mut body = json!({"temperature": null});
        let mut c = ctx("m", "m");
        c.same_protocol = true;
        assert_eq!(apply_payload_rules(&cfg, &c, Some(&client), &mut body), 0);
        assert_eq!(body, json!({"temperature": null}));
    }

    #[test]
    fn default_ignores_the_client_body_when_translating() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"temperature": 0.2}}]}));
        // Same key by coincidence, but another protocol's layout.
        let client = json!({"temperature": 1.0});
        let mut body = json!({});
        let c = ctx("m", "m");
        assert!(!c.same_protocol);
        assert_eq!(apply_payload_rules(&cfg, &c, Some(&client), &mut body), 1);
        assert_eq!(body, json!({"temperature": 0.2}));
    }

    #[test]
    fn default_without_client_body_only_checks_the_upstream_body() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"a": 1, "b": 2}}]}));
        let mut body = json!({"a": "kept"});
        let mut c = ctx("m", "m");
        c.same_protocol = true;
        assert_eq!(apply_payload_rules(&cfg, &c, None, &mut body), 1);
        assert_eq!(body, json!({"a": "kept", "b": 2}));
    }

    #[test]
    fn first_default_rule_wins_per_path() {
        let cfg = cfg(json!({"default": [
            {"models": ["*"], "set": {"top_p": 0.9}},
            {"models": ["*"], "set": {"top_p": 0.1, "seed": 7}}
        ]}));
        let mut body = json!({});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            2
        );
        assert_eq!(body, json!({"top_p": 0.9, "seed": 7}));
    }

    #[test]
    fn first_default_rule_wins_even_when_it_wrote_null() {
        let cfg = cfg(json!({"default": [
            {"models": ["*"], "set": {"stop": null}},
            {"models": ["*"], "set": {"stop": ["END"]}}
        ]}));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(body, json!({"stop": null}));
    }

    #[test]
    fn non_matching_default_rule_does_not_block_a_later_one() {
        let cfg = cfg(json!({"default": [
            {"models": ["claude-*"], "set": {"top_p": 0.9}},
            {"models": ["gpt-*"], "set": {"top_p": 0.1}}
        ]}));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("gpt-5", "x"), None, &mut body);
        assert_eq!(body, json!({"top_p": 0.1}));
    }

    #[test]
    fn default_with_a_scalar_in_the_way_is_skipped() {
        let cfg = cfg(json!({"default": [{"models": ["*"], "set": {"reasoning.effort": "low"}}]}));
        let mut body = json!({"reasoning": "weird"});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            0
        );
        assert_eq!(body, json!({"reasoning": "weird"}));
    }

    // ----- override --------------------------------------------------------

    #[test]
    fn override_always_writes() {
        let cfg =
            cfg(json!({"override": [{"models": ["*"], "set": {"temperature": 0, "user": "gw"}}]}));
        let client = json!({"temperature": 1});
        let mut body = json!({"temperature": 1});
        let mut c = ctx("m", "m");
        c.same_protocol = true;
        assert_eq!(apply_payload_rules(&cfg, &c, Some(&client), &mut body), 2);
        assert_eq!(body, json!({"temperature": 0, "user": "gw"}));
    }

    #[test]
    fn later_override_rule_wins() {
        let cfg = cfg(json!({"override": [
            {"models": ["*"], "set": {"max_tokens": 100}},
            {"models": ["gpt-*"], "set": {"max_tokens": 200}},
            {"models": ["claude-*"], "set": {"max_tokens": 300}}
        ]}));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("gpt-5", "x"), None, &mut body);
        assert_eq!(body, json!({"max_tokens": 200}));
    }

    #[test]
    fn override_beats_default() {
        let cfg = cfg(json!({
            "default": [{"models": ["*"], "set": {"temperature": 0.2}}],
            "override": [{"models": ["*"], "set": {"temperature": 0.7}}]
        }));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(body, json!({"temperature": 0.7}));
    }

    #[test]
    fn override_with_identical_value_is_not_counted() {
        let cfg = cfg(json!({"override": [{"models": ["*"], "set": {"stream": true, "n": 1}}]}));
        let mut body = json!({"stream": true});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            1
        );
        assert_eq!(body, json!({"stream": true, "n": 1}));
    }

    #[test]
    fn override_into_array_element() {
        let cfg =
            cfg(json!({"override": [{"models": ["*"], "set": {"messages.0.role": "developer"}}]}));
        let mut body = json!({"messages": [{"role": "system", "content": "x"}]});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            1
        );
        assert_eq!(body["messages"][0]["role"], "developer");
    }

    #[test]
    fn override_out_of_range_index_is_skipped() {
        let cfg = cfg(json!({"override": [{"models": ["*"], "set": {"messages.5.role": "user"}}]}));
        let mut body = json!({"messages": []});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            0
        );
        assert_eq!(body, json!({"messages": []}));
    }

    #[test]
    fn override_escaped_dot_path() {
        let cfg =
            cfg(json!({"override": [{"models": ["*"], "set": {"metadata.trace\\.id": "t1"}}]}));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(body, json!({"metadata": {"trace.id": "t1"}}));
    }

    // ----- filter ----------------------------------------------------------

    #[test]
    fn filter_removes_paths() {
        let cfg = cfg(
            json!({"filter": [{"models": ["*"], "remove": ["user", "metadata.session", "nope"]}]}),
        );
        let mut body = json!({"user": "u", "metadata": {"session": "s", "keep": 1}, "model": "m"});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            2
        );
        assert_eq!(body, json!({"metadata": {"keep": 1}, "model": "m"}));
    }

    #[test]
    fn filter_runs_after_override() {
        let cfg = cfg(json!({
            "override": [{"models": ["*"], "set": {"service_tier": "flex"}}],
            "filter": [{"models": ["*"], "remove": ["service_tier"]}]
        }));
        let mut body = json!({"model": "m"});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            2
        );
        assert_eq!(body, json!({"model": "m"}));
    }

    #[test]
    fn filter_runs_after_default() {
        let cfg = cfg(json!({
            "default": [{"models": ["*"], "set": {"top_k": 40}}],
            "filter": [{"models": ["*"], "remove": ["top_k"]}]
        }));
        let mut body = json!({});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn filter_keeps_key_order() {
        let cfg = cfg(json!({"filter": [{"models": ["*"], "remove": ["b"]}]}));
        let mut body = json!({"a": 1, "b": 2, "c": 3, "d": 4});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"a":1,"c":3,"d":4}"#
        );
    }

    // ----- sections, scoping, misc ----------------------------------------

    #[test]
    fn full_pipeline_order() {
        let cfg = cfg(json!({
            "default": [
                {"models": ["gpt-*"], "set": {"temperature": 0.3, "top_p": 0.8}}
            ],
            "override": [
                {"models": ["*"], "set": {"top_p": 1.0, "store": false}}
            ],
            "filter": [
                {"models": ["*"], "remove": ["store", "logit_bias"]}
            ]
        }));
        let mut body = json!({"model": "gpt-5", "temperature": 0.9, "logit_bias": {"1": 2}});
        let n = apply_payload_rules(&cfg, &ctx("gpt-5", "smart"), None, &mut body);
        // default top_p, override top_p, override store, filter store, filter logit_bias
        assert_eq!(n, 5);
        assert_eq!(
            body,
            json!({"model": "gpt-5", "temperature": 0.9, "top_p": 1.0})
        );
    }

    #[test]
    fn rules_for_other_models_protocols_and_providers_are_skipped() {
        let cfg = cfg(json!({
            "default": [{"models": ["claude-*"], "set": {"a": 1}}],
            "override": [
                {"models": ["*"], "protocol": "anthropic", "set": {"b": 1}},
                {"models": ["*"], "provider": "elsewhere", "set": {"c": 1}}
            ],
            "filter": [{"models": ["o3"], "remove": ["model"]}]
        }));
        let mut body = json!({"model": "gpt-5"});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("gpt-5", "smart"), None, &mut body),
            0
        );
        assert_eq!(body, json!({"model": "gpt-5"}));
    }

    #[test]
    fn rule_selected_by_requested_name() {
        let cfg = cfg(json!({"override": [{"models": ["smart"], "set": {"x": 1}}]}));
        let mut body = json!({});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("gpt-5", "smart"), None, &mut body),
            1
        );
    }

    #[test]
    fn fields_of_the_wrong_section_are_ignored() {
        let cfg = cfg(json!({
            "override": [{"models": ["*"], "set": {"a": 1}, "remove": ["keep"]}],
            "filter": [{"models": ["*"], "remove": ["gone"], "set": {"b": 2}}]
        }));
        let mut body = json!({"keep": true, "gone": true});
        apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body);
        assert_eq!(body, json!({"keep": true, "a": 1}));
    }

    #[test]
    fn empty_config_changes_nothing() {
        let mut body = json!({"a": 1});
        assert_eq!(
            apply_payload_rules(&PayloadConfig::default(), &ctx("m", "m"), None, &mut body),
            0
        );
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn non_object_bodies_are_left_alone() {
        let cfg = cfg(json!({
            "default": [{"models": ["*"], "set": {"a": 1}}],
            "override": [{"models": ["*"], "set": {"0": 1}}],
            "filter": [{"models": ["*"], "remove": ["0"]}]
        }));
        for original in [Value::Null, json!([1, 2]), json!("text"), json!(3)] {
            let mut body = original.clone();
            assert_eq!(
                apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
                0
            );
            assert_eq!(body, original);
        }
    }

    #[test]
    fn empty_path_in_a_rule_is_ignored() {
        let cfg = cfg(json!({
            "default": [{"models": ["*"], "set": {"": 1}}],
            "override": [{"models": ["*"], "set": {"": 1}}],
            "filter": [{"models": ["*"], "remove": [""]}]
        }));
        let mut body = json!({"a": 1});
        assert_eq!(
            apply_payload_rules(&cfg, &ctx("m", "m"), None, &mut body),
            0
        );
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn config_parsed_from_toml_applies() {
        // The shape users actually write.
        let text = r#"
            [[payload.default]]
            models = ["gemini-*"]
            protocol = "gemini"
            set = { "generationConfig.temperature" = 0.4 }

            [[payload.override]]
            models = ["*"]
            provider = "main"
            set = { "safetySettings" = [] }

            [[payload.filter]]
            models = ["gemini-2.5-*"]
            remove = ["generationConfig.topK"]
        "#;
        let config = switchyard_core::Config::from_toml(text).expect("config parses");
        let mut body = json!({"contents": [], "generationConfig": {"topK": 5}});
        let mut c = ctx("gemini-2.5-flash", "flash");
        c.protocol = Protocol::Gemini;
        let n = apply_payload_rules(&config.payload, &c, None, &mut body);
        assert_eq!(n, 3);
        assert_eq!(
            body,
            json!({"contents": [], "generationConfig": {"temperature": 0.4}, "safetySettings": []})
        );
    }
}
