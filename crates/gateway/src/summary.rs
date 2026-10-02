//! Reasoning summaries on OpenAI Responses upstreams that refuse them.
//!
//! A request translated for a Responses upstream asks for
//! `reasoning.summary` whenever the client wants to see reasoning text (a
//! Chat Completions client says so by turning reasoning on): it is the only
//! way to get any out of that API. OpenAI generates summaries only for
//! *verified* organisations and answers everybody else with a `400`
//! (`param: "reasoning.summary"`, `code: "unsupported_value"`, "Your
//! organization must be verified to generate reasoning summaries…"). The
//! client did not write that field — the gateway did — so the gateway takes
//! it back: the attempt is repeated once on the same credential without the
//! field, and what was learnt is remembered so later translated requests
//! leave it out from the start ([`SummaryRefusals`]).
//!
//! How much is remembered depends on what the refusal is about
//! ([`Refusal`]), because one client's request must not cost every other
//! client its reasoning text:
//!
//! * the organisation is not verified — the whole provider: no model of it
//!   will summarise. Another provider the same request fails over to —
//!   another organisation, for all the gateway knows — is still asked;
//! * the error merely names the parameter ("'reasoning.summary' is not
//!   supported with this model", "'concise' is not supported with the … model")
//!   — that upstream model of the provider, and only when what was refused
//!   is the plain `"auto"` the gateway asks for by default. A detail level
//!   one client chose is that request's affair: it is healed and nothing is
//!   remembered.
//!
//! Everything is remembered for the configuration it was learnt under and
//! no other: a request still in flight when the configuration changes (a new
//! key, another organisation) leaves no trace in what is known afterwards.
//!
//! A Responses client's request is never touched, forwarded or re-encoded:
//! such a client wrote the field itself and is told what the upstream
//! thinks of it. Translated *counting* requests never carry the field at
//! all: it does not change the count.

use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use switchyard_core::UpstreamError;
use switchyard_core::config::Config;

/// Members of `reasoning` that ask for a summary (`generate_summary` is the
/// deprecated spelling).
const SUMMARY_FIELDS: [&str; 2] = ["summary", "generate_summary"];

/// The summary the gateway asks for when the client did not choose a detail
/// level.
const DEFAULT_SUMMARY: &str = "auto";

/// The most upstream models remembered per provider. Model names come from
/// the configuration and from discovery, so this is never reached in
/// practice; it keeps the memory bounded whatever an upstream lists.
const MAX_MODELS_PER_PROVIDER: usize = 512;

/// What a Responses request body asks for by way of a reasoning summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryAsk {
    /// No summary.
    Nothing,
    /// The default, `"auto"`: any summary the model can give.
    Auto,
    /// A particular detail level (`"concise"`, `"detailed"`, …).
    Chosen,
}

/// What an upstream's refusal of a reasoning summary is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The organisation behind the credential is not verified: none of the
    /// provider's models will summarise.
    Organisation,
    /// The error names `reasoning.summary` and says nothing about the
    /// organisation: the model, or the value asked for, is at fault.
    Parameter,
}

/// What is known to refuse reasoning summaries, and under which
/// configuration that was learnt.
struct Known {
    /// The configuration all of this was learnt under.
    config: Arc<Config>,
    /// Providers that refuse summaries whatever the model, by name.
    providers: HashSet<String>,
    /// Upstream models that refuse them, by provider name.
    models: HashMap<String, HashSet<String>>,
}

/// The providers, and single upstream models of providers, whose upstream
/// refused reasoning summaries. Kept in memory for one configuration: a
/// change (a different key, another organisation) starts it afresh, and a
/// request that runs under any other configuration neither sees nor adds
/// anything. At most one entry per configured provider and
/// [`MAX_MODELS_PER_PROVIDER`] models for each.
pub(crate) struct SummaryRefusals {
    known: Mutex<Known>,
}

impl SummaryRefusals {
    /// Nothing known, for requests running under `config`.
    pub(crate) fn new(config: Arc<Config>) -> Self {
        Self {
            known: Mutex::new(Known {
                config,
                providers: HashSet::new(),
                models: HashMap::new(),
            }),
        }
    }

    /// Whether a request running under `config` should leave the summary
    /// out for `model` of `provider`: the provider, or that model of it, is
    /// known to refuse.
    pub(crate) fn covers(&self, config: &Arc<Config>, provider: &str, model: &str) -> bool {
        let known = self.known.lock();
        Arc::ptr_eq(&known.config, config)
            && (known.providers.contains(provider)
                || known
                    .models
                    .get(provider)
                    .is_some_and(|models| models.contains(model)))
    }

    /// Remembers that `provider` refuses reasoning summaries — for `model`
    /// only when one is given. True when that is news; false also when
    /// `config`, under which the refusal was met, is no longer the
    /// configuration in effect (what an earlier key was refused says
    /// nothing about the present one).
    pub(crate) fn remember(
        &self,
        config: &Arc<Config>,
        provider: &str,
        model: Option<&str>,
    ) -> bool {
        let mut known = self.known.lock();
        if !Arc::ptr_eq(&known.config, config) {
            return false;
        }
        match model {
            None => known.providers.insert(provider.to_string()),
            // Already covered by what is known about the provider.
            Some(_) if known.providers.contains(provider) => false,
            Some(model) => {
                let models = known.models.entry(provider.to_string()).or_default();
                models.len() < MAX_MODELS_PER_PROVIDER && models.insert(model.to_string())
            }
        }
    }

    /// Forgets everything: `config` is in effect from now on.
    pub(crate) fn reset(&self, config: Arc<Config>) {
        let mut known = self.known.lock();
        known.config = config;
        known.providers.clear();
        known.models.clear();
    }
}

/// What a Responses request body asks for by way of a reasoning summary.
pub(crate) fn summary_ask(body: &Value) -> SummaryAsk {
    let Some(reasoning) = body.get("reasoning") else {
        return SummaryAsk::Nothing;
    };
    let mut asked = SummaryAsk::Nothing;
    for field in SUMMARY_FIELDS {
        match reasoning.get(field) {
            None | Some(Value::Null) => {}
            Some(Value::String(level)) if level == DEFAULT_SUMMARY => {
                if asked == SummaryAsk::Nothing {
                    asked = SummaryAsk::Auto;
                }
            }
            Some(_) => asked = SummaryAsk::Chosen,
        }
    }
    asked
}

/// Removes the request for a reasoning summary from a Responses request
/// body, and the `reasoning` object with it when nothing else is in it.
/// Everything else — the effort above all — stays as it is.
pub(crate) fn strip_summary(body: &mut Value) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    let Some(reasoning) = root.get_mut("reasoning").and_then(Value::as_object_mut) else {
        return;
    };
    let mut removed = false;
    for field in SUMMARY_FIELDS {
        removed |= reasoning.shift_remove(field).is_some();
    }
    if removed && reasoning.is_empty() {
        root.shift_remove("reasoning");
    }
}

/// Whether an upstream failure is the Responses API refusing to generate a
/// reasoning summary, and what the refusal is about.
///
/// Matched conservatively: the status must be `400`, and either the message
/// speaks of both reasoning summaries and verification
/// ([`Refusal::Organisation`]) or the error names the parameter
/// (`error.param == "reasoning.summary"`, [`Refusal::Parameter`]). Nothing
/// else is worth a second call: any other `400` is about what the client
/// sent.
///
/// An error that names *another* parameter is never the refusal, whatever
/// its message says: such a message may quote what the client wrote ("Tool
/// choice '…' not found"), and a client must not be able to word a request
/// so that the provider is taken for an unverified organisation.
pub(crate) fn summary_refusal(error: &UpstreamError) -> Option<Refusal> {
    if error.status != 400 {
        return None;
    }
    let message = error.info.message.to_lowercase();
    let about_verification = message.contains("reasoning summar") && message.contains("verif");
    let param = match error.body.as_deref().map(str::trim) {
        None => None,
        Some(body) => match serde_json::from_str::<Value>(body) {
            Ok(body) => {
                let detail = body.get("error").unwrap_or(&body);
                detail
                    .get("param")
                    .and_then(Value::as_str)
                    .map(|param| param.trim().to_string())
            }
            // An envelope cut short (only a prefix of a very large error
            // body is kept): which parameter it names cannot be told, and
            // an error that long is quoting the request.
            Err(_) if body.starts_with(['{', '[']) => return None,
            // Not an envelope at all: plain text.
            Err(_) => None,
        },
    };
    match param.as_deref() {
        Some("reasoning.summary" | "reasoning.generate_summary") if about_verification => {
            Some(Refusal::Organisation)
        }
        Some("reasoning.summary" | "reasoning.generate_summary") => Some(Refusal::Parameter),
        // No parameter named (a proxy that rewrites the envelope, a failure
        // reported inside a stream or a response body): the wording alone.
        None | Some("") => about_verification.then_some(Refusal::Organisation),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::local_failure;
    use serde_json::json;
    use switchyard_core::FailureClass;

    /// What OpenAI answers an unverified organisation with.
    const SAID: &str = "Your organization must be verified to generate reasoning summaries. \
                        Please go to: https://platform.openai.com/settings/organization/general \
                        and click on Verify Organization.";

    fn refused(status: u16, message: &str, body: Option<Value>) -> UpstreamError {
        let mut error = local_failure(FailureClass::Request, status, message);
        error.body = body.map(|body| body.to_string());
        error
    }

    #[test]
    fn the_refusal_is_recognised_by_its_wording_or_its_parameter() {
        let envelope = json!({"error": {
            "message": SAID, "type": "invalid_request_error",
            "param": "reasoning.summary", "code": "unsupported_value"
        }});
        assert_eq!(
            summary_refusal(&refused(400, SAID, Some(envelope))),
            Some(Refusal::Organisation)
        );
        // The wording alone (a proxy in front that rewrites the envelope),
        // in any case.
        assert_eq!(
            summary_refusal(&refused(400, SAID, None)),
            Some(Refusal::Organisation)
        );
        assert_eq!(
            summary_refusal(&refused(
                400,
                "ORGANIZATION MUST BE VERIFIED TO GENERATE REASONING SUMMARIES",
                Some(json!("not an envelope"))
            )),
            Some(Refusal::Organisation)
        );
        // An envelope whose parameter is null or empty: the wording decides.
        for param in [Value::Null, json!(""), json!("  ")] {
            let envelope = json!({"error": {"message": SAID, "param": param}});
            assert_eq!(
                summary_refusal(&refused(400, SAID, Some(envelope))),
                Some(Refusal::Organisation)
            );
        }
        // The deprecated spelling of the parameter.
        let legacy = json!({"error": {"message": SAID, "param": "reasoning.generate_summary"}});
        assert_eq!(
            summary_refusal(&refused(400, SAID, Some(legacy))),
            Some(Refusal::Organisation)
        );
        // The parameter alone, whatever the wording: about the model or the
        // value, not about the organisation.
        for message in [
            "Unsupported value.",
            "Unsupported parameter: 'reasoning.summary' is not supported with this model.",
            "Unsupported value: 'concise' is not supported with the 'o3' model. \
             Supported values are: 'auto' and 'detailed'.",
        ] {
            let envelope = json!({"error": {"message": message, "param": "reasoning.summary"}});
            assert_eq!(
                summary_refusal(&refused(400, message, Some(envelope))),
                Some(Refusal::Parameter),
                "{message}"
            );
        }
    }

    #[test]
    fn nothing_else_is_taken_for_the_refusal() {
        // Another status, even with the very same words.
        for status in [401, 403, 404, 429, 500, 0] {
            let envelope = json!({"error": {"message": SAID, "param": "reasoning.summary"}});
            assert_eq!(
                summary_refusal(&refused(status, SAID, Some(envelope))),
                None,
                "{status}"
            );
        }
        // Another parameter, another complaint.
        for (message, param) in [
            ("Unsupported value: 'xhigh'.", json!("reasoning.effort")),
            ("Invalid 'input': expected an array.", json!("input")),
            (
                "Your organization must be verified to stream this model.",
                json!("stream"),
            ),
            ("Reasoning summaries are great.", Value::Null),
            ("", Value::Null),
            // The refusal's own words, quoted back from what a client
            // wrote into another field.
            (
                "Tool choice 'organization must be verified to generate reasoning summaries' \
                 not found in 'tools' parameter.",
                json!("tool_choice"),
            ),
            (SAID, json!("input[0].content")),
        ] {
            let envelope = json!({"error": {"message": message, "param": param}});
            assert_eq!(
                summary_refusal(&refused(400, message, Some(envelope))),
                None,
                "{message}"
            );
        }
        // An envelope cut short names no parameter that could be read; a
        // plain-text body is no envelope and the wording decides.
        let mut cut = refused(400, SAID, None);
        cut.body = Some(format!(r#"{{"error": {{"message": "{SAID}", "par"#));
        assert_eq!(summary_refusal(&cut), None);
        let mut plain = refused(400, SAID, None);
        plain.body = Some(SAID.to_string());
        assert_eq!(summary_refusal(&plain), Some(Refusal::Organisation));
    }

    #[test]
    fn a_refusal_of_another_parameter_is_not_one_of_the_summary() {
        for (message, param) in [
            ("Unsupported value: 'xhigh'.", json!("reasoning.effort")),
            (
                "Unsupported parameter: 'reasoning.effort' is not supported with this model.",
                json!("reasoning.effort"),
            ),
        ] {
            let envelope = json!({"error": {"message": message, "param": param}});
            assert_eq!(
                summary_refusal(&refused(400, message, Some(envelope))),
                None,
                "{message}"
            );
        }
    }

    #[test]
    fn what_a_body_asks_for_is_read_off_both_spellings() {
        for (reasoning, asked) in [
            (json!({"summary": "auto"}), SummaryAsk::Auto),
            (json!({"generate_summary": "auto"}), SummaryAsk::Auto),
            (
                json!({"effort": "high", "summary": "auto", "generate_summary": "auto"}),
                SummaryAsk::Auto,
            ),
            (json!({"summary": "concise"}), SummaryAsk::Chosen),
            (json!({"summary": "detailed"}), SummaryAsk::Chosen),
            (
                json!({"summary": "auto", "generate_summary": "detailed"}),
                SummaryAsk::Chosen,
            ),
            (
                json!({"summary": "concise", "generate_summary": "auto"}),
                SummaryAsk::Chosen,
            ),
            // Not a level at all: whatever it is, it is not the default.
            (json!({"summary": true}), SummaryAsk::Chosen),
            (json!({"summary": "AUTO"}), SummaryAsk::Chosen),
            // An explicit "no summary" is not a request for one.
            (json!({"summary": null}), SummaryAsk::Nothing),
            (json!({"effort": "low"}), SummaryAsk::Nothing),
            (json!({}), SummaryAsk::Nothing),
            (json!("high"), SummaryAsk::Nothing),
        ] {
            let body = json!({"model": "m", "reasoning": reasoning, "input": []});
            assert_eq!(summary_ask(&body), asked, "{reasoning}");
        }
        assert_eq!(summary_ask(&json!({"model": "m"})), SummaryAsk::Nothing);
        assert_eq!(summary_ask(&json!([1, 2, 3])), SummaryAsk::Nothing);
    }

    #[test]
    fn stripping_removes_the_summary_and_nothing_else() {
        let mut body = json!({
            "model": "m",
            "reasoning": {"effort": "high", "summary": "auto", "generate_summary": "detailed"},
            "input": []
        });
        assert_eq!(summary_ask(&body), SummaryAsk::Chosen);
        strip_summary(&mut body);
        assert_eq!(summary_ask(&body), SummaryAsk::Nothing);
        assert_eq!(
            body,
            json!({"model": "m", "reasoning": {"effort": "high"}, "input": []})
        );

        // A `reasoning` object that only asked for the summary goes too.
        let mut body = json!({"model": "m", "reasoning": {"summary": "auto"}, "input": []});
        strip_summary(&mut body);
        assert_eq!(body, json!({"model": "m", "input": []}));

        // Bodies without the field, or of an unexpected shape, are left
        // alone.
        for untouched in [
            json!({"model": "m"}),
            json!({"reasoning": {}}),
            json!({"reasoning": {"effort": "low"}}),
            json!({"reasoning": "high"}),
            json!([1, 2, 3]),
        ] {
            let mut body = untouched.clone();
            assert_eq!(summary_ask(&body), SummaryAsk::Nothing, "{untouched}");
            strip_summary(&mut body);
            assert_eq!(body, untouched);
        }
    }

    #[test]
    fn refusals_are_remembered_per_provider_or_model_until_reset() {
        let config = Arc::new(Config::default());
        let refusals = SummaryRefusals::new(Arc::clone(&config));
        assert!(!refusals.covers(&config, "openai", "gpt-5"));

        // One model of a provider: its other models are still asked.
        assert!(refusals.remember(&config, "azure", Some("gpt-4.1")));
        assert!(
            !refusals.remember(&config, "azure", Some("gpt-4.1")),
            "known"
        );
        assert!(refusals.covers(&config, "azure", "gpt-4.1"));
        assert!(!refusals.covers(&config, "azure", "gpt-5"));
        assert!(!refusals.covers(&config, "openai", "gpt-4.1"));

        // A whole provider: every model of it, and of it only.
        assert!(refusals.remember(&config, "openai", None));
        assert!(!refusals.remember(&config, "openai", None), "already known");
        assert!(
            !refusals.remember(&config, "openai", Some("gpt-5")),
            "nothing new"
        );
        assert!(refusals.covers(&config, "openai", "gpt-5"));
        assert!(refusals.covers(&config, "openai", "o3"));
        assert!(!refusals.covers(&config, "azure", "gpt-5"));

        refusals.reset(Arc::clone(&config));
        assert!(!refusals.covers(&config, "openai", "gpt-5"));
        assert!(!refusals.covers(&config, "azure", "gpt-4.1"));
    }

    #[test]
    fn what_was_learnt_under_another_configuration_does_not_count() {
        let old = Arc::new(Config::default());
        let new = Arc::new(Config::default());
        let refusals = SummaryRefusals::new(Arc::clone(&old));
        assert!(refusals.remember(&old, "openai", None));
        assert!(refusals.remember(&old, "azure", Some("gpt-4.1")));
        // A request that already runs under the next configuration, before
        // the gateway applied it: it is told nothing and teaches nothing.
        assert!(!refusals.covers(&new, "openai", "gpt-5"));
        assert!(!refusals.remember(&new, "other", None));
        assert!(!refusals.covers(&old, "other", "m"));

        refusals.reset(Arc::clone(&new));
        // A request still in flight from before the change: what its key
        // was refused is not held against the new one.
        assert!(!refusals.remember(&old, "openai", None));
        assert!(!refusals.remember(&old, "openai", Some("gpt-5")));
        assert!(!refusals.covers(&new, "openai", "gpt-5"));
        assert!(!refusals.covers(&old, "openai", "gpt-5"));
        // An equal configuration is not the same one.
        assert_eq!(*old, *new);
        assert!(refusals.remember(&new, "openai", None));
        assert!(refusals.covers(&new, "openai", "gpt-5"));
    }

    #[test]
    fn the_models_remembered_per_provider_are_bounded() {
        let config = Arc::new(Config::default());
        let refusals = SummaryRefusals::new(Arc::clone(&config));
        for n in 0..MAX_MODELS_PER_PROVIDER {
            assert!(refusals.remember(&config, "p", Some(&format!("m-{n}"))));
        }
        assert!(!refusals.remember(&config, "p", Some("one-too-many")));
        assert!(!refusals.covers(&config, "p", "one-too-many"));
        assert!(refusals.covers(&config, "p", "m-0"));
        // Another provider has its own allowance, and the provider as a
        // whole can still be remembered.
        assert!(refusals.remember(&config, "q", Some("m")));
        assert!(refusals.remember(&config, "p", None));
        assert!(refusals.covers(&config, "p", "one-too-many"));
    }
}
