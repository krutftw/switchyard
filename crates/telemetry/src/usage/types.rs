//! Query and result types of the usage store. These are returned verbatim by
//! the admin API, so their JSON shape is the dashboard's contract.

use crate::record::RequestRecord;
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use switchyard_core::usage::Usage;

use crate::time::{DAY_MS, HOUR_MS, MINUTE_MS};

/// Additive counters of a set of requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    pub requests: u64,
    /// Requests that did not end ok.
    pub errors: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Includes reasoning tokens.
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    /// Estimated cost in USD.
    pub cost: f64,
    /// Sum of request durations; divide by `requests` for the mean.
    pub duration_ms_sum: u64,
    /// Sum of time-to-first-byte over the `ttfb_count` requests that had one.
    pub ttfb_ms_sum: u64,
    pub ttfb_count: u64,
}

impl Totals {
    /// Counts one request.
    ///
    /// Every counter saturates instead of overflowing: token counts come
    /// from upstream responses and a hostile or broken upstream can report
    /// anything up to `u64::MAX`.
    pub fn add_record(&mut self, record: &RequestRecord) {
        let usage = &record.usage;
        self.requests = self.requests.saturating_add(1);
        if !record.ok {
            self.errors = self.errors.saturating_add(1);
        }
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(usage.reasoning_tokens);
        if let Some(cost) = record.cost {
            self.cost = add_cost(self.cost, cost);
        }
        self.duration_ms_sum = self.duration_ms_sum.saturating_add(record.duration_ms);
        if let Some(ttfb) = record.ttfb_ms {
            self.ttfb_ms_sum = self.ttfb_ms_sum.saturating_add(ttfb);
            self.ttfb_count = self.ttfb_count.saturating_add(1);
        }
    }

    /// Adds another set of counters to this one (saturating).
    pub fn merge(&mut self, other: &Totals) {
        self.requests = self.requests.saturating_add(other.requests);
        self.errors = self.errors.saturating_add(other.errors);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cost = add_cost(self.cost, other.cost);
        self.duration_ms_sum = self.duration_ms_sum.saturating_add(other.duration_ms_sum);
        self.ttfb_ms_sum = self.ttfb_ms_sum.saturating_add(other.ttfb_ms_sum);
        self.ttfb_count = self.ttfb_count.saturating_add(other.ttfb_count);
    }

    /// Prompt tokens (uncached + cache reads + cache writes) plus output
    /// tokens, saturating.
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
    }

    /// Share of requests that failed, `0.0` when there were none.
    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }
}

/// Adds a cost to a running sum. A cost that is not a finite, non-negative
/// number (a corrupt line in a usage file) is ignored, and the sum stays
/// finite, so it always serialises as a JSON number.
pub(crate) fn add_cost(sum: f64, cost: f64) -> f64 {
    if !cost.is_finite() || cost < 0.0 {
        return sum;
    }
    let total = sum + cost;
    if total.is_finite() { total } else { f64::MAX }
}

/// Prompt plus output tokens of one request. Saturating, unlike
/// [`Usage::total_tokens`], because the counts come from the network.
pub(crate) const fn usage_total_tokens(usage: &Usage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
        .saturating_add(usage.output_tokens)
}

/// [`Totals`] of one model, provider or client key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedTotals {
    pub name: String,
    #[serde(flatten)]
    pub totals: Totals,
}

/// Time range of a usage query, ending now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Range {
    /// Last 60 minutes.
    #[serde(rename = "1h")]
    Hour,
    /// Last 24 hours.
    #[default]
    #[serde(rename = "24h")]
    Day,
    /// Last 7 days.
    #[serde(rename = "7d")]
    Week,
    /// Last 30 days.
    #[serde(rename = "30d")]
    Month,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Hour, Range::Day, Range::Week, Range::Month];

    /// The spelling used in query strings and JSON: `1h`, `24h`, `7d`, `30d`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Range::Hour => "1h",
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }

    pub const fn duration_ms(self) -> i64 {
        match self {
            Range::Hour => HOUR_MS,
            Range::Day => DAY_MS,
            Range::Week => 7 * DAY_MS,
            Range::Month => 30 * DAY_MS,
        }
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Range {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "1h" | "60m" | "hour" => Ok(Range::Hour),
            "24h" | "1d" | "day" => Ok(Range::Day),
            "7d" | "1w" | "week" => Ok(Range::Week),
            "30d" | "1mo" | "month" => Ok(Range::Month),
            other => Err(format!(
                "unknown range `{other}` (expected 1h, 24h, 7d or 30d)"
            )),
        }
    }
}

/// Width of a time-series point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BucketSize {
    /// Chosen from the range: minutes for `1h`, hours for `24h` and `7d`,
    /// days for `30d`.
    #[default]
    Auto,
    Minute,
    Hour,
    Day,
}

impl BucketSize {
    pub const fn as_str(self) -> &'static str {
        match self {
            BucketSize::Auto => "auto",
            BucketSize::Minute => "minute",
            BucketSize::Hour => "hour",
            BucketSize::Day => "day",
        }
    }

    /// Width in milliseconds; `None` for [`BucketSize::Auto`].
    pub const fn width_ms(self) -> Option<i64> {
        match self {
            BucketSize::Auto => None,
            BucketSize::Minute => Some(MINUTE_MS),
            BucketSize::Hour => Some(HOUR_MS),
            BucketSize::Day => Some(DAY_MS),
        }
    }
}

impl FromStr for BucketSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(BucketSize::Auto),
            "minute" | "min" | "1m" | "m" => Ok(BucketSize::Minute),
            "hour" | "1h" | "h" => Ok(BucketSize::Hour),
            "day" | "1d" | "d" => Ok(BucketSize::Day),
            other => Err(format!(
                "unknown bucket `{other}` (expected auto, minute, hour or day)"
            )),
        }
    }
}

/// Dimension a time series is broken down by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    #[default]
    None,
    Model,
    Provider,
    Key,
}

impl GroupBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroupBy::None => "none",
            GroupBy::Model => "model",
            GroupBy::Provider => "provider",
            GroupBy::Key => "key",
        }
    }
}

impl FromStr for GroupBy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Ok(GroupBy::None),
            "model" => Ok(GroupBy::Model),
            "provider" => Ok(GroupBy::Provider),
            "key" | "client" | "api_key" => Ok(GroupBy::Key),
            other => Err(format!(
                "unknown group_by `{other}` (expected model, provider or key)"
            )),
        }
    }
}

/// Latency percentiles in milliseconds. All zero when there are no samples.
///
/// Histograms are kept per minute for the last hour and per hour for the
/// last 24 hours, so the percentiles of a `7d` or `30d` summary describe the
/// last 24 hours; [`window_ms`](Latency::window_ms) says which window was
/// used. Values are accurate to two significant digits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latency {
    /// Length of the window the percentiles cover.
    pub window_ms: i64,
    pub p50: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    /// Time to first byte.
    pub ttfb_p50: u64,
    pub ttfb_p95: u64,
    /// Requests the duration percentiles were computed from.
    pub samples: u64,
    /// Requests the time-to-first-byte percentiles were computed from.
    pub ttfb_samples: u64,
}

/// Answer to `GET /usage/summary`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub range: Range,
    /// Start of the covered window, unix milliseconds.
    pub from: i64,
    /// End of the covered window (the query time), unix milliseconds.
    pub to: i64,
    pub totals: Totals,
    pub latency: Latency,
    /// `errors / requests` over the range.
    pub error_rate: f64,
    /// Requests finished in the last 60 seconds.
    pub requests_per_minute: u64,
    /// Tokens (prompt + output) of requests finished in the last 60 seconds.
    pub tokens_per_minute: u64,
    /// Per client-facing model, most requests first.
    pub by_model: Vec<NamedTotals>,
    pub by_provider: Vec<NamedTotals>,
    /// Per client key name.
    pub by_key: Vec<NamedTotals>,
}

/// One series' share of a time-series point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupPoint {
    pub requests: u64,
    pub errors: u64,
    /// Prompt + output tokens.
    pub tokens: u64,
    pub cost: f64,
}

impl GroupPoint {
    pub(crate) fn add(&mut self, totals: &Totals) {
        self.requests = self.requests.saturating_add(totals.requests);
        self.errors = self.errors.saturating_add(totals.errors);
        self.tokens = self.tokens.saturating_add(totals.total_tokens());
        self.cost = add_cost(self.cost, totals.cost);
    }
}

/// One bucket of a time series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimePoint {
    /// Start of the bucket, unix milliseconds (UTC aligned).
    pub t: i64,
    #[serde(flatten)]
    pub totals: Totals,
    /// Breakdown by the requested dimension. Only series with traffic in this
    /// bucket are present; a missing series means zero.
    pub groups: BTreeMap<String, GroupPoint>,
}

/// Answer to `GET /usage/timeseries`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timeseries {
    pub range: Range,
    /// Bucket width that was used (never `auto`).
    pub bucket: BucketSize,
    pub bucket_ms: i64,
    pub group_by: GroupBy,
    /// Start of the first bucket, unix milliseconds.
    pub from: i64,
    /// The query time, unix milliseconds.
    pub to: i64,
    /// Every series name appearing in `points[].groups`, most requests first.
    pub series: Vec<String>,
    /// Contiguous buckets, oldest first; buckets without traffic are present
    /// with zero counters. The first and last bucket may be partial.
    pub points: Vec<TimePoint>,
}

/// Parameters of `GET /usage/summary` and `GET /usage/timeseries`.
/// Deserialises straight from a query string: every field is optional,
/// empty values count as absent and unknown values fall back to the default
/// rather than failing the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UsageQuery {
    /// `1h`, `24h` (default), `7d` or `30d`.
    #[serde(deserialize_with = "lenient::opt_parsed")]
    pub range: Option<Range>,
    /// `auto` (default), `minute`, `hour` or `day`.
    #[serde(deserialize_with = "lenient::opt_parsed")]
    pub bucket: Option<BucketSize>,
    /// `model`, `provider` or `key`; absent for no breakdown.
    #[serde(deserialize_with = "lenient::opt_parsed")]
    pub group_by: Option<GroupBy>,
}

impl UsageQuery {
    pub fn range(&self) -> Range {
        self.range.unwrap_or_default()
    }

    pub fn bucket(&self) -> BucketSize {
        self.bucket.unwrap_or_default()
    }

    pub fn group_by(&self) -> GroupBy {
        self.group_by.unwrap_or_default()
    }
}

/// The once-a-second `stats` frame of the admin WebSocket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsTick {
    /// When the snapshot was taken, unix milliseconds.
    pub at: i64,
    pub in_flight: u64,
    pub active_streams: u64,
    pub ws_connections: u64,
    /// Requests finished in the last 60 seconds.
    pub rpm: u64,
    /// Tokens of requests finished in the last 60 seconds.
    pub tpm: u64,
    /// Share of failed requests among those finished in the last 60 seconds.
    pub error_rate_1m: f64,
    /// Median request duration over the last hour.
    pub p50_ms: u64,
    /// 95th percentile request duration over the last hour.
    pub p95_ms: u64,
    /// Requests `p50_ms` and `p95_ms` were computed from. `0` means no
    /// request finished in the last hour and the two percentiles (then `0`)
    /// say nothing: show "no data", not "0 ms".
    #[serde(default)]
    pub latency_samples: u64,
}

/// Outcome filter of the request list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusFilter {
    Ok,
    Error,
    /// An exact HTTP status.
    Code(u16),
    /// A status class: `4` for 4xx.
    Class(u16),
}

impl StatusFilter {
    pub fn matches(self, record: &RequestRecord) -> bool {
        match self {
            StatusFilter::Ok => record.ok,
            StatusFilter::Error => !record.ok,
            StatusFilter::Code(code) => record.status == code,
            StatusFilter::Class(class) => record.status / 100 == class,
        }
    }
}

impl FromStr for StatusFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim().to_ascii_lowercase();
        match text.as_str() {
            "ok" | "success" | "succeeded" => return Ok(StatusFilter::Ok),
            "error" | "err" | "failed" | "failure" => return Ok(StatusFilter::Error),
            _ => {}
        }
        if let Some(class) = text.strip_suffix("xx")
            && let Ok(class @ 1..=5) = class.parse::<u16>()
        {
            return Ok(StatusFilter::Class(class));
        }
        match text.parse::<u16>() {
            Ok(code @ 100..=599) => Ok(StatusFilter::Code(code)),
            _ => Err(format!(
                "unknown status filter `{s}` (expected ok, error, a status code or a class like 4xx)"
            )),
        }
    }
}

impl fmt::Display for StatusFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusFilter::Ok => f.write_str("ok"),
            StatusFilter::Error => f.write_str("error"),
            StatusFilter::Code(code) => write!(f, "{code}"),
            StatusFilter::Class(class) => write!(f, "{class}xx"),
        }
    }
}

/// Default page size of the request list.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Largest page size the request list serves.
pub const MAX_PAGE_SIZE: usize = 500;

/// Filters and paging of `GET /requests`. Deserialises straight from a query
/// string: every field is optional, empty values count as absent, and an
/// unparseable `limit` or `status` is ignored rather than rejected.
///
/// Two values are refused instead, because guessing would answer with the
/// wrong list: a `since` that is not a whole number fails deserialisation,
/// and a `before` that is not a cursor makes
/// [`UsageStore::try_requests`](crate::UsageStore::try_requests) fail with
/// [`BadCursor`].
///
/// Only **finished** requests are listed: a record exists from the moment a
/// request ends. Requests still in flight are announced on the event bus
/// (`request.started`) and counted by the gauges, but are in no list and
/// cannot be looked up by id until they finish.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RequestQuery {
    /// Page size; defaults to [`DEFAULT_PAGE_SIZE`], capped at
    /// [`MAX_PAGE_SIZE`].
    #[serde(deserialize_with = "lenient::opt_usize")]
    pub limit: Option<usize>,
    /// Cursor: return only requests older than this. Accepts the
    /// `next_before` value of a previous page (`<started_at>:<id>`), a bare
    /// request id, or a bare unix-millisecond timestamp.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub before: Option<String>,
    /// Only requests that started at or after this instant, unix
    /// milliseconds. A filter, not a cursor: `total` counts only those
    /// requests. A value that is not a whole number fails deserialisation.
    #[serde(deserialize_with = "lenient::opt_whole_ms")]
    pub since: Option<i64>,
    /// Exact model name (requested, client-facing or upstream),
    /// case-insensitive. `unknown` selects the requests that have no model
    /// (refused before one could be read from the body), which is the name
    /// the summaries group them under ([`crate::UNKNOWN`]).
    #[serde(deserialize_with = "lenient::opt_string")]
    pub model: Option<String>,
    /// Exact client-facing model name, case-insensitive, compared with
    /// [`RequestRecord::model_name`] alone — the name `by_model` of the
    /// summary and `group_by=model` of the time series count a request
    /// under — so that a row of those opens exactly its requests. `unknown`
    /// selects the requests without a model.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub client_model: Option<String>,
    /// Exact provider name, case-insensitive; `unknown` selects the requests
    /// no provider served: they failed before routing, or every credential
    /// of the model was cooling down.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub provider: Option<String>,
    /// Client key name or id, case-insensitive; `anonymous` selects requests
    /// made without a key.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub key: Option<String>,
    #[serde(deserialize_with = "lenient::opt_status")]
    pub status: Option<StatusFilter>,
    /// Case-insensitive substring searched in the id, model names, provider,
    /// credential label, key name, endpoint and error.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub q: Option<String>,
}

impl RequestQuery {
    /// Effective page size.
    pub fn page_size(&self) -> usize {
        self.limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE)
    }
}

/// A `before` of [`RequestQuery`] that is not a cursor: neither
/// `<started_at>:<id>`, nor a request id the list knows or that carries its
/// creation time (a UUIDv7), nor a unix-millisecond timestamp. Answering it
/// with the first page would make a paging client loop, and with an empty
/// page would hide the mistake, so it is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadCursor;

impl fmt::Display for BadCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "is not a cursor: pass the `next_before` of a previous page, a request id or a \
             unix-millisecond timestamp",
        )
    }
}

impl std::error::Error for BadCursor {}

/// One page of the request list, newest first.
///
/// The list holds finished requests only (see [`RequestQuery`]), and only
/// the most recent [`capacity`](RequestPage::capacity) of them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RequestPage {
    pub items: Vec<Arc<RequestRecord>>,
    /// Pass as `before` to get the next (older) page; `null` on the last
    /// page.
    pub next_before: Option<String>,
    pub has_more: bool,
    /// Requests in memory matching the filters, ignoring paging. Never
    /// more than `capacity`.
    pub total: usize,
    /// How many finished requests the in-memory list can hold
    /// ([`crate::DEFAULT_RECENT_CAPACITY`] unless configured otherwise).
    /// Once that many are held, each new one pushes out the one that
    /// finished longest ago: the list is "the newest `capacity` requests".
    pub capacity: usize,
}

pub(crate) mod lenient {
    //! Deserialisers for query-string fields: values arrive as strings (or
    //! as JSON scalars when the same struct is read from a JSON body), empty
    //! means absent and garbage is ignored.

    use super::*;

    /// Anything with a `FromStr`; a value that does not parse is absent.
    pub(crate) fn opt_parsed<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
    {
        Ok(opt_string(d)?.and_then(|text| text.parse::<T>().ok()))
    }

    pub(crate) fn opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        opt_parsed(d)
    }

    struct Scalar;

    impl<'de> Visitor<'de> for Scalar {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string, a number or nothing")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let v = v.trim();
            Ok((!v.is_empty()).then(|| v.to_string()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(Scalar)
        }
    }

    pub(crate) fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        d.deserialize_any(Scalar)
    }

    pub(crate) fn opt_usize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
        Ok(opt_string(d)?.and_then(|text| text.parse::<usize>().ok()))
    }

    pub(crate) fn opt_status<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<StatusFilter>, D::Error> {
        Ok(opt_string(d)?.and_then(|text| text.parse::<StatusFilter>().ok()))
    }

    /// What [`opt_whole_ms`] says about a value it refuses: a fragment that
    /// reads after the parameter's name, without the value (which is
    /// echoed nowhere).
    pub(crate) const NOT_WHOLE_MS: &str = "must be a whole number of unix milliseconds";

    /// A unix-millisecond instant. Not lenient: empty is absent, but
    /// anything else that is not a whole number is an error, because
    /// ignoring a time filter would answer with a different list.
    pub(crate) fn opt_whole_ms<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        match opt_string(d)? {
            None => Ok(None),
            Some(text) => text
                .parse::<i64>()
                .map(Some)
                .map_err(|_| de::Error::custom(NOT_WHOLE_MS)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RequestStart};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::protocol::Protocol;
    use switchyard_core::usage::Usage;

    fn record(status: u16, ttfb: Option<i64>, cost: Option<f64>) -> RequestRecord {
        let mut b = RecordBuilder::new(RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "gpt-5",
            1_000,
        ));
        b.set_usage(Usage {
            input_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens: 5,
            output_tokens: 40,
            reasoning_tokens: 15,
        })
        .set_cost(cost);
        if let Some(at) = ttfb {
            b.mark_first_byte(at);
        }
        b.finish(status, 1_500)
    }

    #[test]
    fn totals_accumulate() {
        let mut t = Totals::default();
        t.add_record(&record(200, Some(1_100), Some(0.25)));
        t.add_record(&record(500, None, None));
        t.add_record(&record(200, Some(1_300), Some(f64::NAN)));
        assert_eq!(
            t,
            Totals {
                requests: 3,
                errors: 1,
                input_tokens: 30,
                cache_read_tokens: 60,
                cache_write_tokens: 15,
                output_tokens: 120,
                reasoning_tokens: 45,
                cost: 0.25,
                duration_ms_sum: 1_500,
                ttfb_ms_sum: 400,
                ttfb_count: 2,
            }
        );
        assert_eq!(t.total_tokens(), 225);
        assert!((t.error_rate() - 1.0 / 3.0).abs() < 1e-12);
        let mut doubled = t;
        doubled.merge(&t);
        assert_eq!(doubled.requests, 6);
        assert_eq!(doubled.ttfb_count, 4);
        assert_eq!(doubled.cost, 0.5);
        assert_eq!(Totals::default().error_rate(), 0.0);
    }

    #[test]
    fn counters_saturate_and_the_cost_stays_a_number() {
        let mut huge = record(200, Some(1_100), Some(f64::MAX));
        huge.usage = Usage {
            input_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: u64::MAX,
            output_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };
        huge.duration_ms = u64::MAX;
        huge.ttfb_ms = Some(u64::MAX);
        let mut t = Totals::default();
        t.add_record(&huge);
        t.add_record(&huge);
        assert_eq!(t.requests, 2);
        assert_eq!(t.input_tokens, u64::MAX);
        assert_eq!(t.reasoning_tokens, u64::MAX);
        assert_eq!(t.duration_ms_sum, u64::MAX);
        assert_eq!(t.ttfb_ms_sum, u64::MAX);
        assert_eq!(t.total_tokens(), u64::MAX);
        assert_eq!(t.cost, f64::MAX);
        let mut merged = t;
        merged.merge(&t);
        assert_eq!(merged.requests, 4);
        assert_eq!(merged.output_tokens, u64::MAX);
        assert_eq!(merged.cost, f64::MAX);
        // Still valid JSON numbers.
        let value = serde_json::to_value(merged).unwrap();
        assert!(value["cost"].is_number());
        assert_eq!(value["input_tokens"], u64::MAX);
        let mut point = GroupPoint::default();
        point.add(&merged);
        point.add(&merged);
        assert_eq!((point.requests, point.tokens), (8, u64::MAX));
        assert_eq!(point.cost, f64::MAX);

        // Costs that are not a finite, non-negative number are ignored.
        for bogus in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert_eq!(add_cost(1.5, bogus), 1.5);
        }
        assert_eq!(add_cost(1.5, 0.25), 1.75);
        assert_eq!(usage_total_tokens(&huge.usage), u64::MAX);
    }

    #[test]
    fn named_totals_flatten() {
        let value = serde_json::to_value(NamedTotals {
            name: "gpt-5".into(),
            totals: Totals {
                requests: 2,
                ..Totals::default()
            },
        })
        .unwrap();
        assert_eq!(
            value,
            json!({
                "name": "gpt-5", "requests": 2, "errors": 0, "input_tokens": 0,
                "cache_read_tokens": 0, "cache_write_tokens": 0, "output_tokens": 0,
                "reasoning_tokens": 0, "cost": 0.0, "duration_ms_sum": 0,
                "ttfb_ms_sum": 0, "ttfb_count": 0
            })
        );
    }

    #[test]
    fn range_parsing_and_names() {
        for range in Range::ALL {
            assert_eq!(range.as_str().parse::<Range>().unwrap(), range);
            assert_eq!(serde_json::to_value(range).unwrap(), range.as_str());
        }
        assert_eq!("week".parse::<Range>().unwrap(), Range::Week);
        assert_eq!(" 24H ".parse::<Range>().unwrap(), Range::Day);
        assert!("2h".parse::<Range>().is_err());
        assert_eq!(Range::Month.duration_ms(), 2_592_000_000);
        assert_eq!(Range::default(), Range::Day);
    }

    #[test]
    fn bucket_and_group_parsing() {
        assert_eq!("".parse::<BucketSize>().unwrap(), BucketSize::Auto);
        assert_eq!("minute".parse::<BucketSize>().unwrap(), BucketSize::Minute);
        assert_eq!("Hour".parse::<BucketSize>().unwrap(), BucketSize::Hour);
        assert_eq!("day".parse::<BucketSize>().unwrap(), BucketSize::Day);
        assert!("fortnight".parse::<BucketSize>().is_err());
        assert_eq!(BucketSize::Auto.width_ms(), None);
        assert_eq!(BucketSize::Day.width_ms(), Some(86_400_000));
        assert_eq!(serde_json::to_value(BucketSize::Minute).unwrap(), "minute");

        assert_eq!("".parse::<GroupBy>().unwrap(), GroupBy::None);
        assert_eq!("model".parse::<GroupBy>().unwrap(), GroupBy::Model);
        assert_eq!("provider".parse::<GroupBy>().unwrap(), GroupBy::Provider);
        assert_eq!("key".parse::<GroupBy>().unwrap(), GroupBy::Key);
        assert!("colour".parse::<GroupBy>().is_err());
        assert_eq!(serde_json::to_value(GroupBy::Provider).unwrap(), "provider");
    }

    #[test]
    fn status_filter_parsing_and_matching() {
        assert_eq!("ok".parse::<StatusFilter>().unwrap(), StatusFilter::Ok);
        assert_eq!(
            "ERROR".parse::<StatusFilter>().unwrap(),
            StatusFilter::Error
        );
        assert_eq!(
            "429".parse::<StatusFilter>().unwrap(),
            StatusFilter::Code(429)
        );
        assert_eq!(
            "5xx".parse::<StatusFilter>().unwrap(),
            StatusFilter::Class(5)
        );
        assert!("9xx".parse::<StatusFilter>().is_err());
        assert!("42".parse::<StatusFilter>().is_err());
        assert!("maybe".parse::<StatusFilter>().is_err());
        assert_eq!(StatusFilter::Class(4).to_string(), "4xx");

        let ok = record(200, None, None);
        let failed = record(503, None, None);
        assert!(StatusFilter::Ok.matches(&ok) && !StatusFilter::Ok.matches(&failed));
        assert!(StatusFilter::Error.matches(&failed) && !StatusFilter::Error.matches(&ok));
        assert!(StatusFilter::Code(503).matches(&failed));
        assert!(StatusFilter::Class(5).matches(&failed) && !StatusFilter::Class(5).matches(&ok));
    }

    #[test]
    fn request_query_is_lenient() {
        let q: RequestQuery = serde_json::from_value(json!({
            "limit": "25", "before": "", "model": " gpt-5 ", "status": "error", "q": null
        }))
        .unwrap();
        assert_eq!(
            q,
            RequestQuery {
                limit: Some(25),
                model: Some("gpt-5".into()),
                status: Some(StatusFilter::Error),
                ..RequestQuery::default()
            }
        );
        let q: RequestQuery =
            serde_json::from_value(json!({"limit": 7, "status": "bogus", "before": 1234})).unwrap();
        assert_eq!(q.limit, Some(7));
        assert_eq!(q.status, None);
        assert_eq!(q.before.as_deref(), Some("1234"));
        let q: RequestQuery = serde_json::from_value(json!({"limit": "lots"})).unwrap();
        assert_eq!(q.limit, None);
        assert_eq!(q.page_size(), DEFAULT_PAGE_SIZE);
        let q: RequestQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!(q, RequestQuery::default());
    }

    /// `since` is a time filter: ignoring a value that cannot be read would
    /// answer with a longer list than was asked for, so it is refused.
    #[test]
    fn since_must_be_a_whole_number_and_client_model_is_read() {
        let q: RequestQuery = serde_json::from_value(json!({
            "since": "1790942400000", "client_model": " Mock-Echo "
        }))
        .unwrap();
        assert_eq!(q.since, Some(1_790_942_400_000));
        assert_eq!(q.client_model.as_deref(), Some("Mock-Echo"));
        let q: RequestQuery = serde_json::from_value(json!({"since": -5})).unwrap();
        assert_eq!(q.since, Some(-5));
        let q: RequestQuery = serde_json::from_value(json!({"since": ""})).unwrap();
        assert_eq!(q.since, None);
        for bad in [
            json!("yesterday"),
            json!("1.5"),
            json!(1.5),
            json!("99999999999999999999"),
        ] {
            let error = serde_json::from_value::<RequestQuery>(json!({ "since": bad }))
                .unwrap_err()
                .to_string();
            assert!(error.contains(lenient::NOT_WHOLE_MS), "{bad}: {error}");
            assert!(!error.contains("yesterday"), "{error}");
        }
    }

    #[test]
    fn usage_query_is_lenient() {
        let q: UsageQuery =
            serde_json::from_value(json!({"range": "7d", "bucket": "hour", "group_by": "model"}))
                .unwrap();
        assert_eq!(
            (q.range(), q.bucket(), q.group_by()),
            (Range::Week, BucketSize::Hour, GroupBy::Model)
        );
        let q: UsageQuery =
            serde_json::from_value(json!({"range": "", "bucket": "fortnight", "group_by": null}))
                .unwrap();
        assert_eq!(q, UsageQuery::default());
        assert_eq!(
            (q.range(), q.bucket(), q.group_by()),
            (Range::Day, BucketSize::Auto, GroupBy::None)
        );
        let q: UsageQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!(q, UsageQuery::default());
    }

    #[test]
    fn page_size_is_clamped() {
        let mut q = RequestQuery::default();
        assert_eq!(q.page_size(), 50);
        q.limit = Some(0);
        assert_eq!(q.page_size(), 1);
        q.limit = Some(1_000_000);
        assert_eq!(q.page_size(), MAX_PAGE_SIZE);
    }

    #[test]
    fn stats_tick_shape() {
        assert_eq!(
            serde_json::to_value(StatsTick::default()).unwrap(),
            json!({
                "at": 0, "in_flight": 0, "active_streams": 0, "ws_connections": 0,
                "rpm": 0, "tpm": 0, "error_rate_1m": 0.0, "p50_ms": 0, "p95_ms": 0,
                "latency_samples": 0
            })
        );
    }
}
