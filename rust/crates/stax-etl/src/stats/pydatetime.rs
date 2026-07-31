//! `aggregator._parse_ts` / `_local_day` / `_local_hour` — and why
//! `stax_core::queries::pytime::parse_iso` cannot be reused for them.
//!
//! `pytime::parse_iso` returns a UTC epoch: it *applies* the offset. The
//! aggregator never converts. `_local_day` is
//!
//! ```text
//! (_parse_ts(ts) + timedelta(minutes=offset)).strftime("%Y-%m-%d")
//! ```
//!
//! and `strftime` on an aware `datetime` reads the **wall clock as written**,
//! offset and all. `2026-01-01T23:00:00-08:00` is day `2026-01-01` here and
//! `2026-01-02` after a UTC normalisation. Every timestamp on the maintainer's
//! store carries `+00:00` so the two agree there today, but the difference is
//! in the contract, not in the sample, and a codex/cursor project ingested from
//! a machine with a local-offset writer would split the daily buckets.
//!
//! So this module models what CPython models: a naive wall clock plus an
//! *optional* UTC offset. The offset participates in subtraction and in
//! comparison and in nothing else.
//!
//! # `TypeError` is a control-flow path
//!
//! `naive - aware` raises `TypeError` in CPython, and three call sites catch
//! exactly `(ValueError, TypeError)` and fall back — while `_trends` compares
//! `cur_start < t <= end` **outside** its `try`, so a mixed-awareness project
//! makes the whole `trends` section fall to `_empty_trends()` through
//! `_safe`. [`PyDateTime::sub_total_seconds`] and
//! [`PyDateTime::cmp_instant`] return `None` for the mixed case so callers can
//! reproduce each of those branches rather than silently picking one.

use std::fmt::Write as _;

/// A parsed timestamp: the wall clock as written, plus the offset if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyDateTime {
    /// Microseconds since 1970-01-01T00:00:00 **in the value's own frame** —
    /// i.e. `strftime` reads straight off this, no conversion.
    pub wall_us: i64,
    /// UTC offset in seconds east, or `None` for a naive value.
    pub offset_s: Option<i64>,
}

impl PyDateTime {
    /// The instant this names, for comparison and subtraction. Meaningless for
    /// a naive value, which is why every consumer checks awareness first.
    #[must_use]
    fn instant_us(self) -> i64 {
        self.wall_us - self.offset_s.unwrap_or(0) * 1_000_000
    }

    /// `(self - other).total_seconds()`, or `None` when CPython would raise
    /// `TypeError` for mixing a naive and an aware value.
    #[must_use]
    pub fn sub_total_seconds(self, other: Self) -> Option<f64> {
        if self.offset_s.is_some() != other.offset_s.is_some() {
            return None;
        }
        // `timedelta.total_seconds()` is `((days*86400 + seconds) * 10**6 +
        // microseconds) / 10**6` — one exact integer, one division.
        Some((self.instant_us() - other.instant_us()) as f64 / 1_000_000.0)
    }

    /// `self < other` / `self <= other`, or `None` for the mixed case CPython
    /// raises `TypeError` on. (CPython's `==` between mixed values is `False`
    /// rather than an error; only the ordering comparisons raise, and only
    /// ordering is used here.)
    #[must_use]
    pub fn cmp_instant(self, other: Self) -> Option<std::cmp::Ordering> {
        if self.offset_s.is_some() != other.offset_s.is_some() {
            return None;
        }
        Some(self.instant_us().cmp(&other.instant_us()))
    }

    /// `self + timedelta(minutes=minutes)`.
    #[must_use]
    pub fn plus_minutes(self, minutes: i64) -> Self {
        Self {
            wall_us: self
                .wall_us
                .saturating_add(minutes.saturating_mul(60_000_000)),
            offset_s: self.offset_s,
        }
    }

    /// `strftime("%Y-%m-%d")` on the wall clock.
    #[must_use]
    pub fn strftime_date(self) -> String {
        let (year, month, day, ..) = self.civil();
        let mut out = String::with_capacity(10);
        let _ = write!(out, "{year:04}-{month:02}-{day:02}");
        out
    }

    /// `.hour` on the wall clock.
    #[must_use]
    pub fn hour(self) -> i64 {
        let (_, _, _, hour, _, _) = self.civil();
        hour
    }

    fn civil(self) -> (i64, i64, i64, i64, i64, i64) {
        let seconds = self.wall_us.div_euclid(1_000_000);
        civil_from_epoch(seconds)
    }
}

/// `aggregator._parse_ts` — `datetime.fromisoformat(ts.replace("Z", "+00:00"))`.
///
/// `None` stands for the `ValueError` CPython raises, which the three
/// timestamp-arithmetic call sites catch.
///
/// # Coverage
///
/// CPython 3.11+ `fromisoformat` also accepts ISO week dates (`2026-W01-1`),
/// ordinal dates (`2026-001`) and a few compact spellings. Those are accepted
/// here for the basic and extended calendar forms; week and ordinal dates are
/// **not** — nothing has ever written one into `messages.timestamp` (the
/// adapters all emit `datetime.isoformat()`), and rejecting them puts them on
/// the same `None` path a malformed string takes, which is the conservative
/// direction: a day bucket is skipped rather than invented.
#[must_use]
pub fn parse_ts(ts: &str) -> Option<PyDateTime> {
    // `.replace("Z", "+00:00")` replaces EVERY "Z", not just a trailing one.
    let text = ts.replace('Z', "+00:00");
    parse_isoformat(&text)
}

fn parse_isoformat(text: &str) -> Option<PyDateTime> {
    let bytes = text.as_bytes();
    if !text.is_ascii() {
        return None;
    }
    // ── date ────────────────────────────────────────────────────────────
    let (year, month, day, date_len) = if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-'
    {
        (
            parse_uint(&text[0..4])?,
            parse_uint(&text[5..7])?,
            parse_uint(&text[8..10])?,
            10,
        )
    } else if bytes.len() >= 8 && bytes[..8].iter().all(u8::is_ascii_digit) {
        (
            parse_uint(&text[0..4])?,
            parse_uint(&text[4..6])?,
            parse_uint(&text[6..8])?,
            8,
        )
    } else {
        return None;
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);

    if bytes.len() == date_len {
        return Some(PyDateTime {
            wall_us: days * 86_400 * 1_000_000,
            offset_s: None,
        });
    }
    // CPython 3.11+ accepts ANY single character as the date/time separator.
    let rest = &text[date_len + 1..];

    // ── offset ──────────────────────────────────────────────────────────
    let (time_part, offset_s) = split_offset(rest)?;

    // ── time ────────────────────────────────────────────────────────────
    let (hour, minute, second, micros) = parse_time(time_part)?;
    let wall_us = (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000_000 + micros;
    Some(PyDateTime { wall_us, offset_s })
}

fn parse_time(part: &str) -> Option<(i64, i64, i64, i64)> {
    let (whole, frac) = match part.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => match part.split_once(',') {
            Some((w, f)) => (w, Some(f)),
            None => (part, None),
        },
    };
    let (hour, minute, second) = if whole.contains(':') {
        let mut fields = whole.split(':');
        let h = parse_uint(fields.next()?)?;
        let m = fields.next().map_or(Some(0), parse_uint)?;
        let s = fields.next().map_or(Some(0), parse_uint)?;
        if fields.next().is_some() {
            return None;
        }
        (h, m, s)
    } else {
        match whole.len() {
            2 => (parse_uint(whole)?, 0, 0),
            4 => (parse_uint(&whole[0..2])?, parse_uint(&whole[2..4])?, 0),
            6 => (
                parse_uint(&whole[0..2])?,
                parse_uint(&whole[2..4])?,
                parse_uint(&whole[4..6])?,
            ),
            _ => return None,
        }
    };
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let micros = match frac {
        None => 0,
        Some(digits) => {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // CPython truncates past six digits rather than rounding.
            let taken: String = digits.chars().take(6).collect();
            let scale = 10_i64.pow(6 - u32::try_from(taken.len()).ok()?);
            parse_uint(&taken)? * scale
        }
    };
    Some((hour, minute, second, micros))
}

/// Peel a trailing `+HH:MM` / `-HH:MM` (and the compact spellings) off a time
/// string. Returns the time part and the offset in seconds east.
fn split_offset(rest: &str) -> Option<(&str, Option<i64>)> {
    for (index, ch) in rest.char_indices() {
        if index == 0 {
            continue;
        }
        if ch == '+' || ch == '-' {
            let (time_part, offset_part) = rest.split_at(index);
            let sign = if ch == '-' { -1 } else { 1 };
            let magnitude = offset_magnitude_seconds(&offset_part[1..])?;
            return Some((time_part, Some(sign * magnitude)));
        }
    }
    Some((rest, None))
}

fn offset_magnitude_seconds(body: &str) -> Option<i64> {
    // A fractional offset is legal (`+01:00:00.000001`); it is also never
    // written by anything that reaches this store, and the aggregator only ever
    // subtracts two offsets of the same value. Truncating to whole seconds
    // would be a silent lie, so a fraction is rejected outright.
    let compact: String = body.chars().filter(|c| *c != ':').collect();
    if compact.is_empty() || !compact.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let field = |start: usize| parse_uint(compact.get(start..start + 2)?);
    let (h, m, s) = match compact.len() {
        2 => (field(0)?, 0, 0),
        4 => (field(0)?, field(2)?, 0),
        6 => (field(0)?, field(2)?, field(4)?),
        _ => return None,
    };
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(h * 3_600 + m * 60 + s)
}

fn parse_uint(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Hinnant's `civil_from_days`, plus the time of day.
fn civil_from_epoch(seconds: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = seconds.div_euclid(86_400);
    let tod = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, tod / 3_600, (tod % 3_600) / 60, tod % 60)
}

/// `aggregator._local_day`.
#[must_use]
pub fn local_day(ts: &str, offset_minutes: i64) -> Option<String> {
    if ts.is_empty() {
        return None;
    }
    Some(parse_ts(ts)?.plus_minutes(offset_minutes).strftime_date())
}

/// `aggregator._local_hour`.
#[must_use]
pub fn local_hour(ts: &str, offset_minutes: i64) -> Option<i64> {
    if ts.is_empty() {
        return None;
    }
    Some(parse_ts(ts)?.plus_minutes(offset_minutes).hour())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_shape_round_trips() {
        let dt = parse_ts("2025-11-15T06:53:30.900000+00:00").expect("parses");
        assert_eq!(dt.offset_s, Some(0));
        assert_eq!(dt.strftime_date(), "2025-11-15");
        assert_eq!(dt.hour(), 6);
    }

    #[test]
    fn strftime_reads_the_wall_clock_not_utc() {
        // The whole reason this module exists. A UTC-normalising parser calls
        // this 2026-01-02; CPython calls it 2026-01-01.
        assert_eq!(
            local_day("2026-01-01T23:00:00-08:00", 0).as_deref(),
            Some("2026-01-01")
        );
        assert_eq!(local_hour("2026-01-01T23:00:00-08:00", 0), Some(23));
    }

    #[test]
    fn the_offset_shifts_the_bucket_by_wall_minutes() {
        assert_eq!(
            local_day("2026-01-01T23:00:00+00:00", 120).as_deref(),
            Some("2026-01-02")
        );
        assert_eq!(
            local_day("2026-01-01T00:30:00+00:00", -60).as_deref(),
            Some("2025-12-31")
        );
        assert_eq!(local_hour("2026-01-01T23:00:00+00:00", 60), Some(0));
    }

    #[test]
    fn z_is_replaced_everywhere_not_just_at_the_end() {
        assert_eq!(
            parse_ts("2026-01-01T00:00:00Z"),
            parse_ts("2026-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn subtraction_is_in_seconds_and_mixed_awareness_is_none() {
        let a = parse_ts("2026-01-01T00:00:00+00:00").expect("parses");
        let b = parse_ts("2026-01-01T00:00:01.500000+00:00").expect("parses");
        assert_eq!(b.sub_total_seconds(a), Some(1.5));
        let naive = parse_ts("2026-01-01T00:00:00").expect("parses");
        assert_eq!(naive.offset_s, None);
        assert_eq!(b.sub_total_seconds(naive), None);
        assert_eq!(naive.cmp_instant(b), None);
        // Two aware values in different frames compare by instant.
        let east = parse_ts("2026-01-01T05:00:00+05:00").expect("parses");
        assert_eq!(east.cmp_instant(a), Some(std::cmp::Ordering::Equal));
    }

    #[test]
    fn malformed_values_are_none_not_a_panic() {
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("not-a-date"), None);
        assert_eq!(parse_ts("2026-02-30T00:00:00+00:00"), None);
        assert_eq!(parse_ts("2026-13-01"), None);
        assert_eq!(parse_ts("2026-01-01T25:00:00"), None);
        assert_eq!(local_day("", 0), None);
        assert_eq!(local_hour("garbage", 0), None);
        // A bare epoch-millis int rendered as a string — the shape the
        // `build_enriched_dataset` comment says the timestamp COLUMN exists to
        // keep out of here.
        assert_eq!(parse_ts("1763189610900"), None);
    }

    #[test]
    fn leap_years_and_the_compact_spellings() {
        assert!(parse_ts("2024-02-29T00:00:00+00:00").is_some());
        assert_eq!(parse_ts("2026-02-29T00:00:00+00:00"), None);
        assert_eq!(
            parse_ts("20260101T000000+0000"),
            parse_ts("2026-01-01T00:00:00+00:00")
        );
        // Fractional seconds truncate past six digits, they do not round.
        let dt = parse_ts("2026-01-01T00:00:00.1234569").expect("parses");
        assert_eq!(dt.wall_us % 1_000_000, 123_456);
    }
}
