//! Protocol timestamps: UTC, one-second resolution, `YYYY-MM-DDTHH:MM:SSZ`
//! (spec M-1).

use time::OffsetDateTime;

/// The current time in the protocol's format.
#[must_use]
pub fn utc_now() -> String {
    format_utc(OffsetDateTime::now_utc())
}

pub(crate) fn format_utc(t: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Parse a protocol timestamp into Unix seconds, or `None` if it is not one.
#[must_use]
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let month = time::Month::try_from(u8::try_from(mo).ok()?).ok()?;
    let date = time::Date::from_calendar_date(i32::try_from(y).ok()?, month, u8::try_from(d).ok()?)
        .ok()?;
    let tod = time::Time::from_hms(
        u8::try_from(h).ok()?,
        u8::try_from(mi).ok()?,
        u8::try_from(se).ok()?,
    )
    .ok()?;
    Some(date.with_time(tod).assume_utc().unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_the_protocol_shape() {
        let t = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let s = format_utc(t);
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(parse_utc(&s), Some(1_790_000_000));
        assert_eq!(parse_utc("2026-01-01T00:00:00Z"), Some(1_767_225_600));
    }

    #[test]
    fn rejects_anything_else() {
        for bad in [
            "",
            "2026-01-01 00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-01-01T00:00:00+00:00",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }
}
