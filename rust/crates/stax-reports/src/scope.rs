//! `reports/scope.py` — the `(since, until, label)` triple every period-taking
//! endpoint filters on.
//!
//! Three route modules in batch C take a `period=` and turn it into one of
//! these (`/api/optimize`, `/api/optimize/prescriptions`, and — through
//! `reports/export.py` — `/api/export`), so it lives here rather than being
//! transliterated three times and drifting twice.
//!
//! # What is load-bearing, and what merely looks it
//!
//! * **The bounds are ISO strings, and they are compared as strings.**
//!   `Scope.contains` does `timestamp < self.since` on `str`, not on parsed
//!   datetimes. That is lexicographic ordering over ISO-8601, which agrees with
//!   chronological ordering only while every stamp shares a shape — and the
//!   store's do not (`+00:00`, `Z`, and naive stamps all appear). Ported as
//!   written: the comparison is the contract, not the intent.
//! * **`contains` parses only to reject.** After both string comparisons pass,
//!   Python runs `datetime.fromisoformat(ts.replace("Z", "+00:00"))` purely for
//!   its `ValueError`; the parsed value is discarded. So a malformed stamp
//!   inside the window is excluded, and a malformed stamp outside it was
//!   already excluded by the cheaper test. Same order here.
//! * **`today` and `month` zero the microsecond; `7days`/`30days` do not.**
//!   `current.replace(hour=0, …, microsecond=0)` renders without a fractional
//!   part, while `current - timedelta(days=7)` keeps `current`'s microseconds.
//!   The rendered strings differ in length, and they are compared as strings.
//!
//! # The one thing that cannot be byte-diffed
//!
//! `parse_period` reads the clock. `7days` / `30days` are rolling instants, so
//! the two servers in the differ compute bounds a few milliseconds apart and a
//! message inside that gap is a real (if rare) divergence — the same property
//! `CD-prov-week` already carries in the case file, documented there. `today`,
//! `month` and `all` are stable within a calendar day and diff cleanly.

use std::fmt::Write as _;

/// `_MONTH_NAMES` — used only to build the `month` label.
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `@dataclass(frozen=True) class Scope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Lower bound, inclusive-by-string-compare. `None` is unbounded.
    pub since: Option<String>,
    /// Upper bound, inclusive-by-string-compare. `None` is unbounded.
    pub until: Option<String>,
    /// The human phrase report headers print — and, for `/api/optimize`, the
    /// value that goes out in the response body under `"scope"`.
    pub label: String,
}

impl Scope {
    /// `Scope(since=…, until=…, label=…)`.
    #[must_use]
    pub fn new(since: Option<String>, until: Option<String>, label: impl Into<String>) -> Self {
        Self {
            since,
            until,
            label: label.into(),
        }
    }

    /// `Scope.contains(timestamp)`.
    ///
    /// `if not timestamp: return False` catches `None` *and* `""` — Python's
    /// truthiness, so an empty stamp is out of every scope including an
    /// unbounded one.
    #[must_use]
    pub fn contains(&self, timestamp: Option<&str>) -> bool {
        let Some(timestamp) = timestamp.filter(|ts| !ts.is_empty()) else {
            return false;
        };
        if let Some(since) = &self.since
            && timestamp < since.as_str()
        {
            return false;
        }
        if let Some(until) = &self.until
            && timestamp > until.as_str()
        {
            return false;
        }
        // The parse exists only for its exception; the value is thrown away.
        parse_isoformat(&timestamp.replace('Z', "+00:00")).is_some()
    }
}

/// The instant `parse_period` builds a scope around — injected, never read from
/// a global.
///
/// Python's signature is `parse_period(spec, *, now=None)` with
/// `now or datetime.now(UTC)`; the campaign's injection law (finding 5) makes
/// the explicit form the only form here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instant {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micro: u32,
}

impl Instant {
    /// `datetime.now(UTC)`.
    ///
    /// CPython rounds the clock to microseconds half-to-even before building
    /// the `datetime`; a clock outside years 1..=9999 (a broken RTC) falls back
    /// to the epoch rather than panicking, because a report must not crash on
    /// the system clock.
    #[must_use]
    pub fn now_utc() -> Self {
        let now = std::time::SystemTime::now();
        let (secs, nanos) = match now.duration_since(std::time::UNIX_EPOCH) {
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
            reason = "only the sub-second part is rounded, and it is < 1e9"
        )]
        let micros = round_half_even(nanos as f64 / 1000.0) as i64;
        let (secs, micros) = if micros >= 1_000_000 {
            (secs + 1, micros - 1_000_000)
        } else {
            (secs, micros)
        };
        Self::from_epoch(secs, micros).unwrap_or(Self {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
        })
    }

    /// A pinned instant, for tests and for anything that must be reproducible.
    #[must_use]
    pub const fn from_parts(
        year: i64,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        micro: u32,
    ) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro,
        }
    }

    fn from_epoch(seconds: i64, micros: i64) -> Option<Self> {
        let days = seconds.div_euclid(86_400);
        let secs_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        if !(1..=9999).contains(&year) {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "secs_of_day < 86_400 and micros < 1_000_000 by construction"
        )]
        Some(Self {
            year,
            month,
            day,
            hour: (secs_of_day / 3600) as u32,
            minute: ((secs_of_day % 3600) / 60) as u32,
            second: (secs_of_day % 60) as u32,
            micro: micros as u32,
        })
    }

    fn to_epoch_seconds(self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// `current - timedelta(days=n)` — microseconds are carried, not zeroed.
    fn minus_days(self, days: i64) -> Self {
        let shifted = self.to_epoch_seconds() - days * 86_400;
        Self::from_epoch(shifted, i64::from(self.micro)).unwrap_or(self)
    }

    /// `current.replace(...)` — the fields named are overwritten, the rest kept.
    const fn replace(self, day: u32, hour: u32, minute: u32, second: u32, micro: u32) -> Self {
        Self {
            year: self.year,
            month: self.month,
            day,
            hour,
            minute,
            second,
            micro,
        }
    }

    /// `datetime.isoformat()` for a UTC-aware value: the fraction appears only
    /// when `microsecond` is non-zero, and the offset is always `+00:00`.
    #[must_use]
    pub fn isoformat(self) -> String {
        let Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro,
        } = self;
        let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if micro != 0 {
            let _ = write!(out, ".{micro:06}");
        }
        out.push_str("+00:00");
        out
    }
}

/// `parse_period(spec, now=…)`.
///
/// `Err` is the `ValueError` Python raises — every caller in batch C validates
/// the spec against its own allow-list *before* calling, so this branch is
/// defence in depth, not a live error path. Its message is Python's verbatim
/// anyway, in case a caller ever surfaces it.
///
/// # Errors
/// An unrecognised `spec`.
pub fn parse_period(spec: &str, now: Instant) -> Result<Scope, String> {
    match spec {
        "today" => {
            let start = now.replace(now.day, 0, 0, 0, 0);
            // NOTE: `second=59` and `microsecond=0` — the *minute* is 59 and
            // the hour 23, so `until` is one second short of midnight. A
            // message logged in that last second is outside "today".
            let end = now.replace(now.day, 23, 59, 59, 0);
            Ok(Scope::new(
                Some(start.isoformat()),
                Some(end.isoformat()),
                "today",
            ))
        }
        "7days" => Ok(Scope::new(
            Some(now.minus_days(7).isoformat()),
            Some(now.isoformat()),
            "last 7 days",
        )),
        "30days" => Ok(Scope::new(
            Some(now.minus_days(30).isoformat()),
            Some(now.isoformat()),
            "last 30 days",
        )),
        "month" => {
            let first = now.replace(1, 0, 0, 0, 0);
            let last_day = days_in_month(now.year, now.month);
            let last = now.replace(last_day, 23, 59, 59, 0);
            let name = MONTH_NAMES
                .get((now.month as usize).saturating_sub(1))
                .copied()
                .unwrap_or("");
            let year = now.year;
            Ok(Scope::new(
                Some(first.isoformat()),
                Some(last.isoformat()),
                format!("this month ({name} {year})"),
            ))
        }
        "all" => Ok(Scope::new(None, None, "all time")),
        _ => Err(format!(
            "Unknown period '{spec}'. Valid: today, 7days, 30days, month, all"
        )),
    }
}

// ── the calendar arithmetic `datetime` does in C ─────────────────────────────

/// Howard Hinnant's `civil_from_days` — the algorithm CPython's `datetime` uses.
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

/// The inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `calendar.monthrange(year, month)[1]`.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Python's `round()`: half-to-even, unlike Rust's half-away-from-zero.
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if diff > 0.5 {
        return floor + 1.0;
    }
    if diff < 0.5 {
        return floor;
    }
    if (floor / 2.0).fract() == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

/// The `datetime.fromisoformat` acceptance test `Scope.contains` runs for its
/// exception.
///
/// Deliberately the *narrow* form: the caller only needs "did this raise", and
/// the accepted grammar matches `stax_adapters::pytime`'s documented subset
/// (calendar dates only; the week-date and ordinal-date forms CPython 3.11
/// added are rejected here and appear in no session log).
fn parse_isoformat(text: &str) -> Option<()> {
    let bytes = text.as_bytes();
    let (year, month, day, rest) = if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
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
    if rest.is_empty() {
        return Some(());
    }
    // CPython accepts any single character as the date/time separator.
    let rest = &rest[rest.chars().next()?.len_utf8()..];
    let (clock, offset) = match rest.rfind(['+', '-']) {
        Some(index) => (&rest[..index], Some(&rest[index..])),
        None => (rest, None),
    };
    if let Some(offset) = offset {
        parse_offset(offset)?;
    }
    parse_clock(clock)
}

fn parse_clock(clock: &str) -> Option<()> {
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
    let hour = digits(parts[0])?;
    let minute = parts.get(1).map_or(Some(0), |part| digits(part))?;
    let second = parts.get(2).map_or(Some(0), |part| digits(part))?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(())
}

fn parse_offset(text: &str) -> Option<()> {
    match text.as_bytes().first()? {
        b'+' | b'-' => {}
        _ => return None,
    }
    let body = &text[1..];
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
    if minutes > 59 || seconds > 59 || hours * 3600 + minutes * 60 + seconds >= 24 * 3600 {
        return None;
    }
    Some(())
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-31T12:34:56.789012+00:00 — a Friday in a 31-day month.
    fn pinned() -> Instant {
        Instant::from_parts(2026, 7, 31, 12, 34, 56, 789_012)
    }

    #[test]
    fn today_is_midnight_to_one_second_short_of_it() {
        let scope = parse_period("today", pinned()).expect("known spec");
        assert_eq!(scope.since.as_deref(), Some("2026-07-31T00:00:00+00:00"));
        // 23:59:59, not 23:59:59.999999 — `microsecond=0` is in the replace().
        assert_eq!(scope.until.as_deref(), Some("2026-07-31T23:59:59+00:00"));
        assert_eq!(scope.label, "today");
    }

    #[test]
    fn the_rolling_windows_carry_the_instants_microseconds() {
        let scope = parse_period("7days", pinned()).expect("known spec");
        assert_eq!(
            scope.since.as_deref(),
            Some("2026-07-24T12:34:56.789012+00:00")
        );
        assert_eq!(
            scope.until.as_deref(),
            Some("2026-07-31T12:34:56.789012+00:00")
        );
        assert_eq!(scope.label, "last 7 days");

        // 30 days back from 31 July crosses two month boundaries.
        let scope = parse_period("30days", pinned()).expect("known spec");
        assert_eq!(
            scope.since.as_deref(),
            Some("2026-07-01T12:34:56.789012+00:00")
        );
        assert_eq!(scope.label, "last 30 days");
    }

    #[test]
    fn month_spans_the_calendar_month_and_names_it() {
        let scope = parse_period("month", pinned()).expect("known spec");
        assert_eq!(scope.since.as_deref(), Some("2026-07-01T00:00:00+00:00"));
        assert_eq!(scope.until.as_deref(), Some("2026-07-31T23:59:59+00:00"));
        assert_eq!(scope.label, "this month (July 2026)");

        // February in a non-leap year ends on the 28th; 2026 is not a leap year.
        let feb = Instant::from_parts(2026, 2, 10, 1, 2, 3, 0);
        let scope = parse_period("month", feb).expect("known spec");
        assert_eq!(scope.until.as_deref(), Some("2026-02-28T23:59:59+00:00"));
        assert_eq!(scope.label, "this month (February 2026)");

        // …and on the 29th in one.
        let feb = Instant::from_parts(2024, 2, 10, 1, 2, 3, 0);
        let scope = parse_period("month", feb).expect("known spec");
        assert_eq!(scope.until.as_deref(), Some("2024-02-29T23:59:59+00:00"));
    }

    #[test]
    fn all_is_unbounded_on_both_sides() {
        let scope = parse_period("all", pinned()).expect("known spec");
        assert_eq!(scope.since, None);
        assert_eq!(scope.until, None);
        assert_eq!(scope.label, "all time");
    }

    #[test]
    fn an_unknown_spec_is_the_value_error_message_verbatim() {
        assert_eq!(
            parse_period("week", pinned()).unwrap_err(),
            "Unknown period 'week'. Valid: today, 7days, 30days, month, all"
        );
    }

    #[test]
    fn contains_compares_strings_and_rejects_the_empty_one() {
        let scope = parse_period("month", pinned()).expect("known spec");
        assert!(scope.contains(Some("2026-07-15T08:00:00+00:00")));
        assert!(!scope.contains(Some("2026-06-30T23:59:59+00:00")));
        // Falsy in Python — `if not timestamp` catches "" as well as None.
        assert!(!scope.contains(Some("")));
        assert!(!scope.contains(None));
        // Unbounded still rejects the empty stamp.
        let all = parse_period("all", pinned()).expect("known spec");
        assert!(!all.contains(Some("")));
        assert!(all.contains(Some("1999-01-01T00:00:00+00:00")));
    }

    #[test]
    fn a_malformed_stamp_inside_the_window_is_excluded_by_the_parse() {
        let all = parse_period("all", pinned()).expect("known spec");
        assert!(!all.contains(Some("2026-13-01T00:00:00+00:00")));
        assert!(!all.contains(Some("not a timestamp")));
        // The Z form is normalised before parsing, exactly as Python does.
        assert!(all.contains(Some("2026-07-15T08:00:00Z")));
    }

    #[test]
    fn the_string_compare_is_shape_sensitive_and_that_is_inherited() {
        // A `Z`-suffixed stamp sorts AFTER a `+00:00` one at the same instant
        // ('Z' is 0x5A, '+' is 0x2B), so a scope whose bounds render with
        // `+00:00` silently EXCLUDES the `Z` spelling of its own upper edge —
        // and silently INCLUDES the `Z` spelling of the instant one tick before
        // its lower edge. Python has both; the port has both; neither is fixed
        // here. The store carries all three stamp spellings, so this is live.
        let scope = parse_period("month", pinned()).expect("known spec");
        assert!(!scope.contains(Some("2026-07-31T23:59:59Z")));
        assert!(scope.contains(Some("2026-07-31T23:59:59+00:00")));
        assert!(scope.contains(Some("2026-07-01T00:00:00Z")));
    }
}
