//! Value helpers of the data path (3.3 / 3.4): the TIMESTAMP minimum per session offset, UTC
//! formatting of epochs and the calendar classification of MySQL temporal text. Owner:
//! copy-data.

use chrono::{DateTime, NaiveDate};

/// Parse a session offset (`+09:00`, `-05:00`, `−05:00` with U+2212, `+0530`, `UTC`, `SYSTEM`
/// = 0) into seconds east of UTC. `None` for anything else.
pub fn parse_offset(offset: &str) -> Option<i64> {
    let t = offset.trim().replace('\u{2212}', "-");
    if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("system") {
        return Some(0);
    }
    let (sign, rest) = match t.as_bytes().first()? {
        b'+' => (1, &t[1..]),
        b'-' => (-1, &t[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None => (rest, "0"),
    };
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    if h > 14 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

/// The minimum TIMESTAMP literal for a session offset: the instant `1970-01-01 00:00:01` UTC
/// shown in that zone ('+00:00' -> '1970-01-01 00:00:01', '+09:00' -> '1970-01-01 09:00:01',
/// '-05:00' -> '1969-12-31 19:00:01'). BulkDB always calls it with '+00:00' (every target
/// session runs under UTC). An unparsable offset is treated as UTC.
pub fn timestamp_min_literal(offset: &str) -> String {
    let secs = 1 + parse_offset(offset).unwrap_or(0);
    format_epoch_utc(secs).unwrap_or_else(|| "1970-01-01 00:00:01".to_string())
}

/// `YYYY-MM-DD HH:MM:SS` of a UNIX epoch (seconds) in UTC.
pub fn format_epoch_utc(secs: i64) -> Option<String> {
    DateTime::from_timestamp(secs, 0).map(|d| d.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` of microseconds since the epoch (UTC / naive).
pub fn format_micros(micros: i64) -> Option<String> {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let base = format_epoch_utc(secs)?;
    Some(if frac == 0 { base } else { format!("{base}.{frac:06}") })
}

/// Calendar class of a MySQL DATE / DATETIME / TIMESTAMP text value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateClass {
    /// A real calendar date (time part, if any, in range).
    Valid,
    /// `0000-00-00[ 00:00:00]`.
    Zero,
    /// Month or day is 0 but not all of the date (`2020-00-15`, `2020-05-00`).
    PartialZero,
    /// Month 1-12 and day 1-31 but not a real date (`2004-02-31`), or a time out of range.
    Invalid,
    /// Not a date text at all.
    Unparsable,
}

/// Split `YYYY-MM-DD[ HH:MM:SS[.f]]` into its parts.
pub fn split_date(text: &str) -> Option<(i32, u32, u32, &str)> {
    let t = text.trim();
    let (date, time) = match t.find([' ', 'T']) {
        Some(i) => (&t[..i], t[i + 1..].trim()),
        None => (t, ""),
    };
    let mut p = date.split('-');
    let y: i32 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    let d: u32 = p.next()?.parse().ok()?;
    if p.next().is_some() {
        return None;
    }
    Some((y, m, d, time))
}

fn time_ok(time: &str) -> bool {
    if time.is_empty() {
        return true;
    }
    let main = time.split('.').next().unwrap_or("");
    let parts: Vec<&str> = main.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse::<u32>().ok()).collect();
    match nums {
        Some(n) => n.first().is_some_and(|h| *h < 24) && n.get(1).is_none_or(|m| *m < 60) && n.get(2).is_none_or(|s| *s < 60),
        None => false,
    }
}

fn time_is_zero(time: &str) -> bool {
    time.chars().all(|c| c == '0' || c == ':' || c == '.')
}

/// Classify a temporal text (3.4).
pub fn classify_date(text: &str) -> DateClass {
    let Some((y, m, d, time)) = split_date(text) else {
        return DateClass::Unparsable;
    };
    if y == 0 && m == 0 && d == 0 {
        return if time_is_zero(time) { DateClass::Zero } else { DateClass::PartialZero };
    }
    if m == 0 || d == 0 {
        return if m <= 12 && d <= 31 { DateClass::PartialZero } else { DateClass::Invalid };
    }
    if m > 12 || d > 31 || NaiveDate::from_ymd_opt(y, m, d).is_none() || !time_ok(time) {
        return DateClass::Invalid;
    }
    DateClass::Valid
}

/// `min` policy for a partial zero date: every 00 part of the date becomes 01
/// ('2020-05-00' -> '2020-05-01', '2020-00-00' -> '2020-01-01'); the time part is kept.
pub fn fill_zero_parts(text: &str) -> Option<String> {
    let (y, m, d, time) = split_date(text)?;
    let date = format!("{:04}-{:02}-{:02}", y.max(0), m.max(1), d.max(1));
    Some(if time.is_empty() { date } else { format!("{date} {time}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_minimum_per_offset() {
        assert_eq!(timestamp_min_literal("+00:00"), "1970-01-01 00:00:01");
        assert_eq!(timestamp_min_literal("+09:00"), "1970-01-01 09:00:01");
        assert_eq!(timestamp_min_literal("-05:00"), "1969-12-31 19:00:01");
        assert_eq!(timestamp_min_literal("\u{2212}05:00"), "1969-12-31 19:00:01");
        assert_eq!(timestamp_min_literal("+05:30"), "1970-01-01 05:30:01");
        assert_eq!(timestamp_min_literal("UTC"), "1970-01-01 00:00:01");
        assert_eq!(timestamp_min_literal("bogus"), "1970-01-01 00:00:01");
        assert_eq!(parse_offset("+0930"), Some(9 * 3600 + 1800));
    }

    #[test]
    fn epoch_and_micros_formatting() {
        assert_eq!(format_epoch_utc(0).unwrap(), "1970-01-01 00:00:00");
        assert_eq!(format_epoch_utc(1_086_048_000).unwrap(), "2004-06-01 00:00:00");
        assert_eq!(format_micros(1_500_000).unwrap(), "1970-01-01 00:00:01.500000");
        assert_eq!(format_micros(2_000_000).unwrap(), "1970-01-01 00:00:02");
    }

    #[test]
    fn date_classes() {
        assert_eq!(classify_date("2004-02-29"), DateClass::Valid);
        assert_eq!(classify_date("2004-02-29 23:59:59"), DateClass::Valid);
        assert_eq!(classify_date("0000-00-00"), DateClass::Zero);
        assert_eq!(classify_date("0000-00-00 00:00:00"), DateClass::Zero);
        assert_eq!(classify_date("2020-00-15"), DateClass::PartialZero);
        assert_eq!(classify_date("2020-05-00 10:00:00"), DateClass::PartialZero);
        assert_eq!(classify_date("2004-02-31"), DateClass::Invalid);
        assert_eq!(classify_date("2004-04-31"), DateClass::Invalid);
        assert_eq!(classify_date("2003-02-29"), DateClass::Invalid);
        assert_eq!(classify_date("2004-01-01 25:00:00"), DateClass::Invalid);
        assert_eq!(classify_date("hello"), DateClass::Unparsable);
        assert_eq!(fill_zero_parts("2020-05-00").unwrap(), "2020-05-01");
        assert_eq!(fill_zero_parts("2020-00-00 10:11:12").unwrap(), "2020-01-01 10:11:12");
    }
}
