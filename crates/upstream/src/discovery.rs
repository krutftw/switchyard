//! Model discovery: asking a provider which models a credential can use.
//!
//! | kind | endpoint | paging |
//! |---|---|---|
//! | `openai`, `openai-compat` | `GET {root}/models` | none |
//! | `anthropic` | `GET {base}/v1/models?limit=1000` | `has_more` + `last_id` → `after_id` |
//! | `gemini` | `GET {base}/v1beta/models?pageSize=1000` | `nextPageToken` → `pageToken` |
//! | `vertex` | `GET {host}/v1beta1/publishers/google/models` | `nextPageToken` → `pageToken` |
//! | `mock` | none — the built-in list | |
//!
//! The returned [`ModelInfo`]s carry whatever metadata the listing offers
//! (display name, token limits, creation time) and always have
//! `known == false`: the scheduler merges them with its catalog, which is
//! the authority on capabilities such as reasoning support.
//!
//! Vertex AI has no listing for partner models: Claude models served through
//! a `vertex` provider must be configured by hand.

use crate::client::UpstreamClient;
use crate::request::{build_request, list_models_page_url};
use crate::target::{Operation, Target, Timeouts};
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use switchyard_core::config::ProviderKind;
use switchyard_core::util::{str_field, u64_field};
use switchyard_core::{ModelInfo, UpstreamError};

/// Pages fetched before giving up on a listing that never ends.
const MAX_PAGES: usize = 50;
/// Bounds decoded listing data across all pages of one discovery operation.
const MAX_DISCOVERY_BYTES: usize = 16 * 1024 * 1024;
/// A single page is expected to contain at most 1,000 short model records.
const MAX_DISCOVERY_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// Time limits of a discovery call when the caller states none.
const DISCOVERY_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(30),
    request: Duration::from_secs(60),
};

/// One page of a model listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelPage {
    pub models: Vec<ModelInfo>,
    /// Query parameter (name, value) that fetches the next page.
    pub next: Option<(&'static str, String)>,
}

fn positive(value: &Value, key: &str) -> Option<u64> {
    u64_field(value, key).filter(|n| *n > 0)
}

fn text(value: &Value, key: &str) -> Option<String> {
    str_field(value, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn rfc3339_to_unix(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|t| t.timestamp())
}

/// Parses an OpenAI-style listing: `{"object":"list","data":[{"id",…}]}`.
///
/// Compatible servers vary: a bare array, a `models` key, `name` instead of
/// `id`. Aggregators add metadata — `context_length` / `context_window`,
/// `max_completion_tokens`, `top_provider.max_completion_tokens`, a human
/// readable `name` — which is kept when present.
pub fn parse_openai_models(body: &Value) -> ModelPage {
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.get("models").and_then(Value::as_array))
        .or_else(|| body.as_array());
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in entries.into_iter().flatten() {
        let id = match entry {
            // Some servers list plain names.
            Value::String(s) => Some(s.trim().to_string()),
            _ => text(entry, "id").or_else(|| text(entry, "name")),
        };
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut info = ModelInfo::bare(id.clone());
        info.display_name = text(entry, "display_name")
            .or_else(|| text(entry, "name"))
            .filter(|name| *name != id);
        info.description = text(entry, "description");
        info.owned_by = text(entry, "owned_by");
        info.created = entry
            .get("created")
            .and_then(Value::as_i64)
            .filter(|t| *t > 0);
        info.context_window = positive(entry, "context_length")
            .or_else(|| positive(entry, "context_window"))
            .or_else(|| positive(entry, "max_context_length"));
        info.max_output_tokens = positive(entry, "max_completion_tokens")
            .or_else(|| positive(entry, "max_output_tokens"))
            .or_else(|| {
                entry
                    .get("top_provider")
                    .and_then(|p| positive(p, "max_completion_tokens"))
            });
        models.push(info);
    }
    ModelPage { models, next: None }
}

/// Parses an Anthropic listing:
/// `{"data":[{"type":"model","id","display_name","created_at",…}],"has_more","last_id"}`.
pub fn parse_anthropic_models(body: &Value) -> ModelPage {
    let mut models = Vec::new();
    let entries = body.get("data").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let Some(id) = text(entry, "id") else {
            continue;
        };
        let mut info = ModelInfo::bare(id);
        info.display_name = text(entry, "display_name");
        info.owned_by = Some("anthropic".to_string());
        info.created = str_field(entry, "created_at").and_then(rfc3339_to_unix);
        // Both limits may be null or zero for models that do not state them.
        info.context_window = positive(entry, "max_input_tokens");
        info.max_output_tokens = positive(entry, "max_tokens");
        models.push(info);
    }
    let has_more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cursor = text(body, "last_id").or_else(|| models.last().map(|m| m.id.clone()));
    let next = match (has_more, cursor) {
        (true, Some(cursor)) => Some(("after_id", cursor)),
        _ => None,
    };
    ModelPage { models, next }
}

/// Parses a Gemini API listing:
/// `{"models":[{"name":"models/x","displayName","inputTokenLimit","outputTokenLimit","supportedGenerationMethods"}],"nextPageToken"}`.
///
/// Only models that support `generateContent` are kept (the listing also
/// contains embedding, image and speech models). An entry without a
/// `supportedGenerationMethods` field is kept: some compatible servers omit
/// it.
pub fn parse_gemini_models(body: &Value) -> ModelPage {
    let mut models = Vec::new();
    let entries = body.get("models").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let Some(name) = text(entry, "name") else {
            continue;
        };
        let generates = entry
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .is_none_or(|methods| {
                methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            });
        if !generates {
            continue;
        }
        let id = name.strip_prefix("models/").unwrap_or(&name).to_string();
        if id.is_empty() {
            continue;
        }
        let mut info = ModelInfo::bare(id);
        info.display_name = text(entry, "displayName");
        info.description = text(entry, "description");
        info.owned_by = Some("google".to_string());
        info.context_window = positive(entry, "inputTokenLimit");
        info.max_output_tokens = positive(entry, "outputTokenLimit");
        models.push(info);
    }
    ModelPage {
        models,
        next: text(body, "nextPageToken").map(|token| ("pageToken", token)),
    }
}

/// Parses a Vertex AI publisher-model listing:
/// `{"publisherModels":[{"name":"publishers/google/models/x",…}],"nextPageToken"}`.
///
/// The catalogue mixes every kind of Google model and does not say which
/// ones speak `generateContent`; only the Gemini family is kept.
pub fn parse_vertex_models(body: &Value) -> ModelPage {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    let entries = body.get("publisherModels").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let Some(name) = text(entry, "name") else {
            continue;
        };
        let id = name.rsplit('/').next().unwrap_or(&name).to_string();
        if !id.to_ascii_lowercase().starts_with("gemini") || !seen.insert(id.clone()) {
            continue;
        }
        let mut info = ModelInfo::bare(id);
        info.owned_by = Some("google".to_string());
        models.push(info);
    }
    ModelPage {
        models,
        next: text(body, "nextPageToken").map(|token| ("pageToken", token)),
    }
}

/// Parses one listing page in the dialect of `kind`.
pub fn parse_models(kind: ProviderKind, body: &Value) -> ModelPage {
    let mut page = match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompat => parse_openai_models(body),
        ProviderKind::Anthropic => parse_anthropic_models(body),
        ProviderKind::Gemini => parse_gemini_models(body),
        ProviderKind::Vertex => parse_vertex_models(body),
        ProviderKind::Mock => ModelPage::default(),
    };
    page.models.retain(|model| model.id.len() <= 1024);
    page
}

impl UpstreamClient {
    /// Lists the models `target`'s credential can use, following pagination.
    /// `target.model` is ignored.
    pub async fn list_models(&self, target: &Target) -> Result<Vec<ModelInfo>, UpstreamError> {
        self.list_models_with(target, DISCOVERY_TIMEOUTS).await
    }

    /// [`UpstreamClient::list_models`] with explicit time limits (applied to
    /// each page).
    pub async fn list_models_with(
        &self,
        target: &Target,
        timeouts: Timeouts,
    ) -> Result<Vec<ModelInfo>, UpstreamError> {
        if target.kind == ProviderKind::Mock {
            return Ok(crate::mock::mock_models());
        }
        let no_headers = HeaderMap::new();
        let mut models: Vec<ModelInfo> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut cursor: Option<(&'static str, String)> = None;
        let mut remaining_bytes = MAX_DISCOVERY_BYTES;

        for _ in 0..MAX_PAGES {
            let mut built = build_request(target, &Operation::ListModels, b"", &no_headers)?;
            built.url = list_models_page_url(
                target,
                cursor.as_ref().map(|(name, value)| (*name, value.as_str())),
            )?;
            let response = self
                .send_built_unbuffered(target, built, Bytes::new(), timeouts)
                .await?;
            let status = response.status;
            let bytes = response
                .body
                .collect_limited(remaining_bytes.min(MAX_DISCOVERY_PAGE_BYTES))
                .await?;
            remaining_bytes -= bytes.len();
            let json: Value = serde_json::from_slice(&bytes).map_err(|_| {
                let mut error = UpstreamError::transport(format!(
                    "read: provider `{}` answered its model listing (HTTP {status}) with something that is not JSON",
                    target.provider
                ));
                error.body = Some(switchyard_core::util::truncate_chars(
                    &String::from_utf8_lossy(&bytes),
                    2000,
                ));
                error
            })?;

            let page = parse_models(target.kind, &json);
            for model in page.models {
                if seen_ids.insert(model.id.clone()) {
                    models.push(model);
                }
            }
            match page.next {
                // A cursor seen before means the upstream is looping.
                Some((name, value)) if seen_cursors.insert(value.clone()) => {
                    cursor = Some((name, value));
                }
                _ => return Ok(models),
            }
        }
        tracing::warn!(
            provider = %target.provider,
            pages = MAX_PAGES,
            "model listing did not end; using the models found so far"
        );
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn openai_listing() {
        let page = parse_openai_models(&json!({
            "object": "list",
            "data": [
                {"id": "gpt-6-astra", "object": "model", "created": 1767225600, "owned_by": "openai"},
                {"id": "gpt-5.6-terra", "object": "model", "created": 1759000000, "owned_by": "system"},
                {"id": "gpt-6-astra", "object": "model", "created": 1, "owned_by": "dup"},
                {"object": "model"},
                {"id": "  "}
            ]
        }));
        assert_eq!(page.next, None);
        assert_eq!(page.models.len(), 2);
        let first = &page.models[0];
        assert_eq!(first.id, "gpt-6-astra");
        assert_eq!(first.owned_by.as_deref(), Some("openai"));
        assert_eq!(first.created, Some(1767225600));
        assert_eq!(first.display_name, None);
        assert_eq!(first.context_window, None);
        assert!(!first.known);
        assert_eq!(first.thinking, None);
    }

    #[test]
    fn openai_compatible_variations() {
        // OpenRouter-style metadata.
        let page = parse_openai_models(&json!({"data": [{
            "id": "anthropic/claude-opus-5",
            "name": "Anthropic: Claude Opus 5",
            "description": "Flagship model.",
            "created": 1753315200,
            "context_length": 1000000,
            "top_provider": {"context_length": 1000000, "max_completion_tokens": 128000}
        }]}));
        let m = &page.models[0];
        assert_eq!(m.id, "anthropic/claude-opus-5");
        assert_eq!(m.display_name.as_deref(), Some("Anthropic: Claude Opus 5"));
        assert_eq!(m.description.as_deref(), Some("Flagship model."));
        assert_eq!(m.context_window, Some(1_000_000));
        assert_eq!(m.max_output_tokens, Some(128_000));

        // Groq-style fields.
        let page = parse_openai_models(&json!({"data": [{
            "id": "llama-4-70b", "owned_by": "Meta", "context_window": 131072, "max_completion_tokens": 32768
        }]}));
        assert_eq!(page.models[0].context_window, Some(131_072));
        assert_eq!(page.models[0].max_output_tokens, Some(32_768));

        // A bare array, a `models` key with `name`, plain strings.
        assert_eq!(
            parse_openai_models(&json!([{"id": "a"}, {"id": "b"}]))
                .models
                .len(),
            2
        );
        let page = parse_openai_models(&json!({"models": [{"name": "llama3:8b"}, "phi4"]}));
        let ids: Vec<&str> = page.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["llama3:8b", "phi4"]);
        assert_eq!(page.models[0].display_name, None);

        // Garbage yields nothing instead of failing.
        assert!(
            parse_openai_models(&json!({"error": "nope"}))
                .models
                .is_empty()
        );
        assert!(parse_openai_models(&json!("text")).models.is_empty());
        assert!(
            parse_openai_models(&json!({"data": "not a list"}))
                .models
                .is_empty()
        );
    }

    #[test]
    fn anthropic_listing_and_cursor() {
        let page = parse_anthropic_models(&json!({
            "data": [
                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5",
                 "created_at": "2026-07-24T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000,
                 "capabilities": {"thinking": {"supported": true}}},
                {"type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5",
                 "created_at": "2025-10-01T00:00:00Z", "max_input_tokens": null, "max_tokens": 0, "capabilities": null}
            ],
            "first_id": "claude-opus-5",
            "last_id": "claude-haiku-4-5-20251001",
            "has_more": true
        }));
        assert_eq!(
            page.next,
            Some(("after_id", "claude-haiku-4-5-20251001".to_string()))
        );
        let opus = &page.models[0];
        assert_eq!(opus.id, "claude-opus-5");
        assert_eq!(opus.display_name.as_deref(), Some("Claude Opus 5"));
        assert_eq!(opus.owned_by.as_deref(), Some("anthropic"));
        assert_eq!(opus.created, Some(1_784_851_200));
        assert_eq!(opus.context_window, Some(1_000_000));
        assert_eq!(opus.max_output_tokens, Some(128_000));
        // Capabilities are the catalog's business.
        assert_eq!(opus.thinking, None);
        assert!(!opus.known);
        let haiku = &page.models[1];
        assert_eq!(haiku.context_window, None);
        assert_eq!(haiku.max_output_tokens, None);

        // Last page.
        let page = parse_anthropic_models(
            &json!({"data": [{"id": "claude-3-haiku"}], "has_more": false, "last_id": "claude-3-haiku"}),
        );
        assert_eq!(page.next, None);
        // `has_more` without `last_id` falls back to the last entry.
        let page =
            parse_anthropic_models(&json!({"data": [{"id": "a"}, {"id": "b"}], "has_more": true}));
        assert_eq!(page.next, Some(("after_id", "b".to_string())));
        // An empty page never asks for more.
        assert_eq!(
            parse_anthropic_models(&json!({"data": [], "has_more": true})).next,
            None
        );
    }

    #[test]
    fn gemini_listing_keeps_generate_content_models() {
        let page = parse_gemini_models(&json!({
            "models": [
                {"name": "models/gemini-3.8-flash", "baseModelId": "gemini-3.8-flash", "version": "001",
                 "displayName": "Gemini 3.8 Flash", "description": "Fast and versatile.",
                 "inputTokenLimit": 1048576, "outputTokenLimit": 65536,
                 "supportedGenerationMethods": ["generateContent", "countTokens", "createCachedContent"],
                 "thinking": true},
                {"name": "models/gemini-embedding-001", "displayName": "Gemini Embedding",
                 "inputTokenLimit": 2048, "outputTokenLimit": 1,
                 "supportedGenerationMethods": ["embedContent", "countTextTokens"]},
                {"name": "models/imagen-4.0-generate-001", "supportedGenerationMethods": ["predict"]},
                {"name": "tunedModels/my-tuned", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/no-methods-field"},
                {"displayName": "nameless"}
            ],
            "nextPageToken": "page-2-token"
        }));
        let ids: Vec<&str> = page.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-3.8-flash",
                "tunedModels/my-tuned",
                "no-methods-field"
            ]
        );
        assert_eq!(page.next, Some(("pageToken", "page-2-token".to_string())));
        let flash = &page.models[0];
        assert_eq!(flash.display_name.as_deref(), Some("Gemini 3.8 Flash"));
        assert_eq!(flash.description.as_deref(), Some("Fast and versatile."));
        assert_eq!(flash.owned_by.as_deref(), Some("google"));
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(flash.max_output_tokens, Some(65_536));
        assert!(!flash.known);

        assert_eq!(parse_gemini_models(&json!({"models": []})).next, None);
        assert_eq!(parse_gemini_models(&json!({})), ModelPage::default());
    }

    #[test]
    fn vertex_listing_keeps_the_gemini_family() {
        let page = parse_vertex_models(&json!({
            "publisherModels": [
                {"name": "publishers/google/models/gemini-2.5-pro", "versionId": "default"},
                {"name": "publishers/google/models/gemini-2.5-pro", "versionId": "001"},
                {"name": "publishers/google/models/imagen-4.0-generate-001"},
                {"name": "publishers/google/models/text-embedding-005"},
                {"name": "publishers/google/models/gemini-3.8-flash"}
            ],
            "nextPageToken": "t2"
        }));
        let ids: Vec<&str> = page.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemini-2.5-pro", "gemini-3.8-flash"]);
        assert_eq!(page.next, Some(("pageToken", "t2".to_string())));
    }

    #[test]
    fn dispatch_by_kind() {
        let openai = json!({"data": [{"id": "m"}]});
        assert_eq!(parse_models(ProviderKind::Openai, &openai).models.len(), 1);
        assert_eq!(
            parse_models(ProviderKind::OpenaiCompat, &openai)
                .models
                .len(),
            1
        );
        assert_eq!(
            parse_models(ProviderKind::Anthropic, &openai).models.len(),
            1
        );
        assert_eq!(
            parse_models(
                ProviderKind::Gemini,
                &json!({"models": [{"name": "models/g"}]})
            )
            .models
            .len(),
            1
        );
        assert_eq!(
            parse_models(
                ProviderKind::Vertex,
                &json!({"publisherModels": [{"name": "publishers/google/models/gemini-x"}]})
            )
            .models
            .len(),
            1
        );
        assert!(parse_models(ProviderKind::Mock, &openai).models.is_empty());
    }
}
