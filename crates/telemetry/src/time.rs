//! Calendar helpers. Everything is UTC: file names, bucket boundaries and the
//! timestamps the dashboard receives never depend on the server's time zone.

use chrono::{DateTime, NaiveDate, Utc};

pub(crate) const SECOND_MS: i64 = 1_000;
pub(crate) const MINUTE_MS: i64 = 60_000;
pub(crate) const HOUR_MS: i64 = 3_600_000;
pub(crate) const DAY_MS: i64 = 86_400_000;

/// Days since the unix epoch of a unix-millisecond timestamp.
pub(crate) fn day_index(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// `YYYY-MM-DD` (UTC) of a unix-millisecond timestamp.
pub fn utc_day(ms: i64) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => "1970-01-01".to_string(),
    }
}

/// Parses `YYYY-MM-DD` into days since the unix epoch.
pub(crate) fn parse_day(text: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    let midnight = date.and_hms_opt(0, 0, 0)?.and_utc();
    Some(midnight.timestamp().div_euclid(86_400))
}

/// RFC 3339 with millisecond precision, e.g. `2026-10-02T13:09:12.123Z`.
pub(crate) fn rfc3339(ms: i64) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => "1970-01-01T00:00:00.000Z".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_strings_are_utc() {
        // 2026-10-02T23:59:59.999Z and one millisecond later.
        let end_of_day = 1_790_985_599_999;
        assert_eq!(utc_day(end_of_day), "2026-10-02");
        assert_eq!(utc_day(end_of_day + 1), "2026-10-03");
        assert_eq!(utc_day(0), "1970-01-01");
    }

    #[test]
    fn day_round_trip() {
        let ms = 1_790_985_599_999;
        assert_eq!(parse_day(&utc_day(ms)), Some(day_index(ms)));
        assert_eq!(parse_day("1970-01-02"), Some(1));
        assert_eq!(parse_day("2026-13-40"), None);
        assert_eq!(parse_day("garbage"), None);
    }

    #[test]
    fn rfc3339_has_milliseconds() {
        assert_eq!(rfc3339(1_790_985_599_999), "2026-10-02T23:59:59.999Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn negative_timestamps_floor() {
        assert_eq!(day_index(-1), -1);
        assert_eq!(utc_day(-1), "1969-12-31");
    }
}
