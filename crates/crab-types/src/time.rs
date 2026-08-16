//! Shared time-format contracts.

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current wall-clock time as RFC 3339 UTC with millisecond precision.
#[must_use]
pub fn now_rfc3339_millis() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    from_epoch_millis(dur.as_millis() as u64)
}

/// Formats an epoch-millisecond value as RFC 3339 UTC with millisecond precision.
#[must_use]
pub fn from_epoch_millis(total_ms: u64) -> String {
    let secs = (total_ms / 1000) as i64;
    let millis = (total_ms % 1000) as u32;

    let (year, month, day, hour, min, sec) = epoch_secs_to_utc(secs);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
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
        assert_eq!(from_epoch_millis(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_date() {
        assert_eq!(
            from_epoch_millis(1_777_055_537_123),
            "2026-04-24T18:32:17.123Z"
        );
    }

    #[test]
    fn leap_year_feb_29() {
        assert_eq!(
            from_epoch_millis(951_782_400_000),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn end_of_year_2024() {
        assert_eq!(
            from_epoch_millis(1_735_689_599_999),
            "2024-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn now_returns_valid_rfc3339() {
        let ts = now_rfc3339_millis();
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
