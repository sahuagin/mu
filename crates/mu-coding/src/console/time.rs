//! mu-core-time-module-2cef: one home for the console's dependency-free
//! civil-time helpers.
//!
//! Everything the console renders speaks RFC 3339 / ISO 8601 UTC. The cc
//! scanner parses `…Z` transcript timestamps into epoch milliseconds
//! ([`parse_rfc3339_ms`], used by `cc_data`); the mark sidecar formats
//! "now" into the `…+00:00` shape its task_log rows use
//! ([`now_rfc3339_utc`]/[`format_rfc3339_utc`], used by `mark`); and the
//! native event view turns epoch-ms back into an ISO string for the
//! browser to localize (`html`). All of it rests on Howard Hinnant's two
//! civil-date primitives — [`days_from_civil`] and its exact inverse
//! [`civil_from_days`] — collected here so the pair (and the parse/format
//! helpers built on them) round-trip by construction instead of drifting
//! across three separate points of use.
//!
//! Deliberately chrono-free: these are a handful of integer ops over the
//! proleptic Gregorian calendar, and mu-console keeps its dependency
//! surface small. The algorithms are Howard Hinnant's, verbatim:
//! <https://howardhinnant.github.io/date_algorithms.html>.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since the Unix epoch (1970-01-01) for a civil `(y, m, d)` date,
/// via Howard Hinnant's `days_from_civil`. Valid across the proleptic
/// Gregorian calendar; the console only ever feeds it post-1970 dates.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Civil date `(year, month, day)` from days since the Unix epoch — Howard
/// Hinnant's `civil_from_days`, the exact inverse of [`days_from_civil`].
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse an RFC 3339 / ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS`,
/// optional `.fff` fraction, optional `Z`) into epoch milliseconds.
/// Returns `None` on any malformation — cc always emits `…Z` UTC, so we
/// only handle the `Z`/no-offset case and ignore non-UTC offsets rather
/// than mis-parse them. Dependency-free (no chrono in this crate).
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Strip a trailing 'Z'; reject explicit non-UTC offsets to avoid
    // silently dropping them.
    let time = rest.strip_suffix('Z').unwrap_or(rest);
    if time.contains('+') || time.contains('-') {
        return None;
    }

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    let mut t = hms.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Fraction → milliseconds (first 3 digits, zero-padded).
    let millis: i64 = match frac {
        Some(f) => {
            let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            let mut ms = digits;
            ms.truncate(3);
            while ms.len() < 3 {
                ms.push('0');
            }
            ms.parse().ok()?
        }
        None => 0,
    };

    let days = days_from_civil(year, month, day);
    let total_ms = (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis;
    u64::try_from(total_ms).ok()
}

/// Format epoch seconds as `YYYY-MM-DDTHH:MM:SS+00:00`.
pub(crate) fn format_rfc3339_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

/// Format "now" as `YYYY-MM-DDTHH:MM:SS+00:00` — the RFC3339 UTC shape the
/// task_log rows use. Dependency-free (no chrono in this crate).
pub(crate) fn now_rfc3339_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_rfc3339_utc(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_to_epoch_ms() {
        // 1970-01-01T00:00:00Z == 0.
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        // Known epoch: 2026-06-06T07:31:19.771Z.
        // days_from_civil(2026,6,6) computed by the same algorithm.
        let days = days_from_civil(2026, 6, 6);
        let expect = (((days * 24 + 7) * 60 + 31) * 60 + 19) * 1000 + 771;
        assert_eq!(
            parse_rfc3339_ms("2026-06-06T07:31:19.771Z"),
            Some(expect as u64)
        );
        // Without fractional seconds, without Z.
        assert_eq!(
            parse_rfc3339_ms("2026-06-06T07:31:19"),
            Some(((days_from_civil(2026, 6, 6) * 24 + 7) * 60 + 31) as u64 * 60_000 + 19_000)
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert_eq!(parse_rfc3339_ms("not-a-date"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None); // bad month
        assert_eq!(parse_rfc3339_ms("2026-06-06T25:00:00Z"), None); // bad hour
        assert_eq!(parse_rfc3339_ms(""), None);
        // Non-UTC offset: refused rather than mis-parsed.
        assert_eq!(parse_rfc3339_ms("2026-06-06T07:31:19+02:00"), None);
    }

    #[test]
    fn rfc3339_formats_known_epochs() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00+00:00");
        // 1700000000 == 2023-11-14T22:13:20Z (a well-known round number).
        assert_eq!(
            format_rfc3339_utc(1_700_000_000),
            "2023-11-14T22:13:20+00:00"
        );
        // Leap-day boundary: 2024-02-29 exists; 2024-03-01 is the next day.
        assert_eq!(
            format_rfc3339_utc(1_709_164_800),
            "2024-02-29T00:00:00+00:00"
        );
        assert_eq!(
            format_rfc3339_utc(1_709_251_200),
            "2024-03-01T00:00:00+00:00"
        );
    }

    /// The property that seals the consolidation: `days_from_civil` and
    /// `civil_from_days` are exact inverses. Sweeping a wide day range
    /// either side of the epoch (≈ 1696 .. 2517) catches any drift between
    /// the two algorithms that point-tests would miss.
    #[test]
    fn civil_date_pair_round_trips() {
        for days in (-100_000..=200_000).step_by(3) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(
                days_from_civil(y, m as i64, d as i64),
                days,
                "round-trip failed at day offset {days} → {y:04}-{m:02}-{d:02}"
            );
        }
    }
}
