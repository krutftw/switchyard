//! Usage statistics: a ring of recent request records, time buckets broken
//! down by model / provider / client key, latency histograms and their
//! persistence as JSONL.
//!
//! Everything that depends on the current time takes it as a parameter, so
//! the aggregation is deterministic under test.

mod buckets;
mod latency;
mod persist;
mod store;
mod types;

pub use buckets::OTHER;
pub use persist::{LoadReport, USAGE_DIR, usage_dir};
pub use store::{DEFAULT_RECENT_CAPACITY, UsageStore, UsageStoreInfo, UsageStoreOptions};
pub use types::{
    BucketSize, DEFAULT_PAGE_SIZE, GroupBy, GroupPoint, Latency, MAX_PAGE_SIZE, NamedTotals, Range,
    RequestPage, RequestQuery, StatsTick, StatusFilter, TimePoint, Timeseries, Totals, UsageQuery,
    UsageSummary,
};

pub(crate) use types::lenient;
