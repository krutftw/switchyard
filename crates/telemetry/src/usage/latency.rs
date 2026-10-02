//! Latency histograms. One [`LatencySlot`] holds the requests that finished
//! in one minute (or one hour); a percentile query merges the slots of its
//! window, which is what makes the window slide.

use super::types::Latency;
use crate::record::RequestRecord;
use hdrhistogram::Histogram;

/// Significant digits kept by the histograms. Two digits keep a slot at a
/// few kilobytes while a percentile is off by at most one percent.
const SIGNIFICANT_DIGITS: u8 = 2;

/// Durations above this are recorded as this value: one stuck request must
/// not make every histogram allocate buckets up to days.
const MAX_TRACKED_MS: u64 = 6 * 3_600_000;

fn new_histogram() -> Histogram<u64> {
    // Auto-resizing: a slot only grows as far as the slowest request in it.
    Histogram::new(SIGNIFICANT_DIGITS)
        .expect("two significant digits is a valid histogram precision")
}

fn observe(histogram: &mut Histogram<u64>, ms: u64) {
    // `record` (not `saturating_record`) so the histogram grows to fit the
    // value; with the clamp above it the only possible error — a range too
    // large for the platform — cannot occur.
    let _ = histogram.record(ms.min(MAX_TRACKED_MS));
}

/// Duration and time-to-first-byte samples of one time slot.
#[derive(Clone, Debug)]
pub(crate) struct LatencySlot {
    duration: Histogram<u64>,
    ttfb: Histogram<u64>,
}

impl Default for LatencySlot {
    fn default() -> Self {
        LatencySlot {
            duration: new_histogram(),
            ttfb: new_histogram(),
        }
    }
}

impl LatencySlot {
    pub(crate) fn add(&mut self, record: &RequestRecord) {
        observe(&mut self.duration, record.duration_ms);
        if let Some(ttfb) = record.ttfb_ms {
            observe(&mut self.ttfb, ttfb);
        }
    }

    /// Folds another slot into this one.
    pub(crate) fn merge(&mut self, other: &LatencySlot) {
        // Adding can only fail when the target cannot grow to hold the
        // source's range; both are auto-resizing, so it cannot.
        let _ = self.duration.add(&other.duration);
        let _ = self.ttfb.add(&other.ttfb);
    }

    /// Percentiles of everything in the slot.
    pub(crate) fn percentiles(&self, window_ms: i64) -> Latency {
        let d = |q: f64| quantile(&self.duration, q);
        Latency {
            window_ms,
            p50: d(0.50),
            p90: d(0.90),
            p95: d(0.95),
            p99: d(0.99),
            ttfb_p50: quantile(&self.ttfb, 0.50),
            ttfb_p95: quantile(&self.ttfb, 0.95),
            samples: self.duration.len(),
            ttfb_samples: self.ttfb.len(),
        }
    }
}

fn quantile(histogram: &Histogram<u64>, q: f64) -> u64 {
    if histogram.is_empty() {
        0
    } else {
        // The lower edge of the histogram bucket: never above a duration
        // that was actually observed, and exact for round values.
        histogram.lowest_equivalent(histogram.value_at_quantile(q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RequestStart};
    use pretty_assertions::assert_eq;
    use switchyard_core::protocol::Protocol;

    fn record(duration_ms: i64, ttfb_ms: Option<i64>) -> RequestRecord {
        let mut b = RecordBuilder::new(RequestStart::new(
            Protocol::Gemini,
            "POST /v1beta/models/x:generateContent",
            "x",
            0,
        ));
        if let Some(ttfb) = ttfb_ms {
            b.mark_first_byte(ttfb);
        }
        b.finish(200, duration_ms)
    }

    /// Whether `actual` is within the histogram's precision of `expected`.
    fn close(actual: u64, expected: u64) -> bool {
        let tolerance = (expected / 50).max(1);
        actual.abs_diff(expected) <= tolerance
    }

    #[test]
    fn empty_slot_reports_zeros() {
        assert_eq!(
            LatencySlot::default().percentiles(60_000),
            Latency {
                window_ms: 60_000,
                ..Latency::default()
            }
        );
    }

    #[test]
    fn percentiles_of_a_uniform_distribution() {
        let mut slot = LatencySlot::default();
        for ms in 1..=1_000 {
            slot.add(&record(ms, Some(ms / 10)));
        }
        let p = slot.percentiles(3_600_000);
        assert_eq!(p.samples, 1_000);
        assert_eq!(p.ttfb_samples, 1_000);
        assert!(close(p.p50, 500), "p50 = {}", p.p50);
        assert!(close(p.p90, 900), "p90 = {}", p.p90);
        assert!(close(p.p95, 950), "p95 = {}", p.p95);
        assert!(close(p.p99, 990), "p99 = {}", p.p99);
        assert!(close(p.ttfb_p50, 50), "ttfb_p50 = {}", p.ttfb_p50);
        assert!(close(p.ttfb_p95, 95), "ttfb_p95 = {}", p.ttfb_p95);
        assert!(p.p50 <= p.p90 && p.p90 <= p.p95 && p.p95 <= p.p99);
    }

    #[test]
    fn small_values_are_exact() {
        let mut slot = LatencySlot::default();
        for ms in [10, 20, 30, 40, 200] {
            slot.add(&record(ms, None));
        }
        let p = slot.percentiles(1);
        assert_eq!((p.p50, p.p99), (30, 200));
        assert_eq!(p.ttfb_samples, 0);
        assert_eq!((p.ttfb_p50, p.ttfb_p95), (0, 0));
    }

    #[test]
    fn merging_equals_recording_into_one() {
        let (mut a, mut b, mut whole) = (
            LatencySlot::default(),
            LatencySlot::default(),
            LatencySlot::default(),
        );
        for ms in 1..=200 {
            let r = record(ms * 7, Some(ms));
            if ms % 2 == 0 {
                a.add(&r)
            } else {
                b.add(&r)
            }
            whole.add(&r);
        }
        let mut merged = LatencySlot::default();
        merged.merge(&a);
        merged.merge(&b);
        assert_eq!(merged.percentiles(5), whole.percentiles(5));
    }

    #[test]
    fn zero_and_huge_durations_are_accepted() {
        let mut slot = LatencySlot::default();
        slot.add(&record(0, Some(0)));
        slot.add(&record(i64::MAX / 2, None));
        let p = slot.percentiles(1);
        assert_eq!(p.samples, 2);
        assert_eq!(p.p50, 0);
        assert!(close(p.p99, MAX_TRACKED_MS), "p99 = {}", p.p99);
    }
}
