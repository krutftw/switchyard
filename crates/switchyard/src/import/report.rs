//! The "not imported" report: whatever the importer did not read, by key
//! path, with the reason.

use super::Importer;
use super::values::is_zero;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};
use switchyard_core::util::mask_secret;

/// Key paths listed per line of the report before "and N more".
const PATHS_PER_LINE: usize = 6;
/// Longest key shown as written in a path.
const LONGEST_SEGMENT_SHOWN: usize = 40;

impl Importer<'_> {
    /// One line per reason, naming the key paths that were not read.
    pub(super) fn leftover_report(&self) -> Vec<String> {
        let mut paths = Vec::new();
        self.walk_map(self.root, "", &mut paths);
        // Sets, in first-seen order: a list entry repeats its paths.
        let mut by_reason: IndexMap<&'static str, IndexSet<String>> = IndexMap::new();
        for path in paths {
            let reason = explain(&path);
            by_reason
                .entry(reason)
                .or_default()
                .insert(shown_path(&path, reason));
        }
        by_reason
            .into_iter()
            .map(|(reason, paths)| {
                let shown: Vec<&str> = paths
                    .iter()
                    .take(PATHS_PER_LINE)
                    .map(String::as_str)
                    .collect();
                let more = paths.len().saturating_sub(PATHS_PER_LINE);
                let mut line = format!("{reason}: {}", shown.join(", "));
                if more > 0 {
                    line.push_str(&format!(", and {more} more"));
                }
                line
            })
            .collect()
    }

    /// Collects the normalised paths of values that were not read, skipping
    /// the ones that say nothing (`false`, `0`, empty).
    fn walk_map(&self, map: &Map<String, Value>, path: &str, out: &mut Vec<String>) {
        for (key, child) in map {
            let child_path = if path.is_empty() {
                segment(key)
            } else {
                format!("{path}.{}", segment(key))
            };
            if self.consumed.contains(&child_path) {
                continue;
            }
            self.walk(child, child_path, out);
        }
    }

    fn walk(&self, value: &Value, path: String, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => self.walk_map(map, &path, out),
            Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
                let item_path = format!("{path}[]");
                if self.consumed.contains(&item_path) {
                    return;
                }
                for item in items {
                    self.walk(item, item_path.clone(), out);
                }
            }
            other => {
                if !is_zero(other) && !path.is_empty() {
                    out.push(path);
                }
            }
        }
    }
}

/// A key as a path segment: long ones are masked (a secret pasted where a
/// key belongs must not be echoed).
pub(super) fn segment(key: &str) -> String {
    if key.chars().count() > LONGEST_SEGMENT_SHOWN {
        mask_secret(key)
    } else {
        key.to_string()
    }
}

const IMPERSONATION: &str = "client impersonation is deliberately not supported (cloaking, \
     fingerprint profiles, request signing, client-specific tuning)";
const OAUTH: &str = "OAuth logins and credential files (auth-dir) are not supported, only API \
     keys are imported; a Vertex service-account key can be added by hand as \
     [[providers.credentials]] service_account_file";
const ERROR_RULES: &str =
    "request-scoped error rules have no equivalent (upstream errors are classified by the gateway)";
const PER_CREDENTIAL: &str = "per-credential retry and cooldown overrides have no equivalent \
     (see [routing] and [routing.cooldown])";
const PLUGINS: &str = "plugins are not supported";
const AMP: &str = "the Amp integration is not supported";
const QUOTA: &str = "quota-exceeded switches have no equivalent";
const MULTIMEDIA: &str = "multimedia settings have no equivalent";
const OTHER_FAMILIES: &str = "credentials of provider families Switchyard has no native support \
     for; an OpenAI-compatible endpoint can be added by hand as an openai-compat provider";
const UPSTREAM_WEBSOCKET: &str = "relaying to an upstream's Responses WebSocket is not supported \
     (clients can still connect over WebSocket; the upstream is reached over HTTP streaming)";
const MODEL_OPTIONS: &str = "per-model options without an equivalent";
const NO_EQUIVALENT: &str = "settings without an equivalent";
const UNKNOWN: &str = "settings this importer does not know";

/// Why a setting at `path` (normalised, list positions as `[]`) is not
/// imported.
fn explain(path: &str) -> &'static str {
    let segments: Vec<&str> = path
        .split('.')
        .map(|segment| segment.trim_end_matches("[]"))
        .collect();
    let first = segments.first().copied().unwrap_or("");
    let last = segments.last().copied().unwrap_or("");
    let has = |name: &str| segments.contains(&name);

    if first == "client"
        || [
            "cloak",
            "fingerprint-profile",
            "experimental-cch-signing",
            "disable-codex-cloaking",
            "alpha-search",
        ]
        .iter()
        .any(|name| has(name))
    {
        return IMPERSONATION;
    }
    if matches!(
        first,
        "oauth"
            | "auth-dir"
            | "oauth-model-alias"
            | "oauth-excluded-models"
            | "oauth-settings"
            | "oauth-request-scoped-errors"
            | "ws-auth"
            | "auth"
            | "credentials"
    ) {
        return OAUTH;
    }
    if has("request-scoped-errors") {
        return ERROR_RULES;
    }
    if path.contains("[]") && matches!(last, "disable-cooling" | "request-retry") {
        return PER_CREDENTIAL;
    }
    if first == "plugins" {
        return PLUGINS;
    }
    if first.starts_with("amp") {
        return AMP;
    }
    if first == "quota-exceeded" {
        return QUOTA;
    }
    if first == "multimedia" {
        return MULTIMEDIA;
    }
    if first == "api-keys" && segments.len() > 1 {
        let family = segments[1];
        if !matches!(
            family,
            "gemini" | "claude" | "codex" | "vertex" | "openai-compatibility"
        ) {
            return OTHER_FAMILIES;
        }
    }
    if path.contains("[]") && last == "websockets" {
        return UPSTREAM_WEBSOCKET;
    }
    if has("models")
        && matches!(
            last,
            "force-mapping"
                | "is-compat"
                | "image"
                | "input-modalities"
                | "output-modalities"
                | "use-max-completion-tokens"
                | "support-configuration-update"
        )
    {
        return MODEL_OPTIONS;
    }
    if matches!(
        last,
        "trusted-proxies"
            | "commercial-mode"
            | "nonstream-keepalive-interval"
            | "save-cooldown-status"
            | "max-retry-credentials"
            | "session-affinity-subagents"
            | "error-logs-max-files"
            | "redis-usage-queue-retention-seconds"
            | "disable-auto-update-panel"
            | "panel-github-repository"
            | "support-prompt-cache-key"
            | "rebuild-mid-system-message"
            | "generative-language-api-key"
    ) || has("discovery")
        || has("pprof")
        || (matches!(first, "management" | "remote-management") && last == "base-url")
    {
        return NO_EQUIVALENT;
    }
    UNKNOWN
}

/// The path as listed in the report. Whole sections that are out of scope
/// are named once instead of leaf by leaf.
fn shown_path(path: &str, reason: &'static str) -> String {
    let keep = if [PLUGINS, AMP, QUOTA, MULTIMEDIA].contains(&reason)
        || (reason == IMPERSONATION && path.starts_with("client."))
    {
        1
    } else if reason == OTHER_FAMILIES || reason == OAUTH {
        2
    } else if reason == ERROR_RULES {
        // Up to the rule list itself.
        path.split('.')
            .position(|segment| segment.trim_end_matches("[]") == "request-scoped-errors")
            .map_or(usize::MAX, |index| index + 1)
    } else {
        return path.to_string();
    };
    let kept: Vec<&str> = path.split('.').take(keep).collect();
    kept.join(".").trim_end_matches("[]").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanations() {
        assert_eq!(explain("plugins.configs.x.y"), PLUGINS);
        assert_eq!(explain("ampcode.upstream-url"), AMP);
        assert_eq!(explain("amp-upstream-url"), AMP);
        assert_eq!(explain("auth-dir"), OAUTH);
        assert_eq!(explain("oauth.providers.codex.x"), OAUTH);
        assert_eq!(explain("claude-api-key[].cloak.mode"), IMPERSONATION);
        assert_eq!(explain("client.codex.identity-confuse"), IMPERSONATION);
        assert_eq!(
            explain("api-keys.gemini[].request-scoped-errors[].status"),
            ERROR_RULES
        );
        assert_eq!(
            explain("api-keys.gemini[].keys[].request-retry"),
            PER_CREDENTIAL
        );
        assert_eq!(explain("codex-api-key[].websockets"), UPSTREAM_WEBSOCKET);
        assert_eq!(
            explain("api-keys.codex[].keys[].websockets"),
            UPSTREAM_WEBSOCKET
        );
        assert_eq!(explain("api-keys.xai[].keys[].api-key"), OTHER_FAMILIES);
        assert_eq!(
            explain("openai-compatibility[].models[].image"),
            MODEL_OPTIONS
        );
        assert_eq!(explain("server.trusted-proxies"), NO_EQUIVALENT);
        assert_eq!(explain("pprof.enable"), NO_EQUIVALENT);
        assert_eq!(explain("something-else"), UNKNOWN);
    }
}
