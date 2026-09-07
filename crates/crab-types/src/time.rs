//! Shared time-format contracts.

use std::time::{SystemTime, UNIX_EPOCH};

/// A timestamp cannot be represented by Crab's epoch-based RFC 3339 contract.
#[derive(Debug)]
pub enum TimestampError {
    /// The system clock precedes the Unix epoch.
    BeforeEpoch(std::time::SystemTimeError),
    /// The timestamp is later than 9999-12-31T23:59:59.999Z.
    OutOfRange,
}

impl std::fmt::Display for TimestampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeEpoch(_) => f.write_str("timestamp precedes the Unix epoch"),
            Self::OutOfRange => f.write_str("timestamp exceeds the RFC 3339 year range"),
        }
    }
}

impl std::error::Error for TimestampError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeEpoch(source) => Some(source),
            Self::OutOfRange => None,
        }
    }
}

/// Returns the current wall-clock time as RFC 3339 UTC with millisecond precision.
///
/// Returns an error when the clock is before the Unix epoch or beyond year 9999.
pub fn now_rfc3339_millis() -> Result<String, TimestampError> {
    from_system_time(SystemTime::now())
}

/// Formats a system time as RFC 3339 UTC, truncating sub-millisecond precision.
///
/// Returns an error for times before the Unix epoch or beyond year 9999.
pub fn from_system_time(time: SystemTime) -> Result<String, TimestampError> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .map_err(TimestampError::BeforeEpoch)?;
    let millis = u64::try_from(duration.as_millis()).map_err(|_| TimestampError::OutOfRange)?;
    from_epoch_millis(millis)
}

/// Formats Unix epoch milliseconds as RFC 3339 UTC with millisecond precision.
///
/// Returns an error for values above 253402300799999 (the end of year 9999).
pub fn from_epoch_millis(total_ms: u64) -> Result<String, TimestampError> {
    // Validate before the calendar arithmetic narrows the day count. Wider years
    // are not RFC 3339 and must never enter persisted manifests as valid dates.
    if total_ms > 253_402_300_799_999 {
        return Err(TimestampError::OutOfRange);
    }
    let secs = (total_ms / 1000) as i64;
    let millis = (total_ms % 1000) as u32;
    let (year, month, day, hour, min, sec) = epoch_secs_to_utc(secs);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z"
    ))
}

fn epoch_secs_to_utc(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = secs.rem_euclid(86_400) as u32;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    let days = (secs.div_euclid(86_400) + 719_468) as u32;
    let era = days / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero() {
        assert_eq!(from_epoch_millis(0).unwrap(), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_date() {
        assert_eq!(
            from_epoch_millis(1_777_055_537_123).unwrap(),
            "2026-04-24T18:32:17.123Z"
        );
    }

    #[test]
    fn leap_year_feb_29() {
        assert_eq!(
            from_epoch_millis(951_782_400_000).unwrap(),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn end_of_year_2024() {
        assert_eq!(
            from_epoch_millis(1_735_689_599_999).unwrap(),
            "2024-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn rejects_dates_after_last_rfc3339_millisecond() {
        assert_eq!(
            from_epoch_millis(253_402_300_799_999).unwrap(),
            "9999-12-31T23:59:59.999Z"
        );
        for millis in [253_402_300_800_000, u64::MAX] {
            assert!(matches!(
                from_epoch_millis(millis),
                Err(TimestampError::OutOfRange)
            ));
        }
    }

    #[test]
    fn pre_epoch_clock_retains_its_source() {
        use std::error::Error;
        let time = UNIX_EPOCH - std::time::Duration::from_millis(1);
        let error = from_system_time(time).unwrap_err();
        assert!(error.source().unwrap().is::<std::time::SystemTimeError>());
    }

    #[test]
    fn system_time_truncates_submillisecond_precision() {
        let time = UNIX_EPOCH + std::time::Duration::from_nanos(1_999_999);
        assert_eq!(from_system_time(time).unwrap(), "1970-01-01T00:00:00.001Z");
    }

    #[test]
    fn now_returns_valid_rfc3339() {
        let ts = now_rfc3339_millis().unwrap();
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
        assert_eq!(&ts[19..20], ".");
    }
}
