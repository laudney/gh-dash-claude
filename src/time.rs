//! Time in epoch milliseconds, the single unit used everywhere.

use std::time::{SystemTime, UNIX_EPOCH};

/// One day in milliseconds.
pub const DAY_MS: i64 = 86_400_000;

/// The current time in epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse an RFC 3339 timestamp (as written in transcripts, such as
/// `2026-11-02T19:31:00.123Z`) to epoch milliseconds.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let ts: jiff::Timestamp = s.parse().ok()?;
    Some(ts.as_millisecond())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcript_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(
            parse_rfc3339_ms("2026-11-02T19:31:00Z"),
            Some(1_793_647_860_000)
        );
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }
}
