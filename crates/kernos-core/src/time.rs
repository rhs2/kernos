//! Millisecond timestamps and RFC 3339 formatting, without a calendar dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch from the system clock.
pub fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Formats epoch milliseconds as `2026-09-04T12:00:00.000Z`, the one timestamp
/// form every Kernos component exchanges.
pub fn format_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let day_secs = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// Parses an RFC 3339 timestamp (`Z` or a numeric offset, optional fraction)
/// into epoch milliseconds. Returns `None` for anything malformed.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let slice = text.get(from..to)?;
        if !slice.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        slice.parse().ok()
    };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ')
    {
        return None;
    }
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let mut pos = 19;
    let mut millis = 0i64;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while bytes.get(pos).is_some_and(|b| b.is_ascii_digit()) {
            pos += 1;
        }
        let fraction = text.get(start..pos)?;
        if fraction.is_empty() {
            return None;
        }
        let padded: String = format!("{fraction:0<3}").chars().take(3).collect();
        millis = padded.parse().ok()?;
    }
    let offset_secs: i64 = match bytes.get(pos) {
        Some(b'Z') | Some(b'z') => {
            pos += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            let oh = num(pos + 1, pos + 3)?;
            let om = num(pos + 4, pos + 6)?;
            if bytes.get(pos + 3) != Some(&b':') {
                return None;
            }
            pos += 6;
            let total = oh * 3600 + om * 60;
            if *sign == b'+' {
                total
            } else {
                -total
            }
        }
        _ => return None,
    };
    if pos != bytes.len() {
        return None;
    }
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_round_trip() {
        assert_eq!(format_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_ms(1_788_523_200_000), "2026-09-04T12:00:00.000Z");
        assert_eq!(
            parse_rfc3339("2026-09-04T12:00:00.000Z"),
            Some(1_788_523_200_000)
        );
        assert_eq!(
            parse_rfc3339("2026-09-04T12:00:00Z"),
            Some(1_788_523_200_000)
        );
        assert_eq!(
            parse_rfc3339("2026-09-04T12:00:00.5Z"),
            Some(1_788_523_200_500)
        );
        assert_eq!(
            parse_rfc3339("2026-09-04T14:00:00+02:00"),
            Some(1_788_523_200_000)
        );
        for ms in [1i64, 951_782_400_123, 4_102_444_800_999, 1_709_164_800_000] {
            assert_eq!(parse_rfc3339(&format_ms(ms)), Some(ms));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339("2026-13-04T12:00:00Z"), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2026-09-04T12:00:00"), None);
        assert_eq!(parse_rfc3339("2026-09-04T12:00:00.Z"), None);
    }
}
