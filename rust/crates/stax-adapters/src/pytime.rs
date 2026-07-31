//! Python `datetime` semantics over adapter timestamps.
//!
//! [`crate::pyval`] is the home for `str()` / `bool()` / `int()`; this is the
//! home for the other Python builtin these adapters lean on — `datetime`. Six
//! of the twenty providers normalise their timestamps rather than passing the
//! source string through (`copilot._coerce_iso`, `opencode._normalize_timestamp`,
//! `continue_adapter._coerce_timestamp`, `antigravity._to_iso`), and every one
//! of them does it with the same three primitives:
//!
//! * `datetime.fromtimestamp(x, tz=UTC).isoformat()` — [`from_timestamp_iso`],
//! * `datetime.fromisoformat(s).isoformat()` — [`isoformat_roundtrip`],
//! * `datetime.now(tz=UTC).isoformat()` — [`Clock`].
//!
//! Reimplementing those four times is how two ports drift, so they live here
//! once, with the CPython expression each mirrors quoted at the definition.
//!
//! ## Why `Clock` is a parameter
//!
//! Three adapters fall back to *now* when a row carries no parseable timestamp.
//! That value cannot be diffed against Python — two processes never agree on the
//! microsecond — so the fallback is pinned by unit test and kept out of the
//! parity fixtures rather than papered over. Injection (not `set_var`) is the
//! campaign's law: Rust 2024 makes `std::env::set_var` `unsafe` and the
//! workspace forbids `unsafe`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Where "now" comes from for the `datetime.now(tz=UTC)` fallbacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Clock {
    /// The system clock, exactly as `datetime.now(tz=UTC)` reads it.
    #[default]
    Live,
    /// A pinned instant, injected by a test or the parity harness.
    Fixed(SystemTime),
}

impl Clock {
    /// `datetime.now(tz=UTC).isoformat()`.
    ///
    /// CPython rounds the system clock to microseconds half-to-even before
    /// building the `datetime`, so this does too. A clock before the epoch (or
    /// past year 9999 — a machine with a broken RTC) renders as the empty
    /// string rather than panicking: an adapter must never fail on a timestamp.
    #[must_use]
    pub fn now_iso(self) -> String {
        let now = match self {
            Self::Live => SystemTime::now(),
            Self::Fixed(instant) => instant,
        };
        let (secs, nanos) = match now.duration_since(UNIX_EPOCH) {
            Ok(delta) => (
                i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
                i64::from(delta.subsec_nanos()),
            ),
            Err(err) => {
                let delta = err.duration();
                let secs = -i64::try_from(delta.as_secs()).unwrap_or(i64::MAX);
                let nanos = i64::from(delta.subsec_nanos());
                if nanos == 0 {
                    (secs, 0)
                } else {
                    (secs - 1, 1_000_000_000 - nanos)
                }
            }
        };
        #[allow(
            clippy::cast_precision_loss,
            reason = "the half-even rounding below only needs the sub-second part"
        )]
        let micros = round_half_even(nanos as f64 / 1000.0) as i64;
        let (secs, micros) = if micros >= 1_000_000 {
            (secs + 1, micros - 1_000_000)
        } else {
            (secs, micros)
        };
        stamp_from_epoch(secs, micros).map_or_else(String::new, |stamp| stamp.render())
    }
}

/// `datetime.fromtimestamp(seconds, tz=UTC).isoformat()`, or `None` for the
/// `OverflowError` / `OSError` / `ValueError` branch every caller catches.
///
/// The microsecond split is CPython's `datetime._fromtimestamp`:
///
/// ```text
/// frac, t = _math.modf(t)
/// us = round(frac * 1e6)
/// if us >= 1000000: t += 1; us -= 1000000
/// elif us < 0:      t -= 1; us += 1000000
/// ```
///
/// `round()` there is Python's, i.e. half-to-even — [`round_half_even`]. Rust's
/// `f64::round` is half-away-from-zero and would land a microsecond off on
/// exact `.5` inputs.
#[must_use]
pub fn from_timestamp_iso(seconds: f64) -> Option<String> {
    if !seconds.is_finite() {
        return None;
    }
    let whole = seconds.trunc();
    let frac = seconds - whole;
    let micros = round_half_even(frac * 1e6);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "range-checked by `stamp_from_epoch`, which rejects anything \
        outside years 1..=9999"
    )]
    let (mut whole, mut micros) = (whole as i64, micros as i64);
    if micros >= 1_000_000 {
        whole += 1;
        micros -= 1_000_000;
    } else if micros < 0 {
        whole -= 1;
        micros += 1_000_000;
    }
    stamp_from_epoch(whole, micros).map(|stamp| stamp.render())
}

/// [`from_timestamp_iso`] for a whole number of seconds — `antigravity._to_iso`.
#[must_use]
pub fn from_timestamp_secs_iso(seconds: i64) -> Option<String> {
    stamp_from_epoch(seconds, 0).map(|stamp| stamp.render())
}

/// `datetime.fromisoformat(text).isoformat()`, with a naive result pinned to
/// UTC — the shape all three normalising adapters use:
///
/// ```python
/// dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
/// if dt.tzinfo is None:
///     dt = dt.replace(tzinfo=UTC)
/// return dt.isoformat()
/// ```
///
/// `None` is the `ValueError` branch.
///
/// **DIVERGENCE (recorded, narrow).** CPython 3.11 widened `fromisoformat` to
/// most of ISO 8601. This port accepts the calendar-date forms every adapter has
/// ever seen on disk — `YYYY-MM-DD` and `YYYYMMDD`, an optional single-character
/// separator, `HH[:MM[:SS[.f…]]]` in extended or basic form, and a `Z` /
/// `±HH[:MM[:SS[.ffffff]]]` offset — and rejects the week-date (`2026-W01-1`)
/// and ordinal-date (`2026-115`) forms, which no session format writes. A
/// rejection is not a crash on either side: the callers fall through to their
/// float-parse and then to *now*.
#[must_use]
pub fn isoformat_roundtrip(text: &str) -> Option<String> {
    // The `.replace("Z", "+00:00")` is textual and global in Python — a `Z`
    // anywhere becomes an offset, which is why this replaces before parsing
    // rather than treating a trailing `Z` as a special case.
    let text = text.replace('Z', "+00:00");
    let mut stamp = parse_isoformat(&text)?;
    if stamp.offset_seconds.is_none() {
        stamp.offset_seconds = Some(0);
    }
    Some(stamp.render())
}

/// One instant, decomposed the way `datetime.isoformat()` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micro: u32,
    /// `None` is a naive datetime — no offset is printed at all.
    offset_seconds: Option<i32>,
}

impl Stamp {
    /// `datetime.isoformat()`: `YYYY-MM-DDTHH:MM:SS[.ffffff][±HH:MM[:SS]]`.
    ///
    /// The fractional part appears only when `microsecond` is non-zero, and the
    /// offset grows a `:SS` field only when the offset is not a whole number of
    /// minutes — both are `_format_offset` / `isoformat`'s own rules.
    fn render(&self) -> String {
        let Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro,
            offset_seconds,
        } = *self;
        let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if micro != 0 {
            out.push_str(&format!(".{micro:06}"));
        }
        if let Some(offset) = offset_seconds {
            let sign = if offset < 0 { '-' } else { '+' };
            let magnitude = offset.unsigned_abs();
            let (hours, rest) = (magnitude / 3600, magnitude % 3600);
            let (minutes, seconds) = (rest / 60, rest % 60);
            out.push_str(&format!("{sign}{hours:02}:{minutes:02}"));
            if seconds != 0 {
                out.push_str(&format!(":{seconds:02}"));
            }
        }
        out
    }
}

/// Epoch seconds + microseconds → a UTC [`Stamp`], or `None` outside
/// `datetime`'s year range (the `ValueError` / `OverflowError` branch).
fn stamp_from_epoch(seconds: i64, micros: i64) -> Option<Stamp> {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "secs_of_day is 0..86_400 and micros is 0..1_000_000 by construction"
    )]
    Some(Stamp {
        year,
        month,
        day,
        hour: (secs_of_day / 3600) as u32,
        minute: ((secs_of_day % 3600) / 60) as u32,
        second: (secs_of_day % 60) as u32,
        micro: micros as u32,
        offset_seconds: Some(0),
    })
}

/// Days since the Unix epoch → `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, the same algorithm CPython's `datetime`
/// uses in C. [`crate::pyval`] carries the identical routine for the
/// millisecond path; both are twelve lines of arithmetic with no state, so a
/// shared home would buy nothing but a cross-module dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "m is 1..=12 and d is 1..=31 by construction"
    )]
    (year, m as u32, d as u32)
}

/// Python's `round()`: half-to-even, unlike Rust's half-away-from-zero.
///
/// `f64::round` would put `0.5` at `1.0` and `2.5` at `3.0`; CPython puts both
/// on the even neighbour. It matters at exactly one input class — a timestamp
/// whose sub-second part lands on a half microsecond — and it is one line of
/// arithmetic to get right, so it is got right.
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if diff > 0.5 {
        return floor + 1.0;
    }
    if diff < 0.5 {
        return floor;
    }
    // Exactly halfway: take whichever neighbour is even.
    if (floor / 2.0).fract() == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

/// The `fromisoformat` subset this port accepts — see [`isoformat_roundtrip`].
fn parse_isoformat(text: &str) -> Option<Stamp> {
    let bytes = text.as_bytes();
    // Extended `YYYY-MM-DD` first, then basic `YYYYMMDD`; anything else is one
    // of the forms this port deliberately rejects.
    let (year, month, day, mut rest) = if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-'
    {
        (
            digits(&text[0..4])?,
            digits(&text[5..7])?,
            digits(&text[8..10])?,
            &text[10..],
        )
    } else if bytes.len() >= 8 && text[..8].bytes().all(|byte| byte.is_ascii_digit()) {
        (
            digits(&text[0..4])?,
            digits(&text[4..6])?,
            digits(&text[6..8])?,
            &text[8..],
        )
    } else {
        return None;
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(i64::from(year), month) {
        return None;
    }

    let mut stamp = Stamp {
        year: i64::from(year),
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        micro: 0,
        offset_seconds: None,
    };
    if rest.is_empty() {
        return Some(stamp);
    }
    // CPython accepts *any* single character between the date and the time.
    rest = &rest[rest.chars().next()?.len_utf8()..];

    // Split the UTC offset off the tail before parsing the clock.
    let (clock, offset) = match rest.rfind(['+', '-']) {
        Some(index) => (&rest[..index], Some(&rest[index..])),
        None => (rest, None),
    };
    if let Some(offset) = offset {
        stamp.offset_seconds = Some(parse_offset(offset)?);
    }
    parse_clock(clock, &mut stamp)?;
    Some(stamp)
}

/// `HH[:MM[:SS[.f…]]]`, extended or basic, into `stamp`.
fn parse_clock(clock: &str, stamp: &mut Stamp) -> Option<()> {
    if clock.is_empty() {
        return Some(());
    }
    let (head, fraction) = match clock.split_once(['.', ',']) {
        Some((head, fraction)) => (head, Some(fraction)),
        None => (clock, None),
    };
    let parts: Vec<&str> = if head.contains(':') {
        head.split(':').collect()
    } else {
        // Basic format packs the fields two digits at a time.
        match head.len() {
            2 => vec![head],
            4 => vec![&head[0..2], &head[2..4]],
            6 => vec![&head[0..2], &head[2..4], &head[4..6]],
            _ => return None,
        }
    };
    if parts.len() > 3 {
        return None;
    }
    stamp.hour = digits(parts[0])?;
    if let Some(part) = parts.get(1) {
        stamp.minute = digits(part)?;
    }
    if let Some(part) = parts.get(2) {
        stamp.second = digits(part)?;
    }
    if stamp.hour > 23 || stamp.minute > 59 || stamp.second > 59 {
        return None;
    }
    if let Some(fraction) = fraction {
        // 3.11+ takes any number of digits and truncates past microseconds.
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // Right-pad to six digits (`.5` is 500000 µs), then stop — digits past
        // the sixth are truncated, not rounded.
        let mut micro = 0_u32;
        for index in 0..6 {
            let digit = fraction.as_bytes().get(index).map_or(0, |byte| byte - b'0');
            micro = micro * 10 + u32::from(digit);
        }
        stamp.micro = micro;
    }
    Some(())
}

/// `±HH[:MM[:SS[.ffffff]]]` → signed seconds.
fn parse_offset(text: &str) -> Option<i32> {
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &text[1..];
    // Sub-second offsets exist in the ISO grammar and in nobody's session log;
    // they are parsed and dropped, exactly as a whole-second offset would be.
    let body = body.split(['.', ',']).next()?;
    let parts: Vec<&str> = if body.contains(':') {
        body.split(':').collect()
    } else {
        match body.len() {
            2 => vec![body],
            4 => vec![&body[0..2], &body[2..4]],
            6 => vec![&body[0..2], &body[2..4], &body[4..6]],
            _ => return None,
        }
    };
    if parts.len() > 3 {
        return None;
    }
    let hours = digits(parts[0])?;
    let minutes = parts.get(1).map_or(Some(0), |part| digits(part))?;
    let seconds = parts.get(2).map_or(Some(0), |part| digits(part))?;
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total = i32::try_from(hours * 3600 + minutes * 60 + seconds).ok()?;
    // `timezone` rejects anything at or past 24 hours.
    if total >= 24 * 3600 {
        return None;
    }
    Some(sign * total)
}

/// A run of ASCII digits as a `u32`, or `None` — the parser never guesses.
fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days in `month` of `year`, proleptic Gregorian.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn from_timestamp_matches_cpython_on_the_shapes_adapters_produce() {
        // The opencode fixture's `time_created / 1000`.
        assert_eq!(
            from_timestamp_iso(1_745_596_801.0).as_deref(),
            Some("2025-04-25T16:00:01+00:00")
        );
        assert_eq!(
            from_timestamp_iso(0.0).as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        // Sub-second precision survives as microseconds.
        assert_eq!(
            from_timestamp_iso(1.5).as_deref(),
            Some("1970-01-01T00:00:01.500000+00:00")
        );
        // Pre-epoch borrows a second, as CPython's `us < 0` branch does.
        assert_eq!(
            from_timestamp_iso(-0.5).as_deref(),
            Some("1969-12-31T23:59:59.500000+00:00")
        );
        // The OverflowError / ValueError branch.
        assert_eq!(from_timestamp_iso(1e30), None);
        assert_eq!(from_timestamp_iso(f64::NAN), None);
        assert_eq!(from_timestamp_iso(f64::INFINITY), None);
    }

    #[test]
    fn whole_second_timestamps_print_without_a_fraction() {
        assert_eq!(
            from_timestamp_secs_iso(1_745_596_800).as_deref(),
            Some("2025-04-25T16:00:00+00:00")
        );
        assert_eq!(from_timestamp_secs_iso(i64::MAX), None);
    }

    #[test]
    fn round_half_even_is_pythons_round_not_rusts() {
        assert_eq!(round_half_even(0.5), 0.0);
        assert_eq!(round_half_even(1.5), 2.0);
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(-0.5), -0.0);
        assert_eq!(round_half_even(-1.5), -2.0);
        assert_eq!(round_half_even(2.4), 2.0);
        assert_eq!(round_half_even(2.6), 3.0);
    }

    #[test]
    fn isoformat_roundtrip_normalises_the_way_python_does() {
        // The fixture shape: a Z suffix becomes an explicit +00:00.
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00Z").as_deref(),
            Some("2026-04-25T14:00:00+00:00")
        );
        // A naive stamp is pinned to UTC by the callers' `replace(tzinfo=UTC)`.
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00").as_deref(),
            Some("2026-04-25T14:00:00+00:00")
        );
        // Fractional seconds pad to six digits.
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00.5Z").as_deref(),
            Some("2026-04-25T14:00:00.500000+00:00")
        );
        // …and truncate past six.
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00.1234567Z").as_deref(),
            Some("2026-04-25T14:00:00.123456+00:00")
        );
        // A non-UTC offset is preserved, not converted.
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00+05:30").as_deref(),
            Some("2026-04-25T14:00:00+05:30")
        );
        assert_eq!(
            isoformat_roundtrip("2026-04-25T14:00:00-0800").as_deref(),
            Some("2026-04-25T14:00:00-08:00")
        );
        // Date-only and basic forms.
        assert_eq!(
            isoformat_roundtrip("2026-04-25").as_deref(),
            Some("2026-04-25T00:00:00+00:00")
        );
        assert_eq!(
            isoformat_roundtrip("20260425").as_deref(),
            Some("2026-04-25T00:00:00+00:00")
        );
        // A space separator is as valid as `T`.
        assert_eq!(
            isoformat_roundtrip("2026-04-25 14:00:00").as_deref(),
            Some("2026-04-25T14:00:00+00:00")
        );
    }

    #[test]
    fn free_text_and_impossible_dates_are_the_value_error_branch() {
        assert_eq!(isoformat_roundtrip(""), None);
        assert_eq!(isoformat_roundtrip("not a date"), None);
        assert_eq!(isoformat_roundtrip("2026-13-01"), None);
        assert_eq!(isoformat_roundtrip("2026-02-30"), None);
        assert_eq!(isoformat_roundtrip("2026-04-25T25:00:00"), None);
        assert_eq!(isoformat_roundtrip("2026-04-25T14:00:00+banana"), None);
        assert_eq!(isoformat_roundtrip("2026-04-25T14:00:00."), None);
        // Leap day, both ways.
        assert!(isoformat_roundtrip("2024-02-29").is_some());
        assert_eq!(isoformat_roundtrip("2026-02-29"), None);
        assert_eq!(isoformat_roundtrip("1900-02-29"), None);
        assert!(isoformat_roundtrip("2000-02-29").is_some());
    }

    #[test]
    fn a_pinned_clock_renders_the_now_fallback_deterministically() {
        let clock = Clock::Fixed(UNIX_EPOCH + Duration::new(1_745_596_801, 123_456_000));
        assert_eq!(clock.now_iso(), "2025-04-25T16:00:01.123456+00:00");
        // A clock exactly on the second prints no fraction, as isoformat does.
        let clock = Clock::Fixed(UNIX_EPOCH + Duration::new(1_745_596_801, 0));
        assert_eq!(clock.now_iso(), "2025-04-25T16:00:01+00:00");
        // The live clock is only asserted to be well-shaped.
        let live = Clock::default().now_iso();
        assert!(crate::contract::is_iso_8601(&live), "{live}");
    }
}
