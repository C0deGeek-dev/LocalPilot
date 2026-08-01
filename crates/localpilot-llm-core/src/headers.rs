//! Rate-limit header value parsing shared by the provider adapters.
//!
//! Implemented from the public provider documentation: OpenAI publishes reset
//! times as compact duration strings (`"1s"`, `"6m0s"`) on
//! `x-ratelimit-reset-requests` / `x-ratelimit-reset-tokens`
//! (<https://developers.openai.com/api/docs/guides/rate-limits>); Anthropic
//! publishes RFC 3339 timestamps on `anthropic-ratelimit-*-reset` and seconds
//! on `retry-after` (<https://platform.claude.com/docs/en/api/rate-limits>);
//! HTTP itself allows `retry-after` as either delay-seconds or an HTTP-date
//! (RFC 9110 §10.2.3). Every parser here degrades to `None` on malformed
//! input — quota metadata is best-effort, never an error.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parse a `retry-after` value: either non-negative delay-seconds or an
/// HTTP-date (IMF-fixdate), returned as a delay relative to `now`.
#[must_use]
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let reset = parse_http_date_epoch(value)?;
    let now_epoch = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now_epoch)))
}

/// Parse a compact duration string of `<number><unit>` segments, where unit is
/// `h`, `m`, `s`, or `ms` and the number may be fractional: `"1s"`, `"6m0s"`,
/// `"1m30s"`, `"59.812s"`.
#[must_use]
pub(crate) fn parse_compact_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut total_secs = 0f64;
    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        let number: f64 = value[number_start..index].parse().ok()?;
        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let multiplier = match &value[unit_start..index] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total_secs += number * multiplier;
    }
    if total_secs.is_finite() && total_secs >= 0.0 {
        Some(Duration::from_secs_f64(total_secs))
    } else {
        None
    }
}

/// Parse an RFC 3339 timestamp (`2026-06-04T07:13:19Z`, optional fractional
/// seconds, `Z` or `±HH:MM` offset) into Unix epoch seconds (floored).
#[must_use]
pub(crate) fn parse_rfc3339_epoch(value: &str) -> Option<u64> {
    let value = value.trim();
    // Minimum shape: YYYY-MM-DDTHH:MM:SS + zone designator. All structural
    // slicing is checked, so multi-byte garbage degrades to `None`.
    if value.len() < 20 || !value.is_ascii() {
        return None;
    }
    let date = value.get(..10)?;
    let separator = value.as_bytes().get(10).copied()?;
    if separator != b'T' && separator != b't' && separator != b' ' {
        return None;
    }
    let (year, month, day) = parse_date(date)?;
    let time_and_zone = value.get(11..)?;
    let (hour, minute, second) = parse_time(time_and_zone.get(..8)?)?;
    let mut rest = time_and_zone.get(8..)?;
    if rest.starts_with('.') {
        let digits = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map_or(rest.len() - 1, |i| i);
        if digits == 0 {
            return None;
        }
        rest = &rest[1 + digits..];
    }
    let offset_secs: i64 = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1i64,
                b'-' => -1i64,
                _ => return None,
            };
            let (oh, om) = rest[1..].split_once(':')?;
            let oh: i64 = oh.parse().ok()?;
            let om: i64 = om.parse().ok()?;
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(year, month, day)?;
    let local = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))?;
    let epoch = local.checked_sub(offset_secs)?;
    u64::try_from(epoch).ok()
}

/// Parse an IMF-fixdate HTTP-date (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix
/// epoch seconds.
#[must_use]
pub(crate) fn parse_http_date_epoch(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if !zone.eq_ignore_ascii_case("GMT") && !zone.eq_ignore_ascii_case("UTC") {
        return None;
    }
    let day: u32 = day.parse().ok()?;
    let month = match month.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = year.parse().ok()?;
    let (hour, minute, second) = parse_time(time)?;
    let days = days_from_civil(year, month, day)?;
    let epoch = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))?;
    u64::try_from(epoch).ok()
}

fn parse_date(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((year, month, day))
}

fn parse_time(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second.min(59)))
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date. Standard
/// days-from-civil construction; valid for the full header-realistic range.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds() {
        let now = UNIX_EPOCH;
        assert_eq!(parse_retry_after("30", now), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_http_date() {
        // 1994-11-06T08:49:37Z = 784111777.
        let now = UNIX_EPOCH + Duration::from_secs(784_111_677);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(100))
        );
        // A date in the past degrades to zero, not an underflow.
        let later = UNIX_EPOCH + Duration::from_secs(784_111_877);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", later),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert_eq!(parse_retry_after("soon", UNIX_EPOCH), None);
        assert_eq!(parse_retry_after("", UNIX_EPOCH), None);
    }

    #[test]
    fn compact_durations() {
        assert_eq!(parse_compact_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(
            parse_compact_duration("6m0s"),
            Some(Duration::from_secs(360))
        );
        assert_eq!(
            parse_compact_duration("1m30s"),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_compact_duration("1h2m3s"),
            Some(Duration::from_secs(3723))
        );
        assert_eq!(
            parse_compact_duration("59.812s"),
            Some(Duration::from_secs_f64(59.812))
        );
        assert_eq!(
            parse_compact_duration("250ms"),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn compact_duration_garbage_is_none() {
        assert_eq!(parse_compact_duration(""), None);
        assert_eq!(parse_compact_duration("eventually"), None);
        assert_eq!(parse_compact_duration("5x"), None);
        assert_eq!(parse_compact_duration("12"), None);
    }

    #[test]
    fn rfc3339_epoch() {
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_epoch("2026-06-04T07:13:19Z"),
            Some(1_780_557_199)
        );
        assert_eq!(
            parse_rfc3339_epoch("2026-06-04T07:13:19.250Z"),
            Some(1_780_557_199)
        );
        // Offset is applied: 09:13:19+02:00 == 07:13:19Z.
        assert_eq!(
            parse_rfc3339_epoch("2026-06-04T09:13:19+02:00"),
            Some(1_780_557_199)
        );
    }

    #[test]
    fn rfc3339_garbage_is_none() {
        assert_eq!(parse_rfc3339_epoch("not a date"), None);
        assert_eq!(parse_rfc3339_epoch("2026-06-04"), None);
        assert_eq!(parse_rfc3339_epoch("2026-13-04T07:13:19Z"), None);
        assert_eq!(parse_rfc3339_epoch("2026-06-04T07:13:19"), None);
    }

    #[test]
    fn http_date_epoch() {
        assert_eq!(
            parse_http_date_epoch("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date_epoch("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
        assert_eq!(parse_http_date_epoch("06 Nov 1994"), None);
    }

    proptest::proptest! {
        // Parsers are total: adversarial input never panics.
        #[test]
        fn header_parsers_are_total(value in ".*") {
            let _ = parse_retry_after(&value, UNIX_EPOCH);
            let _ = parse_compact_duration(&value);
            let _ = parse_rfc3339_epoch(&value);
            let _ = parse_http_date_epoch(&value);
        }
    }
}
