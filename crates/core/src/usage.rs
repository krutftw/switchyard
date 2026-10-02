//! Token accounting shared by every protocol.

use serde::{Deserialize, Serialize};

/// Token usage of one request, normalised across vendors.
///
/// The four input/output buckets are **disjoint**:
///
/// * [`input_tokens`](Usage::input_tokens): prompt tokens billed at the full
///   input rate, i.e. excluding anything read from or written to a prompt cache;
/// * [`cache_read_tokens`](Usage::cache_read_tokens): prompt tokens served from
///   cache;
/// * [`cache_write_tokens`](Usage::cache_write_tokens): prompt tokens written
///   to cache;
/// * [`output_tokens`](Usage::output_tokens): everything the model generated,
///   **including** reasoning tokens.
///
/// [`reasoning_tokens`](Usage::reasoning_tokens) is a detail of
/// `output_tokens`, not an extra bucket.
///
/// Vendor conventions differ and codecs convert at the boundary: OpenAI and
/// Gemini report a prompt total that *includes* cached tokens, Anthropic
/// reports them separately; Gemini reports thought tokens *outside*
/// `candidatesTokenCount`, OpenAI and Anthropic inside the output count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    /// All prompt-side tokens: uncached + cache reads + cache writes.
    pub const fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Prompt tokens plus output tokens.
    pub const fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output_tokens)
    }

    /// Output tokens that were not reasoning.
    pub const fn visible_output_tokens(&self) -> u64 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }

    pub const fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_tokens == 0
    }

    /// Folds a later usage snapshot into this one. Streams report usage in
    /// pieces (Anthropic sends input counts at `message_start` and output
    /// counts at `message_delta`; OpenAI sends everything in the last chunk),
    /// and every piece is a running total, so a non-zero field in `newer`
    /// replaces the stored value and a zero field leaves it alone.
    pub fn merge(&mut self, newer: &Usage) {
        if newer.input_tokens > 0 {
            self.input_tokens = newer.input_tokens;
        }
        if newer.cache_read_tokens > 0 {
            self.cache_read_tokens = newer.cache_read_tokens;
        }
        if newer.cache_write_tokens > 0 {
            self.cache_write_tokens = newer.cache_write_tokens;
        }
        if newer.output_tokens > 0 {
            self.output_tokens = newer.output_tokens;
        }
        if newer.reasoning_tokens > 0 {
            self.reasoning_tokens = newer.reasoning_tokens;
        }
    }

    /// Builds usage from an "inclusive" prompt total (OpenAI / Gemini style,
    /// where `prompt_total` already contains the cached tokens).
    pub fn from_inclusive(
        prompt_total: u64,
        cache_read: u64,
        cache_write: u64,
        output_total: u64,
        reasoning: u64,
    ) -> Usage {
        Usage {
            input_tokens: prompt_total.saturating_sub(cache_read.saturating_add(cache_write)),
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            output_tokens: output_total,
            reasoning_tokens: reasoning.min(output_total),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals() {
        let u = Usage {
            input_tokens: 10,
            cache_read_tokens: 90,
            cache_write_tokens: 5,
            output_tokens: 40,
            reasoning_tokens: 15,
        };
        assert_eq!(u.prompt_tokens(), 105);
        assert_eq!(u.total_tokens(), 145);
        assert_eq!(u.visible_output_tokens(), 25);
    }

    #[test]
    fn merge_keeps_earlier_fields_when_later_is_zero() {
        let mut u = Usage {
            input_tokens: 100,
            cache_read_tokens: 20,
            ..Usage::default()
        };
        u.merge(&Usage {
            output_tokens: 7,
            ..Usage::default()
        });
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cache_read_tokens, 20);
        assert_eq!(u.output_tokens, 7);
        u.merge(&Usage {
            output_tokens: 9,
            ..Usage::default()
        });
        assert_eq!(u.output_tokens, 9);
    }

    #[test]
    fn inclusive_conversion() {
        let u = Usage::from_inclusive(100, 60, 0, 30, 50);
        assert_eq!(u.input_tokens, 40);
        assert_eq!(u.cache_read_tokens, 60);
        assert_eq!(u.reasoning_tokens, 30);
        let u = Usage::from_inclusive(10, 60, 0, 0, 0);
        assert_eq!(u.input_tokens, 0);
    }
}
