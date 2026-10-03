//! Payload rules: the ones that can be expressed here are converted, the
//! rest is reported.

use super::Importer;
use super::report::segment;
use super::values::{is_simple_path, is_zero, string_list, text};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use switchyard_core::config::{PayloadRule, payload_path_issue};
use switchyard_core::{Config, Protocol};

impl Importer<'_> {
    /// `payload` is the section as the rest of the document is read:
    /// verbatim, so model names and paths are what the file says. `typed`
    /// is the same section with scalars typed, for the values the rules
    /// set, which may be numbers and booleans in any spelling.
    pub(super) fn payload(&mut self, payload: &Value, typed: Option<&Value>, config: &mut Config) {
        let Some(sections) = payload.as_object() else {
            return;
        };
        // The order they are applied in: default, default-raw, override,
        // override-raw, filter.
        for (section, raw, target) in [
            ("default", false, 0),
            ("default-raw", true, 0),
            ("override", false, 1),
            ("override-raw", true, 1),
            ("filter", false, 2),
        ] {
            let Some(rules) = sections.get(section).and_then(Value::as_array) else {
                continue;
            };
            let typed_rules = typed
                .and_then(|typed| typed.get(section))
                .and_then(Value::as_array);
            for (index, rule) in rules.iter().enumerate() {
                let location = format!("payload.{section}[{index}]");
                let typed_params = typed_rules
                    .and_then(|rules| rules.get(index))
                    .and_then(|rule| rule.get("params"))
                    .and_then(Value::as_object);
                let converted = self.payload_rule(rule, typed_params, raw, target == 2, &location);
                match target {
                    0 => config.payload.default.extend(converted),
                    1 => config.payload.overrides.extend(converted),
                    _ => config.payload.filter.extend(converted),
                }
            }
        }
        for name in sections.keys() {
            if !matches!(
                name.as_str(),
                "default" | "default-raw" | "override" | "override-raw" | "filter"
            ) {
                self.not_imported
                    .push(format!("payload.{}: unknown section", segment(name)));
            }
        }
    }

    /// Converts one rule; a rule whose model entries name different upstream
    /// protocols becomes one rule per protocol. `typed_params` are the
    /// rule's `params` with typed scalars.
    fn payload_rule(
        &mut self,
        rule: &Value,
        typed_params: Option<&Map<String, Value>>,
        raw: bool,
        filter: bool,
        location: &str,
    ) -> Vec<PayloadRule> {
        let Some(rule) = rule.as_object() else {
            self.not_imported
                .push(format!("{location}: not a rule; skipped"));
            return Vec::new();
        };

        // What to set or remove.
        let mut set: IndexMap<String, Value> = IndexMap::new();
        let mut remove: Vec<String> = Vec::new();
        let mut complex = 0usize;
        let mut unusable = 0usize;
        // Paths that can never name a field here (empty, spaces), which the
        // configuration refuses.
        let mut invalid = 0usize;
        if filter {
            for path in rule.get("params").map(string_list).unwrap_or_default() {
                if !is_simple_path(&path) {
                    complex += 1;
                } else if payload_path_issue(&path).is_some() {
                    invalid += 1;
                } else {
                    remove.push(path);
                }
            }
        } else if let Some(params) = rule.get("params").and_then(Value::as_object) {
            for (path, value) in params {
                if !is_simple_path(path) {
                    complex += 1;
                    continue;
                }
                if payload_path_issue(path).is_some() {
                    invalid += 1;
                    continue;
                }
                // What is set is a JSON value: `1.50` is the number 1.5
                // there, where a name would be the text.
                let value = typed_params
                    .and_then(|params| params.get(path))
                    .unwrap_or(value);
                let value = if raw {
                    match value {
                        // Raw rules carry JSON text.
                        Value::String(json) => match serde_json::from_str::<Value>(json) {
                            Ok(parsed) => parsed,
                            Err(_) => {
                                unusable += 1;
                                continue;
                            }
                        },
                        other => other.clone(),
                    }
                } else {
                    value.clone()
                };
                // TOML has no null, and no integers beyond 64 bits.
                if toml::Value::try_from(&value).is_err() {
                    unusable += 1;
                    continue;
                }
                set.insert(path.clone(), value);
            }
        }
        if complex > 0 {
            self.not_imported.push(format!(
                "{location}: {complex} of the paths use gjson queries or wildcards, which \
                 payload rules here do not have"
            ));
        }
        if unusable > 0 {
            self.not_imported.push(format!(
                "{location}: {unusable} of the values are not valid JSON or cannot be written \
                 as TOML (null)"
            ));
        }
        if invalid > 0 {
            self.not_imported.push(format!(
                "{location}: {invalid} of the paths can never name a field of a request body \
                 (they are empty or hold spaces)"
            ));
        }
        if set.is_empty() && remove.is_empty() {
            if complex == 0 && unusable == 0 && invalid == 0 {
                self.not_imported
                    .push(format!("{location}: no params; skipped"));
            }
            return Vec::new();
        }

        // Which models, grouped by upstream protocol.
        let mut groups: Vec<(Option<Protocol>, Vec<String>)> = Vec::new();
        let mut conditional = 0usize;
        let mut foreign = 0usize;
        for entry in rule
            .get("models")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let (name, protocol, has_conditions) = match entry {
                Value::Object(entry) => (
                    entry.get("name").and_then(text).unwrap_or_default(),
                    entry.get("protocol").and_then(text).unwrap_or_default(),
                    [
                        "from-protocol",
                        "headers",
                        "match",
                        "not-match",
                        "exist",
                        "not-exist",
                    ]
                    .iter()
                    .any(|key| entry.get(*key).is_some_and(|value| !is_zero(value))),
                ),
                // A bare model name.
                scalar => match text(scalar) {
                    Some(name) => (name, String::new(), false),
                    None => continue,
                },
            };
            if name.is_empty() {
                continue;
            }
            if has_conditions {
                conditional += 1;
                continue;
            }
            let protocol = match protocol.to_ascii_lowercase().as_str() {
                "" => None,
                "openai" => Some(Protocol::OpenaiChat),
                "codex" | "responses" | "openai-response" | "openai-responses" => {
                    Some(Protocol::OpenaiResponses)
                }
                "claude" => Some(Protocol::Anthropic),
                "gemini" => Some(Protocol::Gemini),
                _ => {
                    foreign += 1;
                    continue;
                }
            };
            match groups.iter_mut().find(|(p, _)| *p == protocol) {
                Some((_, names)) => {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                None => groups.push((protocol, vec![name])),
            }
        }
        if conditional > 0 {
            self.not_imported.push(format!(
                "{location}: {conditional} of the model entries have conditions \
                 (from-protocol, headers, match, not-match, exist, not-exist), which payload \
                 rules here do not have"
            ));
        }
        if foreign > 0 {
            self.not_imported.push(format!(
                "{location}: {foreign} of the model entries are for a protocol Switchyard does \
                 not speak"
            ));
        }
        if groups.is_empty() && conditional == 0 && foreign == 0 {
            self.not_imported
                .push(format!("{location}: no models; skipped"));
        }
        groups
            .into_iter()
            .map(|(protocol, models)| PayloadRule {
                models,
                protocol,
                provider: String::new(),
                set: set.clone(),
                remove: remove.clone(),
            })
            .collect()
    }
}
