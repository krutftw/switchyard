//! Model metadata.

use crate::reasoning::{ModelThinking, ThinkingSupport};
use serde::{Deserialize, Serialize};

/// What the gateway knows about a model. Shown in model listings and used to
/// fit requests (reasoning settings, output limits) to the model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model id. In listings this is the client-facing name (alias and prefix
    /// applied); in the catalog it is the vendor's id.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Organisation that owns the model (`openai`, `anthropic`, `google`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    /// Release time, unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// Input context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Maximum output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Reasoning support. `None` means the model does not reason — but only
    /// when [`ModelInfo::known`] is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingSupport>,
    /// Whether this entry describes a model the gateway actually has metadata
    /// for. Entries synthesised from a bare model name (user-configured or
    /// discovered models missing from the catalog) are not `known`, and
    /// requests to them are passed through without capability-based fitting.
    #[serde(default)]
    pub known: bool,
}

impl ModelInfo {
    /// A metadata-less entry for a model the gateway knows only by name.
    pub fn bare(id: impl Into<String>) -> Self {
        ModelInfo {
            id: id.into(),
            ..ModelInfo::default()
        }
    }

    /// Reasoning capability in the form [`crate::reasoning::normalize_depth`]
    /// expects.
    pub fn thinking_caps(&self) -> ModelThinking<'_> {
        match (&self.thinking, self.known) {
            (Some(t), _) => ModelThinking::Supported(t),
            (None, true) => ModelThinking::Unsupported,
            (None, false) => ModelThinking::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoning::Effort;

    #[test]
    fn thinking_caps_distinguishes_unknown_from_unsupported() {
        let mut m = ModelInfo::bare("x");
        assert!(matches!(m.thinking_caps(), ModelThinking::Unknown));
        m.known = true;
        assert!(matches!(m.thinking_caps(), ModelThinking::Unsupported));
        m.thinking = Some(ThinkingSupport::levels(&[Effort::Low]));
        assert!(matches!(m.thinking_caps(), ModelThinking::Supported(_)));
    }
}
