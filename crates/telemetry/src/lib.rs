//! Event bus, request records, usage statistics, body capture and log
//! capture for the Switchyard gateway. See `docs/DESIGN.md` section 7.
//!
//! * [`Telemetry`] — the facade the gateway holds; bundles everything below;
//! * [`EventBus`] / [`Event`] — what the live dashboard is fed from;
//! * [`RequestRecord`], [`RecordBuilder`] — one record per client request;
//! * [`Gauges`] — in-flight requests, open streams, WebSocket connections;
//! * [`UsageStore`] — recent requests, time buckets, latency percentiles,
//!   JSONL persistence;
//! * [`BodyStore`] — opt-in capture of request and response bodies;
//! * [`LogBuffer`], [`capture_layer`], [`FileSink`] — application logs;
//! * [`redact`] — secret redaction shared by all of the above.
//!
//! Conventions of every serialised type: snake_case JSON, timestamps as
//! unix milliseconds (`*_at`), durations in milliseconds (`*_ms`), absent
//! values as `null`. The admin API returns these types verbatim.
//!
//! Everything that depends on the current time takes it as a parameter;
//! only the log layer and [`Telemetry::new`] read the clock themselves.

pub mod bodies;
pub mod bus;
pub mod gauges;
pub mod logs;
mod private_files;
pub mod record;
pub mod redact;
mod telemetry;
mod time;
pub mod usage;

pub use bodies::{
    BodyStore, CapturedBodies, DEFAULT_MAX_BODY_BYTES, REQUESTS_DIR, prepare_body, truncate_body,
};
pub use bus::{DEFAULT_BUS_CAPACITY, Event, EventBus, TOPICS};
pub use gauges::{GaugeGuard, GaugeSnapshot, Gauges};
pub use logs::{
    DEFAULT_LOG_CAPACITY, DEFAULT_LOG_PAGE, FileSink, LOGS_DIR, LogBuffer, LogFile, LogLevel,
    LogLine, LogPage, LogQuery, capture_layer,
};
pub use record::{
    ANONYMOUS, Attempt, ClientInfo, Mode, RecordBuilder, RecordError, RequestRecord, RequestStart,
    Transport, UNKNOWN, error_kind_name, new_request_id, request_id_time_ms,
};
pub use redact::{
    REDACTED, is_secret_key, redact_body, redact_header_value, redact_headers, redact_json_secrets,
    redact_secret_value, redact_text, redact_url,
};
pub use telemetry::{PruneReport, Telemetry, TelemetryOptions};
pub use time::utc_day;
pub use usage::{
    BadCursor, BucketSize, DEFAULT_PAGE_SIZE, DEFAULT_RECENT_CAPACITY, GroupBy, GroupPoint,
    Latency, LoadReport, MAX_PAGE_SIZE, NamedTotals, OTHER, Range, RequestPage, RequestQuery,
    StatsTick, StatusFilter, TimePoint, Timeseries, Totals, USAGE_DIR, UsageQuery, UsageStore,
    UsageStoreInfo, UsageStoreOptions, UsageSummary, usage_dir,
};
