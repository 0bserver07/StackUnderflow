//! `services/plans.py` — a monthly budget, its billing window, and the usage
//! banding the plan widget renders.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `PRESETS` | `PRESETS: dict[str, float \| None]` | [`PRESETS`] |
//! | `Plan` | `@dataclass(frozen=True)` | [`Plan`] |
//! | `get_active_plan` | reads three settings keys | [`get_active_plan`] |
//! | `project_month_end` | linear extrapolation | [`project_month_end`] |
//! | `_period_window` | the billing window | [`period_window`] |
//! | `compute_usage` | `(plan, spend) -> usage dict` | [`compute_usage`] |
//! | `set_plan` / `reset_plan` | settings **writers** | not ported — see below |
//!
//! A *plan* is a monthly USD budget (Claude Pro, Claude Max, …) plus a *reset
//! day* — the day-of-month the billing period rolls over. The dashboard's raw
//! cost figure answers "how much have I spent?"; this module answers "how much
//! is left, and am I tracking to go over?".
//!
//! # What is load-bearing here
//!
//! * **The plan lives in three FILE-ONLY settings keys.** `plan_name`,
//!   `plan_monthly_usd` and `plan_reset_day` are declared `_Opt(default, None)`
//!   — that `None` is the env-var name, so `_Opt.__get__` skips the `os.getenv`
//!   leg entirely and resolves *file → default*. That is why
//!   [`get_active_plan`] takes the parsed `config.json` object and nothing
//!   else: there is no environment leg to miss. `crate::state::Config` does not
//!   carry these three (it carries only the settings the HTTP layer resolved at
//!   startup), and the campaign forbids reaching for a global, so the caller
//!   hands the map in.
//! * **`reset_day` is `s.get("plan_reset_day") or 1`.** Python truthiness, not
//!   `is None`: a persisted `0` — or `false`, or `""` — reads as `1`.
//! * **The window clamps twice.** `min(plan.reset_day, monthrange(...)[1])` is
//!   applied to *this* month to decide which side of the reset we are on, and
//!   again to the *next* (or previous) month to place the other edge. A day-31
//!   reset in February therefore rolls Feb 28 → Mar 31, which the Python
//!   docstring states and is not an accident.
//! * **`days_so_far` has a floor of 1**, so the reset day itself never divides
//!   by zero.
//!
//! # The two clock reads, and why both live on [`Date`]
//!
//! `compute_usage(plan, used, now=None)` defaults `now` to `datetime.now(UTC)`
//! and then immediately takes `now.date()` — so the billing window is anchored
//! on the **UTC** date. `routes/plan.py::_spend_daily_window` anchors its
//! per-day series on `date.today()`, which is the **local** wall-clock date.
//! Those are different dates for seven hours a day on the maintainer's machine
//! (`America/Los_Angeles`, UTC−7), and Python really does use one of each, four
//! call sites apart. Both readers are therefore provided and named for the
//! clock they read: [`Date::today_utc`] and [`Date::today_local`]. Picking one
//! for both would silently "fix" an inconsistency the reference has (DIV-092).
//!
//! # `set_plan` / `reset_plan` are deliberately absent
//!
//! Both are settings *writers* (`Settings.persist` / `Settings.remove`) with no
//! HTTP caller anywhere — `/api/plan` is read-only and there is no
//! `PUT /api/plan`. Their only consumer is `stackunderflow plan set|reset`,
//! which wave 8 owns. Porting them here would put an untested `config.json`
//! writer in a request path that can never reach it. Recorded as DIV-094 rather
//! than written blind.

use std::path::Path;

use serde_json::{Map, Value};

/// `PRESETS` — canonical preset name → monthly USD.
///
/// `custom` maps to `None`: it is the sentinel meaning "the user supplied an
/// arbitrary amount", so it can never resolve a default amount. Written in the
/// Python dict-literal order.
pub const PRESETS: [(&str, Option<f64>); 5] = [
    ("claude-pro", Some(20.0)),
    ("claude-max", Some(200.0)),
    ("cursor-pro", Some(20.0)),
    ("cursor-max", Some(40.0)),
    ("custom", None),
];

/// `@dataclass(frozen=True) class Plan`.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// `name` — a [`PRESETS`] key, or whatever was persisted.
    pub name: String,
    /// `monthly_usd` — the budget, already through `float()`.
    pub monthly_usd: f64,
    /// `reset_day` — day-of-month the window rolls over. Not validated on read;
    /// only `set_plan` clamps it to `[1, 31]`.
    pub reset_day: i64,
}

// ── the calendar `datetime.date` provides ────────────────────────────────────

/// A `datetime.date`, stored as days since the Unix epoch.
///
/// `services/scope.rs` holds the same Hinnant civil-calendar arithmetic
/// privately; it is not re-exported and batch C may not edit that file, so the
/// two exist in parallel. Same algorithm, and the month/leap vectors in the
/// tests below pin them to the same answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    /// Days since 1970-01-01. Chronological order *is* numeric order, which is
    /// what the derived `Ord` relies on.
    epoch_day: i64,
}

impl Date {
    /// `date(year, month, day)`.
    ///
    /// Deliberately **not** validating: Python raises `ValueError` for a day
    /// outside the month, and this rolls into the neighbouring month instead.
    /// The only route to an out-of-range day is a hand-edited negative
    /// `plan_reset_day`, since `_Opt`'s `or 1` already swallows `0` (DIV-094).
    #[must_use]
    pub const fn from_ymd(year: i64, month: u32, day: i64) -> Self {
        Self {
            epoch_day: days_from_civil(year, month, day),
        }
    }

    /// Days since the epoch — `toordinal()` shifted by a constant, and every
    /// use in this batch is a difference or an offset, so the constant cancels.
    #[must_use]
    pub const fn epoch_day(self) -> i64 {
        self.epoch_day
    }

    /// `date.fromordinal(self.toordinal() + days)`.
    #[must_use]
    pub const fn plus_days(self, days: i64) -> Self {
        Self {
            epoch_day: self.epoch_day + days,
        }
    }

    /// `(self - other).days`.
    #[must_use]
    pub const fn days_since(self, other: Self) -> i64 {
        self.epoch_day - other.epoch_day
    }

    /// `(year, month, day)`.
    #[must_use]
    pub const fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(self.epoch_day)
    }

    /// `date.isoformat()` — `YYYY-MM-DD`.
    #[must_use]
    pub fn isoformat(self) -> String {
        let (year, month, day) = self.ymd();
        format!("{year:04}-{month:02}-{day:02}")
    }

    /// `date.fromisoformat(text)`, narrowed to the `YYYY-MM-DD` form.
    ///
    /// Every caller in this batch feeds it a string this module itself rendered
    /// (`window["period_start"]`), so the wider grammar CPython 3.11 added
    /// (week dates, ordinal dates) is unreachable and is not accepted.
    #[must_use]
    pub fn from_isoformat(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        let year: i64 = text.get(0..4)?.parse().ok()?;
        let month: u32 = text.get(5..7)?.parse().ok()?;
        let day: i64 = text.get(8..10)?.parse().ok()?;
        if !(1..=12).contains(&month) || day < 1 || day > i64::from(days_in_month(year, month)) {
            return None;
        }
        Some(Self::from_ymd(year, month, day))
    }

    /// `datetime.now(UTC).date()` — the clock `compute_usage` anchors on.
    #[must_use]
    pub fn today_utc() -> Self {
        Self {
            epoch_day: unix_seconds().div_euclid(86_400),
        }
    }

    /// `date.today()` — the **local wall-clock** date, which is a different
    /// thing (see the module docs and DIV-092).
    ///
    /// The zone comes from `/etc/localtime`, parsed as TZif. `$TZ` is
    /// deliberately not consulted: `bin/stax-server.rs` states that nothing
    /// below it reads the environment, and `/etc/localtime` is precisely what
    /// libc resolves when `$TZ` is unset — the harness's configuration and the
    /// maintainer's. A process running with `$TZ` set to a *different* zone
    /// would diverge; that is DIV-093.
    ///
    /// An unreadable or unparseable zone file degrades to UTC rather than
    /// failing: a plan widget must not 500 because `/etc` is odd.
    #[must_use]
    pub fn today_local() -> Self {
        let now = unix_seconds();
        Self {
            epoch_day: (now + local_utc_offset_seconds(now)).div_euclid(86_400),
        }
    }
}

/// Seconds since the epoch, clamped rather than panicking on a broken RTC.
fn unix_seconds() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
        Err(err) => -i64::try_from(err.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// `calendar.monthrange(year, month)[1]`.
#[must_use]
pub const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's `civil_from_days`, the algorithm CPython's `datetime` uses.
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
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
const fn days_from_civil(year: i64, month: u32, day: i64) -> i64 {
    let month = month as i64;
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ── the zone `date.today()` reads ────────────────────────────────────────────

/// The UTC offset in effect at `at`, in seconds east.
///
/// Zero when the zone cannot be determined, which makes local == UTC — the same
/// answer libc gives for an unset `$TZ` and a missing `/etc/localtime`.
fn local_utc_offset_seconds(at: i64) -> i64 {
    std::fs::read(Path::new("/etc/localtime"))
        .ok()
        .and_then(|bytes| tzif_offset(&bytes, at))
        .unwrap_or(0)
}

/// The six 32-bit counts in a TZif header (RFC 8536 §3.1).
struct TzifCounts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

/// Resolve the UTC offset at `at` out of a TZif file.
///
/// Only what is needed to answer "what is the offset right now" is read: the
/// transition list, the transition-type index, and the `ttinfo` records. The
/// footer's POSIX TZ string — which extrapolates *past* the last transition —
/// is not evaluated. Every zone file tzdata ships carries transitions through
/// 2037, so the footer is unreachable for any plausible clock, and a
/// hand-rolled POSIX-rule evaluator is a large surface for a branch no test can
/// reach.
fn tzif_offset(data: &[u8], at: i64) -> Option<i64> {
    let mut base = 0usize;
    let mut width = 4usize;
    let mut counts = tzif_counts(data, base)?;

    // A version-2+ file repeats the whole thing with 64-bit transition times.
    // The second block is the authoritative one; the first exists only for
    // readers that predate it, and `zic -b slim` leaves it empty.
    if data.get(4).copied().unwrap_or(0) >= b'2' {
        let next = base + 44 + tzif_block_len(&counts, width);
        if data.get(next..next.saturating_add(4)) == Some(b"TZif") {
            counts = tzif_counts(data, next)?;
            base = next;
            width = 8;
        }
    }

    let transitions = base + 44;
    let indices = transitions + counts.timecnt * width;
    let ttinfo = indices + counts.timecnt;
    if counts.typecnt == 0 || data.len() < ttinfo + counts.typecnt * 6 {
        return None;
    }

    // The last transition at or before `at`. Walked rather than bisected: the
    // list is a few hundred entries and the linear form is the one a reader can
    // check against the RFC.
    let mut chosen: Option<usize> = None;
    for index in 0..counts.timecnt {
        let start = transitions + index * width;
        let when = read_be_signed(data.get(start..start + width)?)?;
        if when > at {
            break;
        }
        chosen = Some(index);
    }

    let type_index = match chosen {
        Some(index) => usize::from(*data.get(indices + index)?),
        // RFC 8536: before the first transition, use the first non-DST record;
        // if every record is DST, use the first record.
        None => (0..counts.typecnt)
            .find(|index| data.get(ttinfo + index * 6 + 4).copied() == Some(0))
            .unwrap_or(0),
    };
    if type_index >= counts.typecnt {
        return None;
    }
    let start = ttinfo + type_index * 6;
    read_be_signed(data.get(start..start + 4)?)
}

/// Read the six counts at `offset`, checking the magic first.
fn tzif_counts(data: &[u8], offset: usize) -> Option<TzifCounts> {
    if data.get(offset..offset.checked_add(4)?)? != b"TZif" {
        return None;
    }
    let field = |n: usize| -> Option<usize> {
        let start = offset + 20 + n * 4;
        let raw: [u8; 4] = data.get(start..start + 4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(raw)).ok()
    };
    Some(TzifCounts {
        isutcnt: field(0)?,
        isstdcnt: field(1)?,
        leapcnt: field(2)?,
        timecnt: field(3)?,
        typecnt: field(4)?,
        charcnt: field(5)?,
    })
}

/// The size of one data block, so the version-1 block can be skipped whole.
///
/// A leap-second record is `time_width + 4` bytes: the occurrence time in the
/// block's time width, then a 4-byte correction.
const fn tzif_block_len(counts: &TzifCounts, time_width: usize) -> usize {
    counts.timecnt * time_width
        + counts.timecnt
        + counts.typecnt * 6
        + counts.charcnt
        + counts.leapcnt * (time_width + 4)
        + counts.isstdcnt
        + counts.isutcnt
}

/// A big-endian two's-complement integer of 4 or 8 bytes.
fn read_be_signed(bytes: &[u8]) -> Option<i64> {
    match bytes.len() {
        4 => Some(i64::from(i32::from_be_bytes(bytes.try_into().ok()?))),
        8 => Some(i64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

// ── settings I/O ─────────────────────────────────────────────────────────────

/// A persisted plan value that Python's `str()` / `float()` / `int()` would
/// raise on.
///
/// `get_active_plan` has no `try`, so in Python this is an uncaught exception
/// out of an `async def` handler — a plain-text `500 Internal Server Error`
/// from the error middleware, not a `{"detail": …}` body. The port surfaces it
/// as a JSON 500 instead; that shape difference is DIV-094, and reaching it
/// needs a hand-edited `config.json`.
#[derive(Debug, Clone)]
pub struct PlanConfigError {
    /// The `config.json` key that would not coerce.
    pub key: &'static str,
}

impl std::fmt::Display for PlanConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in config.json is not a usable value", self.key)
    }
}

impl std::error::Error for PlanConfigError {}

/// `get_active_plan()` — the plan from settings, or `None` if no plan is set.
///
/// `config` is the parsed `config.json` object (`settings._load()`); an absent
/// or corrupt file is an empty map there and here, never an error.
///
/// # Errors
/// A persisted value Python's `str()` / `float()` / `int()` would reject — see
/// [`PlanConfigError`].
pub fn get_active_plan(config: &Map<String, Value>) -> Result<Option<Plan>, PlanConfigError> {
    // `s.get("plan_name")` — declared `_Opt(None, None)`, so file → default,
    // and the default is `None`. A JSON `null` in the file is the same `None`.
    let name = config.get("plan_name").filter(|value| !value.is_null());
    let monthly = config
        .get("plan_monthly_usd")
        .filter(|value| !value.is_null());
    // `if name is None or monthly is None: return None`
    let (Some(name), Some(monthly)) = (name, monthly) else {
        return Ok(None);
    };

    // `reset_day = s.get("plan_reset_day") or 1` — TRUTHINESS. A persisted `0`,
    // `false` or `""` reads as 1, where an `is None` test would keep it.
    let reset_day = config
        .get("plan_reset_day")
        .filter(|value| py_truthy(value))
        .cloned()
        .unwrap_or_else(|| Value::from(1));

    Ok(Some(Plan {
        name: py_str(name).ok_or(PlanConfigError { key: "plan_name" })?,
        monthly_usd: py_float(monthly).ok_or(PlanConfigError {
            key: "plan_monthly_usd",
        })?,
        reset_day: py_int(&reset_day).ok_or(PlanConfigError {
            key: "plan_reset_day",
        })?,
    }))
}

/// Python truthiness over a JSON value — `bool(value)`.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        // `bool(0)`, `bool(0.0)` and `bool(-0.0)` are all False; NaN is True.
        Value::Number(number) => number.as_f64().is_none_or(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `str(value)` for the scalar shapes a settings file can hold.
///
/// A list or an object would stringify to Python's `repr` (`"[1]"`,
/// `"{'a': 1}"`) — a notation nothing else in this crate emits, for a plan name
/// that cannot arise from `set_plan`. Refused instead of approximated.
fn py_str(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        // `str(True)` is "True", not "true".
        Value::Bool(flag) => Some(if *flag { "True" } else { "False" }.to_owned()),
        // serde keeps the int/float split the JSON text had, and so does
        // `str()`: `str(5)` is "5" while `str(5.0)` is "5.0".
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// `float(value)`.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        // `float("  20.5 ")` works. `float("20_000")` does too, and Rust's
        // parser rejects the underscore — a spelling no writer in the tree
        // produces.
        Value::String(text) => text.trim().parse::<f64>().ok(),
        // `float(True)` is 1.0 — a bool IS an int in Python.
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        _ => None,
    }
}

/// `int(value)` — truncation toward zero for a float, base-10 only for a string.
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            // `int(3.7)` is 3 and `int(-3.7)` is -3: toward zero, not floor.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a reset day outside i64 is already nonsense; saturating is the safe read"
            )]
            number.as_f64().map(|value| value.trunc() as i64)
        }),
        // `int("3.7")` raises — the string form takes integers only.
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

// ── projection + usage math ──────────────────────────────────────────────────

/// `project_month_end(daily_burn, days_left)` — the *delta*, not the total.
///
/// A simple linear extrapolation; the caller adds `used` to it. Both guards are
/// `<= 0`, so a zero burn and a zero-days-left window both yield exactly `0.0`
/// rather than a signed zero or a NaN.
#[must_use]
pub fn project_month_end(daily_burn: f64, days_left: i64) -> f64 {
    if daily_burn <= 0.0 || days_left <= 0 {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "float(days_left) — days in a billing window, far under 2^53"
    )]
    let days = days_left as f64;
    daily_burn * days
}

/// `_period_window(plan, now=…)` → `(start, end, days_so_far, days_in_period)`.
///
/// `today` is `now.date()` — see the module docs on which clock that is.
#[must_use]
pub fn period_window(plan: &Plan, today: Date) -> (Date, Date, i64, i64) {
    let (year, month, day) = today.ymd();

    let last_day_this_month = i64::from(days_in_month(year, month));
    let reset_clamped = plan.reset_day.min(last_day_this_month);

    let (period_start, period_end) = if i64::from(day) >= reset_clamped {
        // Inside the window that started on THIS month's reset day.
        let start = Date::from_ymd(year, month, reset_clamped);
        let (n_year, n_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let last_day_next_month = i64::from(days_in_month(n_year, n_month));
        let next_reset_clamped = plan.reset_day.min(last_day_next_month);
        let next_reset = Date::from_ymd(n_year, n_month, next_reset_clamped);
        // `period_end` is one day before the next reset, inclusive.
        (start, next_reset.plus_days(-1))
    } else {
        // Inside the window that started on LAST month's reset day.
        let (p_year, p_month) = if month == 1 {
            (year - 1, 12)
        } else {
            (year, month - 1)
        };
        let last_day_prev_month = i64::from(days_in_month(p_year, p_month));
        let prev_reset_clamped = plan.reset_day.min(last_day_prev_month);
        (
            Date::from_ymd(p_year, p_month, prev_reset_clamped),
            Date::from_ymd(year, month, reset_clamped).plus_days(-1),
        )
    };

    let days_in_period = period_end.days_since(period_start) + 1;
    // `max(1, …)` — the floor that keeps the reset day itself from dividing by
    // zero downstream.
    let days_so_far = 1.max(today.days_since(period_start) + 1);
    (period_start, period_end, days_so_far, days_in_period)
}

/// The dict `compute_usage` returns. Field order is the dict-literal order.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// `used` — the period total handed in, through `float()`.
    pub used: f64,
    /// `budget` — `plan.monthly_usd`, through `float()`.
    pub budget: f64,
    /// `remaining` — `budget - used`; negative when over.
    pub remaining: f64,
    /// `pct` — `100 * used / budget`, or `0.0` when the budget is not positive.
    pub pct: f64,
    /// `projected_month_end` — `used` plus the linear tail.
    pub projected_month_end: f64,
    /// `status` — `"ok"` / `"warn"` / `"over"`.
    pub status: &'static str,
    /// `period_start` — ISO date string.
    pub period_start: String,
    /// `period_end` — ISO date string, inclusive.
    pub period_end: String,
    /// `days_so_far` — inclusive, floor 1.
    pub days_so_far: i64,
    /// `days_in_period` — total days in the window.
    pub days_in_period: i64,
}

/// `compute_usage(plan, total_usd_this_period, now=…)`.
#[must_use]
pub fn compute_usage(plan: &Plan, total_usd_this_period: f64, today: Date) -> Usage {
    let used = total_usd_this_period;
    let budget = plan.monthly_usd;

    let (period_start, period_end, days_so_far, days_in_period) = period_window(plan, today);

    #[allow(
        clippy::cast_precision_loss,
        reason = "used / days_so_far is Python's true division, and days_so_far is small"
    )]
    let days_so_far_f = days_so_far as f64;
    let pct = if budget > 0.0 {
        100.0 * used / budget
    } else {
        0.0
    };
    // `days_so_far > 0` is always true after the `max(1, …)` floor; kept because
    // the guard is in the Python and a reader should see the same shape.
    let daily_burn = if days_so_far > 0 {
        used / days_so_far_f
    } else {
        0.0
    };
    let days_left = 0.max(days_in_period - days_so_far);
    let projected = used + project_month_end(daily_burn, days_left);

    // Order matters: `> 100` wins, then `>= 80`. A pct of exactly 100 is "warn",
    // not "over".
    let status = if pct > 100.0 {
        "over"
    } else if pct >= 80.0 {
        "warn"
    } else {
        "ok"
    };

    Usage {
        used,
        budget,
        remaining: budget - used,
        pct,
        projected_month_end: projected,
        status,
        period_start: period_start.isoformat(),
        period_end: period_end.isoformat(),
        days_so_far,
        days_in_period,
    }
}

// ── the spend window both the route and the CLI read ─────────────────────────
//
// WAVE 8 TRANCHE 3. In Python these two live in `routes/plan.py` and the CLI
// imports them (`cli.py::_resolve_period_daily_costs` does
// `from stackunderflow.routes.plan import _spend_daily_window`). The Rust port
// had them PRIVATE inside `stax_server::routes::plan`, which is unreachable from
// `stax-cli` now that the CLI does not depend on the server crate — so they moved
// here, to the one crate both consumers share, and the route delegates. One
// owner per helper: the alternative was a second per-day rollup in the CLI, and
// a second rollup is a second answer to "what did I spend on Tuesday".

/// The `(since, until)` pair both spend halves share.
///
/// ```python
/// since = datetime.combine(start_d, datetime.min.time()).isoformat()
/// until = datetime.combine(end_d + timedelta(days=1), datetime.min.time()).isoformat()
/// ```
///
/// `datetime.min.time()` is midnight and the result is **naive** — no `+00:00`
/// suffix — so the strings are ten characters plus `T00:00:00`. Those bounds are
/// compared as strings against a `ts` column that holds `+00:00` and `Z` forms
/// alike; `"…T00:00:00" < "…T00:00:00+00:00"` lexicographically, which makes the
/// lower bound slightly permissive and the (half-open) upper bound slightly
/// strict. Inherited, not corrected.
///
/// `None` when either date is not an ISO date — the callers turn that into
/// their own error shape (a 500 in the route, a raised `ValueError` in the CLI).
#[must_use]
pub fn window_bounds(period_start: &str, period_end: &str) -> Option<(String, String)> {
    let start_d = Date::from_isoformat(period_start)?;
    let end_d = Date::from_isoformat(period_end)?;
    Some((
        format!("{}T00:00:00", start_d.isoformat()),
        format!("{}T00:00:00", end_d.plus_days(1).isoformat()),
    ))
}

/// `_spend_daily_window` — per-day USD across every project, oldest-first.
///
/// Days with no recorded spend are `0.0`, not elided: a quiet weekend should
/// drag the weighted average down rather than vanish from the series. The query
/// hits `usage_events` only — there is deliberately **no** `messages` fallback
/// here, so on a pre-backfill store the list is all zeroes and the projector
/// degrades to "no data → linear projection of 0".
///
/// The walk runs `start_d → min(end_d, date.today())`, and `date.today()` is
/// the LOCAL date (DIV-092). The last element is therefore today's spend, which
/// is the orientation `weighted_projection` assumes.
///
/// # Errors
/// Any SQLite error from the one `GROUP BY` — `usage_events` missing is one,
/// and the reference raises there too.
pub fn spend_daily_window(
    conn: &rusqlite::Connection,
    period_start: &str,
    period_end: &str,
    since: &str,
    until: &str,
) -> rusqlite::Result<Vec<f64>> {
    let mut by_day: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT substr(ts, 1, 10) AS day, SUM(cost_usd) AS cost \
             FROM usage_events WHERE ts >= ? AND ts < ? \
             GROUP BY day ORDER BY day",
        )?;
        let mut rows = stmt.query([since, until])?;
        while let Some(row) = rows.next()? {
            let day: String = row.get(0)?;
            // `float(r["cost"] or 0.0)` — a NULL SUM and a real 0.0 agree.
            let cost: Option<f64> = row.get(1)?;
            by_day.insert(day, cost.unwrap_or(0.0));
        }
    }

    let (Some(start_d), Some(end_d)) = (
        Date::from_isoformat(period_start),
        Date::from_isoformat(period_end),
    ) else {
        return Ok(Vec::new());
    };
    let last_day = end_d.min(Date::today_local());

    let mut out: Vec<f64> = Vec::new();
    let mut cursor = start_d;
    while cursor <= last_day {
        out.push(by_day.get(&cursor.isoformat()).copied().unwrap_or(0.0));
        cursor = cursor.plus_days(1);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(monthly: f64, reset_day: i64) -> Plan {
        Plan {
            name: "claude-max".to_owned(),
            monthly_usd: monthly,
            reset_day,
        }
    }

    fn cfg(json: Value) -> Map<String, Value> {
        match json {
            Value::Object(map) => map,
            _ => panic!("object"),
        }
    }

    #[test]
    fn a_zero_reset_day_reads_as_one_because_python_tests_truthiness() {
        // `s.get("plan_reset_day") or 1`. An `is None` test would keep the 0
        // and then `date(y, m, 0)` would raise.
        for falsy in [
            serde_json::json!(0),
            serde_json::json!(false),
            serde_json::json!(""),
        ] {
            let config = cfg(serde_json::json!({
                "plan_name": "claude-pro", "plan_monthly_usd": 20, "plan_reset_day": falsy,
            }));
            let found = get_active_plan(&config).expect("coerces").expect("set");
            assert_eq!(found.reset_day, 1);
        }
    }

    #[test]
    fn either_missing_half_means_no_plan_at_all() {
        assert!(
            get_active_plan(&cfg(serde_json::json!({})))
                .expect("empty")
                .is_none()
        );
        assert!(
            get_active_plan(&cfg(serde_json::json!({"plan_name": "claude-pro"})))
                .expect("coerces")
                .is_none()
        );
        assert!(
            get_active_plan(&cfg(serde_json::json!({"plan_monthly_usd": 20.0})))
                .expect("coerces")
                .is_none()
        );
        // An explicit JSON null is Python's None, not a value.
        assert!(
            get_active_plan(&cfg(
                serde_json::json!({"plan_name": null, "plan_monthly_usd": 20.0})
            ))
            .expect("coerces")
            .is_none()
        );
    }

    #[test]
    fn the_three_values_go_through_str_float_and_int() {
        // `Plan(name=str(name), monthly_usd=float(monthly), reset_day=int(reset_day))`
        // — an integer amount becomes a FLOAT, which is what makes the response
        // render `20.0` and not `20`.
        let config = cfg(serde_json::json!({
            "plan_name": "claude-pro", "plan_monthly_usd": 20, "plan_reset_day": 15,
        }));
        let found = get_active_plan(&config).expect("coerces").expect("set");
        assert_eq!(found.monthly_usd, 20.0);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::from(found.monthly_usd)),
            "20.0"
        );

        // A numeric string is accepted by `float()`, `str(7)` is "7", and
        // `int(3.9)` truncates rather than rounds.
        let config = cfg(serde_json::json!({
            "plan_name": 7, "plan_monthly_usd": "20.5", "plan_reset_day": 3.9,
        }));
        let found = get_active_plan(&config).expect("coerces").expect("set");
        assert_eq!(found.name, "7");
        assert_eq!(found.monthly_usd, 20.5);
        assert_eq!(found.reset_day, 3);
    }

    #[test]
    fn an_uncoercible_amount_is_an_error_and_not_a_silent_no_plan() {
        let config = cfg(serde_json::json!({
            "plan_name": "claude-pro", "plan_monthly_usd": "twenty",
        }));
        let err = get_active_plan(&config).expect_err("float('twenty') raises");
        assert_eq!(err.key, "plan_monthly_usd");
    }

    #[test]
    fn a_mid_month_reset_day_puts_today_in_this_months_window() {
        // reset_day 15, today 2026-07-31 → window is 15 Jul .. 14 Aug.
        let (start, end, so_far, total) =
            period_window(&plan(200.0, 15), Date::from_ymd(2026, 7, 31));
        assert_eq!(start.isoformat(), "2026-07-15");
        assert_eq!(end.isoformat(), "2026-08-14");
        assert_eq!(so_far, 17);
        assert_eq!(total, 31);
    }

    #[test]
    fn a_day_before_the_reset_falls_back_into_last_months_window() {
        // reset_day 15, today 2026-07-03 → window is 15 Jun .. 14 Jul.
        let (start, end, so_far, total) =
            period_window(&plan(200.0, 15), Date::from_ymd(2026, 7, 3));
        assert_eq!(start.isoformat(), "2026-06-15");
        assert_eq!(end.isoformat(), "2026-07-14");
        assert_eq!(so_far, 19);
        assert_eq!(total, 30);
    }

    #[test]
    fn a_day_31_reset_clamps_into_february_and_rolls_back_out_to_march() {
        // The case the Python docstring calls out: "a Jan 31 reset-day on a
        // Feb 28 month rolls Feb 28 -> Mar 31."
        let (start, end, so_far, total) =
            period_window(&plan(200.0, 31), Date::from_ymd(2026, 2, 28));
        assert_eq!(start.isoformat(), "2026-02-28");
        assert_eq!(end.isoformat(), "2026-03-30");
        assert_eq!(so_far, 1);
        assert_eq!(total, 31);

        // One day earlier is still January's window, and its end is the day
        // before February's CLAMPED reset (the 28th), not before the 31st.
        let (start, end, _, _) = period_window(&plan(200.0, 31), Date::from_ymd(2026, 2, 27));
        assert_eq!(start.isoformat(), "2026-01-31");
        assert_eq!(end.isoformat(), "2026-02-27");
    }

    #[test]
    fn a_december_window_rolls_the_year_over_in_both_directions() {
        let (start, end, _, total) = period_window(&plan(20.0, 1), Date::from_ymd(2026, 12, 9));
        assert_eq!(start.isoformat(), "2026-12-01");
        assert_eq!(end.isoformat(), "2026-12-31");
        assert_eq!(total, 31);

        // …and a January day before the reset walks back into December.
        let (start, end, _, _) = period_window(&plan(20.0, 15), Date::from_ymd(2026, 1, 2));
        assert_eq!(start.isoformat(), "2025-12-15");
        assert_eq!(end.isoformat(), "2026-01-14");
    }

    #[test]
    fn the_reset_day_itself_has_days_so_far_one_not_zero() {
        let (_, _, so_far, _) = period_window(&plan(20.0, 1), Date::from_ymd(2026, 7, 1));
        assert_eq!(so_far, 1);
    }

    #[test]
    fn the_status_bands_are_open_above_and_closed_below() {
        // pct > 100 -> over; 80 <= pct <= 100 -> warn; else ok. EXACTLY 100 is
        // "warn", which a `>=` in the wrong place would get backwards.
        let today = Date::from_ymd(2026, 7, 15);
        assert_eq!(compute_usage(&plan(100.0, 1), 79.99, today).status, "ok");
        assert_eq!(compute_usage(&plan(100.0, 1), 80.0, today).status, "warn");
        assert_eq!(compute_usage(&plan(100.0, 1), 100.0, today).status, "warn");
        assert_eq!(compute_usage(&plan(100.0, 1), 100.01, today).status, "over");
    }

    #[test]
    fn a_zero_budget_pins_pct_to_zero_and_never_divides() {
        let usage = compute_usage(&plan(0.0, 1), 42.0, Date::from_ymd(2026, 7, 15));
        assert_eq!(usage.pct, 0.0);
        assert_eq!(usage.status, "ok");
        assert_eq!(usage.remaining, -42.0);
    }

    #[test]
    fn the_projection_is_used_plus_burn_times_the_days_left() {
        // 2026-07-15 with a day-1 reset: 15 of 31 days gone, 16 left.
        let usage = compute_usage(&plan(200.0, 1), 30.0, Date::from_ymd(2026, 7, 15));
        assert_eq!(usage.days_so_far, 15);
        assert_eq!(usage.days_in_period, 31);
        assert_eq!(usage.projected_month_end, 30.0 + (30.0 / 15.0) * 16.0);
        assert_eq!(usage.period_start, "2026-07-01");
        assert_eq!(usage.period_end, "2026-07-31");
    }

    #[test]
    fn the_last_day_of_a_window_projects_no_tail_at_all() {
        // days_left is max(0, …), so the final day adds nothing rather than
        // subtracting a day's burn.
        let usage = compute_usage(&plan(200.0, 1), 30.0, Date::from_ymd(2026, 7, 31));
        assert_eq!(usage.days_so_far, 31);
        assert_eq!(usage.projected_month_end, 30.0);
    }

    #[test]
    fn project_month_end_is_the_delta_and_clamps_both_arguments() {
        assert_eq!(project_month_end(10.0, 3), 30.0);
        assert_eq!(project_month_end(0.0, 3), 0.0);
        assert_eq!(project_month_end(-5.0, 3), 0.0);
        assert_eq!(project_month_end(10.0, 0), 0.0);
        assert_eq!(project_month_end(10.0, -1), 0.0);
    }

    #[test]
    fn the_calendar_round_trips_across_a_month_and_a_leap_boundary() {
        assert_eq!(
            Date::from_ymd(2026, 2, 28).plus_days(1).isoformat(),
            "2026-03-01"
        );
        assert_eq!(
            Date::from_ymd(2024, 2, 28).plus_days(1).isoformat(),
            "2024-02-29"
        );
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(
            Date::from_isoformat("2026-07-31").expect("parses").ymd(),
            (2026, 7, 31)
        );
        assert!(Date::from_isoformat("2026-02-30").is_none());
        assert!(Date::from_isoformat("2026-7-31").is_none());
        assert_eq!(
            Date::from_ymd(2026, 8, 1).days_since(Date::from_ymd(2026, 7, 1)),
            31
        );
        assert_eq!(Date::from_ymd(1970, 1, 1).epoch_day(), 0);
    }

    #[test]
    fn the_two_clocks_are_within_a_day_of_each_other() {
        // Both read the same instant; they can legitimately differ by one day
        // (that is DIV-092), but never by more.
        let drift = Date::today_local().days_since(Date::today_utc());
        assert!((-1..=1).contains(&drift), "local vs utc drift {drift}");
    }

    #[test]
    fn a_junk_zone_file_degrades_to_utc_instead_of_failing() {
        assert!(tzif_offset(b"not a zone file", 0).is_none());
        assert!(tzif_offset(b"", 0).is_none());
        assert!(tzif_offset(b"TZif2", 0).is_none());
    }

    #[test]
    fn the_tzif_reader_finds_the_transition_covering_an_instant() {
        // A hand-built version-1 file: two types (+0s and +3600s) and one
        // transition at t=100 into the second type.
        let mut file = Vec::new();
        file.extend_from_slice(b"TZif");
        file.push(0); // version 1
        file.extend_from_slice(&[0u8; 15]);
        for count in [0u32, 0, 0, 1, 2, 0] {
            file.extend_from_slice(&count.to_be_bytes());
        }
        file.extend_from_slice(&100_i32.to_be_bytes()); // transition time
        file.push(1); // …into type 1
        file.extend_from_slice(&0_i32.to_be_bytes()); // type 0: utoff 0
        file.extend_from_slice(&[0, 0]); //             isdst 0, abbr 0
        file.extend_from_slice(&3600_i32.to_be_bytes()); // type 1: utoff +1h
        file.extend_from_slice(&[1, 0]); //                isdst 1, abbr 0

        // Before the transition: the first NON-DST record, per RFC 8536.
        assert_eq!(tzif_offset(&file, 0), Some(0));
        assert_eq!(tzif_offset(&file, 99), Some(0));
        // At and after it: the transition's own type.
        assert_eq!(tzif_offset(&file, 100), Some(3600));
        assert_eq!(tzif_offset(&file, 10_000), Some(3600));
    }

    #[test]
    fn the_real_zone_file_parses_if_the_platform_has_one() {
        // Not an assertion about WHICH zone — the harness runs in
        // America/Los_Angeles and CI may not — only that a whole real TZif file
        // yields a plausible offset rather than the silent UTC fallback.
        let Ok(bytes) = std::fs::read("/etc/localtime") else {
            return;
        };
        if let Some(offset) = tzif_offset(&bytes, unix_seconds()) {
            assert!(
                (-50_400..=50_400).contains(&offset),
                "implausible offset {offset}"
            );
        }
    }

    #[test]
    fn the_presets_carry_custom_as_the_amountless_sentinel() {
        assert_eq!(PRESETS.len(), 5);
        assert_eq!(
            PRESETS.iter().find(|(name, _)| *name == "custom"),
            Some(&("custom", None))
        );
        assert_eq!(
            PRESETS.iter().find(|(name, _)| *name == "claude-max"),
            Some(&("claude-max", Some(200.0)))
        );
    }
}
