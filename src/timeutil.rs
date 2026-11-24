//! Time arithmetic on plain `i64` microseconds since the Unix epoch (UTC).
//!
//! Civil-date math is done with integers, so it is allocation-free and independent of the
//! local time zone.

use std::fmt;

pub const US_PER_SEC: i64 = 1_000_000;
pub const US_PER_MIN: i64 = 60 * US_PER_SEC;
pub const US_PER_HOUR: i64 = 60 * US_PER_MIN;
pub const US_PER_DAY: i64 = 24 * US_PER_HOUR;

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
pub const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub const fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Day index (days since the Unix epoch) containing instant `us`.
#[inline]
pub fn day_of(us: i64) -> i64 {
    us.div_euclid(US_PER_DAY)
}

/// RFC 3339 / ISO 8601 with microseconds and a `Z` suffix.
#[derive(Clone, Copy)]
pub struct Rfc3339(pub i64);

impl fmt::Display for Rfc3339 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = day_of(self.0);
        let (y, mo, d) = civil_from_days(days);
        let rem = self.0 - days * US_PER_DAY;
        let h = rem / US_PER_HOUR;
        let mi = (rem % US_PER_HOUR) / US_PER_MIN;
        let s = (rem % US_PER_MIN) / US_PER_SEC;
        let us = rem % US_PER_SEC;
        write!(f, "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{us:06}Z")
    }
}

/// `YYYY-MM-DD` of a day index.
#[derive(Clone, Copy)]
pub struct Ymd(pub i64);

impl fmt::Display for Ymd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = civil_from_days(self.0);
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// `YYYY-MM-DD HH:MM:SS.ffffff`, the form SQL engines accept as a timestamp literal.
#[derive(Clone, Copy)]
pub struct SqlTimestamp(pub i64);

impl fmt::Display for SqlTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = Rfc3339(self.0).to_string();
        write!(f, "{} {}", &s[..10], &s[11..26])
    }
}

/// `HH:MM:SS` for a non-negative duration in seconds, hours unbounded.
pub fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
}

/// Parses `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS[.f]`, or an RFC 3339 instant into Unix
/// microseconds. Instants without an offset are taken as UTC.
pub fn parse_instant(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let bad = || format!("'{s}' is not a date (YYYY-MM-DD) or a timestamp (YYYY-MM-DD HH:MM:SS, RFC 3339)");
    if t.len() == 10 {
        let (y, m, d) = parse_ymd(t).ok_or_else(bad)?;
        return Ok(days_from_civil(y, m, d) * US_PER_DAY);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Ok(dt.timestamp_micros());
    }
    if t.len() < 19 {
        return Err(bad());
    }
    let (y, m, d) = parse_ymd(&t[..10]).ok_or_else(bad)?;
    let sep = t.as_bytes()[10];
    if sep != b' ' && sep != b'T' {
        return Err(bad());
    }
    let rest = t[11..].trim_end_matches('Z');
    let (hms, frac) = match rest.split_once('.') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    let num = |p: &str, max: i64| p.parse::<i64>().ok().filter(|v| (0..=max).contains(v));
    let (h, mi, sec) = (num(parts[0], 23).ok_or_else(bad)?, num(parts[1], 59).ok_or_else(bad)?, num(parts[2], 60).ok_or_else(bad)?);
    let mut us = 0i64;
    if !frac.is_empty() {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let digits: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
        us = digits.parse().map_err(|_| bad())?;
    }
    Ok(days_from_civil(y, m, d) * US_PER_DAY + h * US_PER_HOUR + mi * US_PER_MIN + sec * US_PER_SEC + us)
}

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// Parses a duration such as `90s`, `15m`, `2h`, `3d`, `1w` or `500ms` into microseconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("'{s}' is not a duration such as 30s, 15m, 2h or 3d"))?;
    let unit_us = match unit.trim() {
        "us" => 1,
        "ms" => 1_000,
        "s" | "" => US_PER_SEC,
        "m" | "min" => US_PER_MIN,
        "h" => US_PER_HOUR,
        "d" => US_PER_DAY,
        "w" => 7 * US_PER_DAY,
        other => return Err(format!("unknown duration unit '{other}' in '{s}' (use us, ms, s, m, h, d or w)")),
    };
    Ok((n * unit_us as f64).round() as i64)
}

/// Current instant truncated to 00:00 UTC.
pub fn today_utc_midnight() -> i64 {
    let now = chrono::Utc::now().timestamp_micros();
    day_of(now) * US_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for z in [-800_000i64, -1, 0, 1, 10_957, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn rfc3339_matches_chrono() {
        for us in [0i64, 1_761_820_649_552_451, 951_782_400_000_001, -1] {
            let ours = Rfc3339(us).to_string();
            let theirs = chrono::DateTime::from_timestamp_micros(us).unwrap().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
            assert_eq!(ours, theirs);
        }
        assert_eq!(SqlTimestamp(1).to_string(), "1970-01-01 00:00:00.000001");
    }

    #[test]
    fn parse_accepts_dates_and_instants() {
        assert_eq!(parse_instant("1970-01-02").unwrap(), US_PER_DAY);
        assert_eq!(parse_instant("1970-01-01T00:00:01Z").unwrap(), US_PER_SEC);
        assert_eq!(parse_instant("1970-01-01T01:00:00+01:00").unwrap(), 0);
        assert_eq!(parse_instant("1970-01-01 00:00:01.5").unwrap(), 1_500_000);
        assert_eq!(parse_instant("1970-01-01 00:01:00").unwrap(), US_PER_MIN);
        assert!(parse_instant("yesterday").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s").unwrap(), 90 * US_PER_SEC);
        assert_eq!(parse_duration("2h").unwrap(), 2 * US_PER_HOUR);
        assert_eq!(parse_duration("1.5m").unwrap(), 90 * US_PER_SEC);
        assert!(parse_duration("2 fortnights").is_err());
        assert_eq!(format_duration(3725.0), "01:02:05");
    }
}
