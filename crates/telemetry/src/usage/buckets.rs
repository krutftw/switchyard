//! Time buckets: additive counters of the requests that finished in one
//! second, minute or hour, with the per-model / per-provider / per-key
//! breakdown.

use super::types::{GroupBy, Totals, usage_total_tokens};
use crate::record::RequestRecord;
use std::borrow::Cow;
use std::collections::HashMap;
use switchyard_core::util::truncate_chars;

/// Series name that collects whatever does not fit: names beyond the
/// per-bucket limit and, in a time series, everything outside the top
/// series.
pub const OTHER: &str = "other";

/// Distinct names kept per dimension in one bucket. Model names of failed
/// requests come straight from clients, so without a bound a client could
/// grow the statistics without limit by inventing names.
pub(crate) const MAX_GROUPS_PER_BUCKET: usize = 200;

/// Longest series name, in characters.
const MAX_NAME_CHARS: usize = 120;

/// Counters of one time bucket.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Bucket {
    pub(crate) totals: Totals,
    pub(crate) by_model: HashMap<String, Totals>,
    pub(crate) by_provider: HashMap<String, Totals>,
    pub(crate) by_key: HashMap<String, Totals>,
}

impl Bucket {
    pub(crate) fn add(&mut self, record: &RequestRecord) {
        self.totals.add_record(record);
        add_to_group(&mut self.by_model, record.model_name(), record);
        add_to_group(&mut self.by_provider, record.provider_name(), record);
        add_to_group(&mut self.by_key, record.key_name(), record);
    }

    /// The breakdown for a dimension; `None` for [`GroupBy::None`].
    pub(crate) fn groups(&self, group_by: GroupBy) -> Option<&HashMap<String, Totals>> {
        match group_by {
            GroupBy::None => None,
            GroupBy::Model => Some(&self.by_model),
            GroupBy::Provider => Some(&self.by_provider),
            GroupBy::Key => Some(&self.by_key),
        }
    }
}

fn series_name(name: &str) -> Cow<'_, str> {
    // Byte length is a cheap upper bound of the character count.
    if name.len() <= MAX_NAME_CHARS || name.chars().count() <= MAX_NAME_CHARS {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(truncate_chars(name, MAX_NAME_CHARS))
    }
}

fn add_to_group(map: &mut HashMap<String, Totals>, name: &str, record: &RequestRecord) {
    let name = series_name(name);
    if let Some(totals) = map.get_mut(name.as_ref()) {
        totals.add_record(record);
        return;
    }
    let key = if map.len() >= MAX_GROUPS_PER_BUCKET {
        OTHER.to_string()
    } else {
        name.into_owned()
    };
    map.entry(key).or_default().add_record(record);
}

/// Merges one bucket's breakdown into an accumulator.
pub(crate) fn merge_groups(into: &mut HashMap<String, Totals>, from: &HashMap<String, Totals>) {
    for (name, totals) in from {
        match into.get_mut(name) {
            Some(existing) => existing.merge(totals),
            None => {
                into.insert(name.clone(), *totals);
            }
        }
    }
}

/// Counters of one second, for the "last minute" rates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SecondSlot {
    pub(crate) requests: u64,
    pub(crate) errors: u64,
    pub(crate) tokens: u64,
}

impl SecondSlot {
    // Saturating throughout: token counts are whatever an upstream reported.
    pub(crate) fn add(&mut self, record: &RequestRecord) {
        self.requests = self.requests.saturating_add(1);
        if !record.ok {
            self.errors = self.errors.saturating_add(1);
        }
        self.tokens = self
            .tokens
            .saturating_add(usage_total_tokens(&record.usage));
    }

    pub(crate) fn merge(&mut self, other: &SecondSlot) {
        self.requests = self.requests.saturating_add(other.requests);
        self.errors = self.errors.saturating_add(other.errors);
        self.tokens = self.tokens.saturating_add(other.tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{ClientInfo, RecordBuilder, RequestStart};
    use pretty_assertions::assert_eq;
    use switchyard_core::protocol::Protocol;
    use switchyard_core::usage::Usage;

    fn record(
        model: &str,
        provider: Option<&str>,
        key: Option<&str>,
        status: u16,
    ) -> RequestRecord {
        let start = RequestStart::new(Protocol::OpenaiChat, "POST /v1/chat/completions", model, 0)
            .with_client(ClientInfo {
                key_name: key.map(str::to_string),
                ..ClientInfo::default()
            });
        let mut b = RecordBuilder::new(start);
        if let Some(provider) = provider {
            b.set_provider(provider);
        }
        b.set_usage(Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..Usage::default()
        });
        b.finish(status, 10)
    }

    #[test]
    fn bucket_breaks_down_by_every_dimension() {
        let mut bucket = Bucket::default();
        bucket.add(&record("gpt-5", Some("openai"), Some("laptop"), 200));
        bucket.add(&record("gpt-5", Some("openrouter"), None, 500));
        bucket.add(&record("sonnet", None, Some("laptop"), 200));
        assert_eq!(bucket.totals.requests, 3);
        assert_eq!(bucket.totals.errors, 1);
        assert_eq!(bucket.by_model["gpt-5"].requests, 2);
        assert_eq!(bucket.by_model["sonnet"].requests, 1);
        assert_eq!(bucket.by_provider["openai"].requests, 1);
        assert_eq!(bucket.by_provider["openrouter"].errors, 1);
        assert_eq!(bucket.by_provider["unknown"].requests, 1);
        assert_eq!(bucket.by_key["laptop"].requests, 2);
        assert_eq!(bucket.by_key["anonymous"].requests, 1);
        for groups in [&bucket.by_model, &bucket.by_provider, &bucket.by_key] {
            let mut sum = Totals::default();
            for totals in groups.values() {
                sum.merge(totals);
            }
            assert_eq!(sum, bucket.totals);
        }
        assert!(bucket.groups(GroupBy::None).is_none());
        assert_eq!(bucket.groups(GroupBy::Key).map(HashMap::len), Some(2));
    }

    #[test]
    fn names_beyond_the_limit_are_folded_into_other() {
        let mut bucket = Bucket::default();
        for i in 0..MAX_GROUPS_PER_BUCKET + 50 {
            bucket.add(&record(&format!("invented-{i}"), Some("p"), None, 404));
        }
        // An established name keeps counting under itself.
        bucket.add(&record("invented-0", Some("p"), None, 404));
        assert_eq!(bucket.by_model.len(), MAX_GROUPS_PER_BUCKET + 1);
        assert_eq!(bucket.by_model[OTHER].requests, 50);
        assert_eq!(bucket.by_model["invented-0"].requests, 2);
        let sum: u64 = bucket.by_model.values().map(|t| t.requests).sum();
        assert_eq!(sum, bucket.totals.requests);
    }

    #[test]
    fn very_long_names_are_truncated() {
        let mut bucket = Bucket::default();
        let long = "m".repeat(5_000);
        bucket.add(&record(&long, None, None, 200));
        let name = bucket.by_model.keys().next().unwrap();
        assert_eq!(name.chars().count(), MAX_NAME_CHARS + 1);
        assert!(name.ends_with('…'));
    }

    #[test]
    fn merge_groups_adds_and_inserts() {
        let mut a = Bucket::default();
        a.add(&record("x", None, None, 200));
        let mut b = Bucket::default();
        b.add(&record("x", None, None, 200));
        b.add(&record("y", None, None, 200));
        let mut acc = HashMap::new();
        merge_groups(&mut acc, &a.by_model);
        merge_groups(&mut acc, &b.by_model);
        assert_eq!(acc["x"].requests, 2);
        assert_eq!(acc["y"].requests, 1);
    }

    #[test]
    fn counters_saturate_instead_of_overflowing() {
        let mut huge = record("x", Some("p"), None, 200);
        huge.usage = Usage {
            input_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: 7,
            output_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };
        huge.duration_ms = u64::MAX;
        huge.ttfb_ms = Some(u64::MAX);
        let mut bucket = Bucket::default();
        bucket.add(&huge);
        bucket.add(&huge);
        assert_eq!(bucket.totals.requests, 2);
        assert_eq!(bucket.totals.input_tokens, u64::MAX);
        assert_eq!(bucket.totals.cache_write_tokens, 14);
        assert_eq!(bucket.totals.duration_ms_sum, u64::MAX);
        assert_eq!(bucket.totals.ttfb_ms_sum, u64::MAX);
        assert_eq!(bucket.totals.total_tokens(), u64::MAX);
        assert_eq!(bucket.by_model["x"].output_tokens, u64::MAX);

        let mut slot = SecondSlot::default();
        slot.add(&huge);
        slot.add(&huge);
        let mut sum = slot;
        sum.merge(&slot);
        assert_eq!((sum.requests, sum.tokens), (4, u64::MAX));
    }

    #[test]
    fn second_slots_count_tokens() {
        let mut slot = SecondSlot::default();
        slot.add(&record("x", None, None, 200));
        slot.add(&record("x", None, None, 429));
        let mut sum = SecondSlot::default();
        sum.merge(&slot);
        sum.merge(&slot);
        assert_eq!(
            sum,
            SecondSlot {
                requests: 4,
                errors: 2,
                tokens: 28
            }
        );
    }
}
